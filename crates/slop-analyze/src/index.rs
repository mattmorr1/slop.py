//! Producing and freshening the SCIP index: which indexer a repo needs, how to
//! run it, and whether the index on disk still describes the source.
//!
//! This lives beside the analysis rather than in the CLI because staleness is
//! a *correctness* property, not a UI one — judging a diff against yesterday's
//! graph is the failure mode, and every entry point (check, gate, MCP, LSP,
//! the hooks) funnels through the repository snapshot, which enforces it.
//!
//! Freshness is decided per document by content, never by mtime: each index
//! artifact carries a stamp recording the blake3 digest of every source file its
//! indexer was asked to cover. A `git checkout`, `cp -p` or an editor that
//! preserves timestamps cannot make a stale document look current.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command as Process;

use anyhow::{bail, Context, Result};
use slop_parse::Language;

pub const SCIP_PYTHON_PACKAGE: &str = "@sourcegraph/scip-python@0.6.6";
pub const SCIP_TYPESCRIPT_PACKAGE: &str = "@sourcegraph/scip-typescript@0.4.0";

/// The SCIP indexer for a language. slop's graph/effect detectors consume any
/// SCIP index; picking the right indexer per repo is all the multi-language
/// index path needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
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
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Python => "python",
            Self::Typescript => "typescript",
            Self::Rust => "rust",
        }
    }

    pub fn for_language(language: Language) -> Option<Self> {
        match language {
            Language::Python => Some(Self::Python),
            Language::JavaScript | Language::TypeScript => Some(Self::Typescript),
            Language::Rust => Some(Self::Rust),
            _ => None,
        }
    }

    fn covers(self, path: &str) -> bool {
        Language::from_path(path).and_then(Self::for_language) == Some(self)
    }

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
        if has("pyproject.toml")
            || has("setup.py")
            || has("requirements.txt")
            || has_top_level_ext(repo, "py")
        {
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

/// Every supported source file under `repo`, repo-relative with `/` separators,
/// skipping symlinks and generated/vendor trees. The one walk that language
/// detection, freshness and coverage all derive from.
// ponytail: full walk per capture (~16 ms on vigil); a per-directory stat cache (git untracked cache) removes it.
pub fn discover_sources(repo: &Path) -> BTreeSet<String> {
    let mut files = BTreeSet::new();
    walk_sources(repo, repo, &mut files);
    files
}

fn walk_sources(repo: &Path, dir: &Path, files: &mut BTreeSet<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_symlink() {
            continue;
        }
        let path = entry.path();
        if kind.is_dir() {
            if !entry
                .file_name()
                .to_str()
                .is_some_and(|name| SKIP_DIRS.contains(&name))
            {
                walk_sources(repo, &path, files);
            }
            continue;
        }
        let Some(relative) = path.strip_prefix(repo).ok().and_then(Path::to_str) else {
            continue;
        };
        let normalized = relative.replace('\\', "/");
        if Language::from_path(&normalized).is_some() && !ignored_source_path(&normalized) {
            files.insert(normalized);
        }
    }
}

/// Every language family with source in this repository, in stable order.
/// Marker files are insufficient for monorepos, so detection uses discovered
/// source files rather than project markers.
pub fn detected_indexers(repo: &Path) -> Vec<Indexer> {
    indexers_for(repo, &discover_sources(repo))
}

fn indexers_for(repo: &Path, files: &BTreeSet<String>) -> Vec<Indexer> {
    let found: BTreeSet<Indexer> = files
        .iter()
        .filter_map(|file| Language::from_path(file).and_then(Indexer::for_language))
        .collect();
    if found.is_empty() {
        vec![Indexer::Auto.resolve(repo)]
    } else {
        found.into_iter().collect()
    }
}

fn shard_path(repo: &Path, indexer: Indexer) -> PathBuf {
    repo.join(".slop/index")
        .join(format!("{}.scip", indexer.label()))
}

