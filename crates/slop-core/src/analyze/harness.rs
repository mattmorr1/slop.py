//! Agent-host hooks (M4d): pure handlers driven by the `slop hook <event>` CLI.
//!
//! - `pre-tool-use` — pre-check a proposed write against policy and the
//!   prewrite sidecar.
//! - `session-start` / `subagent-start` — record the session baseline, inject the world model.
//! - `stop` — the reconcile gate over what the session changed.
//!
//! PreToolUse and Stop read `slop.toml` and the prewrite sidecar only: no SCIP, no graph build.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::parse::Language;

use crate::analyze::missing::Resolver;
use crate::analyze::policy::{Gate, Policy};
use crate::analyze::precheck::precheck;
use crate::analyze::{prewrite, reconcile, session, world};

/// Nearest ancestor of `start` (inclusive) containing `slop.toml`, else the
/// nearest `.git` root, else `start` itself. Hooks are launched from the
/// project dir, but the read may be for a file in a sub-package.
fn find_repo_root(start: &Path) -> PathBuf {
    let mut dir = Some(start);
    let mut git_root: Option<&Path> = None;
    while let Some(d) = dir {
        if d.join("slop.toml").is_file() {
            return d.to_path_buf();
        }
        if git_root.is_none() && d.join(".git").exists() {
            git_root = Some(d);
        }
        dir = d.parent();
    }
    git_root.unwrap_or(start).to_path_buf()
}

fn as_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// The read locus (path + cwd) from a PostToolUse payload.
fn read_target(input: &Value) -> (Option<String>, PathBuf) {
    let file = input
        .get("tool_input")
        .and_then(|t| as_str(t, "file_path"))
        .map(str::to_string);
    let cwd = as_str(input, "cwd")
        .map(PathBuf::from)
        .or_else(|| file.as_deref().and_then(|f| Path::new(f).parent().map(Path::to_path_buf)))
        .unwrap_or_else(|| PathBuf::from("."));
    (file, cwd)
}

/// Make `path` repo-relative (matching SCIP/graph `file` fields). Returns the
/// path unchanged if it's already relative, `None` if it's outside `repo`.
fn repo_relative(repo: &Path, path: &str) -> Option<String> {
    let p = Path::new(path);
    if let Ok(rel) = p.strip_prefix(repo) {
        return Some(rel.to_string_lossy().replace('\\', "/"));
    }
    p.is_relative().then(|| path.to_string())
}

/// The full proposed content of the file a write tool is about to produce.
/// `Write` carries it directly; `Edit`/`MultiEdit` carry fragments that only
/// parse in context, so their replacements are applied to the on-disk file
/// instead — a dedented fragment would fail to parse and silently skip the check.
fn proposed_content(tool: &str, input: &Value, abs_path: &Path) -> Option<String> {
    let ti = input.get("tool_input")?;
    match tool {
        "Write" => as_str(ti, "content").map(str::to_string),
        "Edit" => apply_edit(&std::fs::read_to_string(abs_path).ok()?, ti),
        "MultiEdit" => {
            let mut current = std::fs::read_to_string(abs_path).ok()?;
            for edit in ti.get("edits")?.as_array()? {
                current = apply_edit(&current, edit)?;
            }
            Some(current)
        }
        _ => None,
    }
}

fn apply_edit(current: &str, edit: &Value) -> Option<String> {
    let old = as_str(edit, "old_string")?;
    let new = as_str(edit, "new_string")?;
    if old.is_empty() {
        return None;
    }
    if edit.get("replace_all").and_then(Value::as_bool).unwrap_or(false) {
        Some(current.replace(old, new))
    } else {
        Some(current.replacen(old, new, 1))
    }
}

