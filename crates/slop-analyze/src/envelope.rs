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
use crate::relevance::{entity_text, Features, Lexical, RelevanceModel};
use crate::skeleton::{skeleton_for, strip_noise};
use crate::source::{entity_for, location_index, FileFacts};

/// Graph hops a candidate may be from the target (the model's distance features).
const MAX_HOPS: usize = 4;

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
    /// What this item added to the envelope's coverage when it was chosen.
    pub marginal_gain: u64,
    /// Provably equivalent candidates left out because this one covers them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub equivalents: Vec<String>,
    pub text: String,
}

/// Each feature's contribution to the relevance log-odds, in thousandths: why an
/// item is in the envelope, in the model's own terms (the intercept is omitted).
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct ScoreReasons {
    pub distance: i32,
    pub same_file: i32,
    pub same_dir: i32,
    pub line_gap: i32,
    pub container: i32,
    pub effect: i32,
    pub kind: i32,
    pub lexical: i32,
}

impl ScoreReasons {
    fn from_contributions(c: &[i32; 12]) -> Self {
        Self {
            distance: c[1] + c[2] + c[3] + c[4],
            same_file: c[5],
            same_dir: c[6],
            line_gap: c[7],
            container: c[8],
            effect: c[9],
            kind: c[10],
            lexical: c[11],
        }
    }
}

/// How the envelope spends its budget.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Selection {
    /// Budgeted weighted coverage: a candidate pays only for what nothing chosen
    /// already covers, so equivalent copies and redundant siblings stop costing.
    #[default]
    Coverage,
    /// Pack by independent score; the pre-coverage behaviour, kept as the ablation.
    Ranked,
}

pub struct EnvelopeConfig {
    /// Rough token budget (chars / 4) for everything except the target.
    pub token_budget: usize,
    /// BFS hops from the target that still get full-fidelity source.
    pub edit_zone_hops: usize,
    pub selection: Selection,
}

impl Default for EnvelopeConfig {
    fn default() -> Self {
        Self {
            token_budget: 8000,
            edit_zone_hops: 1,
            selection: Selection::Coverage,
        }
    }
}

/// What choosing an entity covers, weighted by its calibrated relevance. A
/// function's E-class is one unit, so a provably equivalent copy adds nothing.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Unit {
    Class(String),
    Entity(NodeIndex),
}

struct Candidate {
    idx: NodeIndex,
    score: u32,
    reasons: ScoreReasons,
    fidelity: Fidelity,
    text: String,
    /// The skeleton to fall back to when a full rendering no longer fits.
    fallback: Option<String>,
    class: Option<String>,
    units: Vec<(Unit, u64)>,
}

impl Candidate {
    fn gain(&self, covered: &HashSet<&Unit>) -> u64 {
        self.units.iter().filter(|(unit, _)| !covered.contains(unit)).map(|(_, weight)| weight).sum()
    }

    fn cost(&self) -> usize {
        estimate_tokens(&self.text)
    }
}

/// `gain / cost` compared exactly in integers, so selection is reproducible.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Ratio {
    gain: u64,
    cost: u64,
}

impl Ord for Ratio {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (u128::from(self.gain) * u128::from(other.cost))
            .cmp(&(u128::from(other.gain) * u128::from(self.cost)))
            .then(self.gain.cmp(&other.gain))
    }
}

