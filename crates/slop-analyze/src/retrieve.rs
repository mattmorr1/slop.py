//! Prospective retrieval: does this *proposed* code already exist?
//!
//! Every other signal needs a body to analyse, so none of them answers the
//! question an agent faces *before* it writes: extend something that exists, or
//! create a new one? This compares the callee set of proposed content against
//! the neighborhoods of what the repo already has.
//!
//! Same evidence `parallel-implementation` uses retrospectively, taken one
//! moment earlier. The join crosses a namespace boundary — the graph holds
//! entity ids (`services.alerts::send_alert`) while proposed text yields
//! source-level names (`requests.post`) — so both sides reduce to simple names.
//! That is lossy and largely self-correcting: a name ambiguous enough to collide
//! across modules is called often enough to fail the distinctiveness cutoff.

use std::collections::{BTreeMap, BTreeSet};

use petgraph::visit::EdgeRef;
use petgraph::Direction;
use serde::{Deserialize, Serialize};
use slop_graph::{EdgeKind, NodeType};

use crate::build::BuiltGraph;
use crate::detect::{DISTINCTIVE_DF_DIVISOR, MIN_DISTINCTIVE_CALLEES};

/// Where a function lives: 0-based inclusive line range.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Site {
    pub label: String,
    pub file: String,
    pub start: usize,
    pub end: usize,
}

/// A function the repo already has, with the simple names it calls.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    pub label: String,
    pub file: String,
    pub line: usize,
    pub end: usize,
    /// Up to three functions that reference it, for showing how it is called.
    pub callers: Vec<Site>,
    pub callees: BTreeSet<String>,
}

/// An existing function whose neighborhood overlaps the proposed content.
#[derive(Debug, Clone, Serialize)]
pub struct Match {
    pub label: String,
    pub file: String,
    pub line: usize,
    pub end: usize,
    pub callers: Vec<Site>,
    /// Shared callees that few other functions make — the load-bearing overlap.
    pub distinctive: Vec<String>,
    /// Every shared callee, distinctive or not.
    pub shared: usize,
    /// Squared cosine in parts per million. Integer form keeps artifacts stable.
    pub score_ppm: u32,
}

/// Callee frequencies plus every candidate function, built once per graph.
///
/// Frequency is over *simple* names here, where `parallel-implementation`
/// counts full entity ids. Deliberately different: that detector compares two
/// resolved graph nodes and can afford exact ids, while this one has to meet
/// unresolved source text halfway.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Neighborhood {
    callers: usize,
    df: BTreeMap<String, usize>,
    pub functions: Vec<Candidate>,
}

impl Neighborhood {
    pub fn build(built: &BuiltGraph) -> Self {
        let mut df: BTreeMap<String, usize> = BTreeMap::new();
        let mut callers = 0usize;
        let mut functions = Vec::new();
        for (idx, entity) in built.graph.entities() {
            if entity.entity_type != NodeType::Function
                || crate::source::is_test_file(&entity.file)
                || crate::source::is_test_entity(&entity.id)
                || crate::index::ignored_source_path(&entity.file)
            {
                continue;
            }
            let callees: BTreeSet<String> = built
                .graph
                .graph
                .edges_directed(idx, Direction::Outgoing)
                .filter(|e| *e.weight() == EdgeKind::Calls)
                .map(|e| built.graph.entity(e.target()))
                // Constructing a variant is a `Calls` edge too, so without this
                // three functions that merely mention one enum's variants score
                // as high as three that share real work.
                .filter(|t| t.entity_type == NodeType::Function)
                .map(|t| simple_name(&t.id))
                .collect();
            if callees.is_empty() {
                continue;
            }
            callers += 1;
            // Every function contributes frequency, including ones too small to
            // be worth proposing as a home.
            for callee in &callees {
                *df.entry(callee.clone()).or_default() += 1;
            }
            let mut callers: Vec<Site> = built
                .graph
                .graph
                .edges_directed(idx, Direction::Incoming)
                .filter(|e| *e.weight() == EdgeKind::Calls)
                .map(|e| built.graph.entity(e.source()))
                .filter(|c| c.entity_type == NodeType::Function && !crate::source::is_test_file(&c.file))
                .map(|c| Site { label: c.id.clone(), file: c.file.clone(), start: c.source_range.0, end: c.source_range.1 })
                .collect();
            // A `Calls` edge is any reference; keep a few so one real call site is likely.
            callers.sort_by(|a, b| a.label.cmp(&b.label));
            callers.dedup();
            callers.truncate(3);
            functions.push(Candidate {
                label: entity.id.clone(),
                file: entity.file.clone(),
                line: entity.source_range.0,
                end: entity.source_range.1,
                callers,
                callees,
            });
        }
        functions.sort_by(|a, b| a.label.cmp(&b.label));
        Self {
            callers,
            df,
            functions,
        }
    }

