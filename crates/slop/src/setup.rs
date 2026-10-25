use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

const CODEX_BLOCK_START: &str = "# >>> slop managed: mcp";
const CODEX_BLOCK_END: &str = "# <<< slop managed: mcp";
const STATE_PATH: &str = ".slop/install-state.json";

const PRE_TOOL_CMD: &str = "hook pre-tool-use";
// Written by older setups; still recognised so setup and uninstall strip it.
const POST_TOOL_CMD: &str = "hook post-tool-use";
const PROMPT_CMD: &str = "hook user-prompt-submit";
const SESSION_CMD: &str = "hook session-start";
const SUBAGENT_CMD: &str = "hook subagent-start";
/// What Claude Code users run instead: hooks, MCP and the skill ship as the plugin in `plugin/`.
pub const CLAUDE_PLUGIN_STEPS: [&str; 2] = ["/plugin marketplace add mattmorr1/slop.py", "/plugin install slop@slop"];

/// The receipt an older `slop setup` wrote; read only to undo what it lists.
#[derive(Deserialize)]
struct InstallState {
    files: BTreeMap<String, OwnedFile>,
}

#[derive(Deserialize)]
struct OwnedFile {
    installed_hash: String,
    created: bool,
}

#[derive(Clone, Debug)]
struct FileChange {
    path: PathBuf,
    content: Option<Vec<u8>>,
}

/// Undo what an older setup wrote into `repo` if it left a receipt; the plugin replaces it.
pub fn configure(repo: &Path, force: bool) -> Result<Vec<PathBuf>> {
    if !repo.join(STATE_PATH).is_file() {
        return Ok(Vec::new());
    }
    uninstall(repo, force)
}

pub fn doctor(repo: &Path) -> Result<bool> {
    let repo = canonical_repo(repo)?;
    let binary = plugin_binary();
    match &binary {
        Some(found) => println!("ok   plugin binary: {found}"),
        None => println!("fail plugin binary: set SLOP_BIN, put slop {} on PATH, or install uv", env!("CARGO_PKG_VERSION")),
    }
    let markers = ["pyproject.toml", "package.json", "Cargo.toml"];
    let project_markers = markers
        .iter()
        .filter(|name| repo.join(name).is_file())
        .count();
    println!(
        "{} project markers: {project_markers}",
        if project_markers > 0 { "ok  " } else { "warn" }
    );
    let indexes = slop_core::analyze::index::index_paths(&repo, None);
    if indexes.iter().all(|path| path.is_file()) {
        println!("ok   index: {} artifact(s)", indexes.len());
    } else {
        println!("warn index: missing; `slop check` will build it when an indexer is available");
    }
    if repo.join("slop.toml").is_file() {
        println!("ok   policy: slop.toml");
    } else {
        println!("warn policy: no slop.toml, so channel bypasses are not checked (run `slop init`)");
    }
    if repo.join(STATE_PATH).is_file() {
        println!("warn older setup: {STATE_PATH} found; `slop setup` removes what it lists");
    }
    Ok(binary.is_some())
}

/// The binary the plugin launcher (`plugin/bin/slop-run`) would run, checked in its order.
fn plugin_binary() -> Option<String> {
    if let Some(bin) = std::env::var_os("SLOP_BIN") {
        return Some(format!("SLOP_BIN={}", PathBuf::from(bin).display()));
    }
    let version = env!("CARGO_PKG_VERSION");
    let matches = |slop: &PathBuf| Command::new(slop).arg("--version").output().is_ok_and(|out| String::from_utf8_lossy(&out.stdout).trim() == format!("slop {version}"));
    on_path("slop").filter(matches).map(|slop| slop.display().to_string())
        .or_else(|| on_path("uvx").map(|uvx| format!("{} --from slop-cli=={version} slop", uvx.display())))
}

fn on_path(command: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?).map(|dir| dir.join(command)).find(|path| path.is_file())
}

pub fn uninstall(repo: &Path, force: bool) -> Result<Vec<PathBuf>> {
    let repo = canonical_repo(repo)?;
    let state_path = repo.join(STATE_PATH);
    let state: InstallState = serde_json::from_slice(
        &fs::read(&state_path).with_context(|| format!("reading {}", state_path.display()))?,
    )
    .with_context(|| format!("parsing {}", state_path.display()))?;
    let mut changes = Vec::new();
    for (relative, owned) in &state.files {
        if relative == STATE_PATH {
            continue;
        }
        let path = repo.join(relative);
        let Some(current) = read_optional(&path)? else {
            continue;
        };
        let current_hash = hash(&current);
        let updated = if owned.created && current_hash == owned.installed_hash {
            None
        } else {
            remove_owned_content(relative, &current)?
        };
        if updated.is_none() && current_hash != owned.installed_hash && !force {
            bail!(
                "{} changed since setup; refusing to delete it (pass --force)",
                path.display()
            );
        }
        changes.push(FileChange {
            path,
            content: updated,
        });
    }
    changes.push(FileChange {
        path: state_path,
        content: None,
    });
    let paths = changes.iter().map(|c| c.path.clone()).collect();
    apply_transaction(&repo, changes)?;
    Ok(paths)
}


