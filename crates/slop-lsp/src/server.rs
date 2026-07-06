//! Minimal LSP server over stdio. LSP framing is `Content-Length: N\r\n\r\n`
//! headers followed by an N-byte JSON-RPC body (unlike MCP's newline-delimited
//! framing). Synchronous and dependency-light, matching the rest of the
//! workspace. Only the method set an editor needs to surface diagnostics is
//! implemented; other requests get a JSON-RPC "method not found".

use std::collections::HashSet;
use std::io::{BufRead, Write};
use std::path::PathBuf;

use serde_json::{json, Value};

use crate::diagnostics;

/// What the server analyzes: the repo root and an optional explicit index path.
pub struct LspCtx {
    pub repo: PathBuf,
    pub index: Option<PathBuf>,
}

/// Serve the LSP protocol on the given reader/writer until `exit` (or EOF).
/// Split from `serve_stdio` so tests can drive it with in-memory buffers.
pub fn serve<R: BufRead, W: Write>(ctx: &LspCtx, mut input: R, mut output: W) -> anyhow::Result<()> {
    // URIs we have open, and the URIs we last published non-empty diagnostics
    // for — together they tell us which files to clear on the next analysis.
    let mut open: HashSet<String> = HashSet::new();
    let mut published: HashSet<String> = HashSet::new();

    while let Some(msg) = read_message(&mut input)? {
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let id = msg.get("id").cloned();
        match method {
            "initialize" => {
                reply(&mut output, id, initialize_result())?;
            }
            "shutdown" => {
                reply(&mut output, id, Value::Null)?;
            }
            "exit" => break,
            "textDocument/didOpen" => {
                if let Some(uri) = doc_uri(&msg) {
                    open.insert(uri);
                }
                analyze_and_publish(ctx, &open, &mut published, &mut output)?;
            }
            "textDocument/didSave" => {
                analyze_and_publish(ctx, &open, &mut published, &mut output)?;
            }
            "textDocument/didClose" => {
                if let Some(uri) = doc_uri(&msg) {
                    open.remove(&uri);
                    // The editor drops diagnostics for a closed file on its own,
                    // but publishing empty keeps our `published` set honest.
                    publish(&mut output, &uri, &[])?;
                    published.remove(&uri);
                }
            }
            // Notifications we accept but don't act on.
            "initialized" | "textDocument/didChange" | "$/setTrace" => {}
            _ => {
                // Unknown *requests* (those with an id) get an error; unknown
                // notifications are silently ignored, per JSON-RPC.
                if id.is_some() {
                    error(&mut output, id, -32601, &format!("method not found: {method}"))?;
                }
            }
        }
    }
    Ok(())
}

/// The `initialize` result: advertise that we want open/close and save
/// notifications (we re-read from disk, so we analyze on save, not on every
/// keystroke).
fn initialize_result() -> Value {
    json!({
        "capabilities": {
            "textDocumentSync": { "openClose": true, "change": 1, "save": true },
        },
        "serverInfo": { "name": "slop", "version": env!("CARGO_PKG_VERSION") },
    })
}

/// Run the check and publish diagnostics. Clears any file that had diagnostics
/// (or is open) but no longer does, then publishes the current set. On analysis
/// failure — most often a missing/empty SCIP index — surface a `window/showMessage`
/// warning instead of silently going dark.
fn analyze_and_publish<W: Write>(
    ctx: &LspCtx,
    open: &HashSet<String>,
    published: &mut HashSet<String>,
    output: &mut W,
) -> anyhow::Result<()> {
    match diagnostics::compute(&ctx.repo, ctx.index.as_deref()) {
        Ok(by_uri) => {
            let current: HashSet<String> = by_uri.keys().cloned().collect();
            // Anything previously flagged or currently open but now clean gets
            // an explicit empty publish so the editor clears its squiggles.
            for uri in published.iter().chain(open.iter()) {
                if !current.contains(uri) {
                    publish(output, uri, &[])?;
                }
            }
            for (uri, diags) in &by_uri {
                publish(output, uri, diags)?;
            }
            *published = current;
        }
        Err(e) => {
            show_message(output, format!("slop: {e:#}"))?;
        }
    }
    Ok(())
}

/// Extract `params.textDocument.uri` from a document notification.
fn doc_uri(msg: &Value) -> Option<String> {
    msg.get("params")?
        .get("textDocument")?
        .get("uri")?
        .as_str()
        .map(str::to_string)
}

fn publish<W: Write>(output: &mut W, uri: &str, diagnostics: &[Value]) -> anyhow::Result<()> {
    notify(
        output,
        "textDocument/publishDiagnostics",
        json!({ "uri": uri, "diagnostics": diagnostics }),
    )
}

/// A `window/showMessage` warning (type 2 = Warning).
fn show_message<W: Write>(output: &mut W, message: String) -> anyhow::Result<()> {
    notify(output, "window/showMessage", json!({ "type": 2, "message": message }))
}

fn reply<W: Write>(output: &mut W, id: Option<Value>, result: Value) -> anyhow::Result<()> {
    write_message(
        output,
        &json!({ "jsonrpc": "2.0", "id": id.unwrap_or(Value::Null), "result": result }),
    )
}

fn error<W: Write>(
    output: &mut W,
    id: Option<Value>,
    code: i64,
    message: &str,
) -> anyhow::Result<()> {
    write_message(
        output,
        &json!({ "jsonrpc": "2.0", "id": id.unwrap_or(Value::Null), "error": { "code": code, "message": message } }),
    )
}

fn notify<W: Write>(output: &mut W, method: &str, params: Value) -> anyhow::Result<()> {
    write_message(output, &json!({ "jsonrpc": "2.0", "method": method, "params": params }))
}

/// Write one LSP message: `Content-Length` header, blank line, JSON body.
pub fn write_message<W: Write>(output: &mut W, value: &Value) -> anyhow::Result<()> {
    let body = serde_json::to_vec(value)?;
    write!(output, "Content-Length: {}\r\n\r\n", body.len())?;
    output.write_all(&body)?;
    output.flush()?;
    Ok(())
}

/// Read one LSP message: parse `Content-Length` from the header block, then
/// read exactly that many body bytes. Returns `None` at EOF (clean shutdown).
pub fn read_message<R: BufRead>(input: &mut R) -> anyhow::Result<Option<Value>> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            return Ok(None); // EOF
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break; // end of headers
        }
        if let Some(rest) = trimmed
            .strip_prefix("Content-Length:")
            .or_else(|| trimmed.strip_prefix("content-length:"))
        {
            content_length = rest.trim().parse().ok();
        }
    }
    let len = content_length.ok_or_else(|| anyhow::anyhow!("message missing Content-Length"))?;
    let mut body = vec![0u8; len];
    input.read_exact(&mut body)?;
    Ok(Some(serde_json::from_slice(&body)?))
}
