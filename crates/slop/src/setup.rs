use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const SKILL: &str = include_str!("../assets/slop-skill.md");
const CODEX_BLOCK_START: &str = "# >>> slop managed: mcp";
const CODEX_BLOCK_END: &str = "# <<< slop managed: mcp";
const STATE_PATH: &str = ".slop/install-state.json";

const PRE_TOOL_CMD: &str = "hook pre-tool-use";
const POST_TOOL_CMD: &str = "hook post-tool-use";
const PROMPT_CMD: &str = "hook user-prompt-submit";
const SESSION_CMD: &str = "hook session-start";
const SUBAGENT_CMD: &str = "hook subagent-start";
const POST_TOOL_MATCHER: &str = "Read|Write|Edit|MultiEdit";
const PRE_TOOL_MATCHER: &str = "Write|Edit|MultiEdit";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum AgentHost {
    Claude,
    Codex,
}

impl AgentHost {
    pub fn executable(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Editor {
    None,
    VsCode,
    Cursor,
    Zed,
}

impl Editor {
    fn executable(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::VsCode => Some("code"),
            Self::Cursor => Some("cursor"),
            Self::Zed => Some("zed"),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Preferences {
    pub ai: AgentHost,
    pub editor: Editor,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct InstallState {
    schema: u8,
    ai: AgentHost,
    files: BTreeMap<String, OwnedFile>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct OwnedFile {
    installed_hash: String,
    created: bool,
}

#[derive(Clone, Debug)]
struct FileChange {
    path: PathBuf,
    content: Option<Vec<u8>>,
}

pub fn configure(
    repo: &Path,
    ai: Option<AgentHost>,
    editor: Option<Editor>,
    force: bool,
) -> Result<Preferences> {
    let repo = canonical_repo(repo)?;
    let prior = load_preferences().ok();
    let ai = choose_ai(ai.or_else(|| prior.as_ref().map(|p| p.ai)))?;
    let editor = choose_editor(editor.or_else(|| prior.as_ref().map(|p| p.editor)))?;
    let preferences = Preferences { ai, editor };
    let exe = std::env::current_exe().context("resolving the slop binary path")?;
    let changes = install_changes(&repo, &exe, ai, force)?;
    apply_transaction(&repo, changes)?;
    save_preferences(&preferences)?;
    Ok(preferences)
}

pub fn configure_claude(repo: &Path, force: bool) -> Result<Preferences> {
    configure(
        repo,
        Some(AgentHost::Claude),
        load_preferences()
            .ok()
            .map(|p| p.editor)
            .or(Some(Editor::None)),
        force,
    )
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

pub fn launch(
    repo: &Path,
    ai: Option<AgentHost>,
    editor: Option<Editor>,
    no_setup: bool,
    no_editor: bool,
    args: &[String],
) -> Result<()> {
    let repo = canonical_repo(repo)?;
    let saved = load_preferences().ok();
    let ai = ai
        .or_else(|| saved.as_ref().map(|p| p.ai))
        .unwrap_or(AgentHost::Claude);
    let editor = editor
        .or_else(|| saved.as_ref().map(|p| p.editor))
        .unwrap_or(Editor::None);
    if !no_setup {
        let _ = configure(&repo, Some(ai), Some(editor), false)?;
    }
    if !no_editor {
        launch_editor(editor, &repo)?;
    }
    let status = Command::new(ai.executable())
        .current_dir(&repo)
        .args(args)
        .status()
        .with_context(|| format!("launching {} (is it on PATH?)", ai.executable()))?;
    if !status.success() {
        bail!("{} exited with {}", ai.executable(), status);
    }
    Ok(())
}

pub fn delegate(repo: &Path, prompt: &str) -> Result<()> {
    let repo = canonical_repo(repo)?;
    let ai = load_preferences()?.ai;
    let mut command = Command::new(ai.executable());
    command.current_dir(&repo);
    match ai {
        AgentHost::Claude => {
            command.args(["-p", prompt, "--permission-mode", "acceptEdits"]);
        }
        AgentHost::Codex => {
            command.args(["exec", prompt]);
        }
    }
    let status = command
        .status()
        .with_context(|| format!("delegating fix to {}", ai.executable()))?;
    if !status.success() {
        bail!("{} exited with {}", ai.executable(), status);
    }
    Ok(())
}

pub fn open_target(target: &str) -> Result<()> {
    let editor = load_preferences()?.editor;
    let Some(exe) = editor.executable() else {
        bail!("no editor is configured; rerun `slop setup --editor <editor>`");
    };
    let mut command = Command::new(exe);
    if matches!(editor, Editor::VsCode | Editor::Cursor) {
        command.arg("-g");
    }
    command
        .arg(target)
        .spawn()
        .with_context(|| format!("opening {target} in {exe}"))?;
    Ok(())
}

pub fn doctor(repo: &Path) -> Result<bool> {
    let repo = canonical_repo(repo)?;
    let preferences = load_preferences().ok();
    let mut healthy = true;
    match preferences {
        Some(ref p) => {
            healthy &= report_command("AI", p.ai.executable());
            if let Some(editor) = p.editor.executable() {
                healthy &= report_command("editor", editor);
            } else {
                println!("ok   editor: disabled");
            }
        }
        None => {
            println!("fail preferences: run `slop setup --ai <claude|codex> --editor <...>`");
            healthy = false;
        }
    }
    let state = repo.join(STATE_PATH);
    if state.is_file() {
        println!("ok   installation receipt: {}", state.display());
        match read_install_state(&state) {
            Ok(receipt) => {
                for (relative, owned) in receipt.files {
                    let path = repo.join(&relative);
                    match read_optional(&path) {
                        Ok(Some(content)) if hash(&content) == owned.installed_hash => {
                            println!("ok   harness: {relative}");
                        }
                        Ok(Some(content)) if owned_content_present(&relative, &content) => {
                            println!("ok   harness: {relative} (shared config changed)");
                        }
                        Ok(_) => {
                            println!("fail harness: {relative} is missing slop's owned entry");
                            healthy = false;
                        }
                        Err(error) => {
                            println!("fail harness: {relative}: {error}");
                            healthy = false;
                        }
                    }
                }
            }
            Err(error) => {
                println!("fail installation receipt: {error}");
                healthy = false;
            }
        }
    } else {
        println!(
            "fail installation receipt: run `slop setup {}`",
            repo.display()
        );
        healthy = false;
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
    let indexes = slop_analyze::index::index_paths(&repo, None);
    if indexes.iter().all(|path| path.is_file()) {
        println!("ok   index: {} artifact(s)", indexes.len());
    } else {
        println!("warn index: missing; `slop check` will build it when an indexer is available");
    }
    Ok(healthy)
}

fn read_install_state(path: &Path) -> Result<InstallState> {
    serde_json::from_slice(&fs::read(path).with_context(|| format!("reading {}", path.display()))?)
        .with_context(|| format!("parsing {}", path.display()))
}

fn owned_content_present(relative: &str, content: &[u8]) -> bool {
    match relative {
        ".codex/config.toml" => std::str::from_utf8(content)
            .is_ok_and(|text| text.contains(CODEX_BLOCK_START) && text.contains(CODEX_BLOCK_END)),
        ".mcp.json" => serde_json::from_slice::<Value>(content)
            .ok()
            .is_some_and(|value| !value["mcpServers"]["slop"].is_null()),
        ".claude/settings.json" => serde_json::from_slice::<Value>(content)
            .ok()
            .and_then(|value| value.get("hooks").cloned())
            .and_then(|value| value.as_object().cloned())
            .is_some_and(|hooks| {
                hooks
                    .values()
                    .filter_map(Value::as_array)
                    .flatten()
                    .filter_map(|group| group.get("hooks").and_then(Value::as_array))
                    .flatten()
                    .any(is_slop_command)
            }),
        ".claude/skills/slop/SKILL.md" => content == SKILL.as_bytes(),
        _ => false,
    }
}

pub fn load_preferences() -> Result<Preferences> {
    let path = preferences_path()?;
    serde_json::from_slice(&fs::read(&path).with_context(|| format!("reading {}", path.display()))?)
        .with_context(|| format!("parsing {}", path.display()))
}

fn choose_ai(value: Option<AgentHost>) -> Result<AgentHost> {
    if let Some(value) = value {
        return Ok(value);
    }
    if !std::io::stdin().is_terminal() {
        bail!("--ai is required outside an interactive terminal");
    }
    prompt(
        "Preferred AI",
        &[("Claude", AgentHost::Claude), ("Codex", AgentHost::Codex)],
    )
}

fn choose_editor(value: Option<Editor>) -> Result<Editor> {
    if let Some(value) = value {
        return Ok(value);
    }
    if !std::io::stdin().is_terminal() {
        bail!("--editor is required outside an interactive terminal");
    }
    prompt(
        "Preferred editor",
        &[
            ("None", Editor::None),
            ("VS Code", Editor::VsCode),
            ("Cursor", Editor::Cursor),
            ("Zed", Editor::Zed),
        ],
    )
}

fn prompt<T: Copy>(label: &str, options: &[(&str, T)]) -> Result<T> {
    eprintln!("{label}:");
    for (index, (name, _)) in options.iter().enumerate() {
        eprintln!("  {}) {name}", index + 1);
    }
    eprint!("> ");
    std::io::stderr().flush()?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let index = input
        .trim()
        .parse::<usize>()
        .context("expected a numbered choice")?;
    options
        .get(index.saturating_sub(1))
        .map(|(_, value)| *value)
        .context("choice is out of range")
}

fn preferences_path() -> Result<PathBuf> {
    if let Some(root) = std::env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(root).join("slop/config.json"));
    }
    let home =
        std::env::var_os("HOME").context("HOME is not set; cannot locate slop preferences")?;
    Ok(PathBuf::from(home).join(".config/slop/config.json"))
}

fn save_preferences(preferences: &Preferences) -> Result<()> {
    let path = preferences_path()?;
    let parent = path.parent().context("preference path has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let bytes = format!("{}\n", serde_json::to_string_pretty(preferences)?).into_bytes();
    atomic_replace(&path, &bytes)
}

fn canonical_repo(repo: &Path) -> Result<PathBuf> {
    repo.canonicalize()
        .with_context(|| format!("resolving repo path {}", repo.display()))
}

fn install_changes(repo: &Path, exe: &Path, ai: AgentHost, force: bool) -> Result<Vec<FileChange>> {
    let exe = exe.to_string_lossy();
    let repo_text = repo.to_string_lossy();
    let host_changes = match ai {
        AgentHost::Claude => claude_changes(repo, &exe, &repo_text, force)?,
        AgentHost::Codex => codex_changes(repo, &exe, &repo_text, force)?,
    };
    let files = host_changes
        .iter()
        .filter_map(|change| {
            change.content.as_ref().map(|content| {
                (
                    relative(repo, &change.path),
                    OwnedFile {
                        installed_hash: hash(content),
                        created: !change.path.exists(),
                    },
                )
            })
        })
        .collect();
    let mut changes = migration_removals(repo, ai, force)?;
    changes.extend(host_changes);
    let state = InstallState {
        schema: 2,
        ai,
        files,
    };
    changes.push(FileChange {
        path: repo.join(STATE_PATH),
        content: Some(format!("{}\n", serde_json::to_string_pretty(&state)?).into_bytes()),
    });
    Ok(changes)
}

fn migration_removals(repo: &Path, next: AgentHost, force: bool) -> Result<Vec<FileChange>> {
    let path = repo.join(STATE_PATH);
    let Some(bytes) = read_optional(&path)? else {
        return Ok(Vec::new());
    };
    let state: InstallState =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
    if state.ai == next {
        return Ok(Vec::new());
    }
    let mut changes = Vec::new();
    for (relative, owned) in state.files {
        let target = repo.join(&relative);
        let Some(current) = read_optional(&target)? else {
            continue;
        };
        let current_hash = hash(&current);
        let updated = if owned.created && current_hash == owned.installed_hash {
            None
        } else {
            remove_owned_content(&relative, &current)?
        };
        if updated.is_none() && current_hash != owned.installed_hash && !force {
            bail!(
                "{} changed since setup; refusing to replace host integration (pass --force)",
                target.display()
            );
        }
        changes.push(FileChange {
            path: target,
            content: updated,
        });
    }
    Ok(changes)
}

fn claude_changes(repo: &Path, exe: &str, repo_text: &str, force: bool) -> Result<Vec<FileChange>> {
    let mcp_path = repo.join(".mcp.json");
    let settings_path = repo.join(".claude/settings.json");
    let skill_path = repo.join(".claude/skills/slop/SKILL.md");
    let mcp = merge_mcp(read_json(&mcp_path, force)?, exe, repo_text);
    let settings = merge_hooks(read_json(&settings_path, force)?, exe);
    if let Some(existing) = read_optional(&skill_path)? {
        if existing != SKILL.as_bytes() && !force {
            bail!(
                "{} already exists and is not slop-managed (pass --force)",
                skill_path.display()
            );
        }
    }
    Ok(vec![
        FileChange {
            path: mcp_path,
            content: Some(json_bytes(&mcp)?),
        },
        FileChange {
            path: settings_path,
            content: Some(json_bytes(&settings)?),
        },
        FileChange {
            path: skill_path,
            content: Some(SKILL.as_bytes().to_vec()),
        },
    ])
}

fn codex_changes(repo: &Path, exe: &str, repo_text: &str, force: bool) -> Result<Vec<FileChange>> {
    let config_path = repo.join(".codex/config.toml");
    let existing = read_optional(&config_path)?.unwrap_or_default();
    let text = String::from_utf8(existing)
        .with_context(|| format!("{} is not UTF-8", config_path.display()))?;
    let block = format!("{CODEX_BLOCK_START}\n[mcp_servers.slop]\ncommand = {}\nargs = [{}, {}]\n{CODEX_BLOCK_END}\n", toml_string(exe), toml_string("mcp"), toml_string(repo_text));
    let merged = merge_managed_block(&text, &block, force)?;
    Ok(vec![FileChange {
        path: config_path,
        content: Some(merged.into_bytes()),
    }])
}

fn merge_managed_block(existing: &str, block: &str, force: bool) -> Result<String> {
    match (
        existing.find(CODEX_BLOCK_START),
        existing.find(CODEX_BLOCK_END),
    ) {
        (Some(start), Some(end)) if start <= end => {
            let end = end + CODEX_BLOCK_END.len();
            let suffix = existing
                .get(end..)
                .context("invalid managed block boundary")?
                .trim_start_matches('\n');
            Ok(format!("{}{}{}", &existing[..start], block, suffix))
        }
        (None, None) => {
            let parsed = existing.parse::<toml::Value>();
            if !existing.trim().is_empty() && parsed.is_err() && !force {
                bail!("existing Codex config is invalid TOML; fix it or pass --force");
            }
            if parsed
                .ok()
                .and_then(|v| v.get("mcp_servers")?.get("slop").cloned())
                .is_some()
            {
                bail!("existing [mcp_servers.slop] is not slop-managed; rename or remove it first");
            }
            let separator = if existing.is_empty() || existing.ends_with("\n\n") {
                ""
            } else if existing.ends_with('\n') {
                "\n"
            } else {
                "\n\n"
            };
            Ok(format!("{existing}{separator}{block}"))
        }
        _ => {
            bail!("Codex config contains an incomplete slop-managed block; repair it before setup")
        }
    }
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

fn read_json(path: &Path, force: bool) -> Result<Value> {
    let Some(bytes) = read_optional(path)? else {
        return Ok(Value::Null);
    };
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(Value::Null);
    }
    match serde_json::from_slice(&bytes) {
        Ok(value) => Ok(value),
        Err(error) if force => {
            eprintln!(
                "warning: {} is invalid JSON ({error}); replacing it",
                path.display()
            );
            Ok(Value::Null)
        }
        Err(error) => bail!(
            "{} is invalid JSON ({error}); fix it or pass --force",
            path.display()
        ),
    }
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

fn launch_editor(editor: Editor, repo: &Path) -> Result<()> {
    let Some(exe) = editor.executable() else {
        return Ok(());
    };
    Command::new(exe)
        .arg(repo)
        .spawn()
        .with_context(|| format!("launching {exe} (is it on PATH?)"))?;
    Ok(())
}

fn report_command(label: &str, command: &str) -> bool {
    let found = command_exists(command);
    println!("{} {label}: {command}", if found { "ok  " } else { "fail" });
    found
}

fn command_exists(command: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|path| path.join(command).is_file())
}

fn json_bytes(value: &Value) -> Result<Vec<u8>> {
    Ok(format!("{}\n", serde_json::to_string_pretty(value)?).into_bytes())
}
fn hash(content: &[u8]) -> String {
    blake3::hash(content).to_hex().to_string()
}
fn relative(repo: &Path, path: &Path) -> String {
    path.strip_prefix(repo)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}
fn toml_string(value: &str) -> String {
    toml::Value::String(value.to_string()).to_string()
}

fn ensure_object(value: &mut Value) -> &mut serde_json::Map<String, Value> {
    if !value.is_object() {
        *value = json!({});
    }
    value
        .as_object_mut()
        .expect("value was normalized to an object")
}

fn merge_mcp(mut existing: Value, exe: &str, repo: &str) -> Value {
    let servers = ensure_object(&mut existing)
        .entry("mcpServers")
        .or_insert_with(|| json!({}));
    ensure_object(servers).insert(
        "slop".into(),
        json!({ "type": "stdio", "command": exe, "args": ["mcp", repo] }),
    );
    existing
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

fn shell_word(value: &str) -> String {
    if value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"/_-.".contains(&byte))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
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

fn command_group(matcher: Option<&str>, command: String) -> Value {
    let hook = json!({ "type": "command", "command": command });
    matcher.map_or_else(
        || json!({ "hooks": [hook] }),
        |matcher| json!({ "matcher": matcher, "hooks": [hook] }),
    )
}

fn merge_hooks(mut existing: Value, exe: &str) -> Value {
    let hooks_value = ensure_object(&mut existing)
        .entry("hooks")
        .or_insert_with(|| json!({}));
    let hooks = ensure_object(hooks_value);
    for (event, matcher, tail) in [
        ("PreToolUse", Some(PRE_TOOL_MATCHER), PRE_TOOL_CMD),
        ("PostToolUse", Some(POST_TOOL_MATCHER), POST_TOOL_CMD),
        ("UserPromptSubmit", None, PROMPT_CMD),
        ("SessionStart", None, SESSION_CMD),
        ("SubagentStart", None, SUBAGENT_CMD),
    ] {
        let mut groups = strip_slop_groups(hooks.get(event));
        groups.push(command_group(
            matcher,
            format!("{} {tail}", shell_word(exe)),
        ));
        hooks.insert(event.into(), Value::Array(groups));
    }
    existing
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
    fn managed_codex_block_preserves_unowned_text() {
        let existing = "# mine\nmodel = \"x\"\n";
        let once = merge_managed_block(
            existing,
            "# >>> slop managed: mcp\nx = 1\n# <<< slop managed: mcp\n",
            false,
        )
        .unwrap();
        let twice = merge_managed_block(
            &once,
            "# >>> slop managed: mcp\nx = 2\n# <<< slop managed: mcp\n",
            false,
        )
        .unwrap();
        assert!(twice.starts_with(existing));
        assert!(twice.contains("x = 2"));
        assert!(!twice.contains("x = 1"));
    }

    #[test]
    fn exact_hook_matching_preserves_similar_user_command() {
        let existing = json!({ "hooks": { "PostToolUse": [{ "hooks": [
            { "type": "command", "command": "my-slop hook post-tool-use --audit" },
            { "type": "command", "command": "/old/slop hook post-tool-use" }
        ] }] } });
        let out = merge_hooks(existing, "/new/slop");
        let commands: Vec<_> = out["hooks"]["PostToolUse"][0]["hooks"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v["command"].as_str())
            .collect();
        assert_eq!(commands, vec!["my-slop hook post-tool-use --audit"]);
    }

    #[test]
    fn hook_commands_quote_binary_paths_with_spaces() {
        let out = merge_hooks(Value::Null, "/tmp/slop tools/slop");
        assert_eq!(
            out["hooks"]["PostToolUse"][0]["hooks"][0]["command"],
            "'/tmp/slop tools/slop' hook post-tool-use"
        );
        assert_eq!(out, merge_hooks(out.clone(), "/tmp/slop tools/slop"));
    }

    #[test]
    fn claude_merge_is_idempotent() {
        let once = merge_hooks(Value::Null, "/bin/slop");
        assert_eq!(once, merge_hooks(once.clone(), "/bin/slop"));
    }

    #[test]
    fn unmanaged_codex_server_is_refused() {
        let error =
            merge_managed_block("[mcp_servers.slop]\ncommand = \"mine\"\n", "", false).unwrap_err();
        assert!(error.to_string().contains("not slop-managed"));
    }
}