/// Append one line per hook call to `<repo>/.slop/hooks.jsonl`: what fired and what
/// it cost, so dogfooding can measure it. `SLOP_HOOK_LOG=0` turns it off.
pub fn log_hook(input: &Value, event: &str, output: &Value, elapsed: std::time::Duration) {
    if std::env::var("SLOP_HOOK_LOG").is_ok_and(|v| v == "0") {
        return;
    }
    let (file, cwd) = read_target(input);
    let repo = find_repo_root(&cwd);
    let hook = output.get("hookSpecificOutput");
    let context = hook.and_then(|h| as_str(h, "additionalContext")).unwrap_or("");
    let count = |needle: &str| context.lines().filter(|l| l.starts_with(needle)).count();
    let line = json!({
        "at": now(),
        "event": event,
        "tool": as_str(input, "tool_name"),
        "file": file.as_deref().and_then(|f| repo_relative(&repo, f)),
        "ms": elapsed.as_millis() as u64,
        "injected_tokens": context.len() / 4,
        "duplicates": count("slop: new `"),
        "reuse": count("slop: proposed code overlaps"),
        "missing": count("slop: missing"),
        "decision": as_str(output, "decision"),
        "blocked": as_str(output, "reason").map_or(0, |r| r.lines().filter(|l| l.starts_with(|c: char| c.is_ascii_digit())).count()),
    });
    append_log(&repo, &line);
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn append_log(repo: &Path, line: &Value) {
    let path = repo.join(".slop/hooks.jsonl");
    let written = std::fs::create_dir_all(repo.join(".slop")).and_then(|()| {
        use std::io::Write;
        let mut log = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
        writeln!(log, "{line}")
    });
    if let Err(error) = written {
        eprintln!("slop: could not write {}: {error}", path.display());
    }
}

/// A hook's internal error, to stderr and `.slop/hooks.jsonl`: failing open is logged, never silent.
pub fn log_error(repo: &Path, event: &str, error: &anyhow::Error) {
    eprintln!("slop {event}: {error:#}");
    if !std::env::var("SLOP_HOOK_LOG").is_ok_and(|v| v == "0") {
        append_log(repo, &json!({ "at": now(), "event": event, "error": format!("{error:#}") }));
    }
}

/// PreToolUse handler (W2): pre-check a *proposed* write against the sanctioned
/// -channel policy and steer via `additionalContext`, before the edit lands.
///
/// Warn-only on purpose. `permissionDecision: "deny"` is deliberately unused:
/// a false deny blocks real work, and per-language detector precision is not
/// measured yet. Promote a rule to `ask`/`deny` only once its measured precision
/// earns it.
pub fn handle_pre_tool_use(input: &Value) -> Value {
    let tool = as_str(input, "tool_name").unwrap_or("");
    if !matches!(tool, "Write" | "Edit" | "MultiEdit") {
        return json!({});
    }
    let (file, cwd) = read_target(input);
    let Some(file) = file else {
        return json!({});
    };
    let Some(lang) = Language::from_path(&file) else {
        return json!({});
    };
    let repo = find_repo_root(&cwd);
    let Some(rel) = repo_relative(&repo, &file) else {
        return json!({});
    };
    let policy = Policy::load(&repo).inspect_err(|error| log_error(&repo, "PreToolUse", error)).unwrap_or_default();
    if policy.agent.gate == Gate::Off {
        return json!({});
    }
    let Some(content) = proposed_content(tool, input, Path::new(&file)) else {
        return json!({});
    };
    let (sidecar, error) = match prewrite::load_sidecar(&repo) {
        Ok(sidecar) => (sidecar, None),
        Err(error) => (None, Some(error)),
    };
    let assessment = sidecar.as_ref().map(|s| prewrite::assess_from_sidecar(&repo, s, &policy, &rel, &content, 3));
    let mut context: Vec<String> = match &assessment {
        Some(Ok(assessment)) => assessment.steering(&repo),
        _ => precheck(lang, &rel, &content, &policy).iter().map(|finding| finding.steering()).collect(),
    };
    if let Some(error) = error.or_else(|| assessment.and_then(Result::err)) {
        log_error(&repo, "PreToolUse", &error);
        context.push(format!("slop: reuse suggestions unavailable: {error:#}"));
    }
    if lang == Language::Python {
        let before = std::fs::read_to_string(&file).ok();
        context.extend(reconcile::new_missing(&mut Resolver::new(&repo), &rel, before.as_deref(), &content).iter().map(|call| {
            let finding = reconcile::missing_finding(&rel, call, sidecar.as_ref());
            format!("slop: missing `{}` at {}:{}: {}; {}.", call.name, rel, finding.line + 1, finding.fact, finding.action)
        }));
    }
    context.dedup();
    if context.is_empty() {
        return json!({});
    }
    json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "additionalContext": context.join("\n"),
        }
    })
}

