//! Session baseline: the tree as a Claude Code session found it, so the Stop gate
//! judges what the session changed and never the user's pre-existing dirty files.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

pub const SESSION_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Session {
    pub schema: u32,
    /// HEAD at session start; `None` in a repository with no commits.
    pub head: Option<String>,
    /// Source files dirty or untracked at start: blake3 of their content, or "" if deleted.
    pub dirty: BTreeMap<String, String>,
    /// Finding ids the Stop gate has already blocked on once this session.
    #[serde(default)]
    pub blocked: BTreeSet<String>,
}

/// One source file the session changed: its content at session start and now.
#[derive(Debug)]
pub struct Change {
    pub file: String,
    pub before: Option<String>,
    /// Empty when the session deleted the file: every function it had is gone.
    pub after: String,
}

fn git(repo: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("git").arg("-C").arg(repo).args(args).output().context("running git")?;
    if !output.status.success() {
        bail!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(output.stdout)
}

fn is_source(file: &str) -> bool {
    !file.starts_with(".slop/") && file.rsplit_once('.').is_some_and(|(_, ext)| crate::parse::SOURCE_EXTS.contains(&ext))
}

fn listed(bytes: &[u8]) -> impl Iterator<Item = String> + '_ {
    bytes.split(|b| *b == 0).filter(|p| !p.is_empty()).map(|p| String::from_utf8_lossy(p).replace('\\', "/")).filter(|f| is_source(f))
}

/// Source files that differ from `head` (every tracked file without one), plus untracked ones.
fn candidates(repo: &Path, head: Option<&str>) -> Result<BTreeSet<String>> {
    let tracked = match head {
        Some(head) => git(repo, &["diff", "--name-only", "-z", "--no-renames", head, "--"])?,
        None => git(repo, &["ls-files", "-z"])?,
    };
    let untracked = git(repo, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    Ok(listed(&tracked).chain(listed(&untracked)).collect())
}

fn hash(content: &[u8]) -> String {
    blake3::hash(content).to_hex().to_string()
}

/// `.slop/session/<id>.json`; ids come from the host, so anything path-like is refused.
pub fn path(repo: &Path, id: &str) -> Result<PathBuf> {
    if id.is_empty() || id.len() > 128 || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        bail!("invalid session id {id:?}");
    }
    Ok(repo.join(".slop/session").join(format!("{id}.json")))
}

fn blob(repo: &Path, digest: &str) -> PathBuf {
    repo.join(".slop/session/blobs").join(digest)
}

