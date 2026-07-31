//! Drive the MCP server end-to-end with canned JSON-RPC lines against the
//! `toy_repo_slopped` SCIP fixture — the same fixture the detector tests use.

use std::io::BufReader;
use std::path::PathBuf;

use serde_json::Value;
use slop_mcp::{server, ToolCtx};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures")
        .join(name)
}

/// Run a batch of request lines through the server and return one parsed
/// response per line the server emitted.
fn exchange(repo: PathBuf, requests: &[Value]) -> Vec<Value> {
    let ctx = ToolCtx {
        default_repo: repo,
        default_index: None,
    };
    let input: String = requests
        .iter()
        .map(|r| r.to_string() + "\n")
        .collect();
    let mut output = Vec::new();
    server::serve(&ctx, BufReader::new(input.as_bytes()), &mut output).expect("serve");
    String::from_utf8(output)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("valid json response"))
        .collect()
}

/// Text payload of a `tools/call` result, parsed back into JSON.
fn tool_json(resp: &Value) -> Value {
    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .expect("text content");
    serde_json::from_str(text).expect("tool returned json")
}

#[test]
fn initialize_advertises_tools_capability() {
    let out = exchange(
        fixture("toy_repo_slopped"),
        &[serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": "2025-06-18", "capabilities": {} }
        })],
    );
    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["id"], 1);
    assert_eq!(out[0]["result"]["serverInfo"]["name"], "slop");
    assert!(out[0]["result"]["capabilities"]["tools"].is_object());
}

#[test]
fn notifications_get_no_reply() {
    let out = exchange(
        fixture("toy_repo_slopped"),
        &[
            serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            serde_json::json!({"jsonrpc": "2.0", "id": 7, "method": "ping"}),
        ],
    );
    // Only the ping (which has an id) gets a response.
    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["id"], 7);
}

#[test]
fn tools_list_returns_every_tool() {
    let out = exchange(
        fixture("toy_repo_slopped"),
        &[serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})],
    );
    let names: Vec<&str> = out[0]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec![
            "find_capability",
            "validate_change",
            "get_context_envelope",
            "query_subgraph"
        ]
    );
}

#[test]
fn validate_change_returns_the_planted_findings() {
    let out = exchange(
        fixture("toy_repo_slopped"),
        &[serde_json::json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": { "name": "validate_change", "arguments": { "all": true } }
        })],
    );
    assert_eq!(out[0]["result"]["isError"], false);
    let payload = tool_json(&out[0]);
    let rules: Vec<&str> = payload["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["rule"].as_str().unwrap())
        .collect();
    assert!(rules.contains(&"infra-bypass"), "rules: {rules:?}");
    assert!(payload["health"].as_str().unwrap().contains("health"));
}

#[test]
fn query_subgraph_reports_neighbors_and_effects() {
    let out = exchange(
        fixture("toy_repo_slopped"),
        &[serde_json::json!({
            "jsonrpc": "2.0", "id": 4, "method": "tools/call",
            "params": { "name": "query_subgraph",
                        "arguments": { "entity": "services.alerts::send_alert" } }
        })],
    );
    assert_eq!(out[0]["result"]["isError"], false);
    let sg = tool_json(&out[0]);
    assert_eq!(sg["target"], "services.alerts::send_alert");
    assert!(
        !sg["neighbors"].as_array().unwrap().is_empty(),
        "expected neighbors: {sg}"
    );
}

#[test]
fn get_context_envelope_runs_for_a_known_target() {
    let out = exchange(
        fixture("toy_repo_slopped"),
        &[serde_json::json!({
            "jsonrpc": "2.0", "id": 5, "method": "tools/call",
            "params": { "name": "get_context_envelope",
                        "arguments": { "target_entity": "services.alerts::send_alert" } }
        })],
    );
    assert_eq!(out[0]["result"]["isError"], false);
    let env = tool_json(&out[0]);
    assert_eq!(env["target"], "services.alerts::send_alert");
    assert!(env.get("items").is_some());
}

#[test]
fn unknown_method_is_a_json_rpc_error() {
    let out = exchange(
        fixture("toy_repo_slopped"),
        &[serde_json::json!({"jsonrpc": "2.0", "id": 9, "method": "does/not/exist"})],
    );
    assert_eq!(out[0]["error"]["code"], -32601);
    assert_eq!(out[0]["id"], 9);
}
