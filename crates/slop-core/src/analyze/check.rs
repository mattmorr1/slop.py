//! The `check` flow shared by the CLI (`slop check`) and the MCP
//! `reconcile` tool (M4c): build the graph, run detectors, suppress, then
//! either judge the whole repo
//! (`--all`) or just the diff against a git ref (D7's diff-relative mode).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use serde::Serialize;

use crate::analyze::findings::{Finding, Severity};
use crate::analyze::snapshot::{CaptureRequest, RepositorySnapshot, SnapshotFreshness, SnapshotId};
use crate::analyze::scan::{self, CoverageReport, ScanScope};
use crate::analyze::{health, source, suppress};

pub use crate::analyze::snapshot::Freshness;

pub const CHECK_SCHEMA_VERSION: u32 = 1;

pub struct CheckRequest {
    pub repo: PathBuf,
    pub index: Option<PathBuf>,
    pub policy: Option<PathBuf>,
    /// Judge the whole repo instead of the diff (audit-lite).
    pub all: bool,
    /// Git ref to diff against in diff-relative mode.
    pub base: String,
}

impl Default for CheckRequest {
    fn default() -> Self {
        Self {
            repo: PathBuf::new(),
            index: None,
            policy: None,
            all: false,
            base: "HEAD".to_string(),
        }
    }
}

#[derive(Serialize)]
pub struct CheckResult {
    pub schema_version: u32,
    pub snapshot: SnapshotId,
    pub freshness: SnapshotFreshness,
    pub scope: ScanScope,
    pub coverage: CoverageReport,
    pub findings: Vec<Finding>,
    pub current_health: u32,
    pub health_line: String,
    pub blocking: usize,
    /// True when the repo has no slop.toml — infra-bypass stays silent.
    pub policy_is_empty: bool,
}

pub type Analysis = RepositorySnapshot;

/// Resolve `index` (default `<repo>/index.scip`), build the effect graph, and
/// parse the repo's source facts. Errors with an indexing hint when the index
/// is missing. Defaults to [`Freshness::Warn`]; see [`load_analysis_fresh`].
pub fn load_analysis(repo: &Path, index: Option<&Path>) -> Result<Arc<Analysis>> {
    load_analysis_fresh(repo, index, Freshness::Warn)
}

/// [`load_analysis`] with an explicit staleness policy, returning a shared
/// handle so repeated calls in one process don't rebuild the graph.
pub fn load_analysis_fresh(
    repo: &Path,
    index: Option<&Path>,
    freshness: Freshness,
) -> Result<Arc<Analysis>> {
    RepositorySnapshot::capture(CaptureRequest { repo, index, policy: None, freshness })
}

/// Every finding the detectors report over the whole repo after suppressions and
/// submodule exclusion, most severe first, then by (rule, entity).
pub fn effective_findings(analysis: &RepositorySnapshot) -> Vec<Finding> {
    let raw = crate::analyze::detect::run_all(&analysis.built, &analysis.policy, &analysis.facts);
    let suppressions = suppress::scan(analysis.repo(), &analysis.facts);
    let submodules = source::submodule_paths(analysis.repo());
    let mut findings: Vec<Finding> = suppress::filter(raw, &suppressions)
        .into_iter()
        .filter(|finding| !source::in_submodule(&finding.file, &submodules))
        .collect();
    findings.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then_with(|| a.rule.cmp(b.rule))
            .then_with(|| a.entity.cmp(&b.entity))
    });
    findings
}

pub fn run(req: CheckRequest) -> Result<CheckResult> {
    run_with_freshness(req, Freshness::Reindex)
}

pub fn run_with_freshness(req: CheckRequest, freshness: Freshness) -> Result<CheckResult> {
    let analysis = RepositorySnapshot::capture(CaptureRequest {
        repo: &req.repo,
        index: req.index.as_deref(),
        policy: req.policy.as_deref(),
        freshness,
    })?;
    let built = &analysis.built;
    let effective = effective_findings(&analysis);

    let current_health = health::score(&effective, built);
    let coverage = scan::coverage(&analysis);
    let (scope, findings, health_line) = if req.all {
        (
            ScanScope::Repository,
            effective,
            format!("health: {current_health}/100"),
        )
    } else {
        let changed = scan::worktree_changes(&analysis, &req.base)?;
        let findings = scan::filter_to_changes(effective, &changed);
        let line = format!(
            "health: {current_health}/100 · {} finding(s) affect worktree",
            findings.len()
        );
        (
            ScanScope::Worktree { base: req.base },
            findings,
            line,
        )
    };

    let blocking = findings.iter().filter(|f| f.severity == Severity::Blocking).count();
    Ok(CheckResult {
        schema_version: CHECK_SCHEMA_VERSION,
        snapshot: analysis.id().clone(),
        freshness: analysis.freshness().clone(),
        scope,
        coverage,
        findings,
        current_health,
        health_line,
        blocking,
        policy_is_empty: analysis.policy.channels.is_empty(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze::test_support::TempFixture;

    fn fixture(name: &str) -> TempFixture {
        TempFixture::new(name)
    }

    #[test]
    fn check_reports_findings_most_severe_first() {
        let out = run(CheckRequest { repo: fixture("toy_repo_slopped").to_path_buf(), all: true, ..Default::default() }).expect("check");
        assert!(!out.findings.is_empty(), "slopped repo should have findings");
        for pair in out.findings.windows(2) {
            assert!(pair[0].severity >= pair[1].severity);
        }
    }

    #[test]
    fn check_clean_repo_scores_full_health() {
        let out = run(CheckRequest { repo: fixture("toy_repo").to_path_buf(), all: true, ..Default::default() }).expect("check");
        assert!(out.findings.is_empty());
        assert_eq!(out.current_health, 100);
    }

    #[test]
    fn check_and_snapshot_projection_share_fingerprints() {
        let repo = fixture("toy_repo_slopped");
        let snapshot = load_analysis_fresh(&repo, None, Freshness::Reindex).expect("snapshot");
        let effective = effective_findings(&snapshot);
        let check = run(CheckRequest {
            repo: repo.to_path_buf(),
            all: true,
            ..Default::default()
        })
        .expect("check");
        let keys = |findings: &[Finding]| {
            findings
                .iter()
                .map(|finding| (finding.rule, finding.entity.clone()))
                .collect::<Vec<_>>()
        };
        assert_eq!(keys(&effective), keys(&check.findings));
        assert_eq!(snapshot.id(), &check.snapshot);
    }
}
