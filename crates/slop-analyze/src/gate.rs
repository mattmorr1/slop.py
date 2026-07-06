//! Validation gate (M5, D9): the CI / agent-fix-loop entry point. slop runs
//! the same check the CLI runs, then reports a machine-readable verdict and a
//! non-zero exit when anything blocks — it never edits. The driving agent
//! consumes `findings[].fix_guidance`, edits, and re-runs. Process
//! orchestration (`--reindex`, `--worktree`) lives in the CLI; the pure
//! verdict shaping lives here so it's testable without git or scip-python.

use anyhow::Result;
use serde_json::json;

use crate::check::{self, CheckRequest};

pub struct GateOutcome {
    /// Pretty JSON verdict for the driving agent / CI log.
    pub json: String,
    /// Number of Blocking findings — the process exits non-zero when > 0.
    pub blocking: usize,
}

/// Run the check and shape the gate verdict. Blocking findings mean the gate
/// fails (caller exits non-zero); Warning/Advisory are reported but pass.
pub fn evaluate(req: CheckRequest) -> Result<GateOutcome> {
    let result = check::run(req)?;
    let passed = result.blocking == 0;
    let json = serde_json::to_string_pretty(&json!({
        "passed": passed,
        "blocking": result.blocking,
        "total": result.findings.len(),
        "health": result.health_line,
        "policy_is_empty": result.policy_is_empty,
        "findings": result.findings,
    }))?;
    Ok(GateOutcome {
        json,
        blocking: result.blocking,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures")
            .join(name)
    }

    fn gate_all(name: &str) -> GateOutcome {
        evaluate(CheckRequest {
            repo: fixture(name),
            all: true, // whole-repo, so the test doesn't depend on git diff
            ..Default::default()
        })
        .expect("gate")
    }

    #[test]
    fn slopped_repo_blocks() {
        let out = gate_all("toy_repo_slopped");
        assert!(out.blocking > 0, "{}", out.json);
        assert!(out.json.contains("\"passed\": false"));
    }

    #[test]
    fn clean_repo_passes() {
        let out = gate_all("toy_repo");
        assert_eq!(out.blocking, 0, "{}", out.json);
        assert!(out.json.contains("\"passed\": true"));
    }
}
