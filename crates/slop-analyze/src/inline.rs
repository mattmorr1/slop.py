//! Mechanical inlining of judge-confirmed `trivial-wrapper` findings (D9 path
//! a, the write-side of the detector in `check::tier3_wrapper_findings`).
//!
//! A wrapper `def load(p): return read_file(p)` is inlined by rewriting every
//! reference to `load` into `read_file` and deleting the wrapper. This is only
//! attempted for the rename-safe subset — `forward_identity`, where the body
//! passes its parameters to the callee positionally and unchanged. Under that
//! restriction `load` and `read_file` are interchangeable as callables, so even
//! a value-use (`cb = load`) rewrites correctly, and the whole edit reduces to a
//! callee rename plus a definition deletion.
//!
//! Safety rests on the same two gates as `rename` (every occurrence locatable,
//! every touched file re-parses — see `rename::rewrite_occurrences`) plus one
//! more: the callee must be an internal function, i.e. a module-level name of
//! the wrapper's module. That keeps the rewritten `from wrapper_mod import
//! read_file` valid and rules out builtins/stdlib, where it would not resolve.
//! Anything outside the safe subset is skipped with a reason; the finding's
//! guidance still stands for a human or agent to inline by hand.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use petgraph::visit::EdgeRef;
use petgraph::Direction;
use slop_graph::{EdgeKind, NodeType};
use slop_resolve::Resolver;

use crate::build::BuiltGraph;
use crate::findings::Finding;
use crate::rename::{rewrite_occurrences, symbol_for, NameOccurrence};
use crate::source::{self, FileFacts};

/// An inline slop is ready to apply, with counts for the reviewer.
pub struct InlinePlan {
    pub entity: String,
    pub callee: String,
    /// References rewritten to the callee (call sites, imports, value uses).
    pub occurrences: usize,
    /// Distinct files touched, including the wrapper's own.
    pub file_count: usize,
}

pub enum InlineOutcome {
    Planned(InlinePlan),
    Skipped { entity: String, reason: String },
}

/// Per-finding outcomes plus the accumulated final source of every touched
/// file. Reference rewrites for all wrappers are applied first (they don't
/// change line counts, so SCIP occurrence lines stay valid); definition
/// deletions are applied last, once per file.
pub struct Inlines {
    pub outcomes: Vec<InlineOutcome>,
    pub files: HashMap<String, String>,
}

/// Map every graph function entity id to its parser fact, so a finding can
/// recover the forwarder shape (`forward_identity`, def line range) it needs.
fn facts_by_entity<'a>(
    built: &BuiltGraph,
    facts: &'a [FileFacts],
) -> HashMap<String, (&'a str, &'a slop_parse::FunctionFacts)> {
    let index = source::location_index(built);
    let mut map = HashMap::new();
    for ff in facts {
        for fact in &ff.functions {
            if let Some(e) = source::entity_for(built, &index, &ff.file, fact) {
                map.insert(e.id.clone(), (ff.file.as_str(), fact));
            }
        }
    }
    map
}

/// Does the wrapper delegate to an internal function of the given name — i.e. a
/// module-level name, not a builtin/stdlib call that a rewritten import couldn't
/// resolve?
fn callee_is_internal(built: &BuiltGraph, entity: &str, callee: &str) -> bool {
    let Some(node) = built.graph.node(entity) else {
        return false;
    };
    built.graph.graph.edges_directed(node, Direction::Outgoing).any(|e| {
        *e.weight() == EdgeKind::Calls && {
            let t = built.graph.entity(e.target());
            t.entity_type == NodeType::Function && t.name == callee
        }
    })
}

