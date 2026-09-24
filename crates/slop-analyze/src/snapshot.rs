//! Immutable, revision-coherent repository evidence shared by every adapter.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use slop_resolve::{IndexSet, Resolver};

use crate::baseline::Baseline;
use crate::build::{self, BuiltGraph};
use crate::effects;
use crate::index::{self, Digest, Stamp};
use crate::policy::Policy;
use crate::relevance::{self, Lexical, RelevanceModel};
use crate::retrieve::Neighborhood;
use crate::source::{self, FileFacts};

pub const SNAPSHOT_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SnapshotId(String);

impl std::fmt::Display for SnapshotId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::str::FromStr for SnapshotId {
    type Err = &'static str;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            Ok(Self(value.to_ascii_lowercase()))
        } else {
            Err("snapshot id must be a 64-character hexadecimal digest")
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SnapshotFreshness {
    Current,
    Stale { reason: String },
}

/// How much SCIP evidence stands behind one source document.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentState {
    /// Indexed, and the index was built from exactly this content.
    Resolved,
    /// Indexed, but from different content: its graph edges may be wrong.
    Stale,
    /// Edited since indexing, line ranges re-derived by the parser (`overlay`):
    /// fit for context, but edges are the last index's, so never for a deny.
    Reparsed,
    /// No index document: parser facts only.
    Unresolved,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Freshness {
    Reindex,
    Warn,
}

pub struct CaptureRequest<'a> {
    pub repo: &'a Path,
    pub index: Option<&'a Path>,
    pub policy: Option<&'a Path>,
    pub freshness: Freshness,
}

pub struct RepositorySnapshot {
    id: SnapshotId,
    repo: PathBuf,
    freshness: SnapshotFreshness,
    /// Only documents that are not [`DocumentState::Resolved`].
    degraded: BTreeMap<String, DocumentState>,
    digests: BTreeMap<String, Digest>,
    pub built: BuiltGraph,
    pub facts: Vec<FileFacts>,
    pub sources: BTreeMap<String, Arc<str>>,
    indexed_files: BTreeSet<String>,
    pub resolver: IndexSet,
    pub policy: Policy,
    pub baseline: Baseline,
    pub relevance: RelevanceModel,
    neighborhood: OnceLock<Neighborhood>,
    lexical: OnceLock<Lexical>,
    entity_index: OnceLock<crate::envelope::EntityIndex>,
    /// Entities of reparsed documents whose function no longer exists.
    gone: std::collections::HashSet<petgraph::graph::NodeIndex>,
}

impl RepositorySnapshot {
    pub fn capture(request: CaptureRequest<'_>) -> Result<Arc<Self>> {
        capture(request)
    }

    pub fn id(&self) -> &SnapshotId {
        &self.id
    }

    pub fn repo(&self) -> &Path {
        &self.repo
    }

    pub fn freshness(&self) -> &SnapshotFreshness {
        &self.freshness
    }

    pub fn document_state(&self, file: &str) -> DocumentState {
        match self.degraded.get(file) {
            Some(state) => *state,
            None if self.sources.contains_key(file) => DocumentState::Resolved,
            None => DocumentState::Unresolved,
        }
    }

    /// Whether any document changed since its index was built (stale or reparsed).
    pub fn has_unindexed_edits(&self) -> bool {
        self.degraded.values().any(|state| matches!(state, DocumentState::Stale | DocumentState::Reparsed))
    }

    /// Whether `entity` was deleted by an edit the index has not seen yet.
    pub fn is_gone(&self, entity: &str) -> bool {
        self.built.graph.node(entity).is_some_and(|idx| self.gone.contains(&idx))
    }

    pub fn indexed_files(&self) -> &BTreeSet<String> {
        &self.indexed_files
    }

    pub fn neighborhood(&self) -> &Neighborhood {
        self.neighborhood
            .get_or_init(|| Neighborhood::build(&self.built))
    }

    pub fn lexical(&self) -> &Lexical {
        self.lexical.get_or_init(|| Lexical::build(&self.built, &self.sources))
    }

    pub fn entity_index(&self) -> &crate::envelope::EntityIndex {
        self.entity_index.get_or_init(|| crate::envelope::EntityIndex::build(&self.built, &self.facts))
    }
}

