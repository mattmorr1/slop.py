//! Detectors. Deterministic set per §3.4: infra-bypass (flagship),
//! circular imports, dead islands, purity-lies.

use petgraph::visit::EdgeRef;
use petgraph::Direction;
use slop_graph::{EdgeKind, Effect, NodeType};

use crate::build::BuiltGraph;
use crate::findings::{Finding, Severity};
use crate::policy::Policy;

/// Infra-bypass: an entity acquires a raw effect *directly* (a `HasEffect`
/// edge to a seed source) while the policy names a sanctioned channel for
/// that effect and the entity is outside it. Transitive effects — calling
/// the sanctioned channel — are exactly what code is supposed to do and are
/// never flagged.
pub fn infra_bypass(built: &BuiltGraph, policy: &Policy) -> Vec<Finding> {
    let graph = &built.graph;
    // One finding per (entity, effect), aggregating every raw source it
    // touches — per-source findings triple-report a single bypass.
    let mut offenders: Vec<(petgraph::graph::NodeIndex, slop_graph::Effect, Vec<String>)> =
        Vec::new();

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
                if policy.channels_for(effect).is_empty() {
                    continue; // no policy for this effect => silent (D8)
                }
                if policy.is_sanctioned(effect, &entity.id) {
                    continue;
                }
                match offenders
                    .iter_mut()
                    .find(|(n, e, _)| *n == idx && *e == effect)
                {
                    Some((_, _, sources)) => {
                        if !sources.contains(&source.id) {
                            sources.push(source.id.clone());
                        }
                    }
                    None => offenders.push((idx, effect, vec![source.id.clone()])),
                }
            }
        }
    }

    let mut findings: Vec<Finding> = offenders
        .into_iter()
        .map(|(idx, effect, mut sources)| {
            sources.sort();
            let entity = graph.entity(idx);
            let channels = policy.channels_for(effect);
            Finding {
                rule: "infra-bypass",
                severity: Severity::Blocking,
                entity: entity.id.clone(),
                file: entity.file.clone(),
                lines: entity.source_range,
                message: format!(
                    "`{}` acquires a raw {:?} effect directly via `{}`; this codebase routes {:?} I/O through {}",
                    entity.id,
                    effect,
                    sources.join("`, `"),
                    effect,
                    channels.join(", "),
                ),
                fix_guidance: format!(
                    "Delegate the {:?} operation to the sanctioned channel `{}` instead of using `{}` directly",
                    effect,
                    channels.join("` or `"),
                    sources.join("`, `"),
                ),
            }
        })
        .collect();

    findings.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.lines.0.cmp(&b.lines.0))
    });
    findings
}

/// Circular imports: Tarjan SCC on the `Imports` subgraph. A cycle is the
/// finding (D6) — one per SCC, anchored on every member module so the diff
/// filter keeps it when any member file changes.
pub fn circular_import(built: &BuiltGraph) -> Vec<Finding> {
    use petgraph::graph::DiGraph;
    let graph = &built.graph;

    let mut imports: DiGraph<petgraph::graph::NodeIndex, ()> = DiGraph::new();
    let mut map = std::collections::HashMap::new();
    for edge in graph.graph.edge_indices() {
        if graph.graph[edge] != EdgeKind::Imports {
            continue;
        }
        let (a, b) = graph.graph.edge_endpoints(edge).unwrap();
        let ia = *map.entry(a).or_insert_with(|| imports.add_node(a));
        let ib = *map.entry(b).or_insert_with(|| imports.add_node(b));
        imports.add_edge(ia, ib, ());
    }

    let mut findings = Vec::new();
    for scc in petgraph::algo::tarjan_scc(&imports) {
        if scc.len() < 2 {
            continue;
        }
        let mut members: Vec<&str> = scc
            .iter()
            .map(|&i| graph.entity(imports[i]).id.as_str())
            .collect();
        members.sort();
        let cycle = members.join(" -> ");
        for &i in &scc {
            let entity = graph.entity(imports[i]);
            findings.push(Finding {
                rule: "circular-import",
                severity: Severity::Blocking,
                entity: entity.id.clone(),
                file: entity.file.clone(),
                lines: entity.source_range,
                message: format!("`{}` is part of an import cycle: {cycle}", entity.id),
                fix_guidance: format!(
                    "Break the cycle {cycle} — move the shared dependency into a module neither imports, or defer the import into the function that needs it"
                ),
            });
        }
    }
    findings
}

/// Dead island: a function nothing in the repo calls and that isn't a
/// declared entry point — the classic hallucinated-structure signal.
pub fn dead_island(built: &BuiltGraph, policy: &Policy) -> Vec<Finding> {
    let graph = &built.graph;
    let mut findings = Vec::new();
    for (idx, entity) in graph.entities() {
        if entity.entity_type != NodeType::Function {
            continue;
        }
        if policy.is_entry_point(&entity.id) {
            continue;
        }
        let referenced = graph
            .graph
            .edges_directed(idx, Direction::Incoming)
            .any(|e| *e.weight() == EdgeKind::Calls);
        if referenced {
            continue;
        }
        findings.push(Finding {
            rule: "dead-island",
            severity: Severity::Warning,
            entity: entity.id.clone(),
            file: entity.file.clone(),
            lines: entity.source_range,
            message: format!(
                "`{}` is never referenced anywhere in the codebase and is not a declared entry point",
                entity.id
            ),
            fix_guidance: format!(
                "Delete `{}`, or declare it in slop.toml `entry_points` if it is a public API",
                entity.id
            ),
        });
    }
    findings
}

const PURE_NAME_PREFIXES: &[&str] = &[
    "calculate_", "compute_", "parse_", "format_", "validate_", "normalize_", "convert_",
    "is_", "to_", "as_",
];
const IO_EFFECTS: &[Effect] = &[
    Effect::Net,
    Effect::FsRead,
    Effect::FsWrite,
    Effect::Db,
    Effect::Env,
];

/// Purity-lie: the name promises a pure computation; the (transitive)
/// effect signature says I/O.
pub fn purity_lie(built: &BuiltGraph) -> Vec<Finding> {
    let graph = &built.graph;
    let mut findings = Vec::new();
    for (_, entity) in graph.entities() {
        if entity.entity_type != NodeType::Function {
            continue;
        }
        let name = entity.id.rsplit("::").next().unwrap_or(&entity.id);
        if !PURE_NAME_PREFIXES.iter().any(|p| name.starts_with(p)) {
            continue;
        }
        let io: Vec<Effect> = IO_EFFECTS
            .iter()
            .copied()
            .filter(|&e| entity.effect_signature.contains(e))
            .collect();
        if io.is_empty() {
            continue;
        }
        findings.push(Finding {
            rule: "purity-lie",
            severity: Severity::Warning,
            entity: entity.id.clone(),
            file: entity.file.clone(),
            lines: entity.source_range,
            message: format!(
                "`{name}` is named like a pure computation but its effect signature is {io:?}",
            ),
            fix_guidance: format!(
                "Rename `{name}` to reflect the I/O it performs, or extract the pure computation from the I/O"
            ),
        });
    }
    findings
}

/// All detectors, sorted by severity then location.
pub fn run_all(built: &BuiltGraph, policy: &Policy) -> Vec<Finding> {
    let mut findings = infra_bypass(built, policy);
    findings.extend(circular_import(built));
    findings.extend(dead_island(built, policy));
    findings.extend(purity_lie(built));
    findings.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.lines.0.cmp(&b.lines.0))
            .then_with(|| a.rule.cmp(b.rule))
    });
    findings
}
