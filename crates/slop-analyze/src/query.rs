//! Read-only subgraph queries around a single entity, for the MCP
//! `query_subgraph` tool (M4c). BFS over the same proximity edges the
//! context envelope uses, but returned as plain serializable data — the
//! agent asks "what does `X` call / import / carry?" without slop needing
//! to be the one holding petgraph handles.

use petgraph::graph::NodeIndex;
use petgraph::visit::EdgeRef;
use petgraph::Direction;
use serde::Serialize;
use slop_graph::{EdgeKind, Effect};

use crate::build::BuiltGraph;

#[derive(Debug, Clone, Serialize)]
pub struct Neighbor {
    pub id: String,
    pub entity_type: String,
    /// The edge kind that first reached this node (Calls/Imports/…).
    pub edge: String,
    /// "out" (target depends on neighbor) or "in" (neighbor depends on target).
    pub direction: String,
    pub distance: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Subgraph {
    pub target: String,
    pub entity_type: String,
    pub effects: Vec<Effect>,
    pub neighbors: Vec<Neighbor>,
}

fn edge_label(k: EdgeKind) -> &'static str {
    match k {
        EdgeKind::Contains => "Contains",
        EdgeKind::Calls => "Calls",
        EdgeKind::Imports => "Imports",
        EdgeKind::Inherits => "Inherits",
        EdgeKind::HasEffect => "HasEffect",
    }
}

fn type_label(t: slop_graph::NodeType) -> &'static str {
    use slop_graph::NodeType::*;
    match t {
        Module => "Module",
        Class => "Class",
        Function => "Function",
        Import => "Import",
        EffectSource => "EffectSource",
    }
}

/// BFS out to `depth` hops from `entity`. `kinds`, when set, restricts which
/// edge kinds are traversed and reported. Returns `None` if `entity` isn't a
/// node in the graph.
pub fn subgraph(
    built: &BuiltGraph,
    entity: &str,
    depth: usize,
    kinds: Option<&[EdgeKind]>,
) -> Option<Subgraph> {
    let start = built.graph.node(entity)?;
    let graph = &built.graph.graph;
    let allowed = |k: EdgeKind| kinds.map_or(true, |ks| ks.contains(&k));

    let mut seen: std::collections::HashSet<NodeIndex> = std::collections::HashSet::new();
    seen.insert(start);
    let mut frontier = vec![start];
    let mut neighbors = Vec::new();

    for dist in 1..=depth {
        let mut next = Vec::new();
        for &node in &frontier {
            for e in graph.edges_directed(node, Direction::Outgoing) {
                if allowed(*e.weight()) && seen.insert(e.target()) {
                    let ent = &graph[e.target()];
                    neighbors.push(Neighbor {
                        id: ent.id.clone(),
                        entity_type: type_label(ent.entity_type).to_string(),
                        edge: edge_label(*e.weight()).to_string(),
                        direction: "out".to_string(),
                        distance: dist,
                    });
                    next.push(e.target());
                }
            }
            for e in graph.edges_directed(node, Direction::Incoming) {
                if allowed(*e.weight()) && seen.insert(e.source()) {
                    let ent = &graph[e.source()];
                    neighbors.push(Neighbor {
                        id: ent.id.clone(),
                        entity_type: type_label(ent.entity_type).to_string(),
                        edge: edge_label(*e.weight()).to_string(),
                        direction: "in".to_string(),
                        distance: dist,
                    });
                    next.push(e.source());
                }
            }
        }
        frontier = next;
        if frontier.is_empty() {
            break;
        }
    }

    let target = built.graph.entity(start);
    Some(Subgraph {
        target: target.id.clone(),
        entity_type: type_label(target.entity_type).to_string(),
        effects: target.effect_signature.0.clone(),
        neighbors,
    })
}
