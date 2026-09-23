//! Drive the MCP server end-to-end with canned JSON-RPC lines against the
//! `toy_repo_slopped` SCIP fixture — the same fixture the detector tests use.

use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;
use slop_mcp::{server, ToolCtx};

static FIXTURE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures")
        .join(name)
}

struct TempFixture(PathBuf);

impl TempFixture {
    fn new(source: &Path) -> Self {
        let sequence = FIXTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "slop-mcp-fixture-{}-{}-{sequence}",
            source.file_name().unwrap().to_string_lossy(),
            std::process::id()
        ));
        copy_sources(source, &path).expect("copy fixture sources");
        let index = path.join("index.scip");
        std::fs::copy(source.join("index.scip"), &index).expect("copy fixture index");
        slop_analyze::index::write_stamp(&path, &index).expect("stamp fixture index");
        Self(path)
    }
}

fn copy_sources(source: &Path, destination: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(destination)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == "index.scip" || name == "index.scip.stamp" {
            continue;
        }
        if entry.file_type()?.is_dir() {
            copy_sources(&entry.path(), &destination.join(name))?;
        } else {
            std::fs::copy(entry.path(), destination.join(name))?;
        }
    }
    Ok(())
}

impl Drop for TempFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Run a batch of request lines through the server and return one parsed
/// response per line the server emitted.
fn exchange(repo: PathBuf, requests: &[Value]) -> Vec<Value> {
    let fixture = TempFixture::new(&repo);
    let ctx = ToolCtx {
        default_repo: fixture.0.clone(),
        default_index: None,
    };
    let input: String = requests.iter().map(|r| r.to_string() + "\n").collect();
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
    assert!(out[0]["result"]["instructions"]
        .as_str()
        .unwrap()
        .contains("validate_change"));
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
            "assess_write",
            "get_context_envelope",
            "query_subgraph"
        ]
    );
}

#[test]
fn assess_write_returns_snapshot_bound_reuse_evidence() {
    let out = exchange(
        fixture("toy_repo_slopped"),
        &[serde_json::json!({
            "jsonrpc": "2.0", "id": 6, "method": "tools/call",
            "params": { "name": "assess_write", "arguments": {
                "file": "services/new_notify.py",
                "content": "def emit(rows):\n    report = build_report(rows)\n    send_notification(report)\n"
            }}
        })],
    );
    assert_eq!(out[0]["result"]["isError"], false);
    let assessment = tool_json(&out[0]);
    assert_eq!(assessment["schema_version"], 1);
    assert_eq!(assessment["snapshot"].as_str().unwrap().len(), 64);
    assert!(assessment["reuse_suggestions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|value| value["label"]
            .as_str()
            .unwrap()
            .ends_with("notify_with_report")));
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
    assert_eq!(payload["schema_version"], 1);
    assert_eq!(payload["snapshot"].as_str().unwrap().len(), 64);
    assert_eq!(payload["freshness"]["state"], "current");
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
    assert_eq!(env["snapshot"].as_str().unwrap().len(), 64);
    assert_eq!(env["freshness"]["state"], "current");
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
