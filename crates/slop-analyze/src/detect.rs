//! Detectors. M1 ships the flagship: infra-bypass (D2).

use petgraph::visit::EdgeRef;
use petgraph::Direction;
use slop_graph::{EdgeKind, NodeType};

use crate::build::BuiltGraph;
use crate::findings::{Finding, Severity};
use crate::policy::Policy;

/// Infra-bypass: an entity acquires a raw effect *directly* (a `HasEffect`
/// edge to a seed source) while the policy names a sanctioned channel for
/// that effect and the entity is outside it. Transitive effects — calling
/// the sanctioned channel — are exactly what code is supposed to do and are
/// never flagged.
pub fn infra_bypass(built: &BuiltGraph, policy: &Policy) -> Vec<Finding> {
    let mut findings = Vec::new();
    let graph = &built.graph;

    for (idx, entity) in graph.entities() {
        // Modules acquire effects through import statements alone; judging
        // them would flag every file that imports urllib for any reason.
        // Functions and classes are where acquisition is behavior.
        if !matches!(entity.entity_type, NodeType::Function | NodeType::Class) {
            continue;
        }
        for edge in graph.graph.edges_directed(idx, Direction::Outgoing) {
            if *edge.weight() != EdgeKind::HasEffect {
                continue;
            }
            let source = graph.entity(edge.target());
            for &effect in &source.effect_signature.0 {
                let channels = policy.channels_for(effect);
                if channels.is_empty() {
                    continue; // no policy for this effect => silent (D8)
                }
                if policy.is_sanctioned(effect, &entity.id) {
                    continue;
                }
                findings.push(Finding {
                    rule: "infra-bypass",
                    severity: Severity::Blocking,
                    entity: entity.id.clone(),
                    file: entity.file.clone(),
                    lines: entity.source_range,
                    message: format!(
                        "`{}` acquires a raw {:?} effect directly via `{}`; this codebase routes {:?} I/O through {}",
                        entity.id,
                        effect,
                        source.id,
                        effect,
                        channels.join(", "),
                    ),
                    fix_guidance: format!(
                        "Delegate the {:?} operation to the sanctioned channel `{}` instead of calling `{}` directly",
                        effect,
                        channels.join("` or `"),
                        source.id,
                    ),
                });
            }
        }
    }

    findings.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.lines.0.cmp(&b.lines.0))
    });
    findings.dedup_by(|a, b| a.entity == b.entity && a.rule == b.rule && a.message == b.message);
    findings
}

/// All M1 detectors.
pub fn run_all(built: &BuiltGraph, policy: &Policy) -> Vec<Finding> {
    infra_bypass(built, policy)
}
