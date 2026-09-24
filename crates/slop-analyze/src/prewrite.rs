//! Snapshot-bound assessment of proposed source before it reaches the worktree.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::policy::Policy;
use crate::precheck::{precheck, PrecheckFinding};
use crate::retrieve::{self, Match, Neighborhood, Site};
use crate::snapshot::{RepositorySnapshot, SnapshotFreshness, SnapshotId};

pub const PREWRITE_SCHEMA_VERSION: u32 = 2;
/// Suggestions shown with their body and a call site; the rest stay one line.
const EXPANDED_SUGGESTIONS: usize = 2;
const MAX_BODY_LINES: usize = 30;
pub const MIN_REUSE_SCORE_PPM: u32 = 125_000;
const MAX_SIDECAR_BYTES: u64 = 64 * 1024 * 1024;
static SIDECAR_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Serialize)]
pub struct WriteAssessment {
    pub schema_version: u32,
    pub snapshot: SnapshotId,
    pub freshness: SnapshotFreshness,
    pub file: String,
    pub policy_findings: Vec<PrecheckFinding>,
    /// New functions provably equivalent (E-sound) to one the repo already has.
    pub duplicates: Vec<Duplicate>,
    pub reuse_suggestions: Vec<Match>,
}

#[derive(Debug, Serialize)]
pub struct Duplicate {
    pub name: String,
    pub existing: Site,
}

/// E-sound hash to every function that has it, outside tests.
pub type EquivalenceIndex = BTreeMap<String, Vec<Site>>;

impl WriteAssessment {
    /// Steering lines; the top suggestions carry their body and one call site read
    /// from `repo`, because models reuse what they see used, not what they see named.
    pub fn steering(&self, repo: &Path) -> Vec<String> {
        let mut lines: Vec<String> = self
            .duplicates
            .iter()
            .map(|dup| {
                format!(
                    "slop: new `{}` is provably equivalent (same behaviour for every input) to existing `{}` at {}:{}. Call it instead of adding a copy.",
                    dup.name, dup.existing.label, dup.existing.file, dup.existing.start + 1
                )
            })
            .collect();
        lines.extend(self.policy_findings.iter().map(PrecheckFinding::steering));
        for (rank, suggestion) in self.reuse_suggestions.iter().enumerate() {
            let mut line = format!(
                "slop: proposed code overlaps existing `{}` at {}:{} through calls [{}]. Review that implementation before adding another; shared calls do not prove equivalent behavior.",
                suggestion.label,
                suggestion.file,
                suggestion.line + 1,
                suggestion.distinctive.join(", ")
            );
            if rank < EXPANDED_SUGGESTIONS {
                if let Some(body) = body_of(repo, &suggestion.file, suggestion.line, suggestion.end, &suggestion.label) {
                    line.push_str(&format!("\n```\n{body}\n```"));
                }
                if let Some((caller, at, call)) = suggestion.callers.iter().find_map(|c| call_site(repo, c, &suggestion.label).map(|(n, l)| (c, n, l))) {
                    line.push_str(&format!("\nUsed like this in `{}` ({}:{}): `{call}`", caller.label, caller.file, at + 1));
                }
            }
            lines.push(line);
        }
        lines
    }
}

fn simple(label: &str) -> &str {
    let last = label.rsplit("::").next().unwrap_or(label);
    last.rsplit(['.', ')']).find(|part| !part.is_empty()).unwrap_or(last)
}

/// The function's lines as they are on disk now, if its first lines still name it.
fn body_of(repo: &Path, file: &str, start: usize, end: usize, label: &str) -> Option<String> {
    let text = std::fs::read_to_string(repo.join(file)).ok()?;
    let lines: Vec<&str> = text.lines().skip(start).take(end.checked_sub(start)? + 1).collect();
    let name = simple(label);
    lines.iter().take(4).any(|l| l.contains(name)).then_some(())?;
    let shown = lines.len().min(MAX_BODY_LINES);
    let mut body = lines[..shown].join("\n");
    if shown < lines.len() {
        body.push_str(&format!("\n    # ... {} more lines", lines.len() - shown));
    }
    Some(body)
}

/// The first line (0-based) inside `caller` that calls `label`'s simple name.
fn call_site(repo: &Path, caller: &Site, label: &str) -> Option<(usize, String)> {
    let text = std::fs::read_to_string(repo.join(&caller.file)).ok()?;
    let needle = format!("{}(", simple(label));
    text.lines()
        .enumerate()
        .skip(caller.start)
        .take(caller.end.checked_sub(caller.start)? + 1)
        .find(|(_, l)| l.contains(&needle))
        .map(|(n, l)| (n, l.trim().to_string()))
}

#[derive(Deserialize)]
struct PrewriteSidecar {
    schema_version: u32,
    snapshot: SnapshotId,
    freshness: SnapshotFreshness,
    neighborhood: Neighborhood,
    equivalents: EquivalenceIndex,
}

#[derive(Serialize)]
struct PrewriteSidecarRef<'a> {
    schema_version: u32,
    snapshot: &'a SnapshotId,
    freshness: &'a SnapshotFreshness,
    neighborhood: &'a Neighborhood,
    equivalents: &'a EquivalenceIndex,
}

