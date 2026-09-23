//! Static test coverage of *effectful* code: which I/O-performing functions no
//! test reaches, over the call graph rather than by executing anything.
//!
//! The graph already holds test-to-implementation `Calls` edges — test entities
//! were only ever *filtered out* of other detectors, never used as a traversal
//! source. Reading them the other way answers a question that is specifically
//! an AI-slop signal: a model asked for a feature writes the feature, and
//! skips the test.
//!
//! Deliberately not a coverage tool. `pytest-cov`/`tarpaulin` measure execution
//! and are more accurate at it. Two things this does instead: it needs no test
//! run (so it works at write time, where you cannot execute a suite), and it is
//! *effect-weighted* — an untested function that talks to the network is a
//! finding, an untested pure helper is not, where line-coverage treats both the
//! same.
//!
//! Codebase-relative, like every other rule here (D8): a repo that doesn't test
//! its I/O has no norm to deviate from, so the rule stays silent rather than
//! reporting every function in it.
//!
//! It also wants *logic*, not wiring. Dogfooding this rule on slop itself, the
//! findings split cleanly: setup delivery and `Baseline::save` are untested
//! behaviour, while `serve_stdio` and `run` are straight-line delegations whose
//! only effect is to hand off. A branchless effectful function has no path to
//! regress silently down, so the branch count from the parser (already computed
//! for `complexity-spike`) is what separates the two.

use std::collections::{HashSet, VecDeque};

use petgraph::graph::NodeIndex;
use petgraph::visit::EdgeRef;
use petgraph::Direction;
use slop_graph::{EdgeKind, Effect, NodeType};

use crate::build::BuiltGraph;
use crate::detect::IO_EFFECTS;
use crate::findings::{Finding, Severity};
use crate::policy::Policy;
use crate::source::{is_test_entity, is_test_file};

/// Below this many test functions, "tested" isn't this codebase's habit and the
/// rule would just enumerate the repo.
const MIN_TEST_FUNCTIONS: usize = 5;

/// Fraction of effectful functions a test must already reach before the
/// stragglers count as deviations. A repo at 20% is not *trying* to test its
/// I/O; telling it about the other 80% is noise, not a finding.
const MIN_COVERAGE: f64 = 0.5;

/// Branches a function needs before "untested" is worth saying. At zero it is a
/// straight line: it cannot take a wrong path, so a test asserts only that the
/// wiring is wired. Dogfooding bore this out — it dropped `Baseline::save` (three
/// statements) and setup delivery (whose merge decisions are separately tested)
/// while keeping every function with
/// real logic behind it.
pub const MIN_BRANCH_POINTS: u32 = 1;

/// Is this entity a test? Test files (pytest/jest conventions) and in-file test
/// modules (Rust's `mod tests`) both count.
fn is_test(entity: &slop_graph::CodeEntity) -> bool {
    is_test_file(&entity.file) || is_test_entity(&entity.id)
}

/// Everything reachable from `roots` by following `Calls` forward. Directed and
/// `Calls`-only on purpose: `Contains` would leak through a shared module and
/// make every sibling look tested.
fn reachable_from(built: &BuiltGraph, roots: Vec<NodeIndex>) -> HashSet<NodeIndex> {
    let graph = &built.graph.graph;
    let mut seen: HashSet<NodeIndex> = roots.iter().copied().collect();
    let mut queue: VecDeque<NodeIndex> = roots.into();
    while let Some(n) = queue.pop_front() {
        for e in graph.edges_directed(n, Direction::Outgoing) {
            if *e.weight() == EdgeKind::Calls && seen.insert(e.target()) {
                queue.push_back(e.target());
            }
        }
    }
    seen
}

