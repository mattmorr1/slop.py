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
/// V8 heap ceiling for the Node indexers, in MB.
const NODE_HEAP_MB: u32 = 8192;
/// Above this many Python sources scip-python runs per directory shard: a single
/// Pyright pass over Sentry (8k files) exhausted an 8 GB heap; one shard took 1.6 GB.
const SHARD_ABOVE: usize = 3000;
const SHARD_MAX: usize = 1500;
const MAX_LOOSE_TARGETS: usize = 64;

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
            Language::JavaScript | Language::TypeScript | Language::Tsx => Some(Self::Typescript),
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

/// Directories (repo-relative, sorted) holding a `tsconfig.json`, outside vendor trees.
fn tsconfig_projects(repo: &Path) -> Vec<String> {
    fn walk(repo: &Path, dir: &Path, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            let path = entry.path();
            if kind.is_dir() && !entry.file_name().to_str().is_some_and(|name| SKIP_DIRS.contains(&name)) {
                walk(repo, &path, out);
            } else if kind.is_file() && entry.file_name() == "tsconfig.json" {
                let relative = dir.strip_prefix(repo).unwrap_or(dir).to_string_lossy().replace('\\', "/");
                out.push(if relative.is_empty() { ".".into() } else { relative });
            }
        }
    }
    let mut out = Vec::new();
    walk(repo, repo, &mut out);
    out.sort();
    out
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
    let python: Vec<&str> = files.iter().map(String::as_str).filter(|f| indexer == Indexer::Python && indexer.covers(f)).collect();
    let run = if python.len() > SHARD_ABOVE {
        invoke_python_shards(repo, &staged, project, &python)
    } else {
        invoke(indexer, repo, &staged, project)
    };
    let outcome = run
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

/// Directory targets of at most [`SHARD_MAX`] files where splitting helps; the few
/// files directly inside a split directory become single-file targets. A directory
/// with many loose files stays one shard: a run per file would cost more than it saves.
fn python_shards(files: &[&str]) -> Vec<String> {
    fn plan(prefix: &str, files: Vec<&str>, out: &mut Vec<String>) {
        let whole = || if prefix.is_empty() { ".".to_string() } else { prefix.to_string() };
        let mut children: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        let mut loose = Vec::new();
        for &file in &files {
            let rest = if prefix.is_empty() { file } else { &file[prefix.len() + 1..] };
            match rest.split_once('/') {
                Some((child, _)) => children.entry(child).or_default().push(file),
                None => loose.push(file),
            }
        }
        if files.len() <= SHARD_MAX || loose.len() > MAX_LOOSE_TARGETS || children.is_empty() {
            out.push(whole());
            return;
        }
        out.extend(loose.into_iter().map(str::to_string));
        for (child, files) in children {
            plan(&if prefix.is_empty() { child.to_string() } else { format!("{prefix}/{child}") }, files, out);
        }
    }
    let mut out = Vec::new();
    plan("", files.to_vec(), &mut out);
    out
}

/// scip-python once per shard, a few at a time, merged into one repo-relative index.
fn invoke_python_shards(repo: &Path, out: &Path, project: &str, files: &[&str]) -> Result<()> {
    let targets = python_shards(files);
    let dir = sibling(out, ".shards");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let next = std::sync::atomic::AtomicUsize::new(0);
    // A shard peaks near 1.6 GB (Sentry); four fit a 16 GB machine.
    let workers = std::thread::available_parallelism().map_or(1, usize::from).div_ceil(2).clamp(1, 4);
    eprintln!("scip-python: {} files in {} shards, {workers} at a time", files.len(), targets.len());
    let failures: Vec<String> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                scope.spawn(|| {
                    let mut failed = Vec::new();
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(target) = targets.get(i) else { return failed };
                        let heap = format!("--max-old-space-size={NODE_HEAP_MB}");
                        let status = Process::new("npx")
                            .arg("--yes")
                            .args([SCIP_PYTHON_PACKAGE, "index"])
                            .arg(repo)
                            .args(["--project-name", project, "--project-version", "0", "--quiet", "--target-only", target, "--output"])
                            .arg(dir.join(format!("{i}.scip")))
                            .current_dir(repo)
                            .env("NODE_OPTIONS", std::env::var("NODE_OPTIONS").ok().filter(|o| o.contains("--max-old-space-size")).unwrap_or(heap))
                            .stdout(std::io::stderr())
                            .status();
                        match status {
                            Ok(status) if status.success() => {}
                            Ok(status) => failed.push(format!("{target}: exited with {status}")),
                            Err(error) => failed.push(format!("{target}: {error}")),
                        }
                    }
                })
            })
            .collect();
        handles.into_iter().flat_map(|handle| handle.join().expect("shard worker panicked")).collect()
    });
    if !failures.is_empty() {
        let _ = std::fs::remove_dir_all(&dir);
        bail!("{} of {} scip-python shards failed: {}", failures.len(), targets.len(), failures.join("; "));
    }
    let parts: Vec<(PathBuf, String)> = targets.iter().enumerate().map(|(i, t)| (dir.join(format!("{i}.scip")), t.clone())).collect();
    let root = format!("file://{}", repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf()).display());
    let merged = slop_resolve::merge_shards(&parts, &root, out);
    std::fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
    merged.map(|documents| eprintln!("scip-python: merged {documents} documents"))
}

