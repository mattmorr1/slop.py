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

use std::hash::{Hash, Hasher};

use serde_json::{json, Value};

use crate::compress::{self, CompressConfig};
use crate::policy::Policy;
use crate::skeleton::strip_noise;

/// Only bother remapping when the strip saves at least this fraction of
/// characters — otherwise pass the read through untouched.
const MIN_COMPRESSION_GAIN: f64 = 0.10;

/// How many recently-edited files the session edit zone remembers.
const MAX_ZONE_FILES: usize = 12;

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

/// Session edit-zone state file for `repo`, keyed by its path (in the system
/// temp dir — hooks are separate short-lived processes, so the zone has to
/// persist across invocations somewhere).
fn zone_file(repo: &Path) -> PathBuf {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    repo.hash(&mut h);
    std::env::temp_dir().join(format!("slop-editzone-{:x}.json", h.finish()))
}

/// Append a repo-relative path to the edit zone (most-recent last, capped,
/// deduped). Best-effort — a write failure just means no zoning next read.
fn record_edit(zone_path: &Path, rel: &str) {
    let mut files = load_zone(zone_path);
    files.retain(|f| f != rel);
    files.push(rel.to_string());
    let overflow = files.len().saturating_sub(MAX_ZONE_FILES);
    files.drain(0..overflow);
    let _ = std::fs::write(
        zone_path,
        serde_json::to_string(&json!({ "files": files })).unwrap_or_default(),
    );
}

fn load_zone(zone_path: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(zone_path) else {
        return Vec::new();
    };
    serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| {
            v.get("files").and_then(Value::as_array).map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
        })
        .unwrap_or_default()
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

/// Zoned graph-distance compression of a read (task's core token-efficiency
/// win): skeletonize entities beyond the edit zone. Returns `None` — so the
/// caller falls back to a plain noise-strip — when it isn't worth it (small
/// file, no graph/index, edited files not in the graph, nothing skeletonized).
pub fn compress_read(
    repo: &Path,
    rel_file: &str,
    source: &str,
    edit_files: &[String],
) -> Option<String> {
    let config = CompressConfig::default();
    if source.lines().count() < config.min_lines {
        return None;
    }
    let analysis = crate::check::load_analysis(repo, None).ok()?;
    let loci: Vec<String> = analysis
        .built
        .graph
        .entities()
        .filter(|(_, e)| edit_files.contains(&e.file))
        .map(|(_, e)| e.id.clone())
        .collect();
    if loci.is_empty() {
        return None;
    }
    let (out, stats) = compress::compress_file(
        &analysis.built,
        &analysis.facts,
        source,
        rel_file,
        &loci,
        &config,
        None,
    );
    (stats.skeletonized > 0).then_some(out)
}

/// PostToolUse handler. Returns the JSON to print to stdout: either a
/// `hookSpecificOutput` with `updatedToolOutput`/`additionalContext`, or an
/// empty object (no-op passthrough).
pub fn handle_post_tool_use(input: &Value) -> Value {
    let tool = as_str(input, "tool_name").unwrap_or("");

    // Writes/edits define the edit zone that later reads compress against.
    if matches!(tool, "Write" | "Edit" | "MultiEdit") {
        if let Some(path) = input
            .get("tool_input")
            .and_then(|t| as_str(t, "file_path"))
            .filter(|p| p.ends_with(".py"))
        {
            let repo = find_repo_root(&read_target(input).1);
            if let Some(rel) = repo_relative(&repo, path) {
                record_edit(&zone_file(&repo), &rel);
            }
        }
        return json!({});
    }

    if tool != "Read" {
        return json!({});
    }
    let (file, cwd) = read_target(input);
    let Some(file) = file.filter(|f| f.ends_with(".py")) else {
        return json!({});
    };
    let repo = find_repo_root(&cwd);
    let steering = channel_steering(&Policy::load(&repo).unwrap_or_default());

    let mut hook = json!({ "hookEventName": "PostToolUse" });
    let mut changed = false;

    if let Some(src) = read_output_text(input) {
        // Prefer zoned graph-distance compression; fall back to noise-strip.
        let zone = load_zone(&zone_file(&repo));
        let compressed = if zone.is_empty() {
            None
        } else {
            repo_relative(&repo, &file).and_then(|rel| compress_read(&repo, &rel, &src, &zone))
        };
        let remapped = compressed.unwrap_or_else(|| strip_noise(&src));
        let gain = 1.0 - (remapped.len() as f64 / src.len().max(1) as f64);
        if gain >= MIN_COMPRESSION_GAIN {
            hook["updatedToolOutput"] = Value::String(remapped);
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

    #[test]
    fn edit_zone_roundtrips_deduped_and_capped() {
        let dir = std::env::temp_dir().join(format!("slop-zone-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("zone.json");
        let _ = std::fs::remove_file(&path);

        record_edit(&path, "a.py");
        record_edit(&path, "b.py");
        record_edit(&path, "a.py"); // dedup -> moves a.py to the end
        assert_eq!(load_zone(&path), vec!["b.py".to_string(), "a.py".to_string()]);

        for i in 0..MAX_ZONE_FILES + 5 {
            record_edit(&path, &format!("f{i}.py"));
        }
        assert_eq!(load_zone(&path).len(), MAX_ZONE_FILES);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn write_records_edit_zone() {
        // A .py Write records the edited file (repo root falls back to cwd when
        // there's no slop.toml/.git).
        let dir = std::env::temp_dir().join(format!("slop-write-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let zone = zone_file(&dir);
        let _ = std::fs::remove_file(&zone);
        let file_path = dir.join("mod.py");

        let out = handle_post_tool_use(&json!({
            "tool_name": "Edit",
            "cwd": dir.to_string_lossy(),
            "tool_input": { "file_path": file_path.to_string_lossy() }
        }));
        assert_eq!(out, json!({})); // edits are silent, side-effect only
        assert_eq!(load_zone(&zone), vec!["mod.py".to_string()]);
        let _ = std::fs::remove_file(&zone);
    }

    #[test]
    fn compress_read_skips_small_files() {
        // Below min_lines it declines (caller noise-strips instead) rather than
        // paying for a graph build.
        let tiny = "def f():\n    return 1\n";
        assert_eq!(
            compress_read(Path::new("/nope"), "m.py", tiny, &["x.py".to_string()]),
            None
        );
    }
}
