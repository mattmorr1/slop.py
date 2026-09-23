//! Immutable, revision-coherent repository evidence shared by every adapter.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use slop_resolve::{IndexSet, Resolver};

use crate::baseline::Baseline;
use crate::build::{self, BuiltGraph};
use crate::policy::Policy;
use crate::retrieve::Neighborhood;
use crate::source::{self, FileFacts};
use crate::{effects, index};

pub const SNAPSHOT_SCHEMA_VERSION: u32 = 1;

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
    pub built: BuiltGraph,
    pub facts: Vec<FileFacts>,
    pub sources: BTreeMap<String, Arc<str>>,
    indexed_files: BTreeSet<String>,
    pub resolver: IndexSet,
    pub policy: Policy,
    pub baseline: Baseline,
    neighborhood: OnceLock<Neighborhood>,
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

    pub fn indexed_files(&self) -> &BTreeSet<String> {
        &self.indexed_files
    }

    pub fn neighborhood(&self) -> &Neighborhood {
        self.neighborhood
            .get_or_init(|| Neighborhood::build(&self.built))
    }
}

#[derive(Clone, PartialEq, Eq)]
struct FileStamp {
    modified: Option<std::time::SystemTime>,
    len: u64,
}

#[derive(Clone, PartialEq, Eq)]
struct CacheKey {
    repo: PathBuf,
    indexes: Vec<(PathBuf, FileStamp)>,
    policy: PathBuf,
    policy_stamp: Option<FileStamp>,
    baseline_stamp: Option<FileStamp>,
    source_count: usize,
    source_len: u64,
    newest_source: Option<std::time::SystemTime>,
}

static CACHE: Mutex<Option<(CacheKey, Arc<RepositorySnapshot>)>> = Mutex::new(None);

fn stamp(path: &Path) -> Option<FileStamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some(FileStamp {
        modified: meta.modified().ok(),
        len: meta.len(),
    })
}

fn source_state(
    dir: &Path,
    count: &mut usize,
    len: &mut u64,
    newest: &mut Option<std::time::SystemTime>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if index::SKIP_DIRS.contains(&name) {
                continue;
            }
            source_state(&path, count, len, newest);
            continue;
        }
        let Some(ext) = path.extension().and_then(|s| s.to_str()) else {
            continue;
        };
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(index::ignored_source_path)
        {
            continue;
        }
        if !slop_parse::SOURCE_EXTS.contains(&ext) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        *count += 1;
        *len = len.saturating_add(meta.len());
        if let Ok(modified) = meta.modified() {
            *newest = Some(newest.map_or(modified, |current| current.max(modified)));
        }
    }
}

fn cache_key(repo: &Path, index_paths: &[PathBuf], policy_path: &Path) -> Option<CacheKey> {
    let mut source_count = 0;
    let mut source_len = 0;
    let mut newest_source = None;
    source_state(repo, &mut source_count, &mut source_len, &mut newest_source);
    Some(CacheKey {
        repo: repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf()),
        indexes: index_paths
            .iter()
            .map(|path| {
                Some((
                    path.canonicalize().unwrap_or_else(|_| path.clone()),
                    stamp(path)?,
                ))
            })
            .collect::<Option<Vec<_>>>()?,
        policy: policy_path
            .canonicalize()
            .unwrap_or_else(|_| policy_path.to_path_buf()),
        policy_stamp: stamp(policy_path),
        baseline_stamp: stamp(&repo.join(crate::baseline::BASELINE_FILE)),
        source_count,
        source_len,
        newest_source,
    })
}

fn hash_file(hasher: &mut blake3::Hasher, path: &Path) -> Result<()> {
    let mut file = File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("reading {}", path.display()))?;
        if read == 0 {
            return Ok(());
        }
        hasher.update(&buffer[..read]);
    }
}

fn discover_sources(repo: &Path, dir: &Path, files: &mut BTreeSet<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            let name = entry.file_name();
            if name.to_str().is_some_and(|part| index::SKIP_DIRS.contains(&part)) {
                continue;
            }
            discover_sources(repo, &path, files);
            continue;
        }
        let Some(relative) = path.strip_prefix(repo).ok().and_then(Path::to_str) else {
            continue;
        };
        let normalized = relative.replace('\\', "/");
        if slop_parse::Language::from_path(&normalized).is_some()
            && !index::ignored_source_path(&normalized)
        {
            files.insert(normalized);
        }
    }
}

