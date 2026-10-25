//! Agent-host hooks (M4d): pure handlers driven by the `slop hook <event>` CLI.
//!
//! - `pre-tool-use` — pre-check a proposed write against policy and the
//!   prewrite sidecar.
//! - `post-tool-use` — attach sanctioned-channel steering to source reads.
//!   `slop setup` no longer registers it; it stays so older installs keep working.
//! - `user-prompt-submit` — inject the repo's sanctioned-channel policy as
//!   pre-hoc steering ("route Net through core.http.Client; don't use urllib").
//! - `session-start` / `subagent-start` — inject the codebase world model.
//!
//! The per-call hooks derive steering from `slop.toml` alone (cheap — no SCIP
//! index, no graph build). The heavier graph-backed checks live behind
//! `validate_change` / `slop gate`.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::parse::Language;

use crate::analyze::policy::Policy;
use crate::analyze::precheck::precheck;
use crate::analyze::prewrite;
use crate::analyze::world;

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

/// Sanctioned-channel policy rendered as agent-facing steering, or `None`
/// when the repo has no policy (stay silent — no cold-start noise, D8).
pub fn channel_steering(policy: &Policy) -> Option<String> {
    if policy.channels.is_empty() {
        return None;
    }
    let mut keys: Vec<&String> = policy.channels.keys().collect();
    keys.sort();
    let mut lines = vec![
        "slop: this codebase routes effects through sanctioned channels — reuse them, don't reimplement:".to_string(),
    ];
    for key in keys {
        let chans = &policy.channels[key];
        if !chans.is_empty() {
            lines.push(format!("  - {}: {}", key, chans.join(", ")));
        }
    }
    Some(lines.join("\n"))
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
        "at": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()),
        "event": event,
        "tool": as_str(input, "tool_name"),
        "file": file.as_deref().and_then(|f| repo_relative(&repo, f)),
        "ms": elapsed.as_millis() as u64,
        "injected_tokens": context.len() / 4,
        "duplicates": count("slop: new `"),
        "reuse": count("slop: proposed code overlaps"),
    });
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
    let policy = Policy::load(&repo).unwrap_or_default();
    let Some(rel) = repo_relative(&repo, &file) else {
        return json!({});
    };
    let Some(content) = proposed_content(tool, input, Path::new(&file)) else {
        return json!({});
    };
    let mut context = match prewrite::assess_from_sidecar(&repo, &policy, &rel, &content, 3) {
        Ok(Some(assessment)) => assessment.steering(&repo),
        Ok(None) => precheck(lang, &rel, &content, &policy)
            .iter()
            .map(|finding| finding.steering())
            .collect(),
        Err(error) => {
            let mut context: Vec<String> = precheck(lang, &rel, &content, &policy)
                .iter()
                .map(|finding| finding.steering())
                .collect();
            context.push(format!("slop: reuse suggestions unavailable: {error:#}"));
            context
        }
    };
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

/// PostToolUse handler: sanctioned-channel steering on a source-file read as
/// `additionalContext`, or an empty object (no-op passthrough).
pub fn handle_post_tool_use(input: &Value) -> Value {
    if as_str(input, "tool_name") != Some("Read") {
        return json!({});
    }
    let (file, cwd) = read_target(input);
    if !file.is_some_and(|f| Language::from_path(&f).is_some()) {
        return json!({});
    }
    match channel_steering(&Policy::load(&find_repo_root(&cwd)).unwrap_or_default()) {
        Some(steering) => json!({
            "hookSpecificOutput": { "hookEventName": "PostToolUse", "additionalContext": steering }
        }),
        None => json!({}),
    }
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
    let analysis = crate::analyze::check::load_analysis(&repo, None).ok();
    let sidecar_error = analysis
        .as_ref()
        .and_then(|snapshot| snapshot.write_prewrite_sidecar().err());
    let fallback_policy = Policy::load(&repo).unwrap_or_default();
    let policy = analysis
        .as_ref()
        .map_or(&fallback_policy, |snapshot| &snapshot.policy);
    let built = analysis.as_ref().map(|a| &a.built);
    let env = analysis
        .as_ref()
        .map(|a| crate::analyze::config::env_vars(&repo, &a.facts))
        .unwrap_or_default();

    let Some(mut context) = world::render(policy, built, &env, world::DEFAULT_BUDGET) else {
        return json!({});
    };
    if let Some(error) = sidecar_error {
        context.push_str(&format!("\nslop: prewrite sidecar unavailable: {error:#}"));
    }
    json!({
        "hookSpecificOutput": {
            "hookEventName": event,
            "additionalContext": context,
        }
    })
}

/// UserPromptSubmit handler: inject the repo's sanctioned-channel policy plus
/// a pointer to slop's validation tool as pre-hoc steering.
pub fn handle_user_prompt_submit(input: &Value) -> Value {
    let cwd = as_str(input, "cwd")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let policy = Policy::load(&find_repo_root(&cwd)).unwrap_or_default();

    let Some(channels) = channel_steering(&policy) else {
        return json!({});
    };
    let context = format!(
        "{channels}\nBefore finishing, validate new code with slop's `validate_change` tool; \
         use `query_subgraph` to find the right channel for an effect."
    );
    json!({
        "hookSpecificOutput": {
            "hookEventName": "UserPromptSubmit",
            "additionalContext": context,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze::test_support::TempFixture;

    fn input_in_dir_without_policy() -> Value {
        json!({"cwd": "/does/not/matter"})
    }

    #[test]
    fn non_read_tools_pass_through() {
        let out = handle_post_tool_use(&json!({
            "tool_name": "Bash", "tool_input": {"command": "ls"}, "tool_output": "x"
        }));
        assert_eq!(out, json!({}));
    }

    #[test]
    fn non_python_reads_pass_through() {
        let out = handle_post_tool_use(&json!({
            "tool_name": "Read", "tool_input": {"file_path": "/x/README.md"},
            "tool_output": "# title\n\n\n\nbody"
        }));
        assert_eq!(out, json!({}));
    }

    #[test]
    fn channel_steering_lists_sorted_channels() {
        let policy: Policy = toml::from_str(
            "[channels]\nnet = [\"core.http.Client\"]\ndb = [\"core.db.Session\"]",
        )
        .unwrap();
        let s = channel_steering(&policy).unwrap();
        // Sorted keys: db before net.
        assert!(s.find("db:").unwrap() < s.find("net:").unwrap());
        assert!(s.contains("core.http.Client"));
    }

    #[test]
    fn empty_policy_yields_no_steering() {
        assert!(channel_steering(&Policy::default()).is_none());
        // ...and UserPromptSubmit stays silent.
        let out = handle_user_prompt_submit(&input_in_dir_without_policy());
        assert_eq!(out, json!({}));
    }

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
        // Reads are the PostToolUse hook's business, not this one's.
        assert_eq!(
            handle_pre_tool_use(&json!({
                "tool_name": "Read", "tool_input": {"file_path": "/x/a.py"}
            })),
            json!({})
        );
    }
}