/// Resolve the artifact set for a repository. An explicit path keeps the
/// single-index compatibility seam. Auto mode stores one artifact per language
/// only when the repository is actually polyglot.
pub fn index_paths(repo: &Path, explicit: Option<&Path>) -> Vec<PathBuf> {
    index_paths_for(repo, explicit, &discover_sources(repo))
}

/// As [`index_paths`], plus a legacy root `index.scip` while any polyglot shard
/// is missing: its documents still count as (stamped or stale) coverage.
pub fn index_paths_for(
    repo: &Path,
    explicit: Option<&Path>,
    files: &BTreeSet<String>,
) -> Vec<PathBuf> {
    if let Some(path) = explicit {
        return vec![path.to_path_buf()];
    }
    let indexers = indexers_for(repo, files);
    let root = repo.join("index.scip");
    if indexers.len() == 1 {
        return vec![root];
    }
    let mut paths: Vec<PathBuf> = indexers
        .into_iter()
        .map(|indexer| shard_path(repo, indexer))
        .collect();
    if root.exists() && !paths.iter().all(|path| path.exists()) {
        paths.push(root);
    }
    paths
}

/// Is there a top-level file with extension `ext` in `repo`? A cheap language
/// signal for repos without config-file markers.
fn has_top_level_ext(repo: &Path, ext: &str) -> bool {
    std::fs::read_dir(repo)
        .map(|entries| {
            entries
                .flatten()
                .any(|e| e.path().extension().and_then(|s| s.to_str()) == Some(ext))
        })
        .unwrap_or(false)
}

/// Directories never worth walking for source files.
pub(crate) const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    ".venv",
    "venv",
    "target",
    "__pycache__",
    "build",
    "dist",
    "htmlcov",
    "coverage",
    ".next",
    ".nuxt",
    ".svelte-kit",
];

pub(crate) fn ignored_source_path(path: &str) -> bool {
    path.split(['/', '\\'])
        .any(|component| SKIP_DIRS.contains(&component))
        || path.ends_with(".min.js")
}

pub type Digest = [u8; 32];

pub fn hash_source(path: &Path) -> Result<Digest> {
    let mut file = File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("reading {}", path.display()))?;
        if read == 0 {
            return Ok(*hasher.finalize().as_bytes());
        }
        hasher.update(&buffer[..read]);
    }
}

/// What one indexer run was asked to cover: every source file's digest, taken
/// *before* the indexer started, so an edit racing the run reads stale, never fresh.
/// `error` marks a failed run, remembered so the same content is not retried.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Stamp {
    pub files: BTreeMap<String, Digest>,
    pub error: Option<String>,
}

const STAMP_HEADER: &str = "slop-stamp 2";

fn sibling(index_path: &Path, suffix: &str) -> PathBuf {
    let mut path = index_path.as_os_str().to_os_string();
    path.push(suffix);
    PathBuf::from(path)
}

/// The stamp of a successfully built artifact.
pub fn stamp_path(index_path: &Path) -> PathBuf {
    sibling(index_path, ".stamp")
}

/// The stamp of the last failed attempt to build this artifact.
pub fn failure_path(index_path: &Path) -> PathBuf {
    sibling(index_path, ".failed")
}

impl Stamp {
    fn render(&self) -> String {
        let mut out = format!("{STAMP_HEADER}\n");
        if let Some(error) = &self.error {
            out.push_str("error ");
            out.push_str(&error.replace('\n', " "));
            out.push('\n');
        }
        for (file, digest) in &self.files {
            out.push_str(&blake3::Hash::from_bytes(*digest).to_hex());
            out.push(' ');
            out.push_str(file);
            out.push('\n');
        }
        out
    }

    /// `None` for a missing, legacy (pre-content) or corrupt stamp: every
    /// document it would have vouched for is then treated as stale.
    fn parse(text: &str) -> Option<Self> {
        let mut lines = text.lines();
        if lines.next()? != STAMP_HEADER {
            return None;
        }
        let mut stamp = Stamp::default();
        for line in lines {
            if let Some(error) = line.strip_prefix("error ") {
                stamp.error = Some(error.to_string());
                continue;
            }
            let (hex, file) = line.split_at_checked(64)?;
            let digest = blake3::Hash::from_hex(hex).ok()?;
            stamp
                .files
                .insert(file.strip_prefix(' ')?.to_string(), *digest.as_bytes());
        }
        Some(stamp)
    }

