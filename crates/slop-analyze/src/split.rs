//! Signal (1): statements of a function grouped by dataflow.
//!
//! **Not wired into `detect::run_all`, on evidence.** The premise was that a
//! body forming two components with no dataflow between them is two functions
//! concatenated. Adjudicated over 1184 Python functions: 60 fired, 29 had
//! non-interleaved parts, 5 survived a guard filter, ~1 was worth acting on.
//! Four systematic reasons two statements exchange no values while still
//! belonging together — none of which dataflow can see:
//!
//! - **control coupling** — `if not x: raise` consumes its value and leaves, so
//!   every guard is its own component. Idiomatic early return *guarantees* a hit.
//! - **effect coupling** — clear-then-reload both mutate shared state; the
//!   medium is mutation, not values, so the dependency is invisible here.
//! - **interleaving** — components woven together admit no cut at all.
//! - **recursive traversal** — handle this node, then recurse into children:
//!   disconnected by construction.
//!
//! Kept rather than deleted because the SCIP def-use plumbing below is correct
//! and is what signal (2) (min-cut over the same graph) needs. What failed was
//! the interpretation, not the mechanism: "provable" bought a fact about the
//! code, and the fact turned out not to be evidence for the claim.
//!
//! Def-use comes from SCIP occurrences (D20), so scoping, shadowing and
//! `self.field` dataflow are the indexer's problem rather than a per-language
//! binder table's. tree-sitter supplies only the line-to-statement partition
//! (`FunctionFacts::stmt_spans`), which SCIP does not encode.
//!
//! Granularity is the variable, not the live range: an indexer gives one symbol
//! per binding per scope, so reusing `out` for two unrelated accumulators reads
//! as one thread of dataflow and merges what are really two components. That
//! under-reports, never over-reports, and separating live ranges needs to tell a
//! plain write from a read-modify-write — which SCIP's roles alone cannot.

use std::collections::{HashMap, HashSet};

use petgraph::unionfind::UnionFind;
use slop_parse::FunctionFacts;
use slop_resolve::{Occurrence, Resolver, SymbolKind};

use crate::build::BuiltGraph;
use crate::findings::{Finding, Severity};
use crate::source::{self, FileFacts};

/// Below this a function is not worth partitioning even when it does split.
const MIN_STMTS: usize = 4;
/// A one-statement component is a stray line, not half a function.
const MIN_PART_STMTS: usize = 2;

/// One dataflow component of a body.
struct Part {
    /// First statement's start line through the last statement's end line.
    lines: (u32, u32),
    stmts: usize,
    /// Symbols the part reads without defining — the values that would cross
    /// into it as parameters. Its width is the interface width of the split;
    /// the names are best-effort, since an indexer need not name a local.
    width: usize,
    names: Vec<String>,
}

pub fn split_candidate(
    built: &BuiltGraph,
    facts: &[FileFacts],
    resolver: &dyn Resolver,
) -> Vec<Finding> {
    let index = source::location_index(built);
    let mut findings = Vec::new();
    for ff in facts {
        if source::is_test_file(&ff.file) {
            continue;
        }
        let occs = resolver.occurrences_in(&ff.file);
        for fact in &ff.functions {
            if fact.stmt_spans.len() < MIN_STMTS {
                continue;
            }
            let Some(entity) = source::entity_for(built, &index, &ff.file, fact) else {
                continue;
            };
            if source::is_test_entity(&entity.id) {
                continue;
            }
            let parts = components(fact, occs, &ff.file, resolver);
            if parts.len() < 2 {
                continue;
            }
            findings.push(finding(entity, fact, &parts));
        }
    }
    findings
}

/// Statements grouped by dataflow, keeping only components substantial enough
/// to be a function on their own.
fn components(
    fact: &FunctionFacts,
    occs: &[Occurrence],
    file: &str,
    resolver: &dyn Resolver,
) -> Vec<Part> {
    let spans = &fact.stmt_spans;
    let n = spans.len();
    let mut defs: Vec<HashSet<&str>> = vec![HashSet::new(); n];
    let mut uses: Vec<HashSet<&str>> = vec![HashSet::new(); n];

    for occ in occs {
        if !carries_dataflow(&occ.symbol) {
            continue;
        }
        // A parameter's declaration sits in the signature, outside every body
        // span, so it is dropped here — which is exactly the rule that keeps two
        // halves reading one parameter from counting as connected. That shared
        // read *is* the interface width, not a dependency.
        let Some(s) = stmt_of(spans, occ.range.start_line) else {
            continue;
        };
        if occ.is_definition {
            defs[s].insert(&occ.symbol);
        } else {
            uses[s].insert(&occ.symbol);
        }
    }

    let mut uf = UnionFind::<usize>::new(n);
    for j in 0..n {
        for i in 0..j {
            // A later statement reading a name an earlier one bound is dataflow.
            if !defs[i].is_disjoint(&uses[j]) {
                uf.union(i, j);
            }
        }
    }

    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for (i, root) in uf.into_labeling().into_iter().enumerate() {
        groups.entry(root).or_default().push(i);
    }

    let mut parts: Vec<Part> = groups
        .values()
        .filter(|m| m.len() >= MIN_PART_STMTS)
        .map(|m| part(m, spans, &defs, &uses, occs, file, resolver))
        .collect();
    parts.sort_by_key(|p| p.lines.0);
    parts
}

