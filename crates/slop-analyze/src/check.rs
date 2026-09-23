//! The `check` flow shared by the CLI (`slop check`) and the MCP
//! `validate_change` tool (M4c): build the graph, run detectors (+
//! optional Tier-3), suppress, baseline, then either judge the whole repo
//! (`--all`) or just the diff against a git ref (D7's diff-relative mode).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use serde::Serialize;

use crate::findings::{Finding, Severity};
use crate::policy::Policy;
use crate::snapshot::{CaptureRequest, RepositorySnapshot, SnapshotFreshness, SnapshotId};
use crate::scan::{self, CoverageReport, ScanScope};
use crate::{build, health, source, suppress};

pub use crate::snapshot::Freshness;

pub const CHECK_SCHEMA_VERSION: u32 = 1;

pub struct CheckRequest {
    pub repo: PathBuf,
    pub index: Option<PathBuf>,
    pub policy: Option<PathBuf>,
    /// Judge the whole repo instead of the diff (audit-lite).
    pub all: bool,
    /// Run Tier-3 semantic-redundancy via the Claude API (needs
    /// `ANTHROPIC_API_KEY`). Slow and network-bound — off by default.
    pub tier3: bool,
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
            tier3: false,
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

fn tier3_findings(
    built: &build::BuiltGraph,
    facts: &[source::FileFacts],
    repo: &Path,
) -> Result<Vec<Finding>> {
    use slop_llm::{judge_from_env, JudgeInput};

    let candidates = crate::tier3::candidates(built, facts, repo);
    if candidates.is_empty() {
        eprintln!("tier3: no semantic-redundancy candidates");
        return Ok(Vec::new());
    }
    let judge = judge_from_env()?;
    eprintln!(
        "tier3: judging {} semantic-redundancy pair(s) via {}",
        candidates.len(),
        judge.model()
    );
    let render = |c: &crate::tier3::CandidateFn| {
        format!(
            "# {} ({}:{})\n# docstring: {}\n{}",
            c.entity,
            c.file,
            c.lines.0 + 1,
            c.docstring.as_deref().unwrap_or("<none>"),
            c.snippet
        )
    };
    let inputs: Vec<JudgeInput> = candidates
        .iter()
        .enumerate()
        .map(|(i, pair)| JudgeInput {
            index: i,
            a_label: pair.a.entity.clone(),
            a_context: render(&pair.a),
            b_label: pair.b.entity.clone(),
            b_context: render(&pair.b),
        })
        .collect();
    let verdicts = judge.judge(&inputs)?;

    let mut findings = Vec::new();
    for verdict in verdicts.into_iter().filter(|v| v.redundant) {
        let Some(pair) = candidates.get(verdict.index) else {
            continue;
        };
        findings.push(Finding {
            rule: "semantic-redundancy",
            severity: Severity::Advisory,
            entity: pair.a.entity.clone(),
            file: pair.a.file.clone(),
            lines: pair.a.lines,
            related: vec![crate::findings::EvidenceLocus {
                entity: pair.b.entity.clone(),
                file: pair.b.file.clone(),
                lines: pair.b.lines,
            }],
            message: format!(
                "`{}` and `{}` appear to serve the same purpose ({} confidence): {}",
                pair.a.entity, pair.b.entity, verdict.confidence, verdict.reason
            ),
            fix_guidance: format!(
                "Unify `{}` and `{}` behind one implementation",
                pair.a.entity, pair.b.entity
            ),
        });
    }
    Ok(findings)
}

/// Tier-3 gate for `trivial-wrapper`: the structural pass finds one-line
/// forwarders with 1-2 callers; the judge rules whether each name earns its
/// keep. Only judge-confirmed slop becomes a Warning — no structural
/// false-positive gates CI on its own.
pub fn tier3_wrapper_findings(
    built: &build::BuiltGraph,
    policy: &Policy,
    facts: &[source::FileFacts],
    _repo: &Path,
) -> Result<Vec<Finding>> {
    use slop_llm::{judge_from_env, JudgeInput};

    let candidates = crate::detect::trivial_wrapper_candidates(built, policy, facts);
    if candidates.is_empty() {
        eprintln!("tier3: no trivial-wrapper candidates");
        return Ok(Vec::new());
    }
    let judge = judge_from_env()?;
    eprintln!(
        "tier3: judging {} trivial-wrapper(s) via {}",
        candidates.len(),
        judge.model()
    );
    let inputs: Vec<JudgeInput> = candidates
        .iter()
        .enumerate()
        .map(|(i, c)| JudgeInput {
            index: i,
            a_label: c.name.clone(),
            a_context: format!(
                "{}\n# {} caller(s); whole body: return {}(...)",
                c.signature, c.callers, c.forward_target
            ),
            b_label: c.forward_target.clone(),
            b_context: String::new(),
        })
        .collect();
    let verdicts = judge.judge_wrappers(&inputs)?;

    let mut findings = Vec::new();
    for v in verdicts.into_iter().filter(|v| v.redundant) {
        let Some(c) = candidates.get(v.index) else {
            continue;
        };
        let fix_guidance = if c.forward_identity {
            format!(
                "Inline `{}`: replace `{}(...)` at its {} call site(s) with `{}(...)`, then delete the wrapper",
                c.name, c.name, c.callers, c.forward_target
            )
        } else {
            format!(
                "Inline `{}`'s body into its {} call site(s) and delete the wrapper",
                c.name, c.callers
            )
        };
        findings.push(Finding {
            rule: "trivial-wrapper",
            severity: Severity::Warning,
            entity: c.entity.clone(),
            file: c.file.clone(),
            lines: c.lines,
            related: Vec::new(),
            message: format!(
                "`{}` only forwards to `{}` ({} caller(s)) and its name adds nothing the call site would miss: {}",
                c.name, c.forward_target, c.callers, v.reason
            ),
            fix_guidance,
        });
    }
    Ok(findings)
}

/// Confirms or demotes Tier-2 `duplicate-structural` findings: matching
/// shape is a weak signal on its own (see `detect::shape_groups`), so when
/// `--tier3` is on, ask the judge whether each group's canonical member and
/// its first peer actually serve the same purpose. Returns
/// `(canonical entity, confirmed redundant, reason)` for `run` to apply
/// against the findings `detect::run_all` already produced.
fn tier3_structural_verdicts(
    built: &build::BuiltGraph,
    facts: &[source::FileFacts],
    repo: &Path,
) -> Result<Vec<(String, bool, String)>> {
    use slop_llm::{judge_from_env, JudgeInput};

    let candidates = crate::tier3::structural_candidates(built, facts, repo);
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let judge = judge_from_env()?;
    eprintln!(
        "tier3: confirming {} duplicate-structural group(s) via {}",
        candidates.len(),
        judge.model()
    );
    let render = |c: &crate::tier3::CandidateFn| {
        format!(
            "# {} ({}:{})\n# docstring: {}\n{}",
            c.entity,
            c.file,
            c.lines.0 + 1,
            c.docstring.as_deref().unwrap_or("<none>"),
            c.snippet
        )
    };
    let inputs: Vec<JudgeInput> = candidates
        .iter()
        .enumerate()
        .map(|(i, pair)| JudgeInput {
            index: i,
            a_label: pair.a.entity.clone(),
            a_context: render(&pair.a),
            b_label: pair.b.entity.clone(),
            b_context: render(&pair.b),
        })
        .collect();
    let verdicts = judge.judge(&inputs)?;

    Ok(verdicts
        .into_iter()
        .filter_map(|v| {
            let pair = candidates.get(v.index)?;
            Some((pair.a.entity.clone(), v.redundant, v.reason))
        })
        .collect())
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

/// One finding plus whether the baseline already grandfathers it. The
/// interactive dashboard (and any "show me everything" view) needs the full
/// set *with* the grandfathered ones marked, not silently dropped the way
/// `run` drops them — that opacity is exactly what makes `slop baseline`
/// confusing ("why is my repo suddenly clean?").
pub struct AuditFinding {
    pub finding: Finding,
    pub grandfathered: bool,
}

/// A whole-repo audit: every finding, tagged, most-severe first, with health
/// scored two ways — counting everything vs. counting only what isn't
/// grandfathered (what `run` reports).
pub struct AuditResult {
    pub snapshot: SnapshotId,
    pub freshness: SnapshotFreshness,
    pub findings: Vec<AuditFinding>,
    /// Health counting every finding (the honest state of the repo).
    pub health_all: u32,
    /// Health counting only un-grandfathered findings (what a gated loop acts on).
    pub health_new: u32,
    /// How many findings the baseline grandfathers.
    pub grandfathered: usize,
    pub policy_is_empty: bool,
}

/// Run the full detector set over the whole repo and return every finding,
/// tagging each with whether the baseline grandfathers it. Unlike `run`, this
/// never hides grandfathered findings — it marks them, so a UI can show the
/// real state and let the user choose what to look at.
fn evaluated_findings(analysis: &RepositorySnapshot, tier3: bool) -> Result<Vec<AuditFinding>> {
    let (built, facts) = (&analysis.built, &analysis.facts);
    let mut raw = crate::detect::run_all(built, &analysis.policy, facts, analysis.repo());
    raw.extend(crate::detect::effect_creep(built, &analysis.baseline));
    if tier3 {
        for (entity, redundant, reason) in tier3_structural_verdicts(built, facts, analysis.repo())? {
            let Some(f) = raw.iter_mut().find(|f| f.rule == "duplicate-structural" && f.entity == entity)
            else {
                continue;
            };
            if redundant {
                f.severity = Severity::Warning;
                f.message = format!("{} — confirmed by semantic review: {reason}", f.message);
            } else {
                f.message = format!(
                    "{} (unconfirmed — semantic review found a different purpose: {reason})",
                    f.message
                );
            }
        }
        raw.extend(tier3_findings(built, facts, analysis.repo())?);
        raw.extend(tier3_wrapper_findings(built, &analysis.policy, facts, analysis.repo())?);
    }
    let suppressions = suppress::scan(analysis.repo(), facts);
    let submodules = source::submodule_paths(analysis.repo());
    let all: Vec<_> = suppress::filter(raw, &suppressions)
        .into_iter()
        .filter(|f| !source::in_submodule(&f.file, &submodules))
        .collect();
    let grandfathered_set: std::collections::HashSet<(&str, &str)> = analysis
        .baseline
        .findings
        .iter()
        .map(|e| (e.rule.as_str(), e.entity.as_str()))
        .collect();

    let mut findings: Vec<AuditFinding> = all
        .into_iter()
        .map(|f| {
            let grandfathered = grandfathered_set.contains(&(f.rule, f.entity.as_str()));
            AuditFinding { finding: f, grandfathered }
        })
        .collect();
    // Most-severe first, then a stable (rule, entity) order within a severity.
    findings.sort_by(|a, b| {
        b.finding
            .severity
            .cmp(&a.finding.severity)
            .then_with(|| a.finding.rule.cmp(b.finding.rule))
            .then_with(|| a.finding.entity.cmp(&b.finding.entity))
    });

    Ok(findings)
}

pub fn effective_findings(analysis: &RepositorySnapshot, tier3: bool) -> Result<Vec<Finding>> {
    Ok(evaluated_findings(analysis, tier3)?
        .into_iter()
        .filter(|record| !record.grandfathered)
        .map(|record| record.finding)
        .collect())
}

pub fn baseline_findings(analysis: &RepositorySnapshot) -> Vec<Finding> {
    let raw = crate::detect::run_all(
        &analysis.built,
        &analysis.policy,
        &analysis.facts,
        analysis.repo(),
    );
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

pub fn audit_snapshot(analysis: &RepositorySnapshot) -> Result<AuditResult> {
    let findings = evaluated_findings(analysis, false)?;
    let all: Vec<Finding> = findings.iter().map(|record| record.finding.clone()).collect();
    let new_only: Vec<Finding> = findings
        .iter()
        .filter(|record| !record.grandfathered)
        .map(|record| record.finding.clone())
        .collect();
    let grandfathered = findings.len() - new_only.len();
    Ok(AuditResult {
        snapshot: analysis.id().clone(),
        freshness: analysis.freshness().clone(),
        findings,
        health_all: health::score(&all, &analysis.built),
        health_new: health::score(&new_only, &analysis.built),
        grandfathered,
        policy_is_empty: analysis.policy.channels.is_empty(),
    })
}

pub fn audit(repo: &Path, index: Option<&Path>) -> Result<AuditResult> {
    let analysis = load_analysis_fresh(repo, index, Freshness::Reindex)?;
    audit_snapshot(&analysis)
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
    let effective = effective_findings(&analysis, req.tier3)?;

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
    use crate::test_support::TempFixture;

    fn fixture(name: &str) -> TempFixture {
        TempFixture::new(name)
    }

    #[test]
    fn audit_reports_all_findings_severity_ordered_with_no_baseline() {
        let out = audit(&fixture("toy_repo_slopped"), None).expect("audit");
        assert!(!out.findings.is_empty(), "slopped repo should have findings");
        // No baseline in the fixture -> nothing grandfathered, both healths equal.
        assert_eq!(out.grandfathered, 0);
        assert!(out.findings.iter().all(|f| !f.grandfathered));
        assert_eq!(out.health_all, out.health_new);
        // Most-severe first.
        for pair in out.findings.windows(2) {
            assert!(pair[0].finding.severity >= pair[1].finding.severity);
        }
    }

    #[test]
    fn audit_clean_repo_scores_full_health() {
        let out = audit(&fixture("toy_repo"), None).expect("audit");
        assert!(out.findings.is_empty());
        assert_eq!(out.health_all, 100);
    }

    #[test]
    fn check_audit_and_fix_projection_share_fingerprints() {
        let repo = fixture("toy_repo_slopped");
        let snapshot = load_analysis_fresh(&repo, None, Freshness::Reindex).expect("snapshot");
        let audit = audit_snapshot(&snapshot).expect("audit");
        let fix = effective_findings(&snapshot, false).expect("fix projection");
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
        let active: Vec<Finding> = audit
            .findings
            .into_iter()
            .filter(|record| !record.grandfathered)
            .map(|record| record.finding)
            .collect();
        assert_eq!(keys(&active), keys(&fix));
        assert_eq!(keys(&fix), keys(&check.findings));
        assert_eq!(snapshot.id(), &check.snapshot);
    }
}