static CACHE: Mutex<Option<(Digest, Arc<RepositorySnapshot>)>> = Mutex::new(None);

/// A source file's identity short of its bytes. ctime is load-bearing: userspace
/// cannot set it, so a backdated same-length edit still changes the tuple.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Stat {
    len: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
    ino: u64,
}

#[cfg(unix)]
fn stat(path: &Path) -> Option<Stat> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(path).ok()?;
    Some(Stat {
        len: meta.len(),
        mtime: (meta.mtime(), meta.mtime_nsec()),
        ctime: (meta.ctime(), meta.ctime_nsec()),
        ino: meta.ino(),
    })
}

#[cfg(not(unix))]
fn stat(_: &Path) -> Option<Stat> {
    None
}

/// git's racily-clean rule: a file changed within this many seconds of being hashed
/// is rehashed next time, since a coarse ctime clock cannot order a same-tick write.
const RACY_WINDOW_SECS: i64 = 2;

type Sources = BTreeMap<String, Arc<str>>;

/// (stat before reading, digest, second the digest was taken), per absolute path.
static DIGESTS: Mutex<BTreeMap<PathBuf, (Stat, Digest, i64)>> = Mutex::new(BTreeMap::new());

fn read_source(path: &Path, relative: &str) -> Result<Arc<str>> {
    let source =
        std::fs::read_to_string(path).with_context(|| format!("reading source {relative}"))?;
    Ok(Arc::from(source))
}

/// Digest every source, reading only files whose stat tuple changed or was racy.
/// Returns what it had to read, so a cache miss need not read those files again.
fn digest_sources(
    repo: &Path,
    files: &BTreeSet<String>,
) -> Result<(BTreeMap<String, Digest>, Sources)> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64);
    let (mut digests, mut read) = (BTreeMap::new(), BTreeMap::new());
    for file in files {
        let path = repo.join(file);
        let before = stat(&path);
        let known = DIGESTS
            .lock()
            .ok()
            .and_then(|known| known.get(&path).copied());
        let trusted = known.filter(|(seen, _, hashed_at)| {
            before == Some(*seen) && seen.ctime.0 < hashed_at - RACY_WINDOW_SECS
        });
        if let Some((_, digest, _)) = trusted {
            digests.insert(file.clone(), digest);
            continue;
        }
        let source = read_source(&path, file)?;
        let digest = *blake3::hash(source.as_bytes()).as_bytes();
        if let (Some(before), Ok(mut known)) = (before, DIGESTS.lock()) {
            known.insert(path, (before, digest, now));
        }
        digests.insert(file.clone(), digest);
        read.insert(file.clone(), source);
    }
    Ok((digests, read))
}

/// Sources for a new snapshot: bytes already read, else the previous snapshot's
/// shared text when its digest still matches, else a fresh read (re-digested).
fn collect_sources(
    repo: &Path,
    digests: &mut BTreeMap<String, Digest>,
    mut read: BTreeMap<String, Arc<str>>,
) -> Result<BTreeMap<String, Arc<str>>> {
    let previous = CACHE
        .lock()
        .ok()
        .and_then(|cache| cache.as_ref().map(|(_, snapshot)| snapshot.clone()))
        .filter(|snapshot| snapshot.repo == repo);
    digests
        .iter_mut()
        .map(|(file, digest)| {
            let source = match read.remove(file) {
                Some(source) => source,
                None => match previous
                    .as_ref()
                    .filter(|snapshot| snapshot.digests.get(file) == Some(digest))
                    .and_then(|snapshot| snapshot.sources.get(file))
                {
                    Some(shared) => shared.clone(),
                    None => {
                        let source = read_source(&repo.join(file), file)?;
                        *digest = *blake3::hash(source.as_bytes()).as_bytes();
                        source
                    }
                },
            };
            Ok((file.clone(), source))
        })
        .collect()
}

/// Index artifacts are keyed by (length, mtime): only indexers write them, through
/// an atomic rename. Sources are keyed by content because anything may edit them.
fn artifact_stat(path: &Path) -> Option<(u64, std::time::SystemTime)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.len(), meta.modified().ok()?))
}