fn part(
    members: &[usize],
    spans: &[(u32, u32)],
    defs: &[HashSet<&str>],
    uses: &[HashSet<&str>],
    occs: &[Occurrence],
    file: &str,
    resolver: &dyn Resolver,
) -> Part {
    let bound: HashSet<&str> = members.iter().flat_map(|&i| defs[i].iter().copied()).collect();
    let free: HashSet<&str> = members
        .iter()
        .flat_map(|&i| uses[i].iter().copied())
        .filter(|s| !bound.contains(s))
        .collect();
    let mut names: Vec<String> = free
        .iter()
        .filter_map(|s| display_name(resolver, file, occs, s))
        .collect();
    names.sort();
    let last = *members.last().unwrap_or(&0);
    Part {
        lines: (spans[members[0]].0, spans[last].1),
        stmts: members.len(),
        width: free.len(),
        names,
    }
}

/// Symbol kinds whose definitions and reads are dataflow inside one body.
/// `Term` earns its place: it is how an indexer names `self.field`, so field
/// assignment links statements that a name-matching walk would miss entirely.
fn carries_dataflow(symbol: &str) -> bool {
    matches!(
        SymbolKind::from_symbol(symbol),
        SymbolKind::Local | SymbolKind::Parameter | SymbolKind::Term
    )
}

/// Index of the statement whose span covers `line`. Spans are disjoint and in
/// source order, so the last one starting at or before `line` is the candidate.
fn stmt_of(spans: &[(u32, u32)], line: u32) -> Option<usize> {
    let i = spans.partition_point(|&(start, _)| start <= line).checked_sub(1)?;
    (line <= spans[i].1).then_some(i)
}

/// Best-effort human name for a symbol, read through the resolver at one of its
/// occurrences. Indexers are not obliged to name locals; an unnamed one still
/// counts toward interface width, it just cannot be printed.
fn display_name(
    resolver: &dyn Resolver,
    file: &str,
    occs: &[Occurrence],
    symbol: &str,
) -> Option<String> {
    let occ = occs.iter().find(|o| o.symbol == symbol)?;
    let name = &resolver
        .resolve(file, occ.range.start_line, occ.range.start_col)?
        .display_name;
    // An unnamed symbol falls back to its last SCIP descriptor, which for an
    // external package is the whole `rust-analyzer cargo <crate> <ver> Ty#field`
    // string. A real identifier never contains a space.
    (!name.is_empty() && !name.contains(' ')).then(|| name.clone())
}

fn finding(entity: &slop_graph::CodeEntity, fact: &FunctionFacts, parts: &[Part]) -> Finding {
    // Interleaved components are still two jobs, but the boundary is not a
    // single cut through the body, so the proposal is worth less. Say which.
    let interleaved = parts.windows(2).any(|w| w[0].lines.1 >= w[1].lines.0);
    let shape = if interleaved { "interleaved" } else { "consecutive" };
    let spans = parts
        .iter()
        .map(|p| format!("{}-{} ({} stmts)", p.lines.0 + 1, p.lines.1 + 1, p.stmts))
        .collect::<Vec<_>>()
        .join(", ");
    let widths = parts
        .iter()
        .map(|p| {
            if p.names.is_empty() {
                format!("{} value(s)", p.width)
            } else {
                format!("{} value(s): {}", p.width, p.names.join(", "))
            }
        })
        .collect::<Vec<_>>()
        .join(" | ");
    Finding {
        rule: "split-candidate",
        severity: Severity::Warning,
        entity: entity.id.clone(),
        file: entity.file.clone(),
        lines: (fact.start_line as usize, fact.end_line as usize),
        message: format!(
            "{} statements form {} {shape} groups that exchange no values: lines {spans}",
            fact.stmt_spans.len(),
            parts.len(),
        ),
        fix_guidance: format!(
            "each group is a function already; splitting costs only its inputs — {widths}"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::stmt_of;

    #[test]
    fn stmt_of_maps_continuation_lines_to_their_statement() {
        let spans = [(1, 1), (3, 6), (7, 7)];
        assert_eq!(stmt_of(&spans, 1), Some(0));
        // A continuation line of the multi-line statement, not its own statement.
        assert_eq!(stmt_of(&spans, 5), Some(1));
        assert_eq!(stmt_of(&spans, 7), Some(2));
        // A parameter declaration on the signature line belongs to no statement.
        assert_eq!(stmt_of(&spans, 0), None);
        // A comment line between statements likewise.
        assert_eq!(stmt_of(&spans, 2), None);
        assert_eq!(stmt_of(&spans, 9), None);
    }
}
