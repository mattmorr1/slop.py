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
/// `decorated` holds entity IDs of functions with any decorator: those are
/// exempt (framework registration — routes, MCP handlers, fixtures,
/// properties — is invocation without a by-name reference).
pub fn dead_island(
    built: &BuiltGraph,
    policy: &Policy,
    decorated: &std::collections::HashSet<String>,
) -> Vec<Finding> {
    let graph = &built.graph;

    // Methods of classes with class-level external dependencies (base
    // classes, class decorators — e.g. `class Net(nn.Module)`) implement a
    // framework contract and get called by the framework, not by name.
    // Exempting them trades a few false negatives for a large real
    // false-positive class (found dogfooding geoguessrbot's torch models).
    let mut framework_classes: std::collections::HashSet<petgraph::graph::NodeIndex> =
        std::collections::HashSet::new();
    for (idx, entity) in graph.entities() {
        if entity.entity_type != NodeType::Class {
            continue;
        }
        let has_external_dep = graph
            .graph
            .edges_directed(idx, Direction::Outgoing)
            .any(|e| {
                *e.weight() == EdgeKind::Calls
                    && graph.entity(e.target()).entity_type == NodeType::EffectSource
            });
        if has_external_dep {
            framework_classes.insert(idx);
        }
    }

    let mut findings = Vec::new();
    for (idx, entity) in graph.entities() {
        if entity.entity_type != NodeType::Function {
            continue;
        }
        if policy.is_entry_point(&entity.id) {
            continue;
        }
        if decorated.contains(&entity.id) {
            continue;
        }
        let framework_method = graph
            .graph
            .edges_directed(idx, Direction::Incoming)
            .any(|e| {
                *e.weight() == EdgeKind::Contains && framework_classes.contains(&e.source())
            });
        if framework_method {
            continue;
        }
        let referenced = graph
            .graph
            .edges_directed(idx, Direction::Incoming)
            .any(|e| *e.weight() == EdgeKind::Calls);
        if referenced {
            continue;
        }
        // Methods are dispatched via a receiver (`obj.method()`), which SCIP
        // routinely fails to resolve when the receiver type is dynamic — so an
        // unreferenced method is a much weaker "dead" signal than an
        // unreferenced free function (which is called by name). Downgrade
        // methods to Advisory rather than emitting Warning-grade false
        // positives (found dogfooding vigil: `TTLCache.set`, etc.).
        let is_method = graph
            .graph
            .edges_directed(idx, Direction::Incoming)
            .any(|e| {
                *e.weight() == EdgeKind::Contains
                    && graph.entity(e.source()).entity_type == NodeType::Class
            });
        let (severity, message) = if is_method {
            (
                Severity::Advisory,
                format!(
                    "`{}` has no resolved caller — possibly dead, but methods are often dispatched dynamically (SCIP can miss the call site)",
                    entity.id
                ),
            )
        } else {
            (
                Severity::Warning,
                format!(
                    "`{}` is never referenced anywhere in the codebase and is not a declared entry point",
                    entity.id
                ),
            )
        };
        findings.push(Finding {
            rule: "dead-island",
            severity,
            entity: entity.id.clone(),
            file: entity.file.clone(),
            lines: entity.source_range,
            message,
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

const COMPLEXITY_THRESHOLD: u32 = 10;

/// Entity label for a parsed function: the graph node's ID when the join
/// succeeds, a file-derived fallback otherwise.
fn label(
    built: &BuiltGraph,
    index: &std::collections::HashMap<(String, usize), petgraph::graph::NodeIndex>,
    file: &str,
    fact: &slop_parse::FunctionFacts,
) -> String {
    crate::source::entity_for(built, index, file, fact)
        .map(|e| e.id.clone())
        .unwrap_or_else(|| format!("{}::{}", file.trim_end_matches(".py").replace('/', "."), fact.name))
}

struct Member {
    label: String,
    file: String,
    lines: (usize, usize),
    body_hash: String,
    structural_hash: String,
    docstring: Option<String>,
}

fn collect_members(built: &BuiltGraph, facts: &[crate::source::FileFacts]) -> Vec<Member> {
    let index = crate::source::location_index(built);
    let mut members = Vec::new();
    for ff in facts {
        for fact in &ff.functions {
            if fact.body_hash.is_empty() {
                continue;
            }
            let entity = crate::source::entity_for(built, &index, &ff.file, fact);
            members.push(Member {
                label: entity.map(|e| e.id.clone()).unwrap_or_else(|| {
                    format!("{}::{}", ff.file.trim_end_matches(".py").replace('/', "."), fact.name)
                }),
                file: ff.file.clone(),
                lines: (fact.start_line as usize, fact.end_line as usize),
                body_hash: fact.body_hash.clone(),
                structural_hash: fact.structural_hash.clone(),
                docstring: entity.and_then(|e| e.docstring.clone()),
            });
        }
    }
    members
}

// One finding per group, not one per member — a group of N duplicates
// triple-reports (or worse) the same fact if every member gets its own
// finding. Pick a stable representative (earliest in the file) and list
// the rest as peers, same principle as infra_bypass's per-entity
// aggregation above.
fn canonical(members: &[Member], group: &[usize]) -> usize {
    *group
        .iter()
        .min_by(|&&a, &&b| {
            (members[a].file.as_str(), members[a].lines.0)
                .cmp(&(members[b].file.as_str(), members[b].lines.0))
        })
        .expect("group is non-empty")
}

/// One member of a duplication group, identified for Tier-3 confirmation
/// (`check::tier3_findings`) as well as finding output.
pub struct DupEntity {
    pub entity: String,
    pub file: String,
    pub lines: (usize, usize),
    pub docstring: Option<String>,
}

pub struct StructuralGroup {
    pub canonical: DupEntity,
    pub peers: Vec<DupEntity>,
}

fn dup_entity(m: &Member) -> DupEntity {
    DupEntity {
        entity: m.label.clone(),
        file: m.file.clone(),
        lines: m.lines,
        docstring: m.docstring.clone(),
    }
}

fn shape_groups(members: &[Member]) -> Vec<StructuralGroup> {
    use std::collections::HashMap;
    let mut by_shape: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, m) in members.iter().enumerate() {
        by_shape.entry(m.structural_hash.as_str()).or_default().push(i);
    }
    let mut groups = Vec::new();
    for group in by_shape.values().filter(|g| g.len() > 1) {
        let distinct_bodies: std::collections::HashSet<&str> =
            group.iter().map(|&i| members[i].body_hash.as_str()).collect();
        if distinct_bodies.len() < 2 {
            continue; // fully covered by Tier-1
        }
        let rep = canonical(members, group);
        let peers: Vec<DupEntity> = group
            .iter()
            .filter(|&&j| j != rep && members[j].body_hash != members[rep].body_hash)
            .map(|&j| dup_entity(&members[j]))
            .collect();
        if peers.is_empty() {
            continue;
        }
        groups.push(StructuralGroup { canonical: dup_entity(&members[rep]), peers });
    }
    // HashMap iteration order is random per process; sort by canonical
    // location so output order — and which groups survive the judge-pair
    // cap in `tier3::structural_candidates` — is deterministic (D4).
    groups.sort_by(|a, b| {
        (a.canonical.file.as_str(), a.canonical.lines.0)
            .cmp(&(b.canonical.file.as_str(), b.canonical.lines.0))
    });
    groups
}

/// Tier-2 shape-duplicate groups (same control flow, different names and
/// literals), independent of finding output — `check::tier3_findings` uses
/// this to ask the LLM judge whether each group is a real duplicate worth
/// unifying or a coincidental shape match (see the comment on
/// `duplicate-structural` below for why that call can't be made on tokens
/// alone).
pub fn structural_duplicate_groups(built: &BuiltGraph, facts: &[crate::source::FileFacts]) -> Vec<StructuralGroup> {
    shape_groups(&collect_members(built, facts))
}

/// Tier-1 (exact) and Tier-2 (structural) duplication, complexity spikes,
/// and over-commenting — the parser-backed deterministic detectors.
pub fn source_detectors(built: &BuiltGraph, facts: &[crate::source::FileFacts]) -> Vec<Finding> {
    use std::collections::HashMap;
    let index = crate::source::location_index(built);
    let mut findings = Vec::new();

    for ff in facts {
        for fact in &ff.functions {
            let entity_label = label(built, &index, &ff.file, fact);
            let lines = (fact.start_line as usize, fact.end_line as usize);

            if fact.complexity > COMPLEXITY_THRESHOLD {
                findings.push(Finding {
                    rule: "complexity-spike",
                    severity: Severity::Warning,
                    entity: entity_label.clone(),
                    file: ff.file.clone(),
                    lines,
                    message: format!(
                        "`{}` has cyclomatic complexity {} (threshold {COMPLEXITY_THRESHOLD})",
                        fact.name, fact.complexity
                    ),
                    fix_guidance: "Split conditional branches into smaller functions".into(),
                });
            }

            if fact.comment_lines >= 4 && fact.comment_lines * 2 >= fact.code_lines.max(1) {
                findings.push(Finding {
                    rule: "over-commenting",
                    severity: Severity::Advisory,
                    entity: entity_label.clone(),
                    file: ff.file.clone(),
                    lines,
                    message: format!(
                        "`{}` has {} comment lines against {} code lines",
                        fact.name, fact.comment_lines, fact.code_lines
                    ),
                    fix_guidance:
                        "Delete comments that restate the code; keep only constraints the code cannot express"
                            .into(),
                });
            }
        }
    }

    let members = collect_members(built, facts);

    // Tier-1: identical bodies. One finding per exact-duplicate set.
    let mut by_body: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, m) in members.iter().enumerate() {
        by_body.entry(m.body_hash.as_str()).or_default().push(i);
    }
    for group in by_body.values().filter(|g| g.len() > 1) {
        let rep = canonical(&members, group);
        let others: Vec<&str> = group
            .iter()
            .filter(|&&j| j != rep)
            .map(|&j| members[j].label.as_str())
            .collect();
        findings.push(Finding {
            rule: "duplicate-exact",
            severity: Severity::Warning,
            entity: members[rep].label.clone(),
            file: members[rep].file.clone(),
            lines: members[rep].lines,
            message: format!(
                "`{}` has a body identical to `{}` (modulo comments/whitespace)",
                members[rep].label,
                others.join("`, `")
            ),
            fix_guidance: format!("Keep one implementation and delete or delegate the rest: `{}`", others.join("`, `")),
        });
    }

    // Tier-2: same structure, different tokens. One finding per group (see
    // comment above `canonical`).
    //
    // Whether a shape match is worth unifying is a semantic question this
    // detector can't answer on tokens alone (see slop-check dogfooding on
    // vigil: `get_case`/`get_finding`-style thin REST wrappers converge on
    // identical control flow by construction, not because anyone
    // copy-pasted logic worth extracting — while a renamed-copy-paste like
    // this crate's own `clean_rows`/`scale_rows` toy fixture is exactly the
    // real duplicate this tier exists to catch, even though it *also*
    // renames every local variable). Confidence lives in Tier-3's LLM
    // judge, wired in by `check::tier3_findings`, which corroborates or
    // demotes these when `--tier3` is on.
    for group in shape_groups(&members) {
        let peer_labels: Vec<&str> = group.peers.iter().map(|p| p.entity.as_str()).collect();
        findings.push(Finding {
            rule: "duplicate-structural",
            // Advisory by default: a shape match is a *candidate*, not a
            // confirmed duplicate — thin per-resource wrappers collide on
            // control flow without being copy-paste (dogfooding vigil:
            // `calculate_case_metrics` ~ `get_template`). `--tier3` promotes
            // the ones its judge confirms to Warning (see check::run).
            severity: Severity::Advisory,
            entity: group.canonical.entity.clone(),
            file: group.canonical.file.clone(),
            lines: group.canonical.lines,
            message: format!(
                "`{}` is structurally identical to `{}` (same shape, renamed variables/literals) — candidate duplicate; confirm with --tier3",
                group.canonical.entity,
                peer_labels.join("`, `")
            ),
            fix_guidance: format!(
                "Unify with `{}` behind one parameterized implementation",
                peer_labels.join("`, `")
            ),
        });
    }

    findings
}

/// All detectors, sorted by severity then location.
pub fn run_all(
    built: &BuiltGraph,
    policy: &Policy,
    facts: &[crate::source::FileFacts],
) -> Vec<Finding> {
    let index = crate::source::location_index(built);
    let decorated: std::collections::HashSet<String> = facts
        .iter()
        .flat_map(|ff| {
            ff.functions
                .iter()
                .filter(|f| f.decorated)
                .map(|f| label(built, &index, &ff.file, f))
        })
        .collect();

    let mut findings = infra_bypass(built, policy);
    findings.extend(circular_import(built));
    findings.extend(dead_island(built, policy, &decorated));
    findings.extend(purity_lie(built));
    findings.extend(source_detectors(built, facts));
    findings.extend(crate::naming::naming_convention(built));
    findings.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.lines.0.cmp(&b.lines.0))
            .then_with(|| a.rule.cmp(b.rule))
    });
    findings
}