fn capture_sources(repo: &Path, indexed: &BTreeSet<String>) -> Result<BTreeMap<String, Arc<str>>> {
    let mut files = indexed.clone();
    discover_sources(repo, repo, &mut files);
    let mut sources = BTreeMap::new();
    for relative in files {
        if slop_parse::Language::from_path(&relative).is_none()
            || index::ignored_source_path(&relative)
        {
            continue;
        }
        let source = std::fs::read_to_string(repo.join(&relative))
            .with_context(|| format!("reading indexed source {relative}"))?;
        sources.insert(relative, Arc::<str>::from(source));
    }
    Ok(sources)
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
        hasher.update(index_path.to_string_lossy().as_bytes());
        hasher.update(&[0]);
        hash_file(&mut hasher, index_path)?;
    }
    for (relative, source) in sources {
        hasher.update(relative.as_bytes());
        hasher.update(&[0]);
        hasher.update(source.as_bytes());
    }
    let baseline_path = repo.join(crate::baseline::BASELINE_FILE);
    for path in [policy_path, baseline_path.as_path()] {
        hasher.update(
            path.file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .as_bytes(),
        );
        hasher.update(&[0]);
        if path.exists() {
            hash_file(&mut hasher, path)?;
        }
    }
    Ok(SnapshotId(hasher.finalize().to_hex().to_string()))
}

fn capture(request: CaptureRequest<'_>) -> Result<Arc<RepositorySnapshot>> {
    let repo = request
        .repo
        .canonicalize()
        .with_context(|| format!("canonicalizing repository root {}", request.repo.display()))?;
    let mut index_paths = index::index_paths(&repo, request.index);
    let policy_path = request
        .policy
        .map(Path::to_path_buf)
        .unwrap_or_else(|| repo.join("slop.toml"));

    if request.freshness == Freshness::Reindex {
        index_paths = index::ensure_indexes(&repo, request.index)?;
        let stale = index_paths
            .iter()
            .filter_map(|path| index::index_staleness(&repo, path))
            .collect::<Vec<_>>();
        if !stale.is_empty() {
            eprintln!("slop: {} — reindexing detected languages", stale.join("; "));
            index::run_scip_index(&repo, request.index)?;
            index_paths = index::index_paths(&repo, request.index);
        }
    }
    let missing: Vec<_> = index_paths.iter().filter(|path| !path.exists()).collect();
    if !missing.is_empty() {
        bail!(
            "missing SCIP index artifact(s): {} — generate them with:\n  slop index {}",
            missing
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", "),
            repo.display(),
        );
    }

    let stale_reasons: Vec<_> = index_paths
        .iter()
        .filter_map(|path| index::index_staleness(&repo, path))
        .collect();
    let stale_reason = (!stale_reasons.is_empty()).then(|| stale_reasons.join("; "));
    if request.freshness == Freshness::Warn {
        if let Some(reason) = &stale_reason {
            eprintln!("warning: {reason}");
        }
    }
    let freshness = stale_reason
        .map(|reason| SnapshotFreshness::Stale { reason })
        .unwrap_or(SnapshotFreshness::Current);

    let before = cache_key(&repo, &index_paths, &policy_path)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "index metadata unavailable"))?;
    if let Some(snapshot) = CACHE.lock().ok().and_then(|cache| {
        cache
            .as_ref()
            .filter(|(key, _)| key == &before)
            .map(|(_, value)| value.clone())
    }) {
        return Ok(snapshot);
    }

    let resolver = IndexSet::load(&index_paths)?;
    if resolver.definition_count() == 0 {
        bail!(
            "SCIP index set has no definitions — an indexer failed. Regenerate it:\n  slop index {}",
            repo.display(),
        );
    }
    let mut built = build::build_graph(&resolver);
    effects::infer_effects(&mut built);
    let indexed_files: BTreeSet<String> =
        resolver.files().into_iter().map(str::to_string).collect();
    let sources = capture_sources(&repo, &indexed_files)?;
    let facts = source::parse_corpus(&sources);
    let policy = Policy::load_file(&policy_path)?;
    let baseline = Baseline::load(&repo)?;
    let id = snapshot_id(&repo, &index_paths, &policy_path, &sources)?;
    let after = cache_key(&repo, &index_paths, &policy_path)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "index metadata unavailable"))?;
    if before != after {
        bail!("repository changed while capturing analysis; retry the command");
    }

    let snapshot = Arc::new(RepositorySnapshot {
        id,
        repo,
        freshness,
        built,
        facts,
        sources,
        indexed_files,
        resolver,
        policy,
        baseline,
        neighborhood: OnceLock::new(),
    });
    if let Ok(mut cache) = CACHE.lock() {
        *cache = Some((after, snapshot.clone()));
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

    #[test]
    fn repeated_capture_has_stable_identity() {
        let repo = fixture("toy_repo");
        let request = || CaptureRequest {
            repo: &repo,
            index: None,
            policy: None,
            freshness: Freshness::Warn,
        };
        let first = RepositorySnapshot::capture(request()).expect("first capture");
        let second = RepositorySnapshot::capture(request()).expect("second capture");
        assert_eq!(first.id(), second.id());
    }
}
