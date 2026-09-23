//! Context envelope (D11, M4b): effect-typed relevance scoring + budgeted
//! greedy packing around an edit locus. This is Aider's repo-map problem
//! with PageRank swapped for a semantic, effect-typed scorer — global
//! topological centrality is the wrong signal for "what does *this* edit
//! need to see."
//!
//! Score = call/containment distance + type-contract adjacency (same
//! class/module as the target) + effect-signature relevance (Jaccard) +
//! convention-exemplar bonus (dominant-pattern channel owners, D8). Pack
//! greedily by score under a token budget: full fidelity inside the edit
//! zone (target + its direct neighborhood), skeletons beyond.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::Arc;

use petgraph::graph::NodeIndex;
use petgraph::visit::EdgeRef;
use petgraph::Direction;
use slop_graph::{CodeEntity, EdgeKind, NodeType};

use crate::build::BuiltGraph;
use crate::infer;
use crate::skeleton::{skeleton_for, strip_noise};
use crate::source::{entity_for, location_index, FileFacts};

const DISTANCE_WEIGHT: u32 = 3000;
const ADJACENCY_WEIGHT: u32 = 2000;
const EFFECT_WEIGHT: u32 = 2000;
const EXEMPLAR_WEIGHT: u32 = 1000;

/// Distance-independent edge kinds that count as "the same call/containment
/// neighborhood" for BFS proximity.
const PROXIMITY_EDGES: [EdgeKind; 3] = [EdgeKind::Contains, EdgeKind::Calls, EdgeKind::Imports];

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Fidelity {
    Full,
    Skeleton,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct EnvelopeItem {
    pub entity: String,
    pub file: String,
    pub fidelity: Fidelity,
    pub score: u32,
    pub reasons: ScoreReasons,
    pub text: String,
}

#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct ScoreReasons {
    pub distance: u32,
    pub adjacency: u32,
    pub effect_overlap: u32,
    pub sanctioned_exemplar: u32,
}

impl ScoreReasons {
    fn total(self) -> u32 {
        self.distance + self.adjacency + self.effect_overlap + self.sanctioned_exemplar
    }
}

pub struct EnvelopeConfig {
    /// Rough token budget (chars / 4) for everything except the target.
    pub token_budget: usize,
    /// BFS hops from the target that still get full-fidelity source.
    pub edit_zone_hops: usize,
}

impl Default for EnvelopeConfig {
    fn default() -> Self {
        Self {
            token_budget: 8000,
            edit_zone_hops: 1,
        }
    }
}

fn estimate_tokens(text: &str) -> usize {
    text.len() / 4 + 1
}

/// Multi-source BFS over proximity edges (Contains/Calls/Imports, undirected):
/// distance from each node to its *nearest* start. Shared by the envelope
/// (one start = the edit target) and `compress` (many starts = the edit zone).
pub(crate) fn proximity_distances(
    built: &BuiltGraph,
    starts: &[NodeIndex],
) -> HashMap<NodeIndex, usize> {
    let graph = &built.graph.graph;
    let mut dist = HashMap::new();
    let mut queue = VecDeque::new();
    for &s in starts {
        if dist.insert(s, 0usize).is_none() {
            queue.push_back(s);
        }
    }
    while let Some(n) = queue.pop_front() {
        let d = dist[&n];
        let mut neighbors = Vec::new();
        for e in graph.edges_directed(n, Direction::Outgoing) {
            if PROXIMITY_EDGES.contains(e.weight()) {
                neighbors.push(e.target());
            }
        }
        for e in graph.edges_directed(n, Direction::Incoming) {
            if PROXIMITY_EDGES.contains(e.weight()) {
                neighbors.push(e.source());
            }
        }
        for nb in neighbors {
            dist.entry(nb).or_insert_with(|| {
                queue.push_back(nb);
                d + 1
            });
        }
    }
    dist
}

fn bfs_distance(built: &BuiltGraph, start: NodeIndex) -> HashMap<NodeIndex, usize> {
    proximity_distances(built, &[start])
}

/// The Contains-parent of `idx` (the class/module a function lives in), if any.
fn container_of(built: &BuiltGraph, idx: NodeIndex) -> Option<NodeIndex> {
    built
        .graph
        .graph
        .edges_directed(idx, Direction::Incoming)
        .find(|e| *e.weight() == EdgeKind::Contains)
        .map(|e| e.source())
}

fn effect_overlap(a: &slop_graph::EffectSet, b: &slop_graph::EffectSet) -> u32 {
    if a.0.is_empty() && b.0.is_empty() {
        return 0;
    }
    let sa: HashSet<_> = a.0.iter().collect();
    let sb: HashSet<_> = b.0.iter().collect();
    let inter = sa.intersection(&sb).count();
    let union = sa.union(&sb).count();
    if union == 0 {
        0
    } else {
        EFFECT_WEIGHT * inter as u32 / union as u32
    }
}

