//! The effect graph: one cyclic, multi-edge-kind directed graph over code
//! entities (D6). Acyclicity is checked per-edge-kind (e.g. `Imports`),
//! never as a global invariant.

use std::collections::HashMap;

use petgraph::graph::{DiGraph, NodeIndex};
use serde::{Deserialize, Serialize};

/// Coarse, conservative effect lattice (D3). `Unknown` is the top element:
/// any unresolved call collapses to it rather than crashing or guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Effect {
    Net,
    FsRead,
    FsWrite,
    Db,
    Env,
    Throws,
    Nondeterminism, // time / random
    StateMutate,
    Concurrency,
    Unknown,
}

/// A set of effects carried by a callable. Empty set = Pure.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectSet(pub Vec<Effect>);

impl EffectSet {
    pub fn pure() -> Self {
        Self::default()
    }
    pub fn is_pure(&self) -> bool {
        self.0.is_empty()
    }
    pub fn insert(&mut self, e: Effect) {
        if let Err(pos) = self.0.binary_search(&e) {
            self.0.insert(pos, e);
        }
    }
    pub fn contains(&self, e: Effect) -> bool {
        self.0.binary_search(&e).is_ok()
    }
    pub fn union_with(&mut self, other: &EffectSet) -> bool {
        let mut changed = false;
        for e in &other.0 {
            if !self.contains(*e) {
                self.insert(*e);
                changed = true;
            }
        }
        changed
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeType {
    Module,
    Class,
    Function,
    Import,
    EffectSource,
}

/// A node in the graph (§3.2 of the execution plan).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeEntity {
    /// e.g. "billing.gateways::StripeGateway::charge_customer"
    pub id: String,
    pub entity_type: NodeType,
    pub name: String,
    /// Full type-annotated signature when known.
    pub signature: String,
    pub docstring: Option<String>,
    /// File the entity is defined in, repo-relative.
    pub file: String,
    /// Byte range in the source file.
    pub source_range: (usize, usize),
    /// Blake3 of the body — Tier-1 duplication.
    pub body_hash: String,
    pub effect_signature: EffectSet,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EdgeKind {
    Contains,
    Calls,
    Imports,
    Inherits,
    HasEffect,
}

/// The one graph. Multi-edge-kind, cyclic; per-kind invariants are the
/// detectors' job (Tarjan SCC on `Imports` finds circular imports).
#[derive(Debug, Default)]
pub struct CodeGraph {
    pub graph: DiGraph<CodeEntity, EdgeKind>,
    by_id: HashMap<String, NodeIndex>,
}

impl CodeGraph {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_entity(&mut self, entity: CodeEntity) -> NodeIndex {
        if let Some(&idx) = self.by_id.get(&entity.id) {
            return idx;
        }
        let id = entity.id.clone();
        let idx = self.graph.add_node(entity);
        self.by_id.insert(id, idx);
        idx
    }

    pub fn add_edge(&mut self, from: NodeIndex, to: NodeIndex, kind: EdgeKind) {
        // Multi-edges of the same kind between the same pair carry no extra
        // information for any current detector; keep the graph minimal.
        let exists = self
            .graph
            .edges_connecting(from, to)
            .any(|e| *e.weight() == kind);
        if !exists {
            self.graph.add_edge(from, to, kind);
        }
    }

    pub fn node(&self, id: &str) -> Option<NodeIndex> {
        self.by_id.get(id).copied()
    }

    pub fn entity(&self, idx: NodeIndex) -> &CodeEntity {
        &self.graph[idx]
    }

    pub fn len(&self) -> usize {
        self.graph.node_count()
    }

    pub fn is_empty(&self) -> bool {
        self.graph.node_count() == 0
    }

    pub fn entities(&self) -> impl Iterator<Item = (NodeIndex, &CodeEntity)> {
        self.graph
            .node_indices()
            .map(move |i| (i, &self.graph[i]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entity(id: &str, ty: NodeType) -> CodeEntity {
        CodeEntity {
            id: id.into(),
            entity_type: ty,
            name: id.rsplit("::").next().unwrap_or(id).into(),
            signature: String::new(),
            docstring: None,
            file: String::new(),
            source_range: (0, 0),
            body_hash: String::new(),
            effect_signature: EffectSet::pure(),
        }
    }

    #[test]
    fn dedupes_nodes_and_edges() {
        let mut g = CodeGraph::new();
        let a = g.add_entity(entity("m::f", NodeType::Function));
        let a2 = g.add_entity(entity("m::f", NodeType::Function));
        assert_eq!(a, a2);
        let b = g.add_entity(entity("m::g", NodeType::Function));
        g.add_edge(a, b, EdgeKind::Calls);
        g.add_edge(a, b, EdgeKind::Calls);
        assert_eq!(g.graph.edge_count(), 1);
        // A different kind between the same pair is a distinct edge.
        g.add_edge(a, b, EdgeKind::Contains);
        assert_eq!(g.graph.edge_count(), 2);
    }

    #[test]
    fn effect_set_union() {
        let mut a = EffectSet::pure();
        a.insert(Effect::Net);
        let mut b = EffectSet::pure();
        b.insert(Effect::Net);
        b.insert(Effect::Db);
        assert!(a.union_with(&b));
        assert!(a.contains(Effect::Db));
        assert!(!a.union_with(&b)); // second union is a no-op
    }
}
