//! Coarse effect inference (D3): a hand-curated seed table of primitive
//! effect sources, direct effect acquisition via references to them, and
//! transitive propagation up the Calls graph to a fixpoint.

use petgraph::graph::NodeIndex;
use petgraph::visit::EdgeRef;
use petgraph::Direction;
use slop_graph::{EdgeKind, Effect, NodeType};

use crate::build::BuiltGraph;
use crate::entity_id::module_of;

/// Seed table entry: any reference into `module_prefix` acquires `effect`.
pub struct Seed {
    pub module_prefix: &'static str,
    pub effect: Effect,
}

/// M1 seed table: Net only. Grows to the full lattice in M2.
///
/// Prefixes are deliberately narrow: `urllib.parse` and `http.cookies` are
/// pure — seeding all of `urllib`/`http` flags URL formatting as network
/// I/O (found dogfooding stress-analysis).
pub const SEEDS: &[Seed] = &[
    Seed { module_prefix: "urllib.request", effect: Effect::Net },
    Seed { module_prefix: "urllib3", effect: Effect::Net },
    Seed { module_prefix: "requests", effect: Effect::Net },
    Seed { module_prefix: "socket", effect: Effect::Net },
    Seed { module_prefix: "http.client", effect: Effect::Net },
    Seed { module_prefix: "httpx", effect: Effect::Net },
    Seed { module_prefix: "aiohttp", effect: Effect::Net },
];

fn module_matches(module: &str, prefix: &str) -> bool {
    module == prefix || module.starts_with(prefix) && module[prefix.len()..].starts_with('.')
}

pub fn seed_effect_for(symbol: &str) -> Option<Effect> {
    let module = module_of(symbol)?;
    SEEDS
        .iter()
        .find(|s| module_matches(&module, s.module_prefix))
        .map(|s| s.effect)
}

/// Mark external seed nodes as effect sources and add `HasEffect` edges from
/// every internal entity that references them. A `HasEffect` edge means
/// *direct* acquisition — the signal infra-bypass keys off. Then propagate
/// effects transitively along `Calls` edges to a fixpoint (cycles converge
/// because set-union is monotone).
pub fn infer_effects(built: &mut BuiltGraph) {
    // Direct acquisition.
    let mut direct: Vec<(NodeIndex, NodeIndex, Effect)> = Vec::new();
    for (symbol, &source) in &built.by_symbol {
        let Some(effect) = seed_effect_for(symbol) else {
            continue;
        };
        if built.graph.entity(source).entity_type != NodeType::EffectSource {
            continue;
        }
        for edge in built.graph.graph.edges_directed(source, Direction::Incoming) {
            if *edge.weight() == EdgeKind::Calls {
                direct.push((edge.source(), source, effect));
            }
        }
    }
    for (user, source, effect) in direct {
        built.graph.graph[source].effect_signature.insert(effect);
        built.graph.graph[user].effect_signature.insert(effect);
        built.graph.add_edge(user, source, EdgeKind::HasEffect);
    }

    // Transitive propagation: caller absorbs callee effects.
    loop {
        let mut changed = false;
        let edges: Vec<(NodeIndex, NodeIndex)> = built
            .graph
            .graph
            .edge_indices()
            .filter(|&e| built.graph.graph[e] == EdgeKind::Calls)
            .filter_map(|e| built.graph.graph.edge_endpoints(e))
            .collect();
        for (caller, callee) in edges {
            let callee_effects = built.graph.graph[callee].effect_signature.clone();
            if built.graph.graph[caller]
                .effect_signature
                .union_with(&callee_effects)
            {
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
}
