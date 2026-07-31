//! Finding what a codebase already has, by intent.
//!
//! The other graph tools (`query_subgraph`, `get_context_envelope`) both take
//! an entity id, and nothing produced one: the graph's only lookup is
//! `node(id)`, an exact match on an id the agent has no way to guess. So an
//! agent about to write `fetch_url` could not ask whether this repo already
//! has one — which is the redundancy slop class, the most common of them all.
//!
//! Ranking is deterministic token overlap, no embeddings and no LLM call: a
//! name, its module path and its docstring are already the words a developer
//! would search for, and reference count breaks ties toward what the codebase
//! actually leans on.

use std::collections::{HashMap, HashSet};

use petgraph::graph::NodeIndex;
use petgraph::visit::EdgeRef;
use serde::Serialize;
use slop_graph::{EdgeKind, Effect, NodeType};

use crate::build::BuiltGraph;
use crate::source::{is_test_entity, is_test_file};

/// A callable the codebase already provides.
#[derive(Serialize)]
pub struct Capability {
    pub entity: String,
    pub file: String,
    /// 1-based, for a `file:line` an editor can open.
    pub line: usize,
    pub effects: Vec<Effect>,
    pub docstring: Option<String>,
    /// How many places call it — how established the building block is.
    pub refs: usize,
}

/// Incoming `Calls` edges per node. Shared with the session world model, which
/// ranks its capability index the same way.
pub fn reference_counts(built: &BuiltGraph) -> HashMap<NodeIndex, usize> {
    let mut refs = HashMap::new();
    for edge in built.graph.graph.edge_references() {
        if *edge.weight() == EdgeKind::Calls {
            *refs.entry(edge.target()).or_default() += 1;
        }
    }
    refs
}

/// Content words of `s`, lowercased, split on non-alphanumerics and camelCase
/// boundaries so `fetchUrl`, `fetch_url` and "fetch a url" all agree.
fn tokens(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for c in s.chars() {
        if c.is_alphanumeric() {
            if c.is_uppercase() && prev_lower && !cur.is_empty() {
                out.push(std::mem::take(&mut cur).to_lowercase());
            }
            cur.push(c);
            prev_lower = c.is_lowercase() || c.is_ascii_digit();
        } else {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur).to_lowercase());
            }
            prev_lower = false;
        }
    }
    if !cur.is_empty() {
        out.push(cur.to_lowercase());
    }
    out.retain(|t| t.len() > 1);
    out
}

/// Weighted overlap: the name is the strongest signal of what something does,
/// the module path next, the docstring weakest (it's the longest text, so an
/// unweighted match would let prose outrank an exact name).
fn score(want: &HashSet<String>, entity: &slop_graph::CodeEntity) -> f64 {
    if want.is_empty() {
        return 0.0;
    }
    let name = entity.id.rsplit("::").next().unwrap_or(&entity.id);
    let module = entity.id.rsplit("::").nth(1).unwrap_or("");
    let hit = |text: &str| tokens(text).iter().filter(|t| want.contains(*t)).count() as f64;
    let doc = entity.docstring.as_deref().unwrap_or("");
    (3.0 * hit(name) + 1.5 * hit(module) + 0.5 * hit(doc)) / want.len() as f64
}

/// Callables matching `intent`, best first. `effect` restricts to functions
/// whose signature carries it — "what already does the network here".
///
/// An empty `intent` with an `effect` filter is a valid "list what exists"
/// query, ranked by reference count alone.
pub fn find(
    built: &BuiltGraph,
    intent: &str,
    effect: Option<Effect>,
    limit: usize,
) -> Vec<Capability> {
    let want: HashSet<String> = tokens(intent).into_iter().collect();
    let refs = reference_counts(built);

    let mut scored: Vec<(f64, usize, Capability)> = built
        .graph
        .entities()
        .filter(|(_, e)| {
            e.entity_type == NodeType::Function
                && !e.file.is_empty()
                && !is_test_file(&e.file)
                && !is_test_entity(&e.id)
                && effect.is_none_or(|want| e.effect_signature.contains(want))
        })
        .filter_map(|(idx, e)| {
            let s = score(&want, e);
            // With no intent every candidate scores 0 and reference count
            // orders them; with an intent, a zero score is simply not a match.
            if s == 0.0 && !want.is_empty() {
                return None;
            }
            let count = refs.get(&idx).copied().unwrap_or(0);
            Some((
                s,
                count,
                Capability {
                    entity: e.id.clone(),
                    file: e.file.clone(),
                    line: e.source_range.0 + 1,
                    effects: e.effect_signature.0.clone(),
                    docstring: e.docstring.clone(),
                    refs: count,
                },
            ))
        })
        .collect();

    scored.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then(b.1.cmp(&a.1))
            .then(a.2.entity.cmp(&b.2.entity))
    });
    scored.into_iter().take(limit).map(|(_, _, c)| c).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use slop_graph::{CodeEntity, CodeGraph, EffectSet};

    fn func(id: &str, file: &str, doc: Option<&str>, effects: &[Effect]) -> CodeEntity {
        let mut sig = EffectSet::pure();
        for &e in effects {
            sig.insert(e);
        }
        CodeEntity {
            id: id.into(),
            entity_type: NodeType::Function,
            name: id.rsplit("::").next().unwrap().into(),
            signature: String::new(),
            docstring: doc.map(str::to_string),
            file: file.into(),
            source_range: (0, 1),
            body_hash: String::new(),
            effect_signature: sig,
        }
    }

    fn built_of(entities: Vec<CodeEntity>) -> BuiltGraph {
        let mut graph = CodeGraph::new();
        for e in entities {
            graph.add_entity(e);
        }
        BuiltGraph { graph, by_symbol: Default::default(), referenced: Default::default() }
    }

    #[test]
    fn intent_words_find_the_matching_capability() {
        let built = built_of(vec![
            func("core::http::fetch_url", "core/http.py", None, &[Effect::Net]),
            func("utils::dates::parse_date", "utils/dates.py", None, &[]),
        ]);
        let hits = find(&built, "fetch a url over the network", None, 5);
        assert_eq!(hits[0].entity, "core::http::fetch_url");
        assert_eq!(hits[0].line, 1, "line is 1-based for file:line");
    }

    #[test]
    fn a_docstring_match_alone_still_ranks_below_a_name_match() {
        let built = built_of(vec![
            func("a::misc::helper", "a/misc.py", Some("parse a date string"), &[]),
            func("utils::dates::parse_date", "utils/dates.py", None, &[]),
        ]);
        let hits = find(&built, "parse date", None, 5);
        assert_eq!(hits[0].entity, "utils::dates::parse_date");
    }

    #[test]
    fn effect_filter_lists_what_owns_an_effect() {
        let built = built_of(vec![
            func("core::http::get", "core/http.py", None, &[Effect::Net]),
            func("utils::dates::parse", "utils/dates.py", None, &[]),
        ]);
        let hits = find(&built, "", Some(Effect::Net), 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].entity, "core::http::get");
    }

    #[test]
    fn test_helpers_are_not_offered_for_reuse() {
        let built = built_of(vec![func(
            "tests::helpers::fetch_url",
            "tests/helpers.py",
            None,
            &[Effect::Net],
        )]);
        assert!(find(&built, "fetch url", None, 5).is_empty());
    }

    #[test]
    fn an_unrelated_intent_matches_nothing() {
        let built = built_of(vec![func("utils::dates::parse_date", "utils/dates.py", None, &[])]);
        assert!(find(&built, "render a chart", None, 5).is_empty());
    }
}