fn is_exemplar(entity_id: &str, proposals: &HashMap<String, Vec<String>>) -> bool {
    let dotted = entity_id.replace("::", ".");
    proposals.values().flatten().any(|chan| {
        dotted == *chan || (dotted.starts_with(chan.as_str()) && dotted[chan.len()..].starts_with('.'))
    })
}

fn render_full(repo_root: &Path, entity: &CodeEntity) -> Option<String> {
    let source = std::fs::read_to_string(repo_root.join(&entity.file)).ok()?;
    let lines: Vec<&str> = source.lines().collect();
    let (start, end) = entity.source_range;
    let end = end.min(lines.len().saturating_sub(1));
    if start > end || start >= lines.len() {
        return None;
    }
    Some(strip_noise(
        &lines[start..=end].join("\n"),
        slop_parse::Language::from_path(&entity.file),
    ))
}

fn render_captured(sources: &BTreeMap<String, Arc<str>>, entity: &CodeEntity) -> Option<String> {
    let source = sources.get(&entity.file)?;
    let lines: Vec<&str> = source.lines().collect();
    let (start, end) = entity.source_range;
    let end = end.min(lines.len().saturating_sub(1));
    if start > end || start >= lines.len() {
        return None;
    }
    Some(strip_noise(
        &lines[start..=end].join("\n"),
        slop_parse::Language::from_path(&entity.file),
    ))
}

/// Build the envelope around `target_entity` (a dotted `id`, `::`-joined).
/// Empty if the target isn't in the graph.
pub fn build_envelope(
    built: &BuiltGraph,
    facts: &[FileFacts],
    repo_root: &Path,
    target_entity: &str,
    config: &EnvelopeConfig,
) -> Vec<EnvelopeItem> {
    build_envelope_with(built, facts, target_entity, config, |entity| {
        render_full(repo_root, entity)
    })
}

pub fn build_captured_envelope(
    built: &BuiltGraph,
    facts: &[FileFacts],
    sources: &BTreeMap<String, Arc<str>>,
    target_entity: &str,
    config: &EnvelopeConfig,
    resolved: impl Fn(&str) -> bool,
) -> (Vec<EnvelopeItem>, usize) {
    let mut items = build_envelope_with(built, facts, target_entity, config, |entity| {
        render_captured(sources, entity)
    });
    let before = items.len();
    items.retain(|item| resolved(&item.file));
    let omitted = before - items.len();
    (items, omitted)
}