fn canonical_repo(repo: &Path) -> Result<PathBuf> {
    repo.canonicalize()
        .with_context(|| format!("resolving repo path {}", repo.display()))
}


fn remove_owned_content(relative: &str, current: &[u8]) -> Result<Option<Vec<u8>>> {
    if relative == ".codex/config.toml" {
        let text = std::str::from_utf8(current).context("Codex config is not UTF-8")?;
        let (Some(start), Some(end)) = (text.find(CODEX_BLOCK_START), text.find(CODEX_BLOCK_END))
        else {
            return Ok(Some(current.to_vec()));
        };
        let end = end + CODEX_BLOCK_END.len();
        let mut out = format!("{}{}", &text[..start], &text[end..]);
        while out.contains("\n\n\n") {
            out = out.replace("\n\n\n", "\n\n");
        }
        return Ok(if out.trim().is_empty() {
            None
        } else {
            Some(out.into_bytes())
        });
    }
    if relative == ".mcp.json" {
        let mut value: Value = serde_json::from_slice(current)?;
        if let Some(servers) = value.get_mut("mcpServers").and_then(Value::as_object_mut) {
            servers.remove("slop");
        }
        return Ok(Some(json_bytes(&value)?));
    }
    if relative == ".claude/settings.json" {
        let value: Value = serde_json::from_slice(current)?;
        return Ok(Some(json_bytes(&remove_hooks(value))?));
    }
    Ok(None)
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("refusing to modify symlink {}", path.display())
        }
        Ok(_) => fs::read(path)
            .map(Some)
            .with_context(|| format!("reading {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("inspecting {}", path.display())),
    }
}

fn apply_transaction(repo: &Path, changes: Vec<FileChange>) -> Result<()> {
    let journal = repo.join(".slop/setup-journal.json");
    let mut staged = Vec::with_capacity(changes.len());
    for (index, change) in changes.iter().enumerate() {
        if let Some(content) = &change.content {
            if let Some(parent) = change.path.parent() {
                if let Err(error) = fs::create_dir_all(parent) {
                    cleanup_staged(&staged);
                    return Err(error).with_context(|| format!("creating {}", parent.display()));
                }
            }
            let stage = sibling(&change.path, &format!("stage-{index}"));
            if let Err(error) = write_new(&stage, content) {
                cleanup_staged(&staged);
                return Err(error);
            }
            staged.push(Some(stage));
        } else {
            staged.push(None);
        }
    }
    let journal_body = json!({ "schema": 1, "targets": changes.iter().map(|c| c.path.to_string_lossy()).collect::<Vec<_>>() });
    atomic_replace(
        &journal,
        format!("{}\n", serde_json::to_string_pretty(&journal_body)?).as_bytes(),
    )?;
    let mut committed: Vec<(PathBuf, Option<PathBuf>)> = Vec::new();
    for (index, change) in changes.iter().enumerate() {
        let backup = if change.path.exists() {
            let backup = sibling(&change.path, &format!("backup-{index}"));
            fs::rename(&change.path, &backup)?;
            Some(backup)
        } else {
            None
        };
        let result = match &staged[index] {
            Some(stage) => fs::rename(stage, &change.path),
            None => Ok(()),
        };
        if let Err(error) = result {
            if let Some(backup) = &backup {
                let _ = fs::rename(backup, &change.path);
            }
            rollback(&committed);
            cleanup_staged(&staged);
            let _ = fs::remove_file(&journal);
            return Err(error).with_context(|| format!("committing {}", change.path.display()));
        }
        committed.push((change.path.clone(), backup));
    }
    for (_, backup) in &committed {
        if let Some(backup) = backup {
            fs::remove_file(backup)?;
        }
    }
    fs::remove_file(&journal)?;
    Ok(())
}

fn rollback(committed: &[(PathBuf, Option<PathBuf>)]) {
    for (path, backup) in committed.iter().rev() {
        let _ = fs::remove_file(path);
        if let Some(backup) = backup {
            let _ = fs::rename(backup, path);
        }
    }
}

fn cleanup_staged(staged: &[Option<PathBuf>]) {
    for path in staged.iter().flatten() {
        let _ = fs::remove_file(path);
    }
}

fn atomic_replace(path: &Path, content: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let stage = sibling(path, "atomic");
    write_new(&stage, content)?;
    fs::rename(&stage, path).with_context(|| format!("replacing {}", path.display()))
}

fn write_new(path: &Path, content: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .with_context(|| format!("staging {}", path.display()))?;
    file.write_all(content)?;
    file.sync_all()?;
    Ok(())
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let name = path.file_name().and_then(OsStr::to_str).unwrap_or("config");
    path.with_file_name(format!(".{name}.slop-{}-{suffix}", std::process::id()))
}


fn json_bytes(value: &Value) -> Result<Vec<u8>> {
    Ok(format!("{}\n", serde_json::to_string_pretty(value)?).into_bytes())
}
fn hash(content: &[u8]) -> String {
    blake3::hash(content).to_hex().to_string()
}

