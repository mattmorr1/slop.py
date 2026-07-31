//! Detectors. Deterministic set per §3.4: infra-bypass (flagship),
//! circular imports, dead islands, purity-lies.

use petgraph::visit::EdgeRef;
use petgraph::Direction;
use slop_graph::{CodeEntity, EdgeKind, Effect, NodeType};

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

/// Is this Imports edge just Rust's module hierarchy? `mod child;` in the parent
/// is a *containment* declaration, and `crate::`/`super::` paths in the child
/// point back up through it, so a module and its own ancestor reference each
/// other by construction — for every module in every crate. That is not an
/// import cycle.
///
/// Deliberately Rust-only: in Python, `pkg/__init__` importing `pkg.sub` while
/// `pkg.sub` imports from `pkg` is a genuine cycle that can fail at import time.
/// Filtering at the edge keeps real Rust cycles (between *sibling* modules)
/// visible instead of suppressing the whole SCC.
fn rust_module_hierarchy(a: &CodeEntity, b: &CodeEntity) -> bool {
    fn is_ancestor(ancestor: &str, descendant: &str) -> bool {
        descendant.len() > ancestor.len()
            && descendant.starts_with(ancestor)
            && descendant[ancestor.len()..].starts_with("::")
    }
    a.file.ends_with(".rs")
        && b.file.ends_with(".rs")
        && (is_ancestor(&a.id, &b.id) || is_ancestor(&b.id, &a.id))
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
        if rust_module_hierarchy(graph.entity(a), graph.entity(b)) {
            continue;
        }
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

    // Trait implementations are reached through the trait, never by name, so
    // they carry no by-name reference to find. Needs the SCIP symbol, which the
    // entity doesn't keep, so invert the symbol map once.
    let trait_impls: std::collections::HashSet<petgraph::graph::NodeIndex> = built
        .by_symbol
        .iter()
        .filter(|(symbol, _)| crate::entity_id::is_trait_impl_method(symbol))
        .map(|(_, &idx)| idx)
        .collect();

    let mut findings = Vec::new();
    for (idx, entity) in graph.entities() {
        if entity.entity_type != NodeType::Function {
            continue;
        }
        // Test doubles/mocks/fixtures are called dynamically by the test
        // framework, invisible to SCIP — `dead-island` here is noise. Matched by
        // path *and* by entity, since Rust and Go keep tests inside the source
        // file under `mod tests`.
        if crate::source::is_test_file(&entity.file) || crate::source::is_test_entity(&entity.id) {
            continue;
        }
        if trait_impls.contains(&idx) {
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

/// Matched against the snake_case-normalized name, so `calculateTotal` in a
/// JS/TS codebase is caught the same as `calculate_total`.
const PURE_NAME_PREFIXES: &[&str] = &[
    "calculate_", "compute_", "parse_", "format_", "validate_", "normalize_", "convert_",
    "is_", "to_", "as_",
];
pub(crate) const IO_EFFECTS: &[Effect] = &[
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
        // A test named `validate_change_returns_...` is describing what it
        // asserts, not promising purity — and reading fixtures is its job.
        if crate::source::is_test_file(&entity.file) || crate::source::is_test_entity(&entity.id) {
            continue;
        }
        let name = entity.id.rsplit("::").next().unwrap_or(&entity.id);
        let name = crate::rename::to_snake_case(name);
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
        .unwrap_or_else(|| format!("{}::{}", crate::source::module_label(file), fact.name))
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
                    format!("{}::{}", crate::source::module_label(&ff.file), fact.name)
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


/// Distinct callees a function needs before a shared neighborhood means
/// anything. Two functions that both call one logger are not parallel
/// implementations of each other.
const MIN_SHARED_CALLEES: usize = 3;

/// Shared callees that must be *distinctive* (below the frequency cutoff) for a
/// neighborhood match to count.
///
/// Set equality alone has poor precision, and dogfooding showed exactly why: the
/// false positives shared only generic vocabulary. `doc_uri` and `take_str` both
/// call `Option::map`/`as_str`/`get`; four `__init__`s all call the same torch
/// constructors. Sharing ubiquitous calls says nothing about doing the same job,
/// so a match needs callees that few other functions make.
const MIN_DISTINCTIVE_CALLEES: usize = 2;

/// A callee called by more than `functions / this` others is common vocabulary,
/// not a distinguishing feature — inverse document frequency, thresholded.
const DISTINCTIVE_DF_DIVISOR: usize = 20;

/// The set of things a function calls, as sorted entity IDs. External callees
/// (`std.fs.read_to_string`, `requests.post`) are included deliberately — they
/// are the strongest part of the signal, because they say what the function
/// *does* rather than who it collaborates with.
fn callee_set(built: &BuiltGraph, idx: petgraph::graph::NodeIndex) -> Vec<String> {
    let mut ids: Vec<String> = built
        .graph
        .graph
        .edges_directed(idx, Direction::Outgoing)
        .filter(|e| *e.weight() == EdgeKind::Calls)
        .map(|e| built.graph.entity(e.target()).id.clone())
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

/// Parallel implementation: functions that call the *same set of things* while
/// their bodies share neither an exact nor a structural hash — independently
/// written code doing one job twice.
///
/// This is duplication detected on **graph shape** rather than token shape, and
/// the two are near-disjoint. `duplicate-exact`/`duplicate-structural` catch
/// copy-paste and copy-paste-then-rename: the same text. This catches the case
/// they structurally cannot — two authors (or two agent sessions) solving the
/// same problem with different code, which is the redundancy an effect graph is
/// uniquely placed to see. Any pair whose shape already matches is left to those
/// rules rather than reported twice.
///
/// Set-based, not sequence-based: `Calls` edges carry no call order (they are
/// deduped on insert), so claiming "same calls in the same order" would
/// overstate what the graph knows.
pub fn parallel_implementation(built: &BuiltGraph, facts: &[crate::source::FileFacts]) -> Vec<Finding> {
    use std::collections::HashMap;

    let members = collect_members(built, facts);
    let by_label: HashMap<&str, usize> = members
        .iter()
        .enumerate()
        .map(|(i, m)| (m.label.as_str(), i))
        .collect();

    // Per candidate function: its member row and what it calls. Collected before
    // grouping so `df` below counts *functions*, not neighborhoods — several
    // functions sharing one neighborhood must each contribute.
    let mut candidates: Vec<(usize, petgraph::graph::NodeIndex, Vec<String>)> = Vec::new();
    let mut df: HashMap<String, usize> = HashMap::new();
    let mut callers = 0usize;
    for (idx, entity) in built.graph.entities() {
        if entity.entity_type != NodeType::Function
            || crate::source::is_test_file(&entity.file)
            || crate::source::is_test_entity(&entity.id)
        {
            continue;
        }
        let callees = callee_set(built, idx);
        if !callees.is_empty() {
            callers += 1;
        }
        // Every function contributes to callee frequency, including the ones too
        // small to be candidates themselves.
        for callee in &callees {
            *df.entry(callee.clone()).or_default() += 1;
        }
        let Some(&member) = by_label.get(entity.id.as_str()) else {
            continue; // no parser facts => no hashes to compare
        };
        if callees.len() >= MIN_SHARED_CALLEES {
            candidates.push((member, idx, callees));
        }
    }
    // A callee this many callers share is common vocabulary, not a feature —
    // a fraction of the calling functions, not of the distinct callees.
    let common_df = (callers / DISTINCTIVE_DF_DIVISOR).max(MIN_DISTINCTIVE_CALLEES);

    let mut by_neighborhood: HashMap<String, Vec<(usize, petgraph::graph::NodeIndex)>> =
        HashMap::new();
    for (member, idx, callees) in &candidates {
        by_neighborhood
            .entry(callees.join("\u{1}"))
            .or_default()
            .push((*member, *idx));
    }

    let mut findings = Vec::new();
    for (key, entries) in &by_neighborhood {
        let group: Vec<usize> = entries.iter().map(|&(m, _)| m).collect();
        let group = group.as_slice();
        // A neighborhood shared by many functions is a codebase pattern by
        // design (every handler calls the same four things), same reasoning as
        // `duplicate-structural`'s convention families.
        if group.len() < 2 || group.len() > CONVENTION_FAMILY_MAX {
            continue;
        }
        let distinctive = key
            .split('\u{1}')
            .filter(|c| df.get(*c).is_some_and(|&n| n <= common_df))
            .count();
        if distinctive < MIN_DISTINCTIVE_CALLEES {
            continue;
        }
        // Sibling methods of one type sharing a callee set is a dispatch table
        // (`Filter::next` and `Filter::label` both match over the same variants)
        // — parallel *roles* of that type, deliberately, not one job done twice.
        // Read from the id prefix, not the `Contains` edge: for Rust methods that
        // edge points at the enclosing *module* rather than the type.
        let owning_type = |m: &Member| {
            let prefix = m.label.rsplit_once("::")?.0;
            built
                .graph
                .node(prefix)
                .filter(|&c| built.graph.entity(c).entity_type == NodeType::Class)
                .map(|_| prefix.to_string())
        };
        let owners: Vec<Option<String>> = group.iter().map(|&i| owning_type(&members[i])).collect();
        if owners[0].is_some() && owners.iter().all(|o| *o == owners[0]) {
            continue;
        }
        // One shared *name* across the group is a role, not a coincidence:
        // every `__init__` builds a thing, every `_check_x` checks one. That is
        // the codebase's vocabulary, and unifying them is not the ask.
        let simple = |m: &Member| m.label.rsplit("::").next().unwrap_or(&m.label).to_string();
        let names: std::collections::HashSet<String> =
            group.iter().map(|&i| simple(&members[i])).collect();
        if names.len() < group.len() {
            continue;
        }
        let rep = canonical(&members, group);
        let peers: Vec<&Member> = group
            .iter()
            .map(|&i| &members[i])
            .filter(|m| {
                m.label != members[rep].label
                    && m.body_hash != members[rep].body_hash
                    && m.structural_hash != members[rep].structural_hash
            })
            .collect();
        if peers.is_empty() {
            continue;
        }
        let shared = key.split('\u{1}').count();
        let names = peers.iter().map(|m| format!("`{}`", m.label)).collect::<Vec<_>>().join(", ");
        let rep_m = &members[rep];
        findings.push(Finding {
            rule: "parallel-implementation",
            // Advisory: a shared neighborhood is strong evidence of one job done
            // twice, but whether they *should* be unified is a judgement about
            // intent. `--tier3` is the promotion path, as with duplicate-structural.
            severity: Severity::Advisory,
            entity: rep_m.label.clone(),
            file: rep_m.file.clone(),
            lines: rep_m.lines,
            message: format!(
                "`{}` and {names} call the same {shared} things but share no code — likely the same job implemented more than once",
                rep_m.label
            ),
            fix_guidance: format!(
                "Compare `{}` with {names}: if they serve one purpose, keep the clearest and route the others to it",
                rep_m.label
            ),
        });
    }
    findings.sort_by(|a, b| a.file.cmp(&b.file).then(a.lines.0.cmp(&b.lines.0)));
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
    // Functions with a branch to get wrong, for `untested-effect`.
    let branching: std::collections::HashSet<String> = facts
        .iter()
        .flat_map(|ff| {
            ff.functions
                .iter()
                .filter(|f| f.branch_points >= crate::coverage::MIN_BRANCH_POINTS)
                .map(|f| label(built, &index, &ff.file, f))
        })
        .collect();

    let mut findings = infra_bypass(built, policy);
    findings.extend(circular_import(built));
    findings.extend(dead_island(built, policy, &decorated));
    findings.extend(purity_lie(built));
    findings.extend(effect_layer_violation(built, policy));
    findings.extend(source_detectors(built, facts));
    findings.extend(parallel_implementation(built, facts));
    findings.extend(crate::coverage::untested_effect(built, policy, &branching, &decorated));
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

    /// Two functions in different modules, calling the same distinctive things,
    /// with bodies that share neither hash: one job implemented twice.
    fn parallel_fixture(a_body: &str, b_body: &str) -> (BuiltGraph, Vec<crate::source::FileFacts>) {
        let facts = vec![
            py_facts("a.py", &format!("def make_report(rows):\n{a_body}")),
            py_facts("b.py", &format!("def build_summary(rows):\n{b_body}")),
        ];
        let mut built = built_with(vec![
            func("a::make_report", &[]),
            func("b::build_summary", &[]),
            func("shared::fetch_rows", &[]),
            func("shared::render_table", &[]),
            func("shared::write_out", &[]),
        ]);
        for caller in ["a::make_report", "b::build_summary"] {
            let from = built.graph.node(caller).unwrap();
            for callee in ["shared::fetch_rows", "shared::render_table", "shared::write_out"] {
                let to = built.graph.node(callee).unwrap();
                built.graph.add_edge(from, to, EdgeKind::Calls);
            }
        }
        (built, facts)
    }

    // Both bodies must clear the parser's significance floor, or they get no
    // hashes and never become members at all.
    const BODY_A: &str = "    out = []\n    for r in rows:\n        if r is not None:\n            out.append(str(r).strip())\n        else:\n            out.append(\"\")\n    return sorted(out)\n";
    const BODY_B: &str = "    total = 0\n    seen = {}\n    while total < len(rows):\n        seen[total] = rows[total] * 2\n        total += 1\n    return (seen, total)\n";

    #[test]
    fn same_callees_different_code_is_a_parallel_implementation() {
        let (built, facts) = parallel_fixture(BODY_A, BODY_B);
        let found = parallel_implementation(&built, &facts);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].rule, "parallel-implementation");
        assert!(found[0].message.contains("build_summary"), "{}", found[0].message);
    }

    #[test]
    fn an_identical_shape_is_left_to_the_duplicate_rules() {
        // Same body => duplicate-exact already reports it; reporting here too
        // would double-count one fact.
        let (built, facts) = parallel_fixture(BODY_A, BODY_A);
        assert!(parallel_implementation(&built, &facts).is_empty());
    }

    #[test]
    fn sharing_only_common_vocabulary_is_not_a_match() {
        // Every function in the repo calls these three, so calling them says
        // nothing about doing the same job.
        let (mut built, facts) = parallel_fixture(BODY_A, BODY_B);
        for i in 0..40 {
            let id = format!("noise::f{i}");
            built.graph.add_entity(CodeEntity { ..func(&id, &[]) });
            let from = built.graph.node(&id).unwrap();
            for callee in ["shared::fetch_rows", "shared::render_table", "shared::write_out"] {
                let to = built.graph.node(callee).unwrap();
                built.graph.add_edge(from, to, EdgeKind::Calls);
            }
        }
        assert!(parallel_implementation(&built, &facts).is_empty());
    }

    #[test]
    fn one_shared_name_across_the_group_is_a_role_not_a_duplicate() {
        let facts = vec![
            py_facts("a.py", &format!("def render(rows):\n{BODY_A}")),
            py_facts("b.py", &format!("def render(rows):\n{BODY_B}")),
        ];
        let mut built = built_with(vec![
            func("a::render", &[]),
            func("b::render", &[]),
            func("shared::one", &[]),
            func("shared::two", &[]),
            func("shared::three", &[]),
        ]);
        for caller in ["a::render", "b::render"] {
            let from = built.graph.node(caller).unwrap();
            for callee in ["shared::one", "shared::two", "shared::three"] {
                let to = built.graph.node(callee).unwrap();
                built.graph.add_edge(from, to, EdgeKind::Calls);
            }
        }
        assert!(parallel_implementation(&built, &facts).is_empty());
    }

    #[test]
    fn sibling_methods_of_one_type_are_a_dispatch_table() {
        let facts = vec![py_facts(
            "a.py",
            &format!("def to_label(self):\n{BODY_A}\ndef to_code(self):\n{BODY_B}"),
        )];
        let mut built = built_with(vec![
            func("a::Filter::to_label", &[]),
            func("a::Filter::to_code", &[]),
            func("shared::one", &[]),
            func("shared::two", &[]),
            func("shared::three", &[]),
        ]);
        // The owning type must exist as a Class node for the rule to see it.
        built.graph.add_entity(CodeEntity {
            entity_type: NodeType::Class,
            ..func("a::Filter", &[])
        });
        for caller in ["a::Filter::to_label", "a::Filter::to_code"] {
            let from = built.graph.node(caller).unwrap();
            for callee in ["shared::one", "shared::two", "shared::three"] {
                let to = built.graph.node(callee).unwrap();
                built.graph.add_edge(from, to, EdgeKind::Calls);
            }
        }
        assert!(parallel_implementation(&built, &facts).is_empty());
    }

}
