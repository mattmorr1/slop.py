//! `slop install`: wire slop's harness into a target repo's agent-host config
//! (`.mcp.json` + `.claude/settings.json`) so the (b) loop — steer on read,
//! validate on the MCP tool, gate the fix — is live without hand-assembling
//! JSON with machine-specific absolute paths.
//!
//! The merges here are pure `Value -> Value` (unit-tested); the CLI driver in
//! `main.rs` supplies the resolved binary path and does the file IO. They are
//! **idempotent**: re-running replaces slop's own entries (picking up a new
//! binary path) while preserving every other MCP server and hook the user has.

use serde_json::{json, Value};

/// Argument tail that marks a `command` string as slop-authored. Used to find
/// and replace prior installs so re-running doesn't stack duplicate hooks.
pub const PRE_TOOL_CMD: &str = "hook pre-tool-use";
pub const POST_TOOL_CMD: &str = "hook post-tool-use";
pub const PROMPT_CMD: &str = "hook user-prompt-submit";
pub const SESSION_CMD: &str = "hook session-start";
pub const SUBAGENT_CMD: &str = "hook subagent-start";

/// Tools the PostToolUse hook is registered for: `Read` drives read-path
/// compression + steering; the write tools let it record the session edit zone
/// that compression measures graph distance against.
const POST_TOOL_MATCHER: &str = "Read|Write|Edit|MultiEdit";

/// The PreToolUse hook only judges proposed writes, so it never sees reads.
const PRE_TOOL_MATCHER: &str = "Write|Edit|MultiEdit";

fn ensure_object(v: &mut Value) -> &mut serde_json::Map<String, Value> {
    if !v.is_object() {
        *v = json!({});
    }
    v.as_object_mut().expect("just ensured object")
}

/// Add (or replace) slop's stdio MCP server in a `.mcp.json` value, preserving
/// any other `mcpServers`. `exe` is the absolute slop binary path, `repo` the
/// absolute repo root the tools default to.
pub fn merge_mcp(mut existing: Value, exe: &str, repo: &str) -> Value {
    let obj = ensure_object(&mut existing);
    let servers = obj.entry("mcpServers").or_insert_with(|| json!({}));
    ensure_object(servers).insert(
        "slop".to_string(),
        json!({
            "type": "stdio",
            "command": exe,
            "args": ["mcp", repo],
        }),
    );
    existing
}

fn is_slop_command(hook: &Value) -> bool {
    hook.get("command")
        .and_then(Value::as_str)
        .is_some_and(|c| {
            [PRE_TOOL_CMD, POST_TOOL_CMD, PROMPT_CMD, SESSION_CMD, SUBAGENT_CMD]
                .iter()
                .any(|cmd| c.contains(cmd))
        })
}

/// Strip slop-authored inner hooks from an event's group array, dropping any
/// group left empty — so a re-install doesn't accumulate stale entries while
/// leaving the user's own hooks in place.
fn strip_slop_groups(groups: Option<&Value>) -> Vec<Value> {
    let Some(arr) = groups.and_then(Value::as_array) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|group| {
            let Some(inner) = group.get("hooks").and_then(Value::as_array) else {
                return Some(group.clone()); // shape we don't manage — leave it
            };
            let kept: Vec<Value> =
                inner.iter().filter(|h| !is_slop_command(h)).cloned().collect();
            if kept.is_empty() {
                return None;
            }
            let mut group = group.clone();
            group["hooks"] = Value::Array(kept);
            Some(group)
        })
        .collect()
}

fn command_group(matcher: Option<&str>, command: String) -> Value {
    let hook = json!({ "type": "command", "command": command });
    match matcher {
        Some(m) => json!({ "matcher": m, "hooks": [hook] }),
        None => json!({ "hooks": [hook] }),
    }
}

