//! Drive the MCP server end-to-end with canned JSON-RPC lines against the
//! `toy_repo_slopped` SCIP fixture — the same fixture the detector tests use.

use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;
use slop_cli::mcp::{server, ToolCtx};

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
        slop_core::analyze::index::write_stamp(&path, &index).expect("stamp fixture index");
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
    exchange_in(&TempFixture::new(&repo).0, requests)
}

fn exchange_in(repo: &Path, requests: &[Value]) -> Vec<Value> {
    let ctx = ToolCtx {
        default_repo: repo.to_path_buf(),
        default_index: None,
        refresher: None,
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
        .contains("reconcile"));
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
            "assess_write",
            "query_subgraph",
            "reconcile",
            "acknowledge"
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
    assert_eq!(assessment["schema_version"], 2);
    assert!(assessment["duplicates"].as_array().is_some_and(Vec::is_empty));
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

fn git(repo: &Path, args: &[&str]) {
    assert!(std::process::Command::new("git").arg("-C").arg(repo).args(args).status().unwrap().success());
}

fn call(id: u64, name: &str, arguments: Value) -> Value {
    serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call", "params": { "name": name, "arguments": arguments } })
}

#[test]
fn reconcile_reports_the_diff_and_acknowledge_clears_it() {
    let fixture = TempFixture::new(&fixture("toy_repo_slopped"));
    let repo = &fixture.0;
    git(repo, &["init", "-q"]);
    git(repo, &["add", "-A"]);
    git(repo, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "init"]);
    std::fs::write(repo.join("services/digest.py"), "import requests\n\n\ndef digest(rows):\n    requests.post(\"http://x\", json=rows)\n    return build_reprot(rows)\n").unwrap();
    let missing = "missing:services/digest.py:build_reprot";
    let out = exchange_in(repo, &[
        call(1, "reconcile", serde_json::json!({})),
        call(2, "acknowledge", serde_json::json!({ "id": missing, "reason": "generated at import time" })),
        call(3, "reconcile", serde_json::json!({})),
        call(4, "acknowledge", serde_json::json!({ "id": missing })),
    ]);
    let first = tool_json(&out[0]);
    assert_eq!(first["scope"], "diff");
    let ids: Vec<&str> = first["provable"].as_array().unwrap().iter().map(|f| f["id"].as_str().unwrap()).collect();
    assert!(ids.contains(&missing) && ids.iter().any(|id| id.starts_with("infra-bypass:services/digest.py:")), "{first}");
    assert!(first["detectors"]["findings"].as_array().unwrap().iter().any(|f| f["rule"] == "infra-bypass"), "{first}");
    assert_eq!(tool_json(&out[1])["acknowledged"], missing);
    let second = tool_json(&out[2]);
    assert_eq!(second["acknowledged"], serde_json::json!([missing]));
    assert!(second["provable"].as_array().unwrap().iter().all(|f| f["id"] != missing));
    assert_eq!(out[3]["result"]["isError"], true);
    assert!(std::fs::read_to_string(repo.join(".slop/acks.jsonl")).unwrap().contains("generated at import time"));
}

/// The call's own capture queues a background reindex; a second, foreground capture
/// reindexed beside it and failed on the rewritten index (B6: 9 retries).
#[test]
fn reconcile_never_reindexes_beside_its_own_queued_refresh() {
    use std::io::{BufRead, Write};
    let fixture = TempFixture::new(&fixture("toy_repo_slopped"));
    let repo = &fixture.0;
    git(repo, &["init", "-q"]);
    git(repo, &["add", "-A"]);
    git(repo, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "init"]);
    let dates = std::fs::read_to_string(repo.join("utils/dates.py")).unwrap();
    std::fs::write(repo.join("utils/dates.py"), dates + "\n\ndef today():\n    return datetime.now()\n").unwrap();
    let bin = repo.join(".fakebin");
    std::fs::create_dir_all(&bin).unwrap();
    let log = repo.join(".fakebin/npx.log");
    std::fs::write(bin.join("npx"), format!("#!/bin/sh\necho run >> '{}'\nsleep 1\nexit 1\n", log.display())).unwrap();
    std::process::Command::new("chmod").arg("+x").arg(bin.join("npx")).status().unwrap();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_slop"))
        .args(["mcp", &repo.to_string_lossy()])
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    writeln!(stdin, "{}", call(1, "reconcile", serde_json::json!({}))).unwrap();
    let mut reply = String::new();
    BufReader::new(child.stdout.take().unwrap()).read_line(&mut reply).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(500));
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
    let reply: Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(reply["result"]["isError"], false, "{reply}");
    let runs = std::fs::read_to_string(&log).unwrap_or_default().lines().count();
    assert!(runs <= 1, "the call reindexed in the foreground beside its queued refresh: {runs} indexer runs");
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
fn unknown_method_is_a_json_rpc_error() {
    let out = exchange(
        fixture("toy_repo_slopped"),
        &[serde_json::json!({"jsonrpc": "2.0", "id": 9, "method": "does/not/exist"})],
    );
    assert_eq!(out[0]["error"]["code"], -32601);
    assert_eq!(out[0]["id"], 9);
}
