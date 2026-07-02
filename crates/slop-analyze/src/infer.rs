//! Dominant-pattern channel inference (D8): if one class/function owns the
//! overwhelming share of direct acquisitions of an effect, propose it as
//! the sanctioned channel for slop.toml. User confirms; we never guess
//! silently into an enforcing policy.

use std::collections::HashMap;

use petgraph::visit::EdgeRef;
use petgraph::Direction;
use slop_graph::{EdgeKind, Effect, NodeType};

use crate::build::BuiltGraph;

const DOMINANCE: f64 = 0.8;

/// effect key (slop.toml channel key) -> proposed dotted channel paths.
pub fn infer_channels(built: &BuiltGraph) -> HashMap<String, Vec<String>> {
    let graph = &built.graph;
    // effect -> acquiring channel candidate -> distinct acquirer count
    let mut counts: HashMap<Effect, HashMap<String, usize>> = HashMap::new();

    for (idx, entity) in graph.entities() {
        if !matches!(entity.entity_type, NodeType::Function | NodeType::Class) {
            continue;
        }
        let has_direct: Vec<Effect> = graph
            .graph
            .edges_directed(idx, Direction::Outgoing)
            .filter(|e| *e.weight() == EdgeKind::HasEffect)
            .flat_map(|e| graph.entity(e.target()).effect_signature.0.clone())
            .collect();
        if has_direct.is_empty() {
            continue;
        }
        // Channel candidate: the class when this is a method, else the
        // function itself.
        let segments: Vec<&str> = entity.id.split("::").collect();
        let candidate = if segments.len() >= 3 {
            segments[..segments.len() - 1].join("::")
        } else {
            entity.id.clone()
        };
        for effect in has_direct {
            *counts
                .entry(effect)
                .or_default()
                .entry(candidate.clone())
                .or_default() += 1;
        }
    }

    let mut proposals: HashMap<String, Vec<String>> = HashMap::new();
    for (effect, candidates) in counts {
        let Some(key) = channel_key(effect) else {
            continue;
        };
        let total: usize = candidates.values().sum();
        let Some((winner, count)) = candidates.into_iter().max_by_key(|(_, c)| *c) else {
            continue;
        };
        if total > 0 && count as f64 / total as f64 >= DOMINANCE {
            proposals
                .entry(key.to_string())
                .or_default()
                .push(winner.replace("::", "."));
        }
    }
    for v in proposals.values_mut() {
        v.sort();
        v.dedup();
    }
    proposals
}

fn channel_key(effect: Effect) -> Option<&'static str> {
    match effect {
        Effect::Net => Some("net"),
        Effect::Db => Some("db"),
        Effect::FsRead | Effect::FsWrite => Some("fs"),
        Effect::Env => Some("env"),
        _ => None,
    }
}

/// Render proposals as a slop.toml snippet.
pub fn to_toml(proposals: &HashMap<String, Vec<String>>) -> String {
    let mut keys: Vec<&String> = proposals.keys().collect();
    keys.sort();
    let mut out = String::from("[channels]\n");
    for key in keys {
        let quoted: Vec<String> = proposals[key].iter().map(|c| format!("\"{c}\"")).collect();
        out.push_str(&format!("{key} = [{}]\n", quoted.join(", ")));
    }
    out
}