/// Every non-test function's E-sound hash; tiny bodies have none (the significance floor).
pub fn equivalence_index(snapshot: &RepositorySnapshot) -> EquivalenceIndex {
    let by_location = crate::source::location_index(&snapshot.built);
    let mut index: EquivalenceIndex = BTreeMap::new();
    for file_facts in snapshot.facts.iter().filter(|f| !crate::source::is_test_file(&f.file)) {
        for fact in file_facts.functions.iter().filter(|f| !f.equiv_hash.is_empty()) {
            let label = crate::source::entity_for(&snapshot.built, &by_location, &file_facts.file, fact)
                .map_or_else(|| fact.name.clone(), |entity| entity.id.clone());
            index.entry(fact.equiv_hash.clone()).or_default().push(Site {
                label,
                file: file_facts.file.clone(),
                start: fact.start_line as usize,
                end: fact.end_line as usize,
            });
        }
    }
    index
}

/// Functions in `source` that are new or changed relative to `current` (the file on
/// disk), keyed by (name, hash) so unchanged code never matches itself, and provably
/// equivalent to an existing one. A same-file match whose name is gone is a rename.
fn duplicates(language: slop_parse::Language, file: &str, source: &str, current: Option<&str>, index: &EquivalenceIndex) -> Vec<Duplicate> {
    let pairs = |text: &str| language.parse(text).map(|fns| fns.into_iter().map(|f| (f.name, f.equiv_hash)).collect::<Vec<_>>()).unwrap_or_default();
    let existing: BTreeSet<(String, String)> = current.map(pairs).unwrap_or_default().into_iter().collect();
    let proposed = pairs(source);
    let names: BTreeSet<&str> = proposed.iter().map(|(name, _)| name.as_str()).collect();
    proposed
        .iter()
        .filter(|pair| !pair.1.is_empty() && !existing.contains(*pair))
        .filter_map(|(name, hash)| {
            let site = index.get(hash)?.iter().find(|site| site.file != file || names.contains(simple(&site.label)))?;
            Some(Duplicate { name: name.clone(), existing: site.clone() })
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn assess(
    snapshot: SnapshotId,
    freshness: SnapshotFreshness,
    policy: &Policy,
    neighborhood: &Neighborhood,
    equivalents: &EquivalenceIndex,
    file: &str,
    source: &str,
    current: Option<&str>,
    limit: usize,
) -> Result<WriteAssessment> {
    let language = slop_parse::Language::from_path(file)
        .with_context(|| format!("unsupported source language for {file}"))?;
    let query = retrieve::query_from_source(language, source);
    let reuse_suggestions = neighborhood
        .matches(&query, None, limit)
        .into_iter()
        .filter(|candidate| candidate.score_ppm >= MIN_REUSE_SCORE_PPM)
        .collect();
    Ok(WriteAssessment {
        schema_version: PREWRITE_SCHEMA_VERSION,
        snapshot,
        freshness,
        file: file.to_string(),
        policy_findings: precheck(language, file, source, policy),
        duplicates: duplicates(language, file, source, current, equivalents),
        reuse_suggestions,
    })
}

impl RepositorySnapshot {
    pub fn assess_write(&self, file: &str, source: &str, limit: usize) -> Result<WriteAssessment> {
        let current = std::fs::read_to_string(self.repo().join(file)).ok();
        assess(
            self.id().clone(),
            self.freshness().clone(),
            &self.policy,
            self.neighborhood(),
            &equivalence_index(self),
            file,
            source,
            current.as_deref(),
            limit,
        )
    }

    pub fn write_prewrite_sidecar(&self) -> Result<PathBuf> {
        if !matches!(self.freshness(), SnapshotFreshness::Current) {
            bail!(
                "refusing to persist prewrite evidence from stale snapshot {}",
                self.id()
            );
        }
        let path = sidecar_path(self.repo());
        let parent = path.parent().context("prewrite sidecar has no parent")?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
        let sequence = SIDECAR_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let staged = parent.join(format!(".prewrite-{}-{sequence}.json", std::process::id()));
        let result = (|| -> Result<()> {
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&staged)
                .with_context(|| format!("creating {}", staged.display()))?;
            let mut writer = BufWriter::new(file);
            serde_json::to_writer(
                &mut writer,
                &PrewriteSidecarRef {
                    schema_version: PREWRITE_SCHEMA_VERSION,
                    snapshot: self.id(),
                    freshness: self.freshness(),
                    neighborhood: self.neighborhood(),
                    equivalents: &equivalence_index(self),
                },
            )?;
            writer.write_all(b"\n")?;
            writer.flush()?;
            writer.get_ref().sync_all()?;
            std::fs::rename(&staged, &path)
                .with_context(|| format!("installing {}", path.display()))?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&staged);
        }
        result.map(|()| path)
    }
}

pub fn sidecar_path(repo: &Path) -> PathBuf {
    let identity = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());
    let digest = blake3::hash(identity.as_os_str().as_encoded_bytes());
    std::env::temp_dir().join(format!("slop-prewrite-v1-{digest}.json"))
}

pub fn assess_from_sidecar(
    repo: &Path,
    policy: &Policy,
    file: &str,
    source: &str,
    limit: usize,
) -> Result<Option<WriteAssessment>> {
    let path = sidecar_path(repo);
    let file_handle = match File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("opening {}", path.display())),
    };
    let size = file_handle.metadata()?.len();
    if size > MAX_SIDECAR_BYTES {
        bail!(
            "prewrite sidecar {} is {size} bytes; limit is {MAX_SIDECAR_BYTES}",
            path.display()
        );
    }
    let sidecar: PrewriteSidecar = serde_json::from_reader(BufReader::new(file_handle))
        .with_context(|| format!("reading {}", path.display()))?;
    if sidecar.schema_version != PREWRITE_SCHEMA_VERSION {
        bail!(
            "prewrite sidecar schema {} is unsupported; expected {}",
            sidecar.schema_version,
            PREWRITE_SCHEMA_VERSION
        );
    }
    let current = std::fs::read_to_string(repo.join(file)).ok();
    assess(
        sidecar.snapshot,
        sidecar.freshness,
        policy,
        &sidecar.neighborhood,
        &sidecar.equivalents,
        file,
        source,
        current.as_deref(),
        limit,
    )
    .map(Some)
}