fn invoke(indexer: Indexer, repo: &Path, out: &Path, project: &str) -> Result<()> {
    let mut inferred_config = None;
    let npx = || {
        let mut cmd = Process::new("npx");
        cmd.arg("--yes");
        // Pyright on a large repo (8k+ files) exhausts V8's default heap and aborts;
        // the limit is a ceiling, not a reservation. A size the user set wins.
        let options = std::env::var("NODE_OPTIONS").unwrap_or_default();
        if !options.contains("--max-old-space-size") {
            cmd.env("NODE_OPTIONS", format!("{options} --max-old-space-size={NODE_HEAP_MB}").trim());
        }
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
            // A monorepo keeps its tsconfigs in subprojects, passed positionally;
            // with none anywhere, scip-typescript infers one (removed after).
            let mut cmd = npx();
            cmd.args([SCIP_TYPESCRIPT_PACKAGE, "index", "--output"])
                .arg(out)
                .current_dir(repo);
            if !repo.join("tsconfig.json").exists() {
                let projects = tsconfig_projects(repo);
                if projects.is_empty() {
                    cmd.arg("--infer-tsconfig");
                    inferred_config = Some(repo.join("tsconfig.json"));
                } else {
                    cmd.args(projects);
                }
            }
            ("scip-typescript", "is npx on PATH?", cmd)
        }
        Indexer::Python | Indexer::Auto => {
            // scip-python resolves the project from its *cwd*, not from the path
            // argument, so without this it indexes wherever slop was invoked from.
            let mut cmd = npx();
            cmd.current_dir(repo);
            // An explicit version: scip-python derives one from git and crashes without
            // it (tarballs, copies); a constant also keeps symbols stable across commits.
            cmd.args([SCIP_PYTHON_PACKAGE, "index"])
                .arg(repo)
                .args(["--project-name", project, "--project-version", "0", "--output"])
                .arg(out);
            ("scip-python", "is npx on PATH?", cmd)
        }
    };
    // Indexer chatter goes to stderr: stdout belongs to slop's own (often JSON) output.
    let status = cmd
        .stdout(std::io::stderr())
        .status()
        .with_context(|| format!("running {tool} ({install_hint})"));
    if let Some(config) = inferred_config {
        std::fs::remove_file(&config)
            .with_context(|| format!("removing the tsconfig {tool} inferred at {}", config.display()))?;
    }
    let status = status?;
    if !status.success() {
        let hint = if indexer == Indexer::Rust { "" } else { " (a Node indexer aborting is usually heap exhaustion: raise NODE_OPTIONS=--max-old-space-size)" };
        bail!("{tool} exited with {status}{hint}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn python_shards_split_only_what_is_too_big() {
        let mut files: Vec<String> = (0..1600).map(|i| format!("src/big/m{i}.py")).collect();
        files.extend((0..10).map(|i| format!("src/small/s{i}.py")));
        files.push("src/loose.py".into());
        files.push("setup.py".into());
        let refs: Vec<&str> = files.iter().map(String::as_str).collect();
        let mut shards = super::python_shards(&refs);
        shards.sort();
        // The root and `src` split; `src/big` is over the limit but flat, so it stays one
        // shard rather than 1,600 single-file runs.
        assert_eq!(shards, ["setup.py", "src/big", "src/loose.py", "src/small"]);
        let small: Vec<&str> = vec!["a/x.py", "b/y.py"];
        assert_eq!(super::python_shards(&small), vec!["."], "a repo under the limit is one shard");
    }

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
    fn tsconfig_projects_are_found_outside_vendor_trees() {
        let dir = temp_repo("tsconfigs");
        for project in ["clients/web", "services/agent", "node_modules/lib"] {
            std::fs::create_dir_all(dir.join(project)).unwrap();
            std::fs::write(dir.join(project).join("tsconfig.json"), "{}").unwrap();
        }
        assert_eq!(tsconfig_projects(&dir), ["clients/web", "services/agent"]);
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
