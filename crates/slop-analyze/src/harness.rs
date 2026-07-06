//! Agent-host hooks (M4d, D10 §3.5): the read-path harness that steers an
//! agent *before* context pollution. Two pure handlers, driven by the
//! `slop hook <event>` CLI:
//!
//! - `post-tool-use` — read-remap: deterministically strip comment/blank
//!   noise from file reads (D11 whitespace/comment strip) and attach
//!   sanctioned-channel steering as `additionalContext`.
//! - `user-prompt-submit` — inject the repo's sanctioned-channel policy as
//!   pre-hoc steering ("route Net through core.http.Client; don't use urllib").
//!
//! Both derive steering from `slop.toml` alone (cheap — no SCIP index, no
//! graph build), so they stay fast enough to run on every read/prompt. The
//! heavier graph-backed checks live behind `validate_change` / `slop gate`.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::policy::Policy;
use crate::skeleton::strip_noise;

/// Only bother remapping when the strip saves at least this fraction of
/// characters — otherwise pass the read through untouched.
const MIN_COMPRESSION_GAIN: f64 = 0.10;

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

/// The file's text content out of a PostToolUse payload, tolerating the
/// field-name variation across agent hosts (`tool_output` vs `tool_response`,
/// string vs `{content|file|text: ...}` object).
fn read_output_text(input: &Value) -> Option<String> {
    for key in ["tool_output", "tool_response"] {
        match input.get(key) {
            Some(Value::String(s)) => return Some(s.clone()),
            Some(Value::Object(_)) => {
                let obj = &input[key];
                for inner in ["content", "file", "text", "stdout"] {
                    if let Some(s) = obj.get(inner).and_then(Value::as_str) {
                        return Some(s.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    None
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

/// PostToolUse handler. Returns the JSON to print to stdout: either a
/// `hookSpecificOutput` with `updatedToolOutput`/`additionalContext`, or an
/// empty object (no-op passthrough).
pub fn handle_post_tool_use(input: &Value) -> Value {
    if as_str(input, "tool_name") != Some("Read") {
        return json!({});
    }
    let (file, cwd) = read_target(input);
    let is_python = file.as_deref().is_some_and(|f| f.ends_with(".py"));
    if !is_python {
        return json!({});
    }

    let policy = Policy::load(&find_repo_root(&cwd)).unwrap_or_default();
    let steering = channel_steering(&policy);

    let mut hook = json!({ "hookEventName": "PostToolUse" });
    let mut changed = false;

    if let Some(src) = read_output_text(input) {
        let stripped = strip_noise(&src);
        let gain = 1.0 - (stripped.len() as f64 / src.len().max(1) as f64);
        if gain >= MIN_COMPRESSION_GAIN {
            hook["updatedToolOutput"] = Value::String(stripped);
            changed = true;
        }
    }
    if let Some(s) = steering {
        hook["additionalContext"] = Value::String(s);
        changed = true;
    }

    if changed {
        json!({ "hookSpecificOutput": hook })
    } else {
        json!({})
    }
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
    fn python_read_strips_comment_noise() {
        let src = "def f():\n    # a comment explaining nothing\n    # another one\n\n\n\n    return 1\n";
        let out = handle_post_tool_use(&json!({
            "tool_name": "Read", "tool_input": {"file_path": "/x/mod.py"},
            "tool_output": src, "cwd": "/x"
        }));
        let remapped = out["hookSpecificOutput"]["updatedToolOutput"]
            .as_str()
            .expect("updatedToolOutput");
        assert!(!remapped.contains("# a comment"));
        assert!(remapped.contains("return 1"));
        assert!(remapped.len() < src.len());
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
}