    pub fn read(path: &Path) -> Option<Self> {
        Self::parse(&std::fs::read_to_string(path).ok()?)
    }

    fn write(&self, path: &Path) -> Result<()> {
        let staged = sibling(path, ".partial");
        std::fs::write(&staged, self.render())
            .with_context(|| format!("writing {}", staged.display()))?;
        std::fs::rename(&staged, path).with_context(|| format!("installing {}", path.display()))
    }
}

fn stamp_for(repo: &Path, files: &BTreeSet<String>, indexer: Indexer) -> Result<Stamp> {
    let files = files
        .iter()
        .filter(|file| indexer == Indexer::Auto || indexer.covers(file))
        .map(|file| Ok((file.clone(), hash_source(&repo.join(file))?)))
        .collect::<Result<_>>()?;
    Ok(Stamp { files, error: None })
}

/// Declare `index_path` current for every source file under `repo` as it is now.
/// For fixtures and for callers that produced the index by other means.
pub fn write_stamp(repo: &Path, index_path: &Path) -> Result<()> {
    stamp_for(repo, &discover_sources(repo), Indexer::Auto)?.write(&stamp_path(index_path))
}

/// Languages whose indexer should run: some file's content is vouched for by no
/// successful stamp, unless the last failure was at exactly this content.
pub fn stale_indexers(
    digests: &BTreeMap<String, Digest>,
    stamps: &[Stamp],
    failures: &[Stamp],
) -> BTreeSet<Indexer> {
    let vouched = |stamps: &[Stamp], file: &String, digest: &Digest| {
        stamps
            .iter()
            .any(|stamp| stamp.files.get(file) == Some(digest))
    };
    digests
        .iter()
        .filter(|(file, digest)| !vouched(stamps, file, digest) && !vouched(failures, file, digest))
        .filter_map(|(file, _)| Language::from_path(file).and_then(Indexer::for_language))
        .collect()
}

/// Regenerate the SCIP index for `repo`, writing to `index` (default
/// `<repo>/index.scip`, or one shard per language in a polyglot repo).
/// Every shard is attempted; failures are reported together afterwards.
pub fn run_scip_index(repo: &Path, index: Option<&Path>) -> Result<()> {
    let files = discover_sources(repo);
    let targets = match index {
        Some(out) => vec![(Indexer::Auto, out.to_path_buf())],
        None => {
            let indexers = indexers_for(repo, &files);
            if indexers.len() == 1 {
                vec![(indexers[0], repo.join("index.scip"))]
            } else {
                indexers
                    .into_iter()
                    .map(|indexer| (indexer, shard_path(repo, indexer)))
                    .collect()
            }
        }
    };
    let project = default_project_name(repo);
    let failures: Vec<String> = targets
        .into_iter()
        .filter_map(|(indexer, out)| {
            run_indexer_over(indexer, repo, &out, &project, &files)
                .err()
                .map(|error| format!("{}: {error:#}", indexer.label()))
        })
        .collect();
    if failures.is_empty() {
        Ok(())
    } else {
        bail!("indexing failed — {}", failures.join("; "))
    }
}

/// Run only the given indexers, returning each failure instead of stopping:
/// one language's broken toolchain must not take the others' evidence down.
pub fn reindex(
    repo: &Path,
    explicit: Option<&Path>,
    files: &BTreeSet<String>,
    indexers: &BTreeSet<Indexer>,
) -> Vec<String> {
    let project = default_project_name(repo);
    if let Some(out) = explicit {
        return run_indexer_over(Indexer::Auto, repo, out, &project, files)
            .err()
            .map(|error| vec![format!("indexing {} failed: {error:#}", out.display())])
            .unwrap_or_default();
    }
    let polyglot = indexers_for(repo, files).len() > 1;
    indexers
        .iter()
        .filter_map(|&indexer| {
            let out = if polyglot {
                shard_path(repo, indexer)
            } else {
                repo.join("index.scip")
            };
            run_indexer_over(indexer, repo, &out, &project, files)
                .err()
                .map(|error| format!("{} indexing failed: {error:#}", indexer.label()))
        })
        .collect()
}

