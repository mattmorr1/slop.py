//! Snapshot-bound assessment of proposed source before it reaches the worktree.

use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::policy::Policy;
use crate::precheck::{precheck, PrecheckFinding};
use crate::retrieve::{self, Match, Neighborhood};
use crate::snapshot::{RepositorySnapshot, SnapshotFreshness, SnapshotId};

pub const PREWRITE_SCHEMA_VERSION: u32 = 1;
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
    pub reuse_suggestions: Vec<Match>,
}

impl WriteAssessment {
    pub fn steering(&self) -> Vec<String> {
        let mut lines: Vec<String> = self
            .policy_findings
            .iter()
            .map(PrecheckFinding::steering)
            .collect();
        lines.extend(self.reuse_suggestions.iter().map(|suggestion| {
            format!(
                "slop: proposed code overlaps existing `{}` at {}:{} through calls [{}]. Review that implementation before adding another; shared calls do not prove equivalent behavior.",
                suggestion.label,
                suggestion.file,
                suggestion.line + 1,
                suggestion.distinctive.join(", ")
            )
        }));
        lines
    }
}

#[derive(Deserialize)]
struct PrewriteSidecar {
    schema_version: u32,
    snapshot: SnapshotId,
    freshness: SnapshotFreshness,
    neighborhood: Neighborhood,
}

#[derive(Serialize)]
struct PrewriteSidecarRef<'a> {
    schema_version: u32,
    snapshot: &'a SnapshotId,
    freshness: &'a SnapshotFreshness,
    neighborhood: &'a Neighborhood,
}

fn assess(
    snapshot: SnapshotId,
    freshness: SnapshotFreshness,
    policy: &Policy,
    neighborhood: &Neighborhood,
    file: &str,
    source: &str,
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
        reuse_suggestions,
    })
}

impl RepositorySnapshot {
    pub fn assess_write(&self, file: &str, source: &str, limit: usize) -> Result<WriteAssessment> {
        assess(
            self.id().clone(),
            self.freshness().clone(),
            &self.policy,
            self.neighborhood(),
            file,
            source,
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
    assess(
        sidecar.snapshot,
        sidecar.freshness,
        policy,
        &sidecar.neighborhood,
        file,
        source,
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