fn cache_key(
    repo: &Path,
    index_paths: &[PathBuf],
    policy_path: &Path,
    digests: &BTreeMap<String, Digest>,
) -> Digest {
    let mut hasher = blake3::Hasher::new();
    hasher.update(repo.as_os_str().as_encoded_bytes());
    for (file, digest) in digests {
        hasher.update(file.as_bytes());
        hasher.update(&[0]);
        hasher.update(digest);
    }
    for path in index_paths {
        hasher.update(path.as_os_str().as_encoded_bytes());
        hasher.update(format!("{:?}", artifact_stat(path)).as_bytes());
        for side in [index::stamp_path(path), index::failure_path(path)] {
            hasher.update(&std::fs::read(side).unwrap_or_default());
            hasher.update(&[0]);
        }
    }
    for path in [
        policy_path.to_path_buf(),
        repo.join(crate::baseline::BASELINE_FILE),
        repo.join(relevance::MODEL_FILE),
    ] {
        hasher.update(&std::fs::read(path).unwrap_or_default());
        hasher.update(&[0]);
    }
    *hasher.finalize().as_bytes()
}

fn snapshot_id(
    repo: &Path,
    index_paths: &[PathBuf],
    policy_path: &Path,
    sources: &BTreeMap<String, Arc<str>>,
) -> Result<SnapshotId> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"slop-repository-snapshot\0");
    hasher.update(&SNAPSHOT_SCHEMA_VERSION.to_le_bytes());
    for index_path in index_paths {
        // Repo-relative: the same content checked out elsewhere is the same snapshot.
        let relative = index_path.strip_prefix(repo).unwrap_or(index_path);
        hasher.update(relative.to_string_lossy().as_bytes());
        hasher.update(&[0]);
        hasher.update(&index::hash_source(index_path)?);
    }
    for (relative, source) in sources {
        hasher.update(relative.as_bytes());
        hasher.update(&[0]);
        hasher.update(source.as_bytes());
    }
    let baseline_path = repo.join(crate::baseline::BASELINE_FILE);
    let model_path = repo.join(relevance::MODEL_FILE);
    for path in [policy_path, baseline_path.as_path(), model_path.as_path()] {
        hasher.update(
            path.file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .as_bytes(),
        );
        hasher.update(&[0]);
        if path.exists() {
            hasher.update(&index::hash_source(path)?);
        }
    }
    Ok(SnapshotId(hasher.finalize().to_hex().to_string()))
}

/// Per-document states plus the reasons, if any, the snapshot is not current.
fn coverage(
    digests: &BTreeMap<String, Digest>,
    indexed: &BTreeSet<String>,
    stamps: &[Stamp],
    failures: &[Stamp],
) -> (BTreeMap<String, DocumentState>, Vec<String>) {
    let vouched = |stamps: &[Stamp], file: &String, digest: &Digest| {
        stamps
            .iter()
            .any(|stamp| stamp.files.get(file) == Some(digest))
    };
    let mut degraded = BTreeMap::new();
    let (mut changed, mut unindexed) = (Vec::new(), Vec::new());
    for (file, digest) in digests {
        let fresh = vouched(stamps, file, digest);
        match (indexed.contains(file), fresh) {
            (true, true) => {}
            (true, false) => {
                degraded.insert(file.clone(), DocumentState::Stale);
                changed.push(file.as_str());
            }
            (false, fresh) => {
                degraded.insert(file.clone(), DocumentState::Unresolved);
                if !fresh && !vouched(failures, file, digest) {
                    unindexed.push(file.as_str());
                }
            }
        }
    }
    let removed = indexed
        .iter()
        .filter(|file| !digests.contains_key(*file))
        .count();
    let summary = |count: usize, what: &str, first: Option<&&str>| {
        format!(
            "{count} {what}{}",
            first
                .map(|file| format!(" (first: {file})"))
                .unwrap_or_default()
        )
    };
    let mut reasons = Vec::new();
    if !changed.is_empty() {
        reasons.push(summary(
            changed.len(),
            "document(s) not vouched for by an index content stamp",
            changed.first(),
        ));
    }
    if !unindexed.is_empty() {
        reasons.push(summary(
            unindexed.len(),
            "document(s) not yet indexed",
            unindexed.first(),
        ));
    }
    if removed > 0 {
        reasons.push(format!("{removed} indexed document(s) no longer exist"));
    }
    reasons.extend(failures.iter().filter_map(|failure| {
        let active = failure
            .files
            .iter()
            .all(|(file, digest)| digests.get(file) == Some(digest));
        failure.error.as_ref().filter(|_| active).cloned()
    }));
    (degraded, reasons)
}