fn build_envelope_with<F>(
    built: &BuiltGraph,
    facts: &[FileFacts],
    target_entity: &str,
    config: &EnvelopeConfig,
    render: F,
) -> Vec<EnvelopeItem>
where
    F: Fn(&CodeEntity) -> Option<String>,
{
    let Some(target_idx) = built.graph.node(target_entity) else {
        return Vec::new();
    };

    let by_location = location_index(built);
    let mut signatures: HashMap<String, String> = HashMap::new();
    for file_facts in facts {
        for fact in &file_facts.functions {
            if let Some(e) = entity_for(built, &by_location, &file_facts.file, fact) {
                signatures.insert(e.id.clone(), fact.signature.clone());
            }
        }
    }

    let proposals = infer::infer_channels(built);
    let distances = bfs_distance(built, target_idx);
    let target_entity_ref = built.graph.entity(target_idx);
    let target_container = container_of(built, target_idx);

    let mut scored: Vec<(NodeIndex, u32, usize, ScoreReasons)> = Vec::new();
    for (idx, entity) in built.graph.entities() {
        if idx == target_idx {
            continue;
        }
        if !matches!(entity.entity_type, NodeType::Function | NodeType::Class) {
            continue;
        }
        let Some(&distance) = distances.get(&idx) else {
            continue; // unreachable from the target: out of scope
        };
        let idx_container = container_of(built, idx);
        let same_container = (target_container.is_some() && target_container == idx_container)
            || Some(idx) == target_container
            || Some(target_idx) == idx_container;
        let reasons = ScoreReasons {
            distance: DISTANCE_WEIGHT / (1 + distance as u32),
            adjacency: if same_container { ADJACENCY_WEIGHT } else { 0 },
            effect_overlap: effect_overlap(&target_entity_ref.effect_signature, &entity.effect_signature),
            sanctioned_exemplar: if is_exemplar(&entity.id, &proposals) { EXEMPLAR_WEIGHT } else { 0 },
        };
        scored.push((idx, reasons.total(), distance, reasons));
    }
    scored.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| a.2.cmp(&b.2))
            .then_with(|| built.graph.entity(a.0).id.cmp(&built.graph.entity(b.0).id))
    });

    let mut items = Vec::new();
    let mut budget = config.token_budget;

    // The target is always included, full fidelity, outside the budget —
    // an envelope that can't afford to show you the thing you're editing
    // isn't an envelope.
    if let Some(text) = render(target_entity_ref) {
        items.push(EnvelopeItem {
            entity: target_entity_ref.id.clone(),
            file: target_entity_ref.file.clone(),
            fidelity: Fidelity::Full,
            score: u32::MAX,
            reasons: ScoreReasons::default(),
            text,
        });
    }

    for (idx, score, distance, reasons) in scored {
        let entity = built.graph.entity(idx);
        let in_zone = distance <= config.edit_zone_hops;
        if in_zone {
            if let Some(text) = render(entity) {
                let cost = estimate_tokens(&text);
                if cost <= budget {
                    budget -= cost;
                    items.push(EnvelopeItem {
                        entity: entity.id.clone(),
                        file: entity.file.clone(),
                        fidelity: Fidelity::Full,
                        score,
                        reasons,
                        text,
                    });
                    continue;
                }
            }
        }
        let sig = signatures.get(&entity.id).map(String::as_str);
        let text = skeleton_for(entity, sig);
        if text.is_empty() {
            continue;
        }
        let cost = estimate_tokens(&text);
        if cost <= budget {
            budget -= cost;
            items.push(EnvelopeItem {
                entity: entity.id.clone(),
                file: entity.file.clone(),
                fidelity: Fidelity::Skeleton,
                score,
                reasons,
                text,
            });
        }
    }

    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{build, effects, source};
    use slop_resolve::{Resolver, ScipResolver};
    use std::path::PathBuf;

    fn fixture(name: &str) -> (build::BuiltGraph, Vec<FileFacts>, PathBuf) {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures")
            .join(name);
        let resolver = ScipResolver::load(&root.join("index.scip")).expect("fixture index");
        let mut built = build::build_graph(&resolver);
        effects::infer_effects(&mut built);
        let facts = source::parse_repo(&root, &resolver.files());
        (built, facts, root)
    }

    #[test]
    fn unknown_target_yields_empty_envelope() {
        let (built, facts, root) = fixture("toy_repo");
        let items = build_envelope(&built, &facts, &root, "nope::nowhere", &EnvelopeConfig::default());
        assert!(items.is_empty());
    }

    #[test]
    fn target_is_always_present_at_full_fidelity() {
        let (built, facts, root) = fixture("toy_repo");
        let items = build_envelope(
            &built,
            &facts,
            &root,
            "core.http_client::HttpClient::get",
            &EnvelopeConfig::default(),
        );
        let target = items
            .iter()
            .find(|i| i.entity == "core.http_client::HttpClient::get")
            .expect("target present");
        assert_eq!(target.fidelity, Fidelity::Full);
        assert!(target.text.contains("def get"));
    }

    #[test]
    fn direct_neighbor_gets_full_fidelity_in_zone() {
        let (built, facts, root) = fixture("toy_repo");
        // fetch_forecast calls HttpClient.get directly (distance 1).
        let items = build_envelope(
            &built,
            &facts,
            &root,
            "services.weather::fetch_forecast",
            &EnvelopeConfig::default(),
        );
        let neighbor = items
            .iter()
            .find(|i| i.entity == "core.http_client::HttpClient::get")
            .expect("direct neighbor present");
        assert_eq!(neighbor.fidelity, Fidelity::Full);
        assert!(neighbor.reasons.effect_overlap > 0);
    }

    #[test]
    fn tiny_budget_still_keeps_the_target() {
        let (built, facts, root) = fixture("toy_repo");
        let cfg = EnvelopeConfig {
            token_budget: 0,
            edit_zone_hops: 1,
        };
        let items = build_envelope(&built, &facts, &root, "services.weather::fetch_forecast", &cfg);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].entity, "services.weather::fetch_forecast");
    }

    #[test]
    fn far_entities_are_skeletons_not_full_source() {
        let (built, facts, root) = fixture("toy_repo_slopped");
        // routing.py has nothing to do with utils.scoring — far in the graph.
        let items = build_envelope(
            &built,
            &facts,
            &root,
            "services.routing::route_event",
            &EnvelopeConfig::default(),
        );
        if let Some(far) = items.iter().find(|i| i.entity == "utils.scoring::compute_risk_score") {
            assert_eq!(far.fidelity, Fidelity::Skeleton);
            assert!(far.text.contains("..."));
        }
    }
}