/// Stop handler (L2): block the end of a turn, once per finding, while the code this session
/// changed holds an unacknowledged provable problem. Any internal error allows the stop.
pub fn handle_stop(input: &Value) -> Value {
    if input.get("stop_hook_active").and_then(Value::as_bool) == Some(true) {
        return json!({});
    }
    let repo = find_repo_root(&read_target(input).1);
    stop(&repo, input).unwrap_or_else(|error| {
        log_error(&repo, "Stop", &error.context("failing open"));
        json!({})
    })
}

fn stop(repo: &Path, input: &Value) -> anyhow::Result<Value> {
    use anyhow::Context;
    let policy = Policy::load(repo)?;
    if policy.agent.gate != Gate::Reconcile {
        return Ok(json!({}));
    }
    let id = as_str(input, "session_id").context("Stop payload has no session_id")?;
    let mut session = session::load(repo, id)?;
    let changes = session::changes(repo, &session)?;
    if changes.is_empty() {
        return Ok(json!({}));
    }
    let sidecar = prewrite::load_sidecar(repo)?;
    let report = reconcile::reconcile(repo, &changes, sidecar.as_ref(), &policy);
    let acked = reconcile::acknowledged(repo)?;
    let fresh: Vec<_> = report.findings.into_iter().filter(|f| !acked.contains(&f.id) && !session.blocked.contains(&f.id)).collect();
    if fresh.is_empty() {
        return Ok(json!({}));
    }
    session.blocked.extend(fresh.iter().map(|f| f.id.clone()));
    session::save(repo, id, &session)?;
    Ok(json!({ "decision": "block", "reason": reconcile::render(&fresh, &report.advisory) }))
}