fn is_slop_command(hook: &Value) -> bool {
    let Some(command) = hook.get("command").and_then(Value::as_str) else {
        return false;
    };
    [
        PRE_TOOL_CMD,
        POST_TOOL_CMD,
        PROMPT_CMD,
        SESSION_CMD,
        SUBAGENT_CMD,
    ]
    .iter()
    .any(|tail| {
        let Some(exe) = command.strip_suffix(&format!(" {tail}")) else {
            return false;
        };
        Path::new(exe.trim_matches(['\'', '"']))
            .file_name()
            .is_some_and(|name| name == "slop")
    })
}

fn strip_slop_groups(groups: Option<&Value>) -> Vec<Value> {
    let Some(groups) = groups.and_then(Value::as_array) else {
        return Vec::new();
    };
    groups
        .iter()
        .filter_map(|group| {
            let Some(hooks) = group.get("hooks").and_then(Value::as_array) else {
                return Some(group.clone());
            };
            let kept: Vec<_> = hooks
                .iter()
                .filter(|hook| !is_slop_command(hook))
                .cloned()
                .collect();
            if kept.is_empty() {
                return None;
            }
            let mut group = group.clone();
            group["hooks"] = Value::Array(kept);
            Some(group)
        })
        .collect()
}

fn remove_hooks(mut existing: Value) -> Value {
    let Some(hooks) = existing.get_mut("hooks").and_then(Value::as_object_mut) else {
        return existing;
    };
    for event in [
        "PreToolUse",
        "PostToolUse",
        "UserPromptSubmit",
        "SessionStart",
        "SubagentStart",
    ] {
        let groups = strip_slop_groups(hooks.get(event));
        if groups.is_empty() {
            hooks.remove(event);
        } else {
            hooks.insert(event.into(), Value::Array(groups));
        }
    }
    existing
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_removal_keeps_similar_user_commands() {
        let existing = json!({ "hooks": {
            "PostToolUse": [{ "hooks": [
                { "type": "command", "command": "my-slop hook post-tool-use --audit" },
                { "type": "command", "command": "/old/slop hook post-tool-use" }
            ] }],
            "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": "'/a b/slop' hook user-prompt-submit" }] }]
        } });
        let out = remove_hooks(existing);
        assert_eq!(out["hooks"]["PostToolUse"][0]["hooks"], json!([{ "type": "command", "command": "my-slop hook post-tool-use --audit" }]));
        assert!(out["hooks"].get("UserPromptSubmit").is_none());
    }

    #[test]
    fn setup_removes_what_an_older_setup_installed() {
        let repo = std::env::temp_dir().join(format!("slop-setup-{}", std::process::id()));
        let _ = fs::remove_dir_all(&repo);
        fs::create_dir_all(repo.join(".claude/skills/slop")).unwrap();
        fs::create_dir_all(repo.join(".codex")).unwrap();
        fs::create_dir_all(repo.join(".slop")).unwrap();
        let settings = json!({ "model": "x", "hooks": { "SessionStart": [{ "hooks": [{ "type": "command", "command": "/bin/slop hook session-start" }] }] } });
        let mcp = json!({ "mcpServers": { "slop": { "command": "/bin/slop" }, "other": { "command": "o" } } });
        let codex = format!("model = \"x\"\n\n{CODEX_BLOCK_START}\n[mcp_servers.slop]\n{CODEX_BLOCK_END}\n");
        let files = [(".claude/settings.json", json_bytes(&settings).unwrap(), false), (".mcp.json", json_bytes(&mcp).unwrap(), false), (".claude/skills/slop/SKILL.md", b"old skill".to_vec(), true), (".codex/config.toml", codex.into_bytes(), false)];
        let owned: serde_json::Map<_, _> = files.iter().map(|(rel, content, created)| (rel.to_string(), json!({ "installed_hash": hash(content), "created": created }))).collect();
        for (rel, content, _) in &files {
            fs::write(repo.join(rel), content).unwrap();
        }
        fs::write(repo.join(STATE_PATH), json_bytes(&json!({ "schema": 2, "ai": "claude", "files": owned })).unwrap()).unwrap();
        assert_eq!(configure(&repo, false).unwrap().len(), files.len() + 1);
        let settings: Value = serde_json::from_slice(&fs::read(repo.join(".claude/settings.json")).unwrap()).unwrap();
        assert_eq!(settings, json!({ "model": "x", "hooks": {} }));
        let mcp: Value = serde_json::from_slice(&fs::read(repo.join(".mcp.json")).unwrap()).unwrap();
        assert_eq!(mcp, json!({ "mcpServers": { "other": { "command": "o" } } }));
        assert_eq!(fs::read_to_string(repo.join(".codex/config.toml")).unwrap(), "model = \"x\"\n\n");
        assert!(!repo.join(".claude/skills/slop/SKILL.md").exists());
        assert!(!repo.join(STATE_PATH).exists());
        assert!(configure(&repo, false).unwrap().is_empty(), "a second run has nothing to remove");
        let _ = fs::remove_dir_all(&repo);
    }
}