/// Add (or replace) slop's PreToolUse + PostToolUse + UserPromptSubmit hooks in
/// a `settings.json` value, preserving unrelated hooks.
pub fn merge_hooks(mut existing: Value, exe: &str) -> Value {
    let obj = ensure_object(&mut existing);
    let hooks_val = obj.entry("hooks").or_insert_with(|| json!({}));
    let hooks = ensure_object(hooks_val);

    let mut pre = strip_slop_groups(hooks.get("PreToolUse"));
    pre.push(command_group(
        Some(PRE_TOOL_MATCHER),
        format!("{exe} {PRE_TOOL_CMD}"),
    ));
    hooks.insert("PreToolUse".to_string(), Value::Array(pre));

    let mut post = strip_slop_groups(hooks.get("PostToolUse"));
    post.push(command_group(
        Some(POST_TOOL_MATCHER),
        format!("{exe} {POST_TOOL_CMD}"),
    ));
    hooks.insert("PostToolUse".to_string(), Value::Array(post));

    let mut prompt = strip_slop_groups(hooks.get("UserPromptSubmit"));
    prompt.push(command_group(None, format!("{exe} {PROMPT_CMD}")));
    hooks.insert("UserPromptSubmit".to_string(), Value::Array(prompt));

    // World model, injected once per session and into every subagent — subagents
    // otherwise start with no codebase context at all.
    for (event, cmd) in [("SessionStart", SESSION_CMD), ("SubagentStart", SUBAGENT_CMD)] {
        let mut groups = strip_slop_groups(hooks.get(event));
        groups.push(command_group(None, format!("{exe} {cmd}")));
        hooks.insert(event.to_string(), Value::Array(groups));
    }

    existing
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_merge_preserves_other_servers() {
        let existing = json!({ "mcpServers": { "other": { "command": "x" } } });
        let out = merge_mcp(existing, "/bin/slop", "/repo");
        assert_eq!(out["mcpServers"]["other"]["command"], "x");
        assert_eq!(out["mcpServers"]["slop"]["command"], "/bin/slop");
        assert_eq!(out["mcpServers"]["slop"]["args"], json!(["mcp", "/repo"]));
    }

    #[test]
    fn mcp_merge_from_empty() {
        let out = merge_mcp(Value::Null, "/bin/slop", "/repo");
        assert_eq!(out["mcpServers"]["slop"]["type"], "stdio");
    }

    #[test]
    fn hooks_merge_is_idempotent_and_preserves_user_hooks() {
        // A user's own PostToolUse hook plus a stale slop one from a prior
        // install at a different path.
        let existing = json!({
            "hooks": {
                "PostToolUse": [
                    { "matcher": "Bash", "hooks": [ { "type": "command", "command": "my-linter" } ] },
                    { "matcher": "Read", "hooks": [ { "type": "command", "command": "/old/slop hook post-tool-use" } ] }
                ]
            }
        });
        let once = merge_hooks(existing, "/new/slop");
        let twice = merge_hooks(once.clone(), "/new/slop");
        assert_eq!(once, twice, "install must be idempotent");

        let post = twice["hooks"]["PostToolUse"].as_array().unwrap();
        // User's Bash hook survives; exactly one slop group; no stale path.
        assert!(post.iter().any(|g| g["matcher"] == "Bash"));
        let slop_groups: Vec<_> = post
            .iter()
            .filter(|g| {
                g["hooks"][0]["command"]
                    .as_str()
                    .unwrap_or("")
                    .contains("post-tool-use")
            })
            .collect();
        assert_eq!(slop_groups.len(), 1);
        assert!(slop_groups[0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .starts_with("/new/slop"));
        // And the write tools are covered so the edit zone gets recorded.
        assert!(slop_groups[0]["matcher"]
            .as_str()
            .unwrap()
            .contains("Write"));
    }

    #[test]
    fn hooks_merge_adds_prompt_hook() {
        let out = merge_hooks(Value::Null, "/bin/slop");
        let prompt = out["hooks"]["UserPromptSubmit"].as_array().unwrap();
        assert_eq!(prompt.len(), 1);
        assert_eq!(
            prompt[0]["hooks"][0]["command"],
            "/bin/slop hook user-prompt-submit"
        );
    }
}