#[cfg(test)]
mod tests {
    use crate::snapshot::{CaptureRequest, Freshness};
    use crate::test_support::TempFixture;

    use super::*;

    fn fixture(name: &str) -> TempFixture {
        TempFixture::new(name)
    }

    #[test]
    fn a_renamed_copy_is_a_duplicate_and_unchanged_code_is_not() {
        let repo = fixture("toy_repo");
        let snapshot = RepositorySnapshot::capture(CaptureRequest { repo: &repo, index: None, policy: None, freshness: Freshness::Warn })
            .expect("snapshot");
        let file = "core/http_client.py";
        let current = std::fs::read_to_string(repo.join(file)).expect("read");
        assert!(snapshot.assess_write(file, &current, 3).expect("assess").duplicates.is_empty(), "unchanged functions must not match themselves");
        let start = current.find("    def get").expect("get");
        let copy = current[start..].replace("def get(", "def fetch_again(").replace("last_error", "previous");
        let proposed = format!("{current}\n{copy}");
        let found = snapshot.assess_write(file, &proposed, 3).expect("assess").duplicates;
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!((found[0].name.as_str(), found[0].existing.label.as_str()), ("fetch_again", "core.http_client::HttpClient::get"));
        let lines = snapshot.assess_write(file, &proposed, 3).expect("assess").steering(&repo);
        assert!(lines[0].contains("provably equivalent") && lines[0].contains("core/http_client.py:"), "{lines:?}");
        let renamed = current.replace("def get(", "def fetch(");
        assert!(snapshot.assess_write(file, &renamed, 3).expect("assess").duplicates.is_empty(), "a pure rename is not a duplicate of itself");
    }

    #[test]
    fn assessment_finds_an_existing_home_with_integer_evidence() {
        let repo = fixture("toy_repo_slopped");
        let snapshot = RepositorySnapshot::capture(CaptureRequest {
            repo: &repo,
            index: None,
            policy: None,
            freshness: Freshness::Warn,
        })
        .expect("snapshot");
        let assessment = snapshot
            .assess_write(
                "services/new_notify.py",
                "def emit(rows):\n    report = build_report(rows)\n    send_notification(report)\n",
                3,
            )
            .expect("assessment");
        assert!(assessment
            .reuse_suggestions
            .iter()
            .any(|suggestion| suggestion.label.ends_with("notify_with_report")));
        assert!(assessment
            .reuse_suggestions
            .iter()
            .all(|suggestion| suggestion.score_ppm >= MIN_REUSE_SCORE_PPM));
    }

    #[test]
    fn sidecar_is_byte_deterministic_and_roundtrips() {
        let repo = fixture("toy_repo_slopped");
        let path = sidecar_path(&repo);
        let _ = std::fs::remove_file(&path);
        let snapshot = RepositorySnapshot::capture(CaptureRequest {
            repo: &repo,
            index: None,
            policy: None,
            freshness: Freshness::Warn,
        })
        .expect("snapshot");
        snapshot.write_prewrite_sidecar().expect("write sidecar");
        let first = std::fs::read(&path).unwrap();
        snapshot.write_prewrite_sidecar().expect("rewrite sidecar");
        assert_eq!(std::fs::read(&path).unwrap(), first);
        let assessment = assess_from_sidecar(
            &repo,
            &snapshot.policy,
            "services/new_notify.py",
            "def emit(rows):\n    report = build_report(rows)\n    send_notification(report)\n",
            3,
        )
        .expect("read sidecar")
        .expect("sidecar exists");
        assert_eq!(assessment.snapshot, snapshot.id().clone());
        assert!(!assessment.reuse_suggestions.is_empty());
        let _ = std::fs::remove_file(path);
    }
}