/// SessionStart / SubagentStart handler (W4): inject the codebase world model
/// once, up front, so the agent designs against what already exists instead of
/// being corrected afterwards.
///
/// This is the one hook that can afford a graph build — it fires once per
/// session, not per tool call — so it carries the capability index the cheap
/// hooks can't. With no index it degrades to policy-only facts.
pub fn handle_session_start(input: &Value, event: &str) -> Value {
    let cwd = as_str(input, "cwd")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let repo = find_repo_root(&cwd);
    if let Some(Err(error)) = as_str(input, "session_id").map(|id| session::start(&repo, id)) {
        log_error(&repo, event, &error);
    }
    let analysis = crate::analyze::check::load_analysis(&repo, None)
        .inspect_err(|error| log_error(&repo, event, error))
        .ok();
    let sidecar_error = analysis
        .as_ref()
        .and_then(|snapshot| snapshot.write_prewrite_sidecar().err());
    if let Some(error) = &sidecar_error {
        log_error(&repo, event, error);
    }
    let fallback_policy = Policy::load(&repo).inspect_err(|error| log_error(&repo, event, error)).unwrap_or_default();
    let policy = analysis
        .as_ref()
        .map_or(&fallback_policy, |snapshot| &snapshot.policy);
    let built = analysis.as_ref().map(|a| &a.built);
    let env = analysis
        .as_ref()
        .map(|a| crate::analyze::config::env_vars(&repo, &a.facts))
        .unwrap_or_default();

    if policy.agent.gate == Gate::Off {
        return json!({});
    }
    let mut context = world::render(policy, built, &env, world::DEFAULT_BUDGET).map_or_else(String::new, |world| world + "\n");
    if let Some(error) = sidecar_error {
        context.push_str(&format!("slop: prewrite sidecar unavailable: {error:#}\n"));
    }
    context.push_str("slop: before finishing, call slop's `reconcile` tool to check what you changed; use `query_subgraph` to find the channel for an effect.");
    json!({
        "hookSpecificOutput": {
            "hookEventName": event,
            "additionalContext": context,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze::test_support::TempFixture;

    /// A repo with a Net channel policy and a file on disk to Edit against.
    fn repo_with_policy(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("slop-pre-{tag}-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(
            dir.join("slop.toml"),
            "[channels]\nnet = [\"core.http_client.HttpClient\"]\n",
        );
        dir
    }

    #[test]
    fn pre_tool_use_steers_a_proposed_bypass() {
        let dir = repo_with_policy("write");
        let target = dir.join("services").join("alerts.py");
        let _ = std::fs::create_dir_all(target.parent().unwrap());
        let out = handle_pre_tool_use(&json!({
            "tool_name": "Write",
            "cwd": dir.to_string_lossy(),
            "tool_input": {
                "file_path": target.to_string_lossy(),
                "content": "import requests\n\ndef send(u):\n    return requests.post(u)\n",
            }
        }));
        let ctx = out["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap_or_default();
        assert!(ctx.contains("core.http_client.HttpClient"), "got {ctx:?}");
        // Warn-only: never asserts a permission decision.
        assert!(out["hookSpecificOutput"].get("permissionDecision").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pre_tool_use_surfaces_snapshot_bound_reuse_suggestion() {
        let repo = TempFixture::new("toy_repo_slopped");
        let snapshot =
            crate::analyze::snapshot::RepositorySnapshot::capture(crate::analyze::snapshot::CaptureRequest {
                repo: &repo,
                index: None,
                policy: None,
                freshness: crate::analyze::snapshot::Freshness::Warn,
            })
            .unwrap();
        let sidecar = snapshot.write_prewrite_sidecar().unwrap();
        let out = handle_pre_tool_use(&json!({
            "tool_name": "Write",
            "cwd": repo.to_string_lossy(),
            "tool_input": {
                "file_path": repo.join("services/new_notify.py").to_string_lossy(),
                "content": "def emit(rows):\n    report = build_report(rows)\n    send_notification(report)\n"
            }
        }));
        let context = out["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(context.contains("notify_with_report"), "{context}");
        let _ = std::fs::remove_file(sidecar);
    }

    #[test]
    fn pre_tool_use_reconstructs_an_edit_from_disk() {
        // An Edit's new_string is a dedented fragment that would not parse on its
        // own — the check has to apply it to the real file first.
        let dir = repo_with_policy("edit");
        let target = dir.join("alerts.py");
        let _ = std::fs::write(&target, "import requests\n\ndef send(u):\n    return None\n");
        let out = handle_pre_tool_use(&json!({
            "tool_name": "Edit",
            "cwd": dir.to_string_lossy(),
            "tool_input": {
                "file_path": target.to_string_lossy(),
                "old_string": "    return None",
                "new_string": "    return requests.post(u)",
            }
        }));
        assert!(out["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap_or_default()
            .contains("core.http_client.HttpClient"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pre_tool_use_is_silent_without_a_policy_and_on_reads() {
        // No slop.toml anywhere => no steering (D8).
        let out = handle_pre_tool_use(&json!({
            "tool_name": "Write",
            "cwd": "/nope",
            "tool_input": { "file_path": "/nope/a.py", "content": "import requests\n" }
        }));
        assert_eq!(out, json!({}));
        assert_eq!(
            handle_pre_tool_use(&json!({
                "tool_name": "Read", "tool_input": {"file_path": "/x/a.py"}
            })),
            json!({})
        );
    }
}
