//! Producing and freshening the SCIP index: which indexer a repo needs, how to
//! run it, and whether the index on disk still describes the source.
//!
//! This lives beside the analysis rather than in the CLI because staleness is
//! a *correctness* property, not a UI one — judging a diff against yesterday's
//! graph is the failure mode, and every entry point (check, gate, MCP, LSP,
//! the hooks) funnels through `check::load_analysis`, which enforces it.

use std::path::{Path, PathBuf};
use std::process::Command as Process;

use anyhow::{bail, Context, Result};

/// The SCIP indexer for a language. slop's graph/effect detectors consume any
/// SCIP index; picking the right indexer per repo is all the multi-language
/// index path needs.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Indexer {
    /// Detect from project markers (pyproject/setup/*.py vs Cargo.toml vs package.json/tsconfig).
    Auto,
    /// `scip-python` (Python).
    Python,
    /// `scip-typescript` (JavaScript / TypeScript).
    Typescript,
    /// `rust-analyzer scip` (Rust) — the one indexer that needs no npx.
    Rust,
}

impl Indexer {
    /// Resolve `Auto` against the repo's project markers.
    pub fn resolve(self, repo: &Path) -> Indexer {
        if self != Indexer::Auto {
            return self;
        }
        let has = |f: &str| repo.join(f).exists();
        // Python markers win when several ecosystems are present — slop's
        // parser-based rules cover Python most fully, so it's the richer target.
        // Cargo.toml is checked before the JS markers because a Rust repo often
        // carries a package.json for web assets, but not the reverse.
        if has("pyproject.toml") || has("setup.py") || has("requirements.txt") || has_top_level_ext(repo, "py") {
            Indexer::Python
        } else if has("Cargo.toml") {
            Indexer::Rust
        } else if has("tsconfig.json") || has("package.json") {
            Indexer::Typescript
        } else {
            Indexer::Python
        }
    }
}

/// Is there a top-level file with extension `ext` in `repo`? A cheap language
/// signal for repos without config-file markers.
fn has_top_level_ext(repo: &Path, ext: &str) -> bool {
    std::fs::read_dir(repo)
        .map(|entries| {
            entries.flatten().any(|e| {
                e.path().extension().and_then(|s| s.to_str()) == Some(ext)
            })
        })
        .unwrap_or(false)
}

/// Directories never worth walking for source-file mtimes.
const SKIP_DIRS: &[&str] = &[".git", "node_modules", ".venv", "venv", "target", "__pycache__"];

/// A human-readable reason the index at `index_path` is out of date, or `None`
/// if it looks current. "Stale" means a tracked source file has been modified
/// more recently than the index — the check would then judge yesterday's graph.
/// Best-effort: any I/O hiccup yields `None` (never block a check on a mtime we
/// couldn't read).
pub fn index_staleness(repo: &Path, index_path: &Path) -> Option<String> {
    let index_mtime = std::fs::metadata(index_path).ok()?.modified().ok()?;
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    newest_source_mtime(repo, &mut newest);
    let (src_mtime, src_path) = newest?;
    if src_mtime <= index_mtime || Some(src_mtime) == read_stamp(index_path) {
        return None;
    }
    let rel = src_path.strip_prefix(repo).unwrap_or(&src_path);
    Some(format!(
        "SCIP index {} is older than {} — findings may be out of date",
        index_path.display(),
        rel.display()
    ))
}

/// What an index was built against: the newest source mtime observed at the
/// time, as nanos since the epoch, in `<index>.stamp`.
///
/// Without it, an index that comes out *still* looking stale — a file touched
/// while the indexer ran, a filesystem whose mtimes outrun the write — makes
/// every subsequent invocation reindex again. That is 15s per command here and
/// minutes on a large workspace, so the loop matters more than the file.
fn stamp_path(index_path: &Path) -> PathBuf {
    let mut p = index_path.as_os_str().to_os_string();
    p.push(".stamp");
    PathBuf::from(p)
}

fn read_stamp(index_path: &Path) -> Option<std::time::SystemTime> {
    let nanos: u128 = std::fs::read_to_string(stamp_path(index_path)).ok()?.trim().parse().ok()?;
    Some(std::time::UNIX_EPOCH + std::time::Duration::from_nanos(nanos as u64))
}

/// Record the newest source mtime `repo` has right now as the index's baseline.
/// Best-effort: a stamp we can't write only costs a redundant reindex later.
pub fn write_stamp(repo: &Path, index_path: &Path) {
    let mut newest = None;
    newest_source_mtime(repo, &mut newest);
    let Some((mtime, _)) = newest else { return };
    if let Ok(d) = mtime.duration_since(std::time::UNIX_EPOCH) {
        let _ = std::fs::write(stamp_path(index_path), d.as_nanos().to_string());
    }
}

