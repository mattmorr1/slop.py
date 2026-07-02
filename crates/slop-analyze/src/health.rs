//! Health score (D12): severity-weighted slop-density, normalized by
//! codebase size. Diff mode reports the delta as the headline number.

use slop_graph::NodeType;

use crate::build::BuiltGraph;
use crate::findings::{Finding, Severity};

fn weight(severity: Severity) -> f64 {
    match severity {
        Severity::Blocking => 10.0,
        Severity::Warning => 3.0,
        Severity::Advisory => 1.0,
    }
}

/// 0-100. Density is findings-weight per function, so a small repo with one
/// blocking finding scores worse than a huge repo with one.
pub fn score(findings: &[Finding], built: &BuiltGraph) -> u32 {
    let functions = built
        .graph
        .entities()
        .filter(|(_, e)| e.entity_type == NodeType::Function)
        .count()
        .max(1) as f64;
    let weighted: f64 = findings.iter().map(|f| weight(f.severity)).sum();
    let density = weighted / functions;
    (100.0 / (1.0 + density)).round() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weights_order_severities() {
        assert!(weight(Severity::Blocking) > weight(Severity::Warning));
        assert!(weight(Severity::Warning) > weight(Severity::Advisory));
    }
}