/// Build the index if it isn't there yet, so no command dead-ends on a missing
/// one. Returns whether it built. Only ever builds when the index is *absent* —
/// a stale index still warns rather than silently costing a rebuild.
pub fn ensure_index(repo: &Path, index_path: &Path) -> Result<bool> {
    if index_path.exists() {
        return Ok(false);
    }
    eprintln!("no {} — indexing first...", index_path.display());
    run_indexer(Indexer::Auto, repo, index_path, &default_project_name(repo))?;
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
    run_indexer_over(indexer, repo, out, project, &discover_sources(repo))
}

/// A zero-document index for a non-empty language is a failed run that exited 0.
fn validate(index: &Path, expected_files: usize) -> Result<()> {
    let documents = slop_resolve::ScipResolver::load(index)?.document_count();
    if documents == 0 && expected_files > 0 {
        bail!("indexer produced no documents for {expected_files} source file(s)");
    }
    Ok(())
}

/// The output is staged beside `out` and installed only after validation, so a
/// failed run never replaces a working index; the failure is stamped instead.
fn run_indexer_over(
    indexer: Indexer,
    repo: &Path,
    out: &Path,
    project: &str,
    files: &BTreeSet<String>,
) -> Result<()> {
    let indexer = indexer.resolve(repo);
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut stamp = stamp_for(repo, files, indexer)?;
    let staged = sibling(out, ".partial");
    let outcome = invoke(indexer, repo, &staged, project)
        .and_then(|()| validate(&staged, stamp.files.len()))
        .and_then(|()| {
            std::fs::rename(&staged, out).with_context(|| format!("installing {}", out.display()))
        });
    match outcome {
        Ok(()) => {
            let _ = std::fs::remove_file(failure_path(out));
            stamp.write(&stamp_path(out))
        }
        Err(error) => {
            let _ = std::fs::remove_file(&staged);
            stamp.error = Some(format!("{error:#}"));
            stamp.write(&failure_path(out))?;
            Err(error)
        }
    }
}

