//! Drive `slop hook` end to end with the payloads in `tests/fixtures/hook_payloads` against a
//! git-initialized copy of `toy_repo_slopped`: the Stop gate's acceptance cases.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

const PARSE_COPY: &str = "from datetime import datetime\n\n\ndef parse_iso(text: str) -> datetime:\n    \"\"\"Parse an ISO-8601 date string.\"\"\"\n    value = text.strip()\n    if value.endswith(\"Z\"):\n        value = value[:-1] + \"+00:00\"\n    return datetime.fromisoformat(value)\n";
const TYPO_CALL: &str = "def digest(rows: list) -> str:\n    return build_reprot(rows)\n";

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn copy_tree(source: &Path, destination: &Path) {
    std::fs::create_dir_all(destination).unwrap();
    for entry in std::fs::read_dir(source).unwrap().map(Result::unwrap) {
        let target = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

struct Repo(PathBuf);

impl Repo {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("slop-hooks-{}-{}", std::process::id(), SEQUENCE.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_dir_all(&path);
        copy_tree(&root().join("tests/fixtures/toy_repo_slopped"), &path);
        std::fs::remove_file(path.join("index.scip.stamp")).ok();
        slop_core::analyze::index::write_stamp(&path, &path.join("index.scip")).unwrap();
        let repo = Self(path.canonicalize().unwrap());
        repo.git(&["init", "-q"]);
        repo.git(&["add", "-A"]);
        repo.git(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "init"]);
        repo
    }

    fn git(&self, args: &[&str]) {
        assert!(Command::new("git").arg("-C").arg(&self.0).args(args).status().unwrap().success());
    }

    fn write(&self, rel: &str, text: &str) {
        std::fs::write(self.0.join(rel), text).unwrap();
    }

    fn payload(&self, payload: &str, session: &str) -> Value {
        let template = std::fs::read_to_string(root().join("tests/fixtures/hook_payloads").join(payload)).unwrap();
        let mut input: Value = serde_json::from_str(&template.replace("SESSION", session)).unwrap();
        input["cwd"] = Value::from(self.0.to_string_lossy().into_owned());
        input
    }

    fn run(&self, event: &str, input: &Value) -> Value {
        let mut child = Command::new(env!("CARGO_BIN_EXE_slop"))
            .args(["hook", event])
            .current_dir(&self.0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input.to_string().as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        serde_json::from_slice(&out.stdout).expect("hook prints json")
    }

    fn pre_write(&self, session: &str, file: &str, content: &str) -> Value {
        let mut input = self.payload("pre_tool_use.json", session);
        input["tool_input"] = serde_json::json!({ "file_path": self.0.join(file), "content": content });
        self.run("pre-tool-use", &input)
    }

    fn stop(&self, session: &str) -> Value {
        self.run("stop", &self.payload("stop.json", session))
    }

    fn start(&self, session: &str) {
        self.run("session-start", &self.payload("session_start.json", session));
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(slop_core::analyze::prewrite::sidecar_path(&self.0));
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn reason(output: &Value) -> &str {
    assert_eq!(output["decision"], "block", "expected a block, got {output}");
    output["reason"].as_str().unwrap()
}

#[test]
fn an_equivalent_copy_blocks_once_then_allows() {
    let repo = Repo::new();
    repo.start("s1");
    repo.write("utils/iso.py", PARSE_COPY);
    let first = repo.stop("s1");
    let text = reason(&first);
    assert!(text.contains("[duplicate:") && text.contains("provably equivalent"), "{text}");
    assert!(text.ends_with("call the slop `acknowledge` tool with the id and a reason."), "{text}");
    assert_eq!(repo.stop("s1"), serde_json::json!({}));
}

#[test]
fn a_missing_helper_blocks_with_a_suggestion() {
    let repo = Repo::new();
    repo.start("s2");
    repo.write("services/digest.py", TYPO_CALL);
    let text = reason(&repo.stop("s2")).to_string();
    assert!(text.contains("[missing:services/digest.py:build_reprot]") && text.contains("did you mean `services.reports.build_report`"), "{text}");
}

#[test]
fn an_acknowledged_id_does_not_block() {
    let repo = Repo::new();
    repo.start("s3");
    repo.write("services/digest.py", TYPO_CALL);
    slop_core::analyze::reconcile::acknowledge(&repo.0, Some("s3"), "missing:services/digest.py:build_reprot", "defined at runtime").unwrap();
    assert_eq!(repo.stop("s3"), serde_json::json!({}));
}

#[test]
fn a_file_dirty_before_the_session_is_ignored() {
    let repo = Repo::new();
    repo.write("utils/iso.py", PARSE_COPY);
    repo.start("s4");
    assert_eq!(repo.stop("s4"), serde_json::json!({}));
}

#[test]
fn a_corrupt_sidecar_fails_open_and_is_logged() {
    let repo = Repo::new();
    repo.start("s5");
    repo.write("utils/iso.py", PARSE_COPY);
    std::fs::write(slop_core::analyze::prewrite::sidecar_path(&repo.0), "{not json").unwrap();
    assert_eq!(repo.stop("s5"), serde_json::json!({}));
    let pre = repo.pre_write("s5", "utils/iso2.py", PARSE_COPY);
    assert!(pre.is_object());
    let log = std::fs::read_to_string(repo.0.join(".slop/hooks.jsonl")).unwrap();
    assert!(log.lines().any(|l| l.contains("\"event\":\"Stop\"") && l.contains("failing open")), "{log}");
}

#[test]
fn stop_hook_active_always_allows() {
    let repo = Repo::new();
    repo.start("s6");
    repo.write("utils/iso.py", PARSE_COPY);
    let mut input = repo.payload("stop.json", "s6");
    input["stop_hook_active"] = Value::Bool(true);
    assert_eq!(repo.run("stop", &input), serde_json::json!({}));
}

/// Fixture-sized latency guard for CI; the real budget (p95 Stop < 500 ms, PreToolUse < 150 ms) is
/// measured on a large repo, and this only catches a hook that started building the graph.
#[test]
fn stop_and_pre_tool_use_stay_cheap() {
    let repo = Repo::new();
    repo.start("s7");
    repo.write("utils/iso.py", PARSE_COPY);
    repo.write("services/digest.py", TYPO_CALL);
    let time = |f: &dyn Fn() -> Value| (0..5).map(|_| { let at = Instant::now(); f(); at.elapsed() }).min().unwrap();
    let stop = time(&|| repo.stop("s7"));
    let pre = time(&|| repo.pre_write("s7", "utils/iso3.py", PARSE_COPY));
    assert!(stop < Duration::from_millis(1500) && pre < Duration::from_millis(1000), "stop {stop:?}, pre {pre:?}");
}

#[test]
fn pre_tool_use_warns_about_a_missing_helper_without_denying() {
    let repo = Repo::new();
    repo.start("s8");
    let out = repo.pre_write("s8", "services/digest.py", TYPO_CALL);
    let context = out["hookSpecificOutput"]["additionalContext"].as_str().unwrap_or_default();
    assert!(context.contains("slop: missing `build_reprot`") && context.contains("services.reports.build_report"), "{out}");
    assert!(out["hookSpecificOutput"].get("permissionDecision").is_none());
}

#[test]
fn gate_off_silences_every_hook() {
    let repo = Repo::new();
    let policy = std::fs::read_to_string(repo.0.join("slop.toml")).unwrap();
    repo.write("slop.toml", &format!("[agent]\ngate = \"off\"\n\n{policy}"));
    repo.start("s9");
    repo.write("services/digest.py", TYPO_CALL);
    assert_eq!(repo.pre_write("s9", "services/digest.py", TYPO_CALL), serde_json::json!({}));
    assert_eq!(repo.stop("s9"), serde_json::json!({}));
}