/// Recursively track the newest-modified source file under `dir`, skipping
/// vendored/build directories. Best-effort — unreadable entries are ignored.
fn newest_source_mtime(dir: &Path, newest: &mut Option<(std::time::SystemTime, PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            let name = entry.file_name();
            if SKIP_DIRS.iter().any(|d| name == *d) {
                continue;
            }
            newest_source_mtime(&path, newest);
        } else if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| slop_parse::SOURCE_EXTS.contains(&e))
        {
            if let Ok(mtime) = entry.metadata().and_then(|m| m.modified()) {
                if newest.as_ref().map(|(t, _)| mtime > *t).unwrap_or(true) {
                    *newest = Some((mtime, path));
                }
            }
        }
    }
}

/// Regenerate the SCIP index for `repo`, writing to `index` (default
/// `<repo>/index.scip`). Used by `slop gate --reindex`; auto-detects the
/// indexer.
pub fn run_scip_index(repo: &Path, index: Option<&Path>) -> Result<()> {
    let out = index
        .map(Path::to_path_buf)
        .unwrap_or_else(|| repo.join("index.scip"));
    run_indexer(Indexer::Auto, repo, &out, "slop-gate")
}

/// Build the index if it isn't there yet, so no command dead-ends on a missing
/// one. Returns whether it built. Every entry point used to fail with "generate
/// one with: slop index <repo>", which made the first command a new user runs an
/// error telling them to run a different command.
///
/// Only ever builds when the index is *absent* — a stale index still warns
/// rather than silently costing a rebuild on every invocation.
pub fn ensure_index(repo: &Path, index_path: &Path) -> Result<bool> {
    if index_path.exists() {
        return Ok(false);
    }
    eprintln!("no {} — indexing first...", index_path.display());
    run_indexer(
        Indexer::Auto,
        repo,
        index_path,
        &default_project_name(repo),
    )?;
    Ok(true)
}

/// scip-python's `--project-name` default: the repo directory name.
pub fn default_project_name(repo: &Path) -> String {
    repo.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".to_string())
}

/// Run the resolved SCIP indexer for `repo`, writing to `out`.
pub fn run_indexer(indexer: Indexer, repo: &Path, out: &Path, project: &str) -> Result<()> {
    let indexer = indexer.resolve(repo);
    let npx = || {
        let mut cmd = Process::new("npx");
        cmd.arg("--yes");
        cmd
    };
    let (tool, install_hint, mut cmd) = match indexer {
        Indexer::Rust => {
            // rust-analyzer emits SCIP natively, so Rust needs no Node toolchain.
            let mut cmd = Process::new("rust-analyzer");
            cmd.arg("scip").arg(repo).arg("--output").arg(out);
            ("rust-analyzer scip", "install it with: rustup component add rust-analyzer", cmd)
        }
        Indexer::Typescript => {
            // scip-typescript reads the project's tsconfig from its cwd and
            // takes just an output path.
            let mut cmd = npx();
            cmd.args(["@sourcegraph/scip-typescript", "index", "--output"])
                .arg(out)
                .current_dir(repo);
            ("scip-typescript", "is npx on PATH?", cmd)
        }
        Indexer::Python | Indexer::Auto => {
            // scip-python resolves the project from its *cwd*, not from the path
            // argument, so without this it indexes wherever slop was invoked
            // from — silently analyzing the wrong codebase for any
            // `slop check <other-repo>`.
            let mut cmd = npx();
            cmd.current_dir(repo);
            cmd.args(["@sourcegraph/scip-python", "index"])
                .arg(repo)
                .args(["--project-name", project, "--output"])
                .arg(out);
            ("scip-python", "is npx on PATH?", cmd)
        }
    };
    let status = cmd
        .status()
        .with_context(|| format!("running {tool} ({install_hint})"))?;
    if !status.success() {
        bail!("{tool} indexing failed for {}", repo.display());
    }
    // Record what this index was built against, so it isn't judged stale
    // straight away by a source file the indexer raced with.
    write_stamp(repo, out);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stamp that didn't round-trip would silently stop suppressing, and the
    /// symptom is an indexer subprocess on every invocation.
    #[test]
    fn stamp_round_trips_the_newest_source_mtime() {
        let dir = std::env::temp_dir().join(format!("slop-stamp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), "fn a() {}\n").unwrap();
        let index = dir.join("index.scip");
        std::fs::write(&index, b"x").unwrap();

        assert_eq!(read_stamp(&index), None);
        write_stamp(&dir, &index);
        let mut newest = None;
        newest_source_mtime(&dir, &mut newest);
        assert_eq!(read_stamp(&index), Some(newest.unwrap().0));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_stamped_index_is_not_reported_stale_again() {
        let dir = std::env::temp_dir().join(format!("slop-stale-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let index = dir.join("index.scip");
        std::fs::write(&index, b"x").unwrap();
        // Source written *after* the index: stale by mtime.
        std::fs::write(dir.join("a.rs"), "fn a() {}\n").unwrap();
        assert!(index_staleness(&dir, &index).is_some());
        write_stamp(&dir, &index);
        assert!(index_staleness(&dir, &index).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }
}