fn invoke(indexer: Indexer, repo: &Path, out: &Path, project: &str) -> Result<()> {
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
            (
                "rust-analyzer scip",
                "install it with: rustup component add rust-analyzer",
                cmd,
            )
        }
        Indexer::Typescript => {
            // scip-typescript reads the project's tsconfig from its cwd and
            // takes just an output path.
            let mut cmd = npx();
            cmd.args([SCIP_TYPESCRIPT_PACKAGE, "index", "--output"])
                .arg(out)
                .current_dir(repo);
            ("scip-typescript", "is npx on PATH?", cmd)
        }
        Indexer::Python | Indexer::Auto => {
            // scip-python resolves the project from its *cwd*, not from the path
            // argument, so without this it indexes wherever slop was invoked from.
            let mut cmd = npx();
            cmd.current_dir(repo);
            cmd.args([SCIP_PYTHON_PACKAGE, "index"])
                .arg(repo)
                .args(["--project-name", project, "--output"])
                .arg(out);
            ("scip-python", "is npx on PATH?", cmd)
        }
    };
    // Indexer chatter goes to stderr: stdout belongs to slop's own (often JSON) output.
    let status = cmd
        .stdout(std::io::stderr())
        .status()
        .with_context(|| format!("running {tool} ({install_hint})"))?;
    if !status.success() {
        bail!("{tool} exited with {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_repo(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("slop-index-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn generated_paths_do_not_enter_source_discovery() {
        assert!(ignored_source_path("clients/web/build/assets/app.js"));
        assert!(ignored_source_path("services/agent/dist/index.js"));
        assert!(ignored_source_path("src/vendor.min.js"));
        assert!(!ignored_source_path("src/distribution.rs"));
    }

    #[test]
    fn stamp_round_trips_including_failures_and_spaces() {
        let stamp = Stamp {
            files: BTreeMap::from([
                ("a b.py".to_string(), [7; 32]),
                ("c.rs".to_string(), [9; 32]),
            ]),
            error: Some("scip-python exited\nwith 1".to_string()),
        };
        let parsed = Stamp::parse(&stamp.render()).expect("parses");
        assert_eq!(parsed.files, stamp.files);
        assert_eq!(parsed.error.as_deref(), Some("scip-python exited with 1"));
        assert_eq!(
            Stamp::parse("1787340726193038082\n"),
            None,
            "legacy mtime stamp"
        );
    }

    /// The failure mode mtime freshness had: same length, older timestamp,
    /// different content. Content stamps must call it stale.
    #[test]
    fn a_backdated_same_length_edit_is_stale() {
        let dir = temp_repo("backdated");
        std::fs::write(dir.join("a.py"), "x = 1\n").unwrap();
        let stamp = stamp_for(&dir, &discover_sources(&dir), Indexer::Auto).unwrap();
        let digests = |dir: &Path| -> BTreeMap<String, Digest> {
            discover_sources(dir)
                .into_iter()
                .map(|file| (file.clone(), hash_source(&dir.join(file)).unwrap()))
                .collect()
        };
        assert!(stale_indexers(&digests(&dir), std::slice::from_ref(&stamp), &[]).is_empty());
        let old = std::fs::metadata(dir.join("a.py"))
            .unwrap()
            .modified()
            .unwrap();
        std::fs::write(dir.join("a.py"), "x = 2\n").unwrap();
        File::options()
            .write(true)
            .open(dir.join("a.py"))
            .unwrap()
            .set_modified(old - std::time::Duration::from_secs(3600))
            .unwrap();
        assert_eq!(
            stale_indexers(&digests(&dir), std::slice::from_ref(&stamp), &[]),
            BTreeSet::from([Indexer::Python])
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A failure stamped at this exact content is not retried; any edit retries.
    #[test]
    fn a_failure_memo_suppresses_retry_only_at_the_same_content() {
        let digests = BTreeMap::from([("web/app.ts".to_string(), [1; 32])]);
        let failed = Stamp {
            files: digests.clone(),
            error: Some("boom".into()),
        };
        assert!(stale_indexers(&digests, &[], std::slice::from_ref(&failed)).is_empty());
        let edited = BTreeMap::from([("web/app.ts".to_string(), [2; 32])]);
        assert_eq!(
            stale_indexers(&edited, &[], &[failed]),
            BTreeSet::from([Indexer::Typescript])
        );
    }

    #[test]
    fn an_empty_index_is_a_failed_run() {
        let dir = temp_repo("empty-index");
        let index = dir.join("index.scip");
        std::fs::write(&index, b"").unwrap();
        assert!(validate(&index, 3).is_err());
        assert!(validate(&index, 0).is_ok());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn detects_every_language_in_a_monorepo() {
        let dir = temp_repo("polyglot");
        std::fs::create_dir_all(dir.join("web")).unwrap();
        std::fs::create_dir_all(dir.join("native")).unwrap();
        std::fs::write(dir.join("tool.py"), "pass\n").unwrap();
        std::fs::write(dir.join("web/app.ts"), "export const x = 1;\n").unwrap();
        std::fs::write(dir.join("native/lib.rs"), "pub fn x() {}\n").unwrap();
        assert_eq!(
            detected_indexers(&dir),
            vec![Indexer::Python, Indexer::Typescript, Indexer::Rust]
        );
        let paths = index_paths(&dir, None);
        assert!(paths.iter().any(|path| path.ends_with("python.scip")));
        assert!(paths.iter().any(|path| path.ends_with("typescript.scip")));
        assert!(paths.iter().any(|path| path.ends_with("rust.scip")));
        assert!(!paths.iter().any(|path| path.ends_with("index.scip")));
        std::fs::write(dir.join("index.scip"), b"").unwrap();
        assert!(
            index_paths(&dir, None)
                .iter()
                .any(|path| path.ends_with("index.scip")),
            "a legacy root index still counts while shards are missing"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