    /// A callee this many functions share is common vocabulary, not a feature.
    pub fn common_df(&self) -> usize {
        (self.callers / DISTINCTIVE_DF_DIVISOR).max(MIN_DISTINCTIVE_CALLEES)
    }

    /// Existing functions whose neighborhood overlaps `proposed`, best first.
    /// `exclude` drops one label, which is what makes leave-one-out evaluation
    /// possible: feed an existing body back in and hide its own entry.
    pub fn matches(
        &self,
        proposed: &BTreeSet<String>,
        exclude: Option<&str>,
        limit: usize,
    ) -> Vec<Match> {
        let cutoff = self.common_df();
        let mut out: Vec<Match> = self
            .functions
            .iter()
            .filter(|c| exclude != Some(c.label.as_str()))
            // If the proposed code calls this function, it is a collaborator, not
            // a home for it. Without this, every caller/callee pair matched: the
            // query text carries its own definition line, so a function and the
            // one that calls it always shared a name.
            .filter(|c| !proposed.contains(&simple_name(&c.label)))
            .filter_map(|c| {
                let shared: Vec<&String> = c.callees.intersection(proposed).collect();
                let mut distinctive: Vec<String> = shared
                    .iter()
                    .filter(|n| self.df.get(**n).is_some_and(|&d| d <= cutoff))
                    .map(|n| (*n).clone())
                    .collect();
                if distinctive.len() < MIN_DISTINCTIVE_CALLEES {
                    return None;
                }
                distinctive.sort();
                // Raw overlap count rewards breadth: a 19-arm command dispatcher
                // calls everything, so it shared four callees with half the repo
                // and ranked above every real match. Normalizing by both set
                // sizes is what makes overlap mean "does the same job" rather
                // than "does many jobs".
                let denominator = c.callees.len().saturating_mul(proposed.len()).max(1);
                let numerator = distinctive.len().saturating_mul(distinctive.len());
                let score_ppm = numerator
                    .saturating_mul(1_000_000)
                    .checked_div(denominator)
                    .unwrap_or(0) as u32;
                Some(Match {
                    label: c.label.clone(),
                    file: c.file.clone(),
                    line: c.line,
                    end: c.end,
                    callers: c.callers.clone(),
                    distinctive,
                    shared: shared.len(),
                    score_ppm,
                })
            })
            .collect();
        out.sort_by(|a, b| {
            b.score_ppm
                .cmp(&a.score_ppm)
                .then(b.distinctive.len().cmp(&a.distinctive.len()))
                .then(a.label.cmp(&b.label))
        });
        out.truncate(limit);
        out
    }
}

/// Last segment of either namespace: `a.b::c` and `a.b.c` both yield `c`.
pub fn simple_name(id: &str) -> String {
    id.rsplit(['.', ':']).next().unwrap_or(id).to_string()
}

/// Simple names the proposed source *references*, as the retrieval query.
///
/// Names it defines are subtracted: `qualified_names` reports a function's own
/// name from its definition line, which made every caller/callee pair look like a
/// match — the caller has the name as a real callee, the callee has it as itself.
pub fn query_from_source(lang: slop_parse::Language, source: &str) -> BTreeSet<String> {
    let defined: BTreeSet<String> = lang
        .parse(source)
        .map(|fns| fns.iter().map(|f| simple_name(&f.name)).collect())
        .unwrap_or_default();
    slop_parse::names::qualified_names(lang, source)
        .iter()
        .map(|n| simple_name(n))
        .filter(|n| !defined.contains(n))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{query_from_source, simple_name};
    use slop_parse::Language;

    #[test]
    fn simple_name_crosses_both_namespaces() {
        assert_eq!(simple_name("services.alerts::send_alert"), "send_alert");
        assert_eq!(simple_name("requests.post"), "post");
        assert_eq!(simple_name("bare"), "bare");
    }

    /// A function's own name is not evidence about where it belongs. Leaving it
    /// in made every caller/callee pair score as a duplicate.
    #[test]
    fn query_excludes_names_the_source_defines() {
        let src = "def before_send_filter(event):\n    cfg = get_settings()\n    return sanitize(event, cfg)\n";
        let q = query_from_source(Language::Python, src);
        assert!(q.contains("get_settings"), "referenced names stay: {q:?}");
        assert!(q.contains("sanitize"));
        assert!(
            !q.contains("before_send_filter"),
            "own name must be dropped: {q:?}"
        );
    }
}
