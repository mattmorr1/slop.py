//! Hand-rolled MCP stdio server: newline-delimited JSON-RPC 2.0 (the MCP
//! stdio framing — one JSON object per line, no Content-Length headers).
//! Synchronous and dependency-light, matching the rest of the workspace
//! (no tokio). Only the method set Claude Code needs is implemented;
//! everything else returns a JSON-RPC "method not found".

use std::io::{BufRead, Write};

use serde_json::{json, Value};

use crate::tools::{self, ToolCtx};

const PROTOCOL_VERSION: &str = "2025-06-18";

/// Serve the MCP protocol on the given reader/writer until EOF. Split from
/// `serve_stdio` so tests can drive it with in-memory buffers.
pub fn serve<R: BufRead, W: Write>(ctx: &ToolCtx, input: R, mut output: W) -> anyhow::Result<()> {
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                write_line(&mut output, &parse_error(e))?;
                continue;
            }
        };
        // Notifications (no `id`) are fire-and-forget: never reply, even on error.
        let id = msg.get("id").cloned();
        let Some(response) = handle(ctx, &msg) else {
            continue;
        };
        if id.is_none() {
            continue;
        }
        let mut response = response;
        response["id"] = id.unwrap_or(Value::Null);
        write_line(&mut output, &response)?;
    }
    Ok(())
}

fn write_line<W: Write>(output: &mut W, value: &Value) -> anyhow::Result<()> {
    output.write_all(serde_json::to_string(value)?.as_bytes())?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}

fn ok(result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "result": result, "id": Value::Null })
}

fn err(code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "error": { "code": code, "message": message }, "id": Value::Null })
}

fn parse_error(e: serde_json::Error) -> Value {
    json!({ "jsonrpc": "2.0", "error": { "code": -32700, "message": format!("parse error: {e}") }, "id": Value::Null })
}

/// Produce a response value (with a placeholder `id` the caller overwrites),
/// or `None` for notifications that need no reply.
fn handle(ctx: &ToolCtx, msg: &Value) -> Option<Value> {
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    match method {
        "initialize" => {
            // Echo the client's protocol version when it offers one supported
            // shape; otherwise advertise ours.
            let version = msg
                .get("params")
                .and_then(|p| p.get("protocolVersion"))
                .and_then(Value::as_str)
                .unwrap_or(PROTOCOL_VERSION);
            Some(ok(json!({
                "protocolVersion": version,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "slop", "version": env!("CARGO_PKG_VERSION") },
                "instructions": "Use find_capability before adding parallel infrastructure and assess_write before creating a new implementation. Use get_context_envelope before editing unfamiliar code. After edits, call validate_change and resolve blocking findings before finishing. Context and findings are snapshot-bound; refresh after writes.",
            })))
        }
        // Post-initialize handshake and keepalives.
        "notifications/initialized" | "notifications/cancelled" => None,
        "ping" => Some(ok(json!({}))),
        "tools/list" => Some(ok(json!({ "tools": tools::definitions() }))),
        "tools/call" => Some(handle_tools_call(ctx, msg)),
        "" => Some(err(-32600, "invalid request: missing method")),
        other => Some(err(-32601, &format!("method not found: {other}"))),
    }
}

fn handle_tools_call(ctx: &ToolCtx, msg: &Value) -> Value {
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let empty = json!({});
    let args = params.get("arguments").unwrap_or(&empty);

    match tools::call(ctx, name, args) {
        Ok(text) => ok(json!({
            "content": [ { "type": "text", "text": text } ],
            "isError": false,
        })),
        // MCP convention: tool execution failures come back as a successful
        // JSON-RPC result with isError=true, so the model sees the message.
        Err(e) => ok(json!({
            "content": [ { "type": "text", "text": format!("error: {e:#}") } ],
            "isError": true,
        })),
    }
}