/// Untested-effect: a function performing I/O that no test reaches.
///
/// `decorated` holds entity IDs carrying a decorator/attribute — framework
/// dispatch, so a test may exercise them without any resolvable call edge.
/// `branching` holds those with at least [`MIN_BRANCH_POINTS`] branches; both
/// are derived from parser facts by [`crate::detect::run_all`], which already
/// holds the location index they need.
pub fn untested_effect(
    built: &BuiltGraph,
    policy: &Policy,
    branching: &HashSet<String>,
    decorated: &HashSet<String>,
) -> Vec<Finding> {
    let graph = &built.graph;
    let mut test_roots = Vec::new();
    let mut candidates: Vec<(NodeIndex, Vec<Effect>)> = Vec::new();

    for (idx, entity) in graph.entities() {
        if entity.entity_type != NodeType::Function || entity.file.is_empty() {
            continue;
        }
        if is_test(entity) {
            test_roots.push(idx);
            continue;
        }
        if policy.is_entry_point(&entity.id) || decorated.contains(&entity.id) {
            continue;
        }
        let io: Vec<Effect> = IO_EFFECTS
            .iter()
            .copied()
            .filter(|&e| entity.effect_signature.contains(e))
            .collect();
        // No parser facts for the file (no parser for its language, or the
        // join missed) => no branch evidence => not judged.
        if !io.is_empty() && branching.contains(&entity.id) {
            candidates.push((idx, io));
        }
    }

    if test_roots.len() < MIN_TEST_FUNCTIONS || candidates.is_empty() {
        return Vec::new();
    }

    let covered = reachable_from(built, test_roots);
    let uncovered: Vec<&(NodeIndex, Vec<Effect>)> = candidates
        .iter()
        .filter(|(idx, _)| !covered.contains(idx))
        .collect();

    let coverage = 1.0 - (uncovered.len() as f64 / candidates.len() as f64);
    if coverage < MIN_COVERAGE {
        return Vec::new();
    }
    let percent = (coverage * 100.0).round() as u32;

    uncovered
        .iter()
        .map(|(idx, io)| {
            let entity = graph.entity(*idx);
            let effects = io.iter().map(|e| format!("{e:?}")).collect::<Vec<_>>().join(", ");
            Finding {
                rule: "untested-effect",
                // Advisory: SCIP can't see a test that reaches this through
                // dynamic dispatch, so an unreached function is evidence, not
                // proof. Promote only if measured precision earns it.
                severity: Severity::Advisory,
                entity: entity.id.clone(),
                file: entity.file.clone(),
                lines: entity.source_range,
                related: Vec::new(),
                message: format!(
                    "`{}` performs {} I/O and no test reaches it, in a codebase that tests {}% of its effectful functions",
                    entity.id, effects, percent
                ),
                fix_guidance: format!(
                    "Add a test that exercises `{}` — it is the {} boundary, where an untested regression is silent",
                    entity.id, effects
                ),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use slop_graph::{CodeEntity, CodeGraph, EffectSet};

    fn func(id: &str, file: &str, effects: &[Effect]) -> CodeEntity {
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
            file: file.into(),
            source_range: (0, 1),
            body_hash: String::new(),
            effect_signature: sig,
        }
    }

    /// `impls` = (id, effects); `calls` = (caller id, callee id).
    fn built_of(nodes: Vec<CodeEntity>, calls: &[(&str, &str)]) -> BuiltGraph {
        let mut graph = CodeGraph::new();
        for n in nodes {
            graph.add_entity(n);
        }
        for (from, to) in calls {
            let a = graph.node(from).expect("caller");
            let b = graph.node(to).expect("callee");
            graph.add_edge(a, b, EdgeKind::Calls);
        }
        BuiltGraph { graph, by_symbol: Default::default(), referenced: Default::default() }
    }

    /// Five tests, four of five effectful functions reached (80%) — the fifth
    /// is the finding.
    fn tested_codebase() -> BuiltGraph {
        let mut nodes = vec![func("app::lonely_writer", "app.py", &[Effect::FsWrite])];
        let mut calls = Vec::new();
        for i in 0..4 {
            nodes.push(func(&format!("app::fetch{i}"), "app.py", &[Effect::Net]));
            nodes.push(func(&format!("tests::test_fetch{i}"), "tests/t.py", &[]));
        }
        for i in 0..4 {
            calls.push((
                format!("tests::test_fetch{i}"),
                format!("app::fetch{i}"),
            ));
        }
        // A fifth test, so MIN_TEST_FUNCTIONS is met without covering more.
        nodes.push(func("tests::test_unrelated", "tests/t.py", &[]));
        let refs: Vec<(&str, &str)> = calls.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
        built_of(nodes, &refs)
    }

    /// Stand-in for "the parser found branches in all of these".
    fn all_branching(built: &BuiltGraph) -> HashSet<String> {
        built.graph.entities().map(|(_, e)| e.id.clone()).collect()
    }

    #[test]
    fn reports_only_the_effectful_function_no_test_reaches() {
        let built = tested_codebase();
        let found = untested_effect(&built, &Policy::default(), &all_branching(&built), &HashSet::new());
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].entity, "app::lonely_writer");
        assert!(found[0].message.contains("80%"), "{}", found[0].message);
    }

    #[test]
    fn transitive_reach_counts_as_tested() {
        let built = built_of(
            vec![
                func("app::handler", "app.py", &[]),
                func("app::writer", "app.py", &[Effect::FsWrite]),
                func("tests::test_a", "tests/t.py", &[]),
                func("tests::test_b", "tests/t.py", &[]),
                func("tests::test_c", "tests/t.py", &[]),
                func("tests::test_d", "tests/t.py", &[]),
                func("tests::test_e", "tests/t.py", &[]),
            ],
            // The test calls a pure handler, which calls the writer.
            &[("tests::test_a", "app::handler"), ("app::handler", "app::writer")],
        );
        assert!(untested_effect(&built, &Policy::default(), &all_branching(&built), &HashSet::new()).is_empty());
    }

    #[test]
    fn an_untested_codebase_has_no_norm_to_deviate_from() {
        // One test, four uncovered effectful functions: no testing habit here,
        // so listing all four would be noise rather than a finding.
        let built = built_of(
            vec![
                func("app::a", "app.py", &[Effect::Net]),
                func("app::b", "app.py", &[Effect::Net]),
                func("app::c", "app.py", &[Effect::Db]),
                func("app::d", "app.py", &[Effect::FsWrite]),
                func("tests::test_a", "tests/t.py", &[]),
            ],
            &[],
        );
        assert!(untested_effect(&built, &Policy::default(), &all_branching(&built), &HashSet::new()).is_empty());
    }

    #[test]
    fn pure_functions_are_never_reported() {
        let mut nodes = vec![func("app::pure_helper", "app.py", &[])];
        let mut calls = Vec::new();
        for i in 0..5 {
            nodes.push(func(&format!("app::fetch{i}"), "app.py", &[Effect::Net]));
            nodes.push(func(&format!("tests::test{i}"), "tests/t.py", &[]));
            calls.push((format!("tests::test{i}"), format!("app::fetch{i}")));
        }
        let refs: Vec<(&str, &str)> = calls.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
        let built = built_of(nodes, &refs);
        assert!(untested_effect(&built, &Policy::default(), &all_branching(&built), &HashSet::new()).is_empty());
    }

    #[test]
    fn framework_dispatched_functions_are_exempt() {
        let built = tested_codebase();
        let decorated: HashSet<String> = ["app::lonely_writer".to_string()].into_iter().collect();
        assert!(untested_effect(&built, &Policy::default(), &all_branching(&built), &decorated).is_empty());
    }

    #[test]
    fn straight_line_wiring_is_not_worth_a_test_finding() {
        // Same graph, but the parser saw no branch in the uncovered function:
        // it delegates, so there is no path for a regression to hide down.
        let built = tested_codebase();
        let branching: HashSet<String> = built
            .graph
            .entities()
            .map(|(_, e)| e.id.clone())
            .filter(|id| id != "app::lonely_writer")
            .collect();
        assert!(untested_effect(&built, &Policy::default(), &branching, &HashSet::new()).is_empty());
    }
}