fn stamps_of(index_paths: &[PathBuf], side: fn(&Path) -> PathBuf) -> Vec<Stamp> {
    index_paths
        .iter()
        .filter_map(|path| Stamp::read(&side(path)))
        .collect()
}

fn capture(request: CaptureRequest<'_>) -> Result<Arc<RepositorySnapshot>> {
    let repo = request
        .repo
        .canonicalize()
        .with_context(|| format!("canonicalizing repository root {}", request.repo.display()))?;
    let policy_path = request
        .policy
        .map(Path::to_path_buf)
        .unwrap_or_else(|| repo.join("slop.toml"));
    let files = index::discover_sources(&repo);
    let (mut digests, read) = digest_sources(&repo, &files)?;

    let mut index_paths = index::index_paths_for(&repo, request.index, &files);
    let mut reindex_errors = Vec::new();
    if request.freshness == Freshness::Reindex {
        let stale = index::stale_indexers(
            &digests,
            &stamps_of(&index_paths, index::stamp_path),
            &stamps_of(&index_paths, index::failure_path),
        );
        if !stale.is_empty() {
            let labels: Vec<_> = stale.iter().map(|indexer| indexer.label()).collect();
            eprintln!(
                "slop: reindexing {} (content changed since indexing)",
                labels.join(", ")
            );
            reindex_errors = index::reindex(&repo, request.index, &files, &stale);
            reindex_errors
                .iter()
                .for_each(|error| eprintln!("warning: {error}"));
            index_paths = index::index_paths_for(&repo, request.index, &files);
        }
    }
    index_paths.retain(|path| path.exists());
    if index_paths.is_empty() {
        let cause = if reindex_errors.is_empty() {
            String::new()
        } else {
            format!(" ({})", reindex_errors.join("; "))
        };
        bail!(
            "no usable SCIP index{cause} — generate one with:\n  slop index {}",
            repo.display(),
        );
    }

    let key = cache_key(&repo, &index_paths, &policy_path, &digests);
    if let Some(snapshot) = CACHE.lock().ok().and_then(|cache| {
        cache
            .as_ref()
            .filter(|(cached, _)| cached == &key)
            .map(|(_, value)| value.clone())
    }) {
        return Ok(snapshot);
    }

    let sources = collect_sources(&repo, &mut digests, read)?;
    let key = cache_key(&repo, &index_paths, &policy_path, &digests);
    let artifacts: Vec<_> = index_paths.iter().map(|path| artifact_stat(path)).collect();
    let stamps = stamps_of(&index_paths, index::stamp_path);
    let failures = stamps_of(&index_paths, index::failure_path);
    let resolver = IndexSet::load(&index_paths)?;
    if resolver.definition_count() == 0 {
        bail!(
            "SCIP index set has no definitions — an indexer failed. Regenerate it:\n  slop index {}",
            repo.display(),
        );
    }
    let indexed_files: BTreeSet<String> =
        resolver.files().into_iter().map(str::to_string).collect();
    let (degraded, reasons) = coverage(&digests, &indexed_files, &stamps, &failures);
    let freshness = if reasons.is_empty() {
        SnapshotFreshness::Current
    } else {
        let reason = reasons.join("; ");
        if request.freshness == Freshness::Warn {
            eprintln!("warning: SCIP evidence is partial — {reason}");
        }
        SnapshotFreshness::Stale { reason }
    };
    let mut built = build::build_graph(&resolver);
    effects::infer_effects(&mut built);
    let facts = source::parse_corpus(&sources);
    let mut degraded = degraded;
    let gone = crate::overlay::remap(&mut built, &facts, &mut degraded);
    let policy = Policy::load_file(&policy_path)?;
    let baseline = Baseline::load(&repo)?;
    let relevance = RelevanceModel::load(&repo)?;
    let id = snapshot_id(&repo, &index_paths, &policy_path, &sources)?;
    if index_paths
        .iter()
        .map(|path| artifact_stat(path))
        .ne(artifacts)
    {
        bail!("an index was rewritten while capturing analysis; retry the command");
    }

    let snapshot = Arc::new(RepositorySnapshot {
        id,
        repo,
        freshness,
        degraded,
        digests,
        built,
        facts,
        sources,
        indexed_files,
        resolver,
        policy,
        baseline,
        relevance,
        neighborhood: OnceLock::new(),
        lexical: OnceLock::new(),
        entity_index: OnceLock::new(),
        gone,
    });
    if let Ok(mut cache) = CACHE.lock() {
        *cache = Some((key, snapshot.clone()));
    }
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempFixture;

    fn fixture(name: &str) -> TempFixture {
        TempFixture::new(name)
    }

    fn warn(repo: &Path) -> CaptureRequest<'_> {
        CaptureRequest {
            repo,
            index: None,
            policy: None,
            freshness: Freshness::Warn,
        }
    }

    #[test]
    fn repeated_capture_has_stable_identity() {
        let repo = fixture("toy_repo");
        let first = RepositorySnapshot::capture(warn(&repo)).expect("first capture");
        let second = RepositorySnapshot::capture(warn(&repo)).expect("second capture");
        // Identity, not pointer equality: the one-slot cache is shared with parallel tests.
        assert_eq!(first.id(), second.id());
        assert_eq!(first.freshness(), &SnapshotFreshness::Current);
    }

    /// Same length, backdated mtime, different bytes: the old metadata key
    /// served the obsolete snapshot. Content identity must not.
    #[test]
    fn a_backdated_same_length_edit_is_a_new_snapshot() {
        let repo = fixture("toy_repo");
        let first = RepositorySnapshot::capture(warn(&repo)).expect("first capture");
        let (file, source) = first
            .sources
            .iter()
            .find(|(_, source)| source.contains("self"))
            .map(|(file, source)| (file.clone(), source.to_string()))
            .expect("a source using self");
        let path = repo.join(&file);
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::fs::write(&path, source.replacen("self", "selg", 1)).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        let second = RepositorySnapshot::capture(warn(&repo)).expect("second capture");
        assert_ne!(first.id(), second.id());
        assert_eq!(second.document_state(&file), DocumentState::Reparsed, "edited since indexing, ranges from the parser");
        assert!(matches!(
            second.freshness(),
            SnapshotFreshness::Stale { .. }
        ));
        let other = second
            .sources
            .keys()
            .find(|other| **other != file)
            .expect("second file");
        assert_eq!(second.document_state(other), DocumentState::Resolved);
    }

    #[test]
    fn coverage_separates_stale_unindexed_and_excluded() {
        let digests = BTreeMap::from([
            ("a.py".to_string(), [1; 32]),
            ("b.py".to_string(), [2; 32]),
            ("c.py".to_string(), [3; 32]),
            ("d.py".to_string(), [4; 32]),
        ]);
        let indexed = BTreeSet::from(["a.py".to_string(), "b.py".to_string()]);
        let stamp = Stamp {
            files: BTreeMap::from([
                ("a.py".to_string(), [1; 32]),
                ("b.py".to_string(), [9; 32]),
                ("c.py".to_string(), [3; 32]),
            ]),
            error: None,
        };
        let (degraded, reasons) = coverage(&digests, &indexed, &[stamp], &[]);
        assert_eq!(degraded.get("a.py"), None);
        assert_eq!(degraded.get("b.py"), Some(&DocumentState::Stale));
        assert_eq!(degraded.get("c.py"), Some(&DocumentState::Unresolved));
        assert_eq!(degraded.get("d.py"), Some(&DocumentState::Unresolved));
        assert_eq!(
            reasons.len(),
            2,
            "c.py was excluded by its indexer, not missed: {reasons:?}"
        );
    }
}

