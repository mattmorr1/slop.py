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
        // Test files: doubles/mocks/fixtures are called dynamically by the test
        // framework, invisible to SCIP — `dead-island` here is noise.
        if crate::source::is_test_file(&entity.file) {
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
        // Referenced anywhere in the repo — a Calls edge, or any other
        // reference SCIP recorded (passed to `Depends(...)`, used as a
        // decorator, exported in `__all__`, imported). The latter catches
        // framework-registered functions that have no in-function call site.
        let referenced = built.referenced.contains(&idx)
            || graph
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

/// Effect-layer violation (D2, §3.4): an entity in a policy-declared layer
/// *directly* acquires an effect that layer forbids — the generalization of
/// "no DB in the presentation layer". Keyed on direct acquisition (`HasEffect`
/// edges to raw seeds), not transitive effects, so a controller that reaches
/// the DB *through* the service layer isn't flagged — only one that does the
/// I/O itself. Silent without a `[[layer]]` policy (D8).
pub fn effect_layer_violation(built: &BuiltGraph, policy: &Policy) -> Vec<Finding> {
    if policy.layers.is_empty() {
        return Vec::new();
    }
    let graph = &built.graph;
    let mut findings = Vec::new();
    for (idx, entity) in graph.entities() {
        if !matches!(entity.entity_type, NodeType::Function | NodeType::Class) {
            continue;
        }
        // Effects this entity acquires *directly* (raw seed references).
        let mut direct: Vec<Effect> = Vec::new();
        for edge in graph.graph.edges_directed(idx, Direction::Outgoing) {
            if *edge.weight() == EdgeKind::HasEffect {
                for &e in &graph.entity(edge.target()).effect_signature.0 {
                    if !direct.contains(&e) {
                        direct.push(e);
                    }
                }
            }
        }
        if direct.is_empty() {
            continue;
        }
        let dotted = entity.id.replace("::", ".");
        for layer in &policy.layers {
            if !layer.contains(&dotted) {
                continue;
            }
            let forbidden = layer.forbidden_effects();
            let mut violated: Vec<Effect> =
                direct.iter().copied().filter(|e| forbidden.contains(e)).collect();
            if violated.is_empty() {
                continue;
            }
            violated.sort();
            findings.push(Finding {
                rule: "effect-layer-violation",
                severity: Severity::Warning,
                entity: entity.id.clone(),
                file: entity.file.clone(),
                lines: entity.source_range,
                message: format!(
                    "`{}` is in the `{}` layer but directly performs {:?} I/O",
                    entity.id, layer.name, violated
                ),
                fix_guidance: format!(
                    "Move the {:?} operation into a lower layer (a service/repository) and call it from `{}`",
                    violated, entity.id
                ),
            });
        }
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

/// Effect-creep (D2, §3.4): a function that was **pure** at baseline now has
/// an I/O effect signature — a purity *regression*, distinct from purity-lie
/// (which is name-based and baseline-free). This is the "delta vs baseline"
/// blocker: it fires only against a `slop baseline` that recorded effects, and
/// only on entities that baseline knew as pure (empty signature). New
/// functions (absent from the baseline) are left to the other rules.
pub fn effect_creep(built: &BuiltGraph, baseline: &crate::baseline::Baseline) -> Vec<Finding> {
    if baseline.effects.is_empty() {
        return Vec::new(); // no effect baseline captured — silent
    }
    let graph = &built.graph;
    let mut findings = Vec::new();
    for (_, entity) in graph.entities() {
        if entity.entity_type != NodeType::Function {
            continue;
        }
        let Some(prior) = baseline.effects.get(&entity.id) else {
            continue; // not in the baseline: new code, not a regression
        };
        if !prior.is_empty() {
            continue; // wasn't pure at baseline — nothing to regress from
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
            rule: "effect-creep",
            severity: Severity::Blocking,
            entity: entity.id.clone(),
            file: entity.file.clone(),
            lines: entity.source_range,
            message: format!(
                "`{}` was pure at baseline but now performs {io:?} I/O",
                entity.id
            ),
            fix_guidance: format!(
                "Restore `{}`'s purity — push the {io:?} out to a caller or an injected dependency; re-baseline only if the effect is intended",
                entity.id
            ),
        });
    }
    findings
}

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

// Cyclomatic complexity (McCabe, incl. boolean operators) is the entry gate,
// but on its own it flags a wall of barely-over-threshold functions whose
// score comes from one fat boolean guard, not real tangle (368 Warnings on
// vigil, median 15). An agent can't drive a useful refactor from those. So a
// spike must *also* show genuine structure: deep nesting or many independent
// control-flow branches. This keeps the flagged set the ones worth splitting.
const COMPLEXITY_THRESHOLD: u32 = 10;
const NESTING_THRESHOLD: u32 = 4;
const BRANCH_THRESHOLD: u32 = 12;
/// A structural shape shared by more than this many functions is a codebase
/// convention (plugin/registry/handler boilerplate — every entry has the same
/// control flow *by design*), not a copy-paste worth unifying. Flagging it just
/// points at N siblings nobody will merge, so `duplicate-structural` skips such
/// families (dogfooding vigil: ~20 MCP tools share one `handle_list_tools`
/// shape).
const CONVENTION_FAMILY_MAX: usize = 8;

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
    for group in by_shape
        .values()
        .filter(|g| g.len() > 1 && g.len() <= CONVENTION_FAMILY_MAX)
    {
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
    let members = dedup_members(built, facts);
    shape_groups(&members)
}

/// Members eligible for duplication analysis: exclude test files (parameterized
/// test cases and shared setup converge on the same shape/body by design — not
/// slop worth unifying).
fn dedup_members(built: &BuiltGraph, facts: &[crate::source::FileFacts]) -> Vec<Member> {
    collect_members(built, facts)
        .into_iter()
        .filter(|m| !crate::source::is_test_file(&m.file))
        .collect()
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

            let tangled = fact.max_nesting_depth >= NESTING_THRESHOLD
                || fact.branch_points >= BRANCH_THRESHOLD;
            if fact.complexity > COMPLEXITY_THRESHOLD && tangled {
                // Anchor guidance on the concrete locus: the deepest-nested
                // block (1-based) is what to lift out first.
                let deep_line = fact.deepest_line as usize + 1;
                findings.push(Finding {
                    rule: "complexity-spike",
                    severity: Severity::Warning,
                    entity: entity_label.clone(),
                    file: ff.file.clone(),
                    lines,
                    message: format!(
                        "`{}` is tangled: {} control-flow branches nested {} deep (cyclomatic {})",
                        fact.name, fact.branch_points, fact.max_nesting_depth, fact.complexity
                    ),
                    fix_guidance: if fact.max_nesting_depth >= NESTING_THRESHOLD {
                        format!(
                            "Extract the deepest block (around line {deep_line}, nested {} deep) into a named helper, or flatten it with early-return guard clauses",
                            fact.max_nesting_depth
                        )
                    } else {
                        format!(
                            "Split the {} branches into smaller functions — group the ones that share a purpose behind one call",
                            fact.branch_points
                        )
                    },
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

    let members = dedup_members(built, facts);

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

/// A `trivial-wrapper` candidate: a one-statement forwarder that survived the
/// structural exclusions and now needs the semantic judge to separate a name
/// that earns its keep from slop. Never a finding on its own — no Warning
/// without tier-3 confirmation (D8).
pub struct WrapperCandidate {
    pub entity: String,
    pub name: String,
    pub file: String,
    pub lines: (usize, usize),
    pub signature: String,
    /// The callee the body delegates to, as written.
    pub forward_target: String,
    /// Positional pass-through with a bare callee — the rename-safe inline case.
    pub forward_identity: bool,
    /// Distinct functions with a resolved `Calls` edge to this one. An
    /// over-count: SCIP records a callback pass (`register(load)`) as a call
    /// too, so this bounds the real caller set from above — safe for the gate,
    /// but the rewrite (fix::plan_inlines) must re-check each textual site.
    pub callers: usize,
}

const MAX_WRAPPER_CALLERS: usize = 2;

fn is_dunder(name: &str) -> bool {
    name.len() > 4 && name.starts_with("__") && name.ends_with("__")
}

/// Structural pre-filter for `trivial-wrapper`: one-statement forwarders with a
/// small, resolved caller set, minus every shape where a thin body is
/// intentional — protocol/dunder methods and any method (SCIP under-resolves
/// dynamic dispatch; a thin override is contract, not slop), decorated /
/// framework-registered functions, declared entry points, and tests. Output is
/// candidates for the semantic judge, never findings.
pub fn trivial_wrapper_candidates(
    built: &BuiltGraph,
    policy: &Policy,
    facts: &[crate::source::FileFacts],
) -> Vec<WrapperCandidate> {
    let graph = &built.graph;
    let index = crate::source::location_index(built);
    let mut out = Vec::new();
    for ff in facts {
        if crate::source::is_test_file(&ff.file) {
            continue;
        }
        for fact in &ff.functions {
            let Some(target) = &fact.forward_target else {
                continue;
            };
            if fact.decorated || is_dunder(&fact.name) {
                continue;
            }
            let Some(entity) = crate::source::entity_for(built, &index, &ff.file, fact) else {
                continue;
            };
            if policy.is_entry_point(&entity.id) {
                continue;
            }
            let Some(idx) = graph.node(&entity.id) else {
                continue;
            };
            let is_method = graph.graph.edges_directed(idx, Direction::Incoming).any(|e| {
                *e.weight() == EdgeKind::Contains
                    && graph.entity(e.source()).entity_type == NodeType::Class
            });
            if is_method {
                continue;
            }
            let callers = graph
                .graph
                .edges_directed(idx, Direction::Incoming)
                .filter(|e| *e.weight() == EdgeKind::Calls)
                .count();
            if callers == 0 || callers > MAX_WRAPPER_CALLERS {
                continue;
            }
            out.push(WrapperCandidate {
                entity: entity.id.clone(),
                name: fact.name.clone(),
                file: ff.file.clone(),
                lines: (fact.start_line as usize, fact.end_line as usize),
                signature: fact.signature.clone(),
                forward_target: target.clone(),
                forward_identity: fact.forward_identity,
                callers,
            });
        }
    }
    out
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
    findings.extend(effect_layer_violation(built, policy));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::baseline::Baseline;
    use slop_graph::{CodeEntity, CodeGraph, EffectSet};
    use std::collections::{BTreeMap, HashMap};

    fn func(id: &str, effects: &[Effect]) -> CodeEntity {
        let mut sig = EffectSet::pure();
        for &e in effects {
            sig.insert(e);
        }
        CodeEntity {
            id: id.into(),
            entity_type: NodeType::Function,
            name: id.rsplit("::").next().unwrap().into(),
            signature: String::new(),
            docstring: None,
            file: "m.py".into(),
            source_range: (0, 1),
            body_hash: String::new(),
            effect_signature: sig,
        }
    }

    fn built_with(entities: Vec<CodeEntity>) -> BuiltGraph {
        let mut graph = CodeGraph::new();
        for e in entities {
            graph.add_entity(e);
        }
        BuiltGraph { graph, by_symbol: HashMap::new(), referenced: Default::default() }
    }

    fn baseline_effects(pairs: &[(&str, &[&str])]) -> Baseline {
        let mut effects = BTreeMap::new();
        for (id, es) in pairs {
            effects.insert(id.to_string(), es.iter().map(|s| s.to_string()).collect());
        }
        Baseline { version: 1, findings: vec![], effects }
    }

    fn member(label: &str, shape: &str, body: &str) -> Member {
        Member {
            label: label.into(),
            file: "m.py".into(),
            lines: (0, 1),
            body_hash: body.into(),
            structural_hash: shape.into(),
            docstring: None,
        }
    }

    #[test]
    fn referenced_anywhere_spares_dead_island() {
        // Two free functions, neither with a Calls edge; one is marked
        // referenced (as a module-level use — Depends/decorator/__all__ would
        // land here). Only the truly-unreferenced one is dead.
        let mut built = built_with(vec![func("m::used", &[]), func("m::dead", &[])]);
        let used = built.graph.node("m::used").expect("node");
        built.referenced.insert(used);
        let findings = dead_island(&built, &Policy::default(), &Default::default());
        let dead: Vec<&str> = findings
            .iter()
            .filter(|f| f.rule == "dead-island")
            .map(|f| f.entity.as_str())
            .collect();
        assert!(dead.contains(&"m::dead"), "unreferenced fn should be dead");
        assert!(!dead.contains(&"m::used"), "referenced fn must be spared");
    }

    fn func_at(id: &str, line: usize) -> CodeEntity {
        let mut e = func(id, &[]);
        e.source_range = (line, line + 1);
        e
    }

    fn py_facts(file: &str, src: &str) -> crate::source::FileFacts {
        crate::source::FileFacts {
            file: file.into(),
            functions: slop_parse::Language::Python.parse(src).unwrap(),
        }
    }

    #[test]
    fn identity_forwarder_with_a_caller_is_a_candidate() {
        let facts = vec![py_facts("m.py", "def load(path):\n    return read_file(path)\n")];
        let mut built = built_with(vec![func_at("m::load", 0), func_at("m::caller", 10)]);
        let (load, caller) =
            (built.graph.node("m::load").unwrap(), built.graph.node("m::caller").unwrap());
        built.graph.add_edge(caller, load, EdgeKind::Calls);
        let cands = trivial_wrapper_candidates(&built, &Policy::default(), &facts);
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].entity, "m::load");
        assert_eq!(cands[0].forward_target, "read_file");
        assert!(cands[0].forward_identity);
        assert_eq!(cands[0].callers, 1);
    }

    #[test]
    fn forwarder_with_no_resolved_caller_is_dead_island_not_a_wrapper() {
        let facts = vec![py_facts("m.py", "def load(path):\n    return read_file(path)\n")];
        let built = built_with(vec![func_at("m::load", 0)]);
        assert!(trivial_wrapper_candidates(&built, &Policy::default(), &facts).is_empty());
    }

    #[test]
    fn forwarder_with_three_callers_is_over_threshold() {
        let facts = vec![py_facts("m.py", "def load(path):\n    return read_file(path)\n")];
        let mut built = built_with(vec![
            func_at("m::load", 0),
            func_at("m::a", 10),
            func_at("m::b", 20),
            func_at("m::c", 30),
        ]);
        let load = built.graph.node("m::load").unwrap();
        for c in ["m::a", "m::b", "m::c"] {
            let idx = built.graph.node(c).unwrap();
            built.graph.add_edge(idx, load, EdgeKind::Calls);
        }
        assert!(trivial_wrapper_candidates(&built, &Policy::default(), &facts).is_empty());
    }

    #[test]
    fn thin_method_is_excluded() {
        let facts = vec![py_facts("m.py", "class C:\n    def wrap(self, x):\n        return g(x)\n")];
        let mut built = built_with(vec![func_at("m::C::wrap", 1), func_at("m::caller", 10)]);
        let cls = built.graph.add_entity(CodeEntity {
            entity_type: NodeType::Class,
            ..func_at("m::C", 0)
        });
        let (wrap, caller) =
            (built.graph.node("m::C::wrap").unwrap(), built.graph.node("m::caller").unwrap());
        built.graph.add_edge(cls, wrap, EdgeKind::Contains);
        built.graph.add_edge(caller, wrap, EdgeKind::Calls);
        assert!(trivial_wrapper_candidates(&built, &Policy::default(), &facts).is_empty());
    }

    #[test]
    fn large_shape_family_is_a_convention_not_a_duplicate() {
        let mut members = Vec::new();
        // A shape shared by 9 functions (> CONVENTION_FAMILY_MAX) — boilerplate.
        for i in 0..9 {
            members.push(member(&format!("big::f{i}"), "S", &format!("b{i}")));
        }
        // A shape shared by 3 — a real candidate.
        for i in 0..3 {
            members.push(member(&format!("small::g{i}"), "T", &format!("c{i}")));
        }
        let groups = shape_groups(&members);
        assert_eq!(groups.len(), 1, "large family should be suppressed");
        assert!(groups[0].canonical.entity.starts_with("small::"));
    }

    #[test]
    fn effect_creep_fires_only_on_pure_to_io_regression() {
        let built = built_with(vec![
            func("m::was_pure", &[Effect::Net]),   // regressed
            func("m::was_networked", &[Effect::Net]), // already effectful
            func("m::new_fn", &[Effect::Db]),      // not in baseline
            func("m::still_pure", &[]),            // pure, stays pure
        ]);
        let baseline = baseline_effects(&[
            ("m::was_pure", &[]),
            ("m::was_networked", &["Net"]),
            ("m::still_pure", &[]),
        ]);
        let f = effect_creep(&built, &baseline);
        assert_eq!(f.len(), 1, "{f:#?}");
        assert_eq!(f[0].entity, "m::was_pure");
        assert_eq!(f[0].severity, Severity::Blocking);
    }

    #[test]
    fn effect_creep_silent_without_effect_baseline() {
        let built = built_with(vec![func("m::f", &[Effect::Net])]);
        assert!(effect_creep(&built, &Baseline::default()).is_empty());
    }
}