impl PartialOrd for Ratio {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Cost-benefit greedy with lazy re-evaluation (CELF): coverage is submodular, so
/// a stale bound only overstates a gain and re-checking the top of the heap is
/// enough. Against the best single fitting candidate, this keeps the classic
/// ½(1 − 1/e) guarantee for budgeted coverage. Returns (candidate, text, gain).
fn select_coverage(candidates: &[Candidate], budget: usize) -> Vec<(usize, bool, u64)> {
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;
    let empty = HashSet::new();
    let mut heap: BinaryHeap<(Ratio, Reverse<usize>)> = candidates
        .iter()
        .enumerate()
        .map(|(i, c)| (Ratio { gain: c.gain(&empty), cost: c.cost() as u64 }, Reverse(i)))
        .collect();
    let (mut covered, mut remaining, mut picked, mut total) = (HashSet::new(), budget, Vec::new(), 0u64);
    while let Some((bound, Reverse(i))) = heap.pop() {
        let candidate = &candidates[i];
        let gain = candidate.gain(&covered);
        if gain == 0 {
            continue;
        }
        if gain != bound.gain {
            heap.push((Ratio { gain, cost: bound.cost }, Reverse(i)));
            continue;
        }
        let fallback = candidate.fallback.as_ref().map(|text| estimate_tokens(text));
        let use_fallback = match (candidate.cost() <= remaining, fallback) {
            (true, _) => false,
            (false, Some(cost)) if cost <= remaining => true,
            _ => continue,
        };
        remaining -= if use_fallback { fallback.unwrap_or_default() } else { candidate.cost() };
        covered.extend(candidate.units.iter().map(|(unit, _)| unit));
        picked.push((i, use_fallback, gain));
        total += gain;
    }
    let best = candidates
        .iter()
        .enumerate()
        .filter(|(_, c)| c.cost() <= budget)
        .map(|(i, c)| (c.gain(&empty), Reverse(i)))
        .max();
    match best {
        Some((gain, Reverse(i))) if gain > total => vec![(i, false, gain)],
        _ => picked,
    }
}

/// Pack by independent score in rank order: the ablation baseline.
fn select_ranked(candidates: &[Candidate], budget: usize) -> Vec<(usize, bool, u64)> {
    let mut remaining = budget;
    let mut picked = Vec::new();
    for (i, candidate) in candidates.iter().enumerate() {
        let fallback = candidate.fallback.as_ref().map(|text| estimate_tokens(text));
        let use_fallback = match (candidate.cost() <= remaining, fallback) {
            (true, _) => false,
            (false, Some(cost)) if cost <= remaining => true,
            _ => continue,
        };
        remaining -= if use_fallback { fallback.unwrap_or_default() } else { candidate.cost() };
        picked.push((i, use_fallback, u64::from(candidate.score)));
    }
    picked
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
/// The id prefix before the last `::`, the benchmark's definition the weights were fit on.
fn container(id: &str) -> &str {
    id.rsplit_once("::").map_or(id, |(prefix, _)| prefix)
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
/// [`build_captured_envelope`] over source read from disk, with the default model.
pub fn build_envelope(
    built: &BuiltGraph,
    facts: &[FileFacts],
    repo_root: &Path,
    target_entity: &str,
    config: &EnvelopeConfig,
) -> Vec<EnvelopeItem> {
    let sources: BTreeMap<String, Arc<str>> = built
        .graph
        .entities()
        .filter_map(|(_, entity)| {
            let text = std::fs::read_to_string(repo_root.join(&entity.file)).ok()?;
            Some((entity.file.clone(), Arc::from(text)))
        })
        .collect();
    let model = RelevanceModel::default_model();
    let lexical = Lexical::build(built, &sources);
    let relevance = Relevance { model: &model, lexical: &lexical };
    build_captured_envelope(built, facts, &sources, target_entity, config, &relevance, |_| true).0
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CatalogEntry {
    pub entity: String,
    pub file: String,
    pub lines: (usize, usize),
    pub skeleton_tokens: usize,
    pub class: bool,
    pub effects: Vec<String>,
}

/// Every function and class an envelope could show, with its skeleton's token
/// cost: the one cost model a benchmark charges every selector with.
pub fn catalog(built: &BuiltGraph, facts: &[FileFacts]) -> Vec<CatalogEntry> {
    let by_location = location_index(built);
    let signatures: HashMap<String, String> = facts
        .iter()
        .flat_map(|file_facts| {
            let by_location = &by_location;
            file_facts.functions.iter().filter_map(move |fact| {
                entity_for(built, by_location, &file_facts.file, fact).map(|e| (e.id.clone(), fact.signature.clone()))
            })
        })
        .collect();
    let mut entries: Vec<CatalogEntry> = built
        .graph
        .entities()
        .filter(|(_, entity)| matches!(entity.entity_type, NodeType::Function | NodeType::Class))
        .map(|(_, entity)| CatalogEntry {
            entity: entity.id.clone(),
            file: entity.file.clone(),
            lines: entity.source_range,
            skeleton_tokens: estimate_tokens(&skeleton_for(entity, signatures.get(&entity.id).map(String::as_str))),
            class: entity.entity_type == NodeType::Class,
            effects: entity.effect_signature.0.iter().map(|effect| format!("{effect:?}")).collect(),
        })
        .collect();
    entries.sort_by(|a, b| a.entity.cmp(&b.entity));
    entries
}

pub fn build_captured_envelope(
    built: &BuiltGraph,
    facts: &[FileFacts],
    sources: &BTreeMap<String, Arc<str>>,
    target_entity: &str,
    config: &EnvelopeConfig,
    relevance: &Relevance<'_>,
    resolved: impl Fn(&str) -> bool,
) -> (Vec<EnvelopeItem>, usize) {
    let target_text = built
        .graph
        .node(target_entity)
        .map(|idx| built.graph.entity(idx))
        .and_then(|entity| sources.get(&entity.file).and_then(|source| entity_text(source, entity)))
        .unwrap_or_default();
    let mut items = build_envelope_with(built, facts, target_entity, config, relevance, &target_text, |entity| {
        render_captured(sources, entity)
    });
    let before = items.len();
    items.retain(|item| resolved(&item.file));
    let omitted = before - items.len();
    (items, omitted)
}

/// The calibrated model and the snapshot's lexical index an envelope ranks with.
pub struct Relevance<'a> {
    pub model: &'a RelevanceModel,
    pub lexical: &'a Lexical,
}

fn directory(file: &str) -> &str {
    file.rsplit_once('/').map_or("", |(dir, _)| dir)
}

fn build_envelope_with<F>(
    built: &BuiltGraph,
    facts: &[FileFacts],
    target_entity: &str,
    config: &EnvelopeConfig,
    relevance: &Relevance<'_>,
    target_text: &str,
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
    let mut classes: HashMap<String, String> = HashMap::new();
    for file_facts in facts {
        for fact in &file_facts.functions {
            if let Some(e) = entity_for(built, &by_location, &file_facts.file, fact) {
                signatures.insert(e.id.clone(), fact.signature.clone());
                let class = [&fact.equiv_hash, &fact.body_hash].into_iter().find(|hash| !hash.is_empty());
                if let Some(class) = class {
                    classes.insert(e.id.clone(), class.clone());
                }
            }
        }
    }

    let distances = bfs_distance(built, target_idx);
    let target = built.graph.entity(target_idx);
    let target_dir = directory(&target.file);
    let lexical = relevance.lexical.scores(target_text);

    // Candidates: the graph neighbourhood and the target's directory. Co-change
    // history says locality matters as much as edges (B4), so both are in play.
    let mut pool: std::collections::BTreeSet<NodeIndex> = distances
        .iter()
        .filter(|(_, distance)| (1..=MAX_HOPS).contains(*distance))
        .map(|(idx, _)| *idx)
        .collect();
    pool.extend(
        built.graph.entities().filter(|(_, entity)| directory(&entity.file) == target_dir).map(|(idx, _)| idx),
    );
    pool.remove(&target_idx);

    let mut candidates: Vec<Candidate> = pool
        .into_iter()
        .filter_map(|idx| {
            let entity = built.graph.entity(idx);
            if !matches!(entity.entity_type, NodeType::Function | NodeType::Class) {
                return None;
            }
            let distance = distances.get(&idx).copied().filter(|distance| *distance <= MAX_HOPS);
            let features = Features {
                distance,
                same_file: entity.file == target.file,
                same_dir: directory(&entity.file) == target_dir,
                line_gap: entity.source_range.0.abs_diff(target.source_range.0),
                same_container: container(&entity.id) == container(&target.id),
                shared_effect: entity.effect_signature.0.iter().any(|effect| target.effect_signature.0.contains(effect)),
                is_class: entity.entity_type == NodeType::Class,
                lexical: lexical.get(&idx).copied().unwrap_or(0.0),
            };
            let (probability, contributions) = relevance.model.probability_ppm(&features);
            let skeleton = skeleton_for(entity, signatures.get(&entity.id).map(String::as_str));
            let full = distance.is_some_and(|d| d <= config.edit_zone_hops).then(|| render(entity)).flatten();
            let (fidelity, text, fallback) = match full {
                Some(text) => (Fidelity::Full, text, (!skeleton.is_empty()).then_some(skeleton)),
                None if skeleton.is_empty() => return None,
                None => (Fidelity::Skeleton, skeleton, None),
            };
            let class = classes.get(&entity.id).cloned();
            let own = class.clone().map_or(Unit::Entity(idx), Unit::Class);
            Some(Candidate {
                idx,
                score: probability,
                reasons: ScoreReasons::from_contributions(&contributions),
                fidelity,
                text,
                fallback,
                class,
                units: vec![(own, u64::from(probability))],
            })
        })
        .collect();
    candidates.sort_by(|a, b| {
        b.score.cmp(&a.score).then_with(|| built.graph.entity(a.idx).id.cmp(&built.graph.entity(b.idx).id))
    });

    let mut items = Vec::new();
    // The target is always included, full fidelity, outside the budget —
    // an envelope that can't afford to show you the thing you're editing
    // isn't an envelope.
    if let Some(text) = render(target) {
        items.push(EnvelopeItem {
            entity: target.id.clone(),
            file: target.file.clone(),
            fidelity: Fidelity::Full,
            score: u32::MAX,
            reasons: ScoreReasons::default(),
            marginal_gain: 0,
            equivalents: Vec::new(),
            text,
        });
    }

    let picked = match config.selection {
        Selection::Coverage => select_coverage(&candidates, config.token_budget),
        Selection::Ranked => select_ranked(&candidates, config.token_budget),
    };
    let chosen: HashSet<usize> = picked.iter().map(|(i, _, _)| *i).collect();
    for (i, use_fallback, gain) in picked {
        let candidate = &candidates[i];
        let entity = built.graph.entity(candidate.idx);
        let equivalents = candidate.class.as_ref().map_or_else(Vec::new, |class| {
            let mut peers: Vec<String> = candidates
                .iter()
                .enumerate()
                .filter(|(j, other)| !chosen.contains(j) && other.class.as_ref() == Some(class))
                .map(|(_, other)| built.graph.entity(other.idx).id.clone())
                .collect();
            peers.sort();
            peers
        });
        let (fidelity, text) = match (use_fallback, &candidate.fallback) {
            (true, Some(skeleton)) => (Fidelity::Skeleton, skeleton.clone()),
            _ => (candidate.fidelity, candidate.text.clone()),
        };
        items.push(EnvelopeItem {
            entity: entity.id.clone(),
            file: entity.file.clone(),
            fidelity,
            score: candidate.score,
            reasons: candidate.reasons,
            marginal_gain: gain,
            equivalents,
            text,
        });
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
        assert!(neighbor.reasons.distance > 0, "a direct edge raises relevance: {:?}", neighbor.reasons);
    }

    #[test]
    fn tiny_budget_still_keeps_the_target() {
        let (built, facts, root) = fixture("toy_repo");
        let cfg = EnvelopeConfig { token_budget: 0, ..EnvelopeConfig::default() };
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

#[cfg(test)]
mod selection {
    use super::*;

    fn candidate(i: usize, class: &str, text_len: usize) -> Candidate {
        Candidate {
            idx: NodeIndex::new(i),
            score: 1000,
            reasons: ScoreReasons::default(),
            fidelity: Fidelity::Skeleton,
            text: "x".repeat(text_len),
            fallback: None,
            class: Some(class.into()),
            units: vec![(Unit::Class(class.into()), 1000)],
        }
    }

    /// A provably equivalent copy covers nothing new, so coverage never pays for it
    /// twice, while score-ranked packing spends budget on every copy.
    #[test]
    fn coverage_skips_equivalent_copies_ranked_does_not() {
        let candidates = vec![candidate(0, "E", 40), candidate(1, "E", 40), candidate(2, "F", 40)];
        let coverage: Vec<usize> = select_coverage(&candidates, 1_000).into_iter().map(|(i, _, _)| i).collect();
        let ranked: Vec<usize> = select_ranked(&candidates, 1_000).into_iter().map(|(i, _, _)| i).collect();
        assert_eq!(coverage, [0, 2]);
        assert_eq!(ranked, [0, 1, 2]);
    }

    /// A full rendering that no longer fits falls back to its skeleton.
    #[test]
    fn coverage_falls_back_to_the_skeleton_when_full_does_not_fit() {
        let mut full = candidate(0, "E", 4_000);
        full.fidelity = Fidelity::Full;
        full.fallback = Some("s".repeat(40));
        let picked = select_coverage(&[full], 100);
        assert_eq!(picked.len(), 1);
        assert!(picked[0].1, "the skeleton was used");
    }
}

/// Entities one proximity hop (Contains/Calls/Imports, either direction) from `entity`.
pub fn neighbors(built: &BuiltGraph, entity: &str) -> Vec<String> {
    let Some(start) = built.graph.node(entity) else { return Vec::new() };
    let mut out: Vec<String> = proximity_distances(built, &[start])
        .into_iter()
        .filter(|(_, distance)| *distance == 1)
        .map(|(idx, _)| built.graph.entity(idx).id.clone())
        .collect();
    out.sort();
    out
}

/// Proximity distance from `entity` to every entity within `max_hops`.
pub fn distances(built: &BuiltGraph, entity: &str, max_hops: usize) -> Vec<(String, usize)> {
    let Some(start) = built.graph.node(entity) else { return Vec::new() };
    let mut out: Vec<(String, usize)> = proximity_distances(built, &[start])
        .into_iter()
        .filter(|(_, distance)| (1..=max_hops).contains(distance))
        .map(|(idx, distance)| (built.graph.entity(idx).id.clone(), distance))
        .collect();
    out.sort();
    out
}