#[cfg(test)]
mod bench {
    use super::*;

    /// `SLOP_BENCH_REPO=<repo> cargo test --release -p slop-analyze warm_capture -- --ignored --nocapture`
    #[test]
    #[ignore = "manual benchmark"]
    fn warm_capture() {
        let repo = PathBuf::from(std::env::var("SLOP_BENCH_REPO").expect("SLOP_BENCH_REPO"));
        let request = || CaptureRequest {
            repo: &repo,
            index: None,
            policy: None,
            freshness: Freshness::Warn,
        };
        let start = std::time::Instant::now();
        let first = RepositorySnapshot::capture(request()).expect("cold capture");
        eprintln!(
            "cold capture {:?} ({} sources)",
            start.elapsed(),
            first.sources.len()
        );
        let mut samples: Vec<_> = (0..50)
            .map(|_| {
                let start = std::time::Instant::now();
                let warm = RepositorySnapshot::capture(request()).expect("warm capture");
                assert!(Arc::ptr_eq(&first, &warm));
                start.elapsed()
            })
            .collect();
        samples.sort();
        eprintln!(
            "warm capture p50={:?} p95={:?} max={:?}",
            samples[24], samples[47], samples[49]
        );
    }
}

#[cfg(test)]
mod bench_parts {
    use super::*;

