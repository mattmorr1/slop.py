//! Validation gate (M5, D9): the CI / agent-fix-loop entry point. slop runs
//! the same check the CLI runs, then reports a machine-readable verdict and a
//! non-zero exit when anything blocks — it never edits. The driving agent
//! consumes `findings[].fix_guidance`, edits, and re-runs. Process
//! orchestration (`--reindex`, `--worktree`) lives in the CLI; the pure
//! verdict shaping lives here so it's testable without git or scip-python.

use anyhow::Result;
use serde_json::json;

use crate::check::{self, CheckRequest};
use crate::findings::Severity;

pub struct GateOutcome {
    /// Pretty JSON verdict for the driving agent / CI log.
    pub json: String,
    /// Findings at or above the fail threshold — the process exits non-zero
    /// when > 0.
    pub failing: usize,
}

/// Run the check and shape the gate verdict. Findings at or above `fail_on`
/// fail the gate (caller exits non-zero); everything else is reported but
/// passes. `fail_on` defaults to `Blocking` at the CLI, but lowering it to
/// `Warning` is what lets the fix-loop act on `duplicate-exact` /
/// `complexity-spike` (both Warnings), not just the deterministic blockers.
pub fn evaluate(req: CheckRequest, fail_on: Severity) -> Result<GateOutcome> {
    let result = check::run(req)?;
    let failing = result
        .findings
        .iter()
        .filter(|f| f.severity >= fail_on)
        .count();
    let json = serde_json::to_string_pretty(&json!({
        "schema_version": result.schema_version,
        "snapshot": result.snapshot,
        "freshness": result.freshness,
        "scope": result.scope,
        "coverage": result.coverage,
        "passed": failing == 0,
        "fail_on": fail_on.to_string(),
        "failing": failing,
        "blocking": result.blocking,
        "total": result.findings.len(),
        "health": result.health_line,
        "current_health": result.current_health,
        "policy_is_empty": result.policy_is_empty,
        "findings": result.findings,
    }))?;
    Ok(GateOutcome { json, failing })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempFixture;

    fn fixture(name: &str) -> TempFixture {
        TempFixture::new(name)
    }

    fn gate_all(name: &str, fail_on: Severity) -> GateOutcome {
        let repo = fixture(name);
        evaluate(
            CheckRequest {
                repo: repo.to_path_buf(),
                all: true, // whole-repo, so the test doesn't depend on git diff
                ..Default::default()
            },
            fail_on,
        )
        .expect("gate")
    }

    #[test]
    fn slopped_repo_blocks() {
        let out = gate_all("toy_repo_slopped", Severity::Blocking);
        assert!(out.failing > 0, "{}", out.json);
        assert!(out.json.contains("\"passed\": false"));
        assert!(out.json.contains("\"schema_version\": 1"));
    }

    #[test]
    fn clean_repo_passes() {
        let out = gate_all("toy_repo", Severity::Blocking);
        assert_eq!(out.failing, 0, "{}", out.json);
        assert!(out.json.contains("\"passed\": true"));
    }

    #[test]
    fn fail_on_warning_catches_non_blocking_slop() {
        // The clean repo still passes at Blocking; the slopped repo, which has
        // Warning-tier findings (duplicate/complexity/purity), fails once the
        // threshold drops to Warning — this is what makes those rules drive
        // the loop.
        let blocking = gate_all("toy_repo_slopped", Severity::Blocking);
        let warning = gate_all("toy_repo_slopped", Severity::Warning);
        assert!(
            warning.failing >= blocking.failing,
            "warning threshold must catch at least as much"
        );
        assert!(warning.failing > 0);
        assert!(warning.json.contains("\"fail_on\": \"WARNING\""));
    }
}
