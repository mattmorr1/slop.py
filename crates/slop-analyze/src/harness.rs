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

use slop_parse::Language;

use crate::compress::{self, CompressConfig};
use crate::policy::Policy;
use crate::precheck::precheck;
use crate::world;

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

/// Session state: files recently edited (the edit zone reads compress
/// against) and files already read (a re-read signals the agent needs the
/// real content — probably to edit it — so it's served verbatim).
#[derive(Default)]
struct Zone {
    edits: Vec<String>,
    reads: Vec<String>,
}

fn arr(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

fn load_state(zone_path: &Path) -> Zone {
    let Ok(text) = std::fs::read_to_string(zone_path) else {
        return Zone::default();
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return Zone::default();
    };
    Zone {
        // `files` is the pre-read-tracking key name; keep reading it as edits.
        edits: if v.get("edits").is_some() { arr(&v, "edits") } else { arr(&v, "files") },
        reads: arr(&v, "reads"),
    }
}

fn save_state(zone_path: &Path, zone: &Zone) {
    let _ = std::fs::write(
        zone_path,
        serde_json::to_string(&json!({ "edits": zone.edits, "reads": zone.reads }))
            .unwrap_or_default(),
    );
}

/// Move `rel` to the most-recent end of `list`, deduped and capped.
fn push_capped(list: &mut Vec<String>, rel: &str) {
    list.retain(|f| f != rel);
    list.push(rel.to_string());
    let overflow = list.len().saturating_sub(MAX_ZONE_FILES);
    list.drain(0..overflow);
}

fn record_edit(zone_path: &Path, rel: &str) {
    let mut zone = load_state(zone_path);
    push_capped(&mut zone.edits, rel);
    save_state(zone_path, &zone);
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
    // No policy => silent (D8), and we skip reading the file at all.
    if policy.channels.is_empty() {
        return json!({});
    }
    let Some(rel) = repo_relative(&repo, &file) else {
        return json!({});
    };
    let Some(content) = proposed_content(tool, input, Path::new(&file)) else {
        return json!({});
    };
    let findings = precheck(lang, &rel, &content, &policy);
    if findings.is_empty() {
        return json!({});
    }
    let context: Vec<String> = findings.iter().map(|f| f.steering()).collect();
    json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "additionalContext": context.join("\n"),
        }
    })
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
            .filter(|p| Language::from_path(p).is_some())
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
    let Some(file) = file.filter(|f| Language::from_path(f).is_some()) else {
        return json!({});
    };
    let repo = find_repo_root(&cwd);
    let steering = channel_steering(&Policy::load(&repo).unwrap_or_default());

    let mut hook = json!({ "hookEventName": "PostToolUse" });
    let mut changed = false;

    if let Some(src) = read_output_text(input) {
        // Edit-invertibility: only ever remap a read by *zoned skeletonization
        // of graph-distant context* (code you're editing elsewhere, so you
        // won't string-edit against it). Everything else is served verbatim —
        // no strip, no skeleton — so an edit can always match the real file.
        // A re-read means the agent came back to the file (likely to edit it),
        // so it's served verbatim too; this self-corrects a first-read edit
        // that failed against a skeleton (fail -> re-read -> verbatim -> ok).
        let compress_reads = std::env::var("SLOP_COMPRESS_READS")
            .map(|v| v != "0")
            .unwrap_or(true);
        let zone_path = zone_file(&repo);
        let mut zone = load_state(&zone_path);
        let rel = repo_relative(&repo, &file);
        let is_reread = rel.as_deref().is_some_and(|r| zone.reads.iter().any(|x| x == r));
        if let Some(r) = &rel {
            push_capped(&mut zone.reads, r);
            save_state(&zone_path, &zone);
        }

        if compress_reads && !is_reread {
            if let Some(out) = rel
                .as_deref()
                .and_then(|r| compress_read(&repo, r, &src, &zone.edits))
            {
                let gain = 1.0 - (out.len() as f64 / src.len().max(1) as f64);
                if gain >= MIN_COMPRESSION_GAIN {
                    hook["updatedToolOutput"] = Value::String(out);
                    changed = true;
                }
            }
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
    let policy = Policy::load(&repo).unwrap_or_default();
    let analysis = crate::check::load_analysis(&repo, None).ok();
    let built = analysis.as_ref().map(|a| &a.built);

    let Some(context) = world::render(&policy, built, world::DEFAULT_BUDGET) else {
        return json!({});
    };
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
    fn read_with_no_edit_zone_is_served_verbatim() {
        // Edit-invertibility: with no edit zone (and no index), a read is never
        // remapped — the agent must be able to string-edit against the real
        // file. No updatedToolOutput.
        let src = "def f():\n    # a comment\n    return 1\n";
        let out = handle_post_tool_use(&json!({
            "tool_name": "Read", "tool_input": {"file_path": "/nope/mod.py"},
            "tool_output": src, "cwd": "/nope"
        }));
        assert!(out.get("hookSpecificOutput").is_none() || out["hookSpecificOutput"].get("updatedToolOutput").is_none());
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
        assert_eq!(load_state(&path).edits, vec!["b.py".to_string(), "a.py".to_string()]);

        for i in 0..MAX_ZONE_FILES + 5 {
            record_edit(&path, &format!("f{i}.py"));
        }
        assert_eq!(load_state(&path).edits.len(), MAX_ZONE_FILES);
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
        assert_eq!(load_state(&zone).edits, vec!["mod.py".to_string()]);
        let _ = std::fs::remove_file(&zone);
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

    #[test]
    fn edit_zone_records_every_supported_language() {
        // W3: the wedge is not Python-only. A .rs edit must register too.
        let dir = std::env::temp_dir().join(format!("slop-lang-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let zone = zone_file(&dir);
        let _ = std::fs::remove_file(&zone);
        for name in ["a.rs", "b.ts", "c.py"] {
            handle_post_tool_use(&json!({
                "tool_name": "Edit",
                "cwd": dir.to_string_lossy(),
                "tool_input": { "file_path": dir.join(name).to_string_lossy() }
            }));
        }
        assert_eq!(load_state(&zone).edits, vec!["a.rs", "b.ts", "c.py"]);
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