    #[test]
    #[ignore = "manual benchmark"]
    fn capture_parts() {
        let repo = PathBuf::from(std::env::var("SLOP_BENCH_REPO").expect("SLOP_BENCH_REPO"));
        let time = |label: &str, f: &mut dyn FnMut()| {
            let mut samples: Vec<_> = (0..30)
                .map(|_| {
                    let s = std::time::Instant::now();
                    f();
                    s.elapsed()
                })
                .collect();
            samples.sort();
            eprintln!("{label}: p50={:?}", samples[15]);
        };
        let files = index::discover_sources(&repo);
        time("discover", &mut || {
            index::discover_sources(&repo);
        });
        time("digest (warm)", &mut || {
            digest_sources(&repo, &files).unwrap();
        });
        let paths: Vec<_> = index::index_paths_for(&repo, None, &files)
            .into_iter()
            .filter(|path| path.exists())
            .collect();
        let once = |label: &str, f: &mut dyn FnMut()| {
            let start = std::time::Instant::now();
            f();
            eprintln!("{label}: {:?}", start.elapsed());
        };
        let mut resolver = None;
        once("scip load", &mut || resolver = Some(IndexSet::load(&paths).unwrap()));
        let resolver = resolver.unwrap();
        let mut built = None;
        once("build_graph", &mut || built = Some(build::build_graph(&resolver)));
        let mut built = built.unwrap();
        once("effects", &mut || effects::infer_effects(&mut built));
        DIGESTS.lock().unwrap().clear();
        let (_, read) = digest_sources(&repo, &files).unwrap();
        once("parse_corpus", &mut || {
            source::parse_corpus(&read);
        });
        let mut by_language: BTreeMap<String, (usize, usize, std::time::Duration)> = BTreeMap::new();
        for (file, text) in &read {
            let Some(language) = slop_parse::Language::from_path(file) else { continue };
            let start = std::time::Instant::now();
            let _ = language.parse(text);
            let entry = by_language.entry(format!("{language:?}")).or_default();
            *entry = (entry.0 + 1, entry.1 + text.len(), entry.2 + start.elapsed());
        }
        eprintln!("per language (files, bytes, time): {by_language:?}");
        once("snapshot_id", &mut || {
            snapshot_id(&repo, &paths, &repo.join("slop.toml"), &read).unwrap();
        });
    }
}

#[cfg(test)]
mod location {
    use super::*;
    use crate::test_support::TempFixture;

    /// ADR 0002 says content-identified: two checkouts of the same bytes agree.
    #[test]
    fn identity_does_not_depend_on_where_the_repo_lives() {
        let (a, b) = (TempFixture::new("toy_repo"), TempFixture::new("toy_repo"));
        let capture = |repo: &Path| {
            RepositorySnapshot::capture(CaptureRequest { repo, index: None, policy: None, freshness: Freshness::Warn })
                .expect("capture")
                .id()
                .clone()
        };
        assert_ne!(&*a, &*b);
        assert_eq!(capture(&a), capture(&b));
    }
}
