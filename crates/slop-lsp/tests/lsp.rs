//! Drive the LSP server end-to-end with real `Content-Length`-framed JSON-RPC
//! against the SCIP fixtures — the same fixtures the detector and MCP tests
//! use. This is what makes the editor surface headless-verifiable: only the
//! thin VS Code launch shim stays untested.

use std::io::BufReader;
use std::path::PathBuf;

use serde_json::{json, Value};
use slop_lsp::diagnostics::path_to_file_uri;
use slop_lsp::server::{read_message, write_message, LspCtx};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures")
        .join(name)
}

/// Feed a batch of messages through the server and return every message it
/// emitted (replies and notifications), framed and parsed back out.
fn exchange(repo: PathBuf, requests: &[Value]) -> Vec<Value> {
    let ctx = LspCtx { repo, index: None };
    let mut input: Vec<u8> = Vec::new();
    for r in requests {
        write_message(&mut input, r).unwrap();
    }
    let mut output = Vec::new();
    server_serve(&ctx, &input, &mut output);

    let mut reader = BufReader::new(&output[..]);
    let mut out = Vec::new();
    while let Ok(Some(msg)) = read_message(&mut reader) {
        out.push(msg);
    }
    out
}

fn server_serve(ctx: &LspCtx, input: &[u8], output: &mut Vec<u8>) {
    slop_lsp::server::serve(ctx, BufReader::new(input), output).expect("serve");
}

/// The `publishDiagnostics` params for `uri`, if any were published.
fn diagnostics_for<'a>(msgs: &'a [Value], uri: &str) -> Option<&'a Vec<Value>> {
    msgs.iter()
        .filter(|m| m["method"] == "textDocument/publishDiagnostics")
        .find(|m| m["params"]["uri"] == uri)
        .and_then(|m| m["params"]["diagnostics"].as_array())
}

fn didopen(uri: &str) -> Value {
    json!({
        "jsonrpc": "2.0", "method": "textDocument/didOpen",
        "params": { "textDocument": { "uri": uri, "languageId": "python", "version": 1, "text": "" } }
    })
}

#[test]
fn initialize_advertises_sync_and_server_name() {
    let out = exchange(
        fixture("toy_repo"),
        &[json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} })],
    );
    let init = &out[0];
    assert_eq!(init["id"], 1);
    assert_eq!(init["result"]["serverInfo"]["name"], "slop");
    assert_eq!(
        init["result"]["capabilities"]["textDocumentSync"]["save"],
        true
    );
}

#[test]
fn didopen_on_slopped_repo_publishes_findings() {
    let repo = fixture("toy_repo_slopped");
    let alerts_uri = path_to_file_uri(&repo.join("services/alerts.py"));
    let out = exchange(repo.clone(), &[didopen(&alerts_uri)]);

    let diags = diagnostics_for(&out, &alerts_uri).expect("alerts.py diagnostics published");
    assert!(!diags.is_empty(), "expected findings on alerts.py");
    // The infra-bypass finding is a blocker -> LSP severity 1 (Error).
    let infra = diags
        .iter()
        .find(|d| d["code"] == "infra-bypass")
        .expect("infra-bypass diagnostic");
    assert_eq!(infra["severity"], 1);
    assert_eq!(infra["source"], "slop");
    assert!(infra["message"].as_str().unwrap().contains("fix:"));
}

#[test]
fn module_level_finding_has_a_sane_range() {
    let repo = fixture("toy_repo_slopped");
    let notify_uri = path_to_file_uri(&repo.join("services/notify.py"));
    let out = exchange(repo.clone(), &[didopen(&notify_uri)]);
    let diags = diagnostics_for(&out, &notify_uri).expect("notify.py diagnostics");
    // circular-import is module-level (end line usize::MAX upstream) — the
    // range must stay finite and small.
    let cyc = diags
        .iter()
        .find(|d| d["code"] == "circular-import")
        .expect("circular-import diagnostic");
    let end = cyc["range"]["end"]["line"].as_u64().unwrap();
    assert!(end < 1000, "range end should be clamped, got {end}");
}

#[test]
fn didopen_on_clean_repo_clears_diagnostics() {
    let repo = fixture("toy_repo");
    let uri = path_to_file_uri(&repo.join("core/http_client.py"));
    let out = exchange(repo, &[didopen(&uri)]);
    // A clean repo still publishes for the open doc, but with an empty array,
    // so the editor clears any prior squiggles.
    let diags = diagnostics_for(&out, &uri).expect("empty publish for open clean doc");
    assert!(diags.is_empty());
}

#[test]
fn shutdown_then_exit_terminates_cleanly() {
    let out = exchange(
        fixture("toy_repo"),
        &[
            json!({ "jsonrpc": "2.0", "id": 9, "method": "shutdown" }),
            json!({ "jsonrpc": "2.0", "method": "exit" }),
        ],
    );
    // shutdown gets a null-result reply; exit ends the loop (no further output).
    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["id"], 9);
    assert!(out[0].get("result").is_some());
}