/// Record the baseline for session `id` once; a resumed or compacted session keeps its first one.
pub fn start(repo: &Path, id: &str) -> Result<PathBuf> {
    let file = path(repo, id)?;
    if file.is_file() {
        return Ok(file);
    }
    let head_out = Command::new("git").arg("-C").arg(repo).args(["rev-parse", "--verify", "-q", "HEAD"]).output().context("running git")?;
    let head = head_out.status.success().then(|| String::from_utf8_lossy(&head_out.stdout).trim().to_string());
    let mut dirty = BTreeMap::new();
    for name in candidates(repo, head.as_deref())? {
        let digest = match std::fs::read(repo.join(&name)) {
            Ok(content) => {
                let digest = hash(&content);
                let stored = blob(repo, &digest);
                if !stored.is_file() {
                    std::fs::create_dir_all(stored.parent().context("blob path has no parent")?)?;
                    std::fs::write(&stored, &content).with_context(|| format!("writing {}", stored.display()))?;
                }
                digest
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(error).with_context(|| format!("reading {name}")),
        };
        dirty.insert(name, digest);
    }
    save(repo, id, &Session { schema: SESSION_SCHEMA_VERSION, head, dirty, blocked: BTreeSet::new() })?;
    Ok(file)
}

pub fn load(repo: &Path, id: &str) -> Result<Session> {
    let file = path(repo, id)?;
    let text = std::fs::read_to_string(&file).with_context(|| format!("reading session baseline {}", file.display()))?;
    let session: Session = serde_json::from_str(&text).with_context(|| format!("parsing {}", file.display()))?;
    if session.schema != SESSION_SCHEMA_VERSION {
        bail!("session schema {} is unsupported; expected {SESSION_SCHEMA_VERSION}", session.schema);
    }
    Ok(session)
}

pub fn save(repo: &Path, id: &str, session: &Session) -> Result<()> {
    let file = path(repo, id)?;
    std::fs::create_dir_all(file.parent().context("session path has no parent")?)?;
    let staged = file.with_extension(format!("json.{}", std::process::id()));
    std::fs::write(&staged, serde_json::to_vec(session)?).with_context(|| format!("writing {}", staged.display()))?;
    std::fs::rename(&staged, &file).with_context(|| format!("installing {}", file.display()))
}

/// Source files whose content differs from the session baseline: from `head` for files
/// clean at start, from the recorded content for files dirty at start.
pub fn changes(repo: &Path, session: &Session) -> Result<Vec<Change>> {
    let mut names = candidates(repo, session.head.as_deref())?;
    names.extend(session.dirty.keys().cloned());
    let mut out = Vec::new();
    for file in names {
        let (after, digest_now) = match std::fs::read_to_string(repo.join(&file)) {
            Ok(text) => {
                let digest = hash(text.as_bytes());
                (text, digest)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (String::new(), String::new()),
            Err(error) if error.kind() == std::io::ErrorKind::InvalidData => continue,
            Err(error) => return Err(error).with_context(|| format!("reading {file}")),
        };
        let before = match session.dirty.get(&file) {
            Some(digest) if *digest == digest_now => continue,
            Some(digest) if digest.is_empty() => None,
            Some(digest) => Some(std::fs::read_to_string(blob(repo, digest)).with_context(|| format!("reading the session-start copy of {file}"))?),
            None => session.head.as_deref().and_then(|head| git(repo, &["show", &format!("{head}:{file}")]).ok()).map(|b| String::from_utf8_lossy(&b).into_owned()),
        };
        if before.as_deref() != Some(after.as_str()) {
            out.push(Change { file, before, after });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(repo: &Path, args: &[&str]) {
        git(repo, args).expect("git");
    }

    #[test]
    fn changes_ignore_files_dirty_before_the_session() {
        let repo = std::env::temp_dir().join(format!("slop-session-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&repo);
        std::fs::create_dir_all(&repo).unwrap();
        run(&repo, &["init", "-q"]);
        std::fs::write(repo.join("a.py"), "a = 1\n").unwrap();
        std::fs::write(repo.join("b.py"), "b = 1\n").unwrap();
        run(&repo, &["add", "."]);
        run(&repo, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "init"]);
        std::fs::write(repo.join("a.py"), "a = 2\n").unwrap();
        std::fs::write(repo.join("notes.md"), "x").unwrap();
        start(&repo, "s1").unwrap();
        assert!(path(&repo, "../x").is_err());
        let session = load(&repo, "s1").unwrap();
        assert!(changes(&repo, &session).unwrap().is_empty(), "pre-existing edits are not the session's");
        std::fs::write(repo.join("a.py"), "a = 3\n").unwrap();
        std::fs::write(repo.join("b.py"), "b = 2\n").unwrap();
        std::fs::write(repo.join("c.py"), "c = 1\n").unwrap();
        let changed: Vec<(String, Option<String>)> = changes(&repo, &session).unwrap().into_iter().map(|c| (c.file, c.before)).collect();
        assert_eq!(changed, vec![
            ("a.py".to_string(), Some("a = 2\n".to_string())),
            ("b.py".to_string(), Some("b = 1\n".to_string())),
            ("c.py".to_string(), None),
        ]);
        let _ = std::fs::remove_dir_all(&repo);
    }
}
