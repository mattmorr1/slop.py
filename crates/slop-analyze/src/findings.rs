//! Findings + severity (D12). Confidence is intrinsic to the detector:
//! deterministic detectors may block, fuzzy detectors are structurally
//! incapable of it.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum Severity {
    Advisory,
    Warning,
    Blocking,
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Severity::Blocking => write!(f, "BLOCKING"),
            Severity::Warning => write!(f, "WARNING"),
            Severity::Advisory => write!(f, "ADVISORY"),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub rule: &'static str,
    pub severity: Severity,
    /// Entity ID, e.g. `services.alerts::send_alert`.
    pub entity: String,
    pub file: String,
    /// 0-based line range of the offending entity's body.
    pub lines: (usize, usize),
    pub message: String,
    /// Machine-readable resolution hint (D9): what a fixer — human or agent —
    /// should do instead.
    pub fix_guidance: String,
}

impl std::fmt::Display for Finding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "{} [{}] {} ({}:{})",
            self.severity,
            self.rule,
            self.entity,
            self.file,
            self.lines.0 + 1,
        )?;
        writeln!(f, "  {}", self.message)?;
        write!(f, "  fix: {}", self.fix_guidance)
    }
}