/// Plan the inlining of every judge-confirmed `trivial-wrapper` finding. Each
/// becomes a ready `InlinePlan` or a `Skipped` with a reason; the caller applies
/// (`files`) or reports. Restricted to the `forward_identity` subset with an
/// internal callee — see the module header.
pub fn plan_inlines(
    built: &BuiltGraph,
    resolver: &dyn Resolver,
    repo: &Path,
    facts: &[FileFacts],
    findings: &[Finding],
) -> Inlines {
    let by_entity = facts_by_entity(built, facts);
    let mut outcomes = Vec::new();
    let mut cache: HashMap<String, String> = HashMap::new();
    let mut touched: HashSet<String> = HashSet::new();
    let mut done: HashSet<String> = HashSet::new();
    // Definition line ranges to delete after every rewrite has landed.
    let mut deletions: HashMap<String, Vec<(u32, u32)>> = HashMap::new();

    for finding in findings.iter().filter(|f| f.rule == "trivial-wrapper") {
        if !done.insert(finding.entity.clone()) {
            continue;
        }
        let skip = |reason: String| InlineOutcome::Skipped {
            entity: finding.entity.clone(),
            reason,
        };
        let Some(&(file, fact)) = by_entity.get(&finding.entity) else {
            outcomes.push(skip("no parser fact for the wrapper — skipped".to_string()));
            continue;
        };
        let (Some(callee), true) = (&fact.forward_target, fact.forward_identity) else {
            outcomes.push(skip(
                "delegation is not a pure positional forward — inline by hand".to_string(),
            ));
            continue;
        };
        if !callee_is_internal(built, &finding.entity, callee) {
            outcomes.push(skip(format!(
                "callee `{callee}` is a builtin or external — a rewritten import would not resolve; left as guidance"
            )));
            continue;
        }
        let Some(symbol) = symbol_for(built, &finding.entity).map(str::to_string) else {
            outcomes.push(skip("no SCIP symbol — can't verify references".to_string()));
            continue;
        };

        // References only: the definition occurrence (the one carrying the body
        // span) is deleted, not rewritten.
        let refs: Vec<NameOccurrence> = resolver
            .files()
            .iter()
            .flat_map(|f| {
                resolver
                    .occurrences_in(f)
                    .iter()
                    .filter(|o| o.symbol == symbol && o.enclosing_range.is_none())
                    .map(move |o| NameOccurrence {
                        file: f.to_string(),
                        line: o.range.start_line,
                        col: o.range.start_col,
                    })
            })
            .collect();
        if refs.is_empty() {
            outcomes.push(skip("no references recorded in the index".to_string()));
            continue;
        }

        // Load pristine source for every file this inline touches — the ref
        // files plus the wrapper's own (for the deletion) — on first sight.
        let mut read_failed = None;
        for f in refs.iter().map(|o| o.file.as_str()).chain(std::iter::once(file)) {
            if cache.contains_key(f) {
                continue;
            }
            match std::fs::read_to_string(repo.join(f)) {
                Ok(s) => {
                    cache.insert(f.to_string(), s);
                }
                Err(e) => {
                    read_failed = Some(format!("reading {f}: {e}"));
                    break;
                }
            }
        }
        if let Some(reason) = read_failed {
            outcomes.push(skip(reason));
            continue;
        }

        match rewrite_occurrences(&cache, &refs, &fact.name, callee) {
            Ok(rewritten) => {
                let mut inline_files: HashSet<String> = rewritten.keys().cloned().collect();
                inline_files.insert(file.to_string());
                for (f, src) in rewritten {
                    cache.insert(f, src);
                }
                for f in &inline_files {
                    touched.insert(f.clone());
                }
                deletions
                    .entry(file.to_string())
                    .or_default()
                    .push((fact.start_line, fact.end_line));
                outcomes.push(InlineOutcome::Planned(InlinePlan {
                    entity: finding.entity.clone(),
                    callee: callee.clone(),
                    occurrences: refs.len(),
                    file_count: inline_files.len(),
                }));
            }
            Err(reason) => outcomes.push(skip(reason)),
        }
    }

    // Deletions last: they shift line numbers, which would invalidate the SCIP
    // occurrence lines the rewrite phase depends on.
    for (file, ranges) in deletions {
        if let Some(src) = cache.get(&file) {
            let new_src = crate::fix::delete_line_ranges(src, &ranges);
            cache.insert(file, new_src);
        }
    }

    let files = cache
        .into_iter()
        .filter(|(f, _)| touched.contains(f))
        .collect();
    Inlines { outcomes, files }
}
