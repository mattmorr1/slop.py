//! Calibrated relevance: the probability that an entity belongs in view when
//! another is edited, from a logistic model over graph and locality features.
//!
//! The weights are fit on co-change history (B4, `bench/context_bench.py`): a
//! past commit that changed several functions says each needed the others in
//! view. Probabilities, not scores, so packing by probability per token is the
//! greedy for expected recall under a budget. A repository may override the
//! default with its own fit in `.slop/relevance.json`, which then becomes part
//! of the snapshot's identity.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use petgraph::graph::NodeIndex;
use serde::{Deserialize, Serialize};
use slop_graph::{CodeEntity, NodeType};

use crate::build::BuiltGraph;

pub const MODEL_FILE: &str = ".slop/relevance.json";

/// Feature order shared with the benchmark that fits the weights.
pub const FEATURES: [&str; 12] = [
    "bias",
    "d1",
    "d2",
    "d3",
    "d4",
    "same_file",
    "same_dir",
    "file_gap",
    "same_container",
    "shared_effect",
    "is_class",
    "lexical",
];

/// Pooled fit over all four B4 repositories (`context_bench.py --fit-all`, seed 20260923),
/// with distances that stop at hubs (`HUB_FAN_IN`).
const DEFAULT_WEIGHTS: [f64; 12] = [-8.453006, 1.771275, 1.137742, 0.177585, -0.493799, 5.887282, 2.158233, -0.433357, -0.324541, 0.648897, -5.979678, 0.622439];
const DEFAULT_SOURCE: &str = "B4 pooled: vigil@01511904, requests@611c6162, flask@d73fa1cd, httpx@b5addb6; hub fan-in 50";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RelevanceModel {
    pub features: Vec<String>,
    pub weights: Vec<f64>,
    /// Where the weights came from (bench run, repositories, commit).
    pub source: String,
}

impl RelevanceModel {
    /// Pooled fit over vigil, requests, flask and httpx co-change history.
    pub fn default_model() -> Self {
        Self {
            features: FEATURES.iter().map(|name| name.to_string()).collect(),
            weights: DEFAULT_WEIGHTS.to_vec(),
            source: DEFAULT_SOURCE.to_string(),
        }
    }

    /// The repository's own fit if it has one, else the default. A present but
    /// malformed file is an error: silently falling back would hide a bad fit.
    pub fn load(repo: &Path) -> Result<Self> {
        let path = repo.join(MODEL_FILE);
        if !path.exists() {
            return Ok(Self::default_model());
        }
        let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let model: Self = serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        if model.features.iter().map(String::as_str).ne(FEATURES) || model.weights.len() != FEATURES.len() {
            bail!("{} must list exactly these features in order: {}", path.display(), FEATURES.join(", "));
        }
        Ok(model)
    }

    /// Probability in parts per million. Rounding to integers keeps packing
    /// decisions identical wherever the float math differs in its last bit.
    pub fn probability_ppm(&self, features: &Features) -> (u32, [i32; 12]) {
        let values = features.values();
        let mut contributions = [0i32; 12];
        let mut logit = 0.0;
        for (i, (weight, value)) in self.weights.iter().zip(values).enumerate() {
            logit += weight * value;
            contributions[i] = (weight * value * 1000.0).round() as i32;
        }
        let probability = 1.0 / (1.0 + (-logit).exp());
        ((probability * 1_000_000.0).round() as u32, contributions)
    }
}

/// One candidate's features relative to the edit target.
#[derive(Debug, Clone, Copy, Default)]
pub struct Features {
    pub distance: Option<usize>,
    pub same_file: bool,
    pub same_dir: bool,
    pub line_gap: usize,
    pub same_container: bool,
    pub shared_effect: bool,
    pub is_class: bool,
    pub lexical: f64,
}

impl Features {
    pub(crate) fn values(&self) -> [f64; 12] {
        let hop = |n| f64::from(u8::from(self.distance == Some(n)));
        [
            1.0,
            hop(1),
            hop(2),
            hop(3),
            hop(4),
            f64::from(u8::from(self.same_file)),
            f64::from(u8::from(self.same_dir && !self.same_file)),
            if self.same_file { (self.line_gap as f64).ln_1p() } else { 0.0 },
            f64::from(u8::from(self.same_container)),
            f64::from(u8::from(self.shared_effect)),
            f64::from(u8::from(self.is_class)),
            self.lexical.ln_1p(),
        ]
    }
}

/// BM25 over every function's and class's own source, built once per snapshot.
/// Per-document term lists, so a request scores only its candidates: postings
/// made every request walk the ~100k documents that contain `self` (Sentry).
pub struct Lexical {
    terms: HashMap<String, u32>,
    /// Each document's (term, frequency), sorted by term.
    docs: HashMap<NodeIndex, Vec<(u32, u32)>>,
    df: HashMap<u32, u32>,
    lengths: HashMap<NodeIndex, u32>,
    documents: usize,
    average: f64,
}

const K1: f64 = 1.2;
const B: f64 = 0.75;

/// Identifier words, lowercased, longer than one character: `parseHTTPDate`
/// yields `parse`, `date`. Matches the benchmark's tokenizer exactly.
pub fn words(text: &str) -> impl Iterator<Item = String> + '_ {
    let bytes = text.as_bytes();
    let mut i = 0;
    std::iter::from_fn(move || {
        while i < bytes.len() {
            let start = i;
            let byte = bytes[i];
            if byte.is_ascii_alphabetic() {
                i += 1;
                while i < bytes.len() && (bytes[i].is_ascii_lowercase() || bytes[i].is_ascii_digit()) {
                    i += 1;
                }
            } else if byte.is_ascii_digit() {
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
            } else {
                i += 1;
                continue;
            }
            if i - start > 1 {
                return Some(text[start..i].to_ascii_lowercase());
            }
        }
        None
    })
}

pub fn entity_text<'a>(source: &'a str, entity: &CodeEntity) -> Option<String> {
    let (start, end) = entity.source_range;
    let lines: Vec<&'a str> = source.lines().skip(start).take(end.checked_sub(start)? + 1).collect();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

impl Lexical {
    pub fn build(built: &BuiltGraph, sources: &std::collections::BTreeMap<String, std::sync::Arc<str>>) -> Self {
        let mut terms: HashMap<String, u32> = HashMap::new();
        let mut docs: HashMap<NodeIndex, Vec<(u32, u32)>> = HashMap::new();
        let mut df: HashMap<u32, u32> = HashMap::new();
        let mut lengths = HashMap::new();
        let mut total: u64 = 0;
        for (idx, entity) in built.graph.entities() {
            if !matches!(entity.entity_type, NodeType::Function | NodeType::Class) {
                continue;
            }
            let Some(text) = sources.get(&entity.file).and_then(|source| entity_text(source, entity)) else {
                continue;
            };
            let mut counts: HashMap<u32, u32> = HashMap::new();
            let mut length = 0;
            for word in words(&text) {
                let next = terms.len() as u32;
                *counts.entry(*terms.entry(word).or_insert(next)).or_default() += 1;
                length += 1;
            }
            lengths.insert(idx, length);
            total += u64::from(length);
            let mut list: Vec<(u32, u32)> = counts.into_iter().collect();
            list.sort_unstable();
            list.iter().for_each(|(term, _)| *df.entry(*term).or_default() += 1);
            docs.insert(idx, list);
        }
        let documents = lengths.len();
        // An integer total: a float sum in HashMap order would differ per process.
        let average = total as f64 / documents.max(1) as f64;
        Self { terms, docs, df, lengths, documents, average }
    }

    /// BM25 of each candidate against `query` (each distinct word counted once).
    /// Terms are summed in term-id order, so a score is the same whichever
    /// documents are asked about.
    pub fn scores_for(&self, query: &str, candidates: impl IntoIterator<Item = NodeIndex>) -> HashMap<NodeIndex, f64> {
        let mut distinct: Vec<u32> = words(query).filter_map(|word| self.terms.get(&word).copied()).collect();
        distinct.sort_unstable();
        distinct.dedup();
        let n = self.documents as f64;
        let idf: Vec<f64> = distinct
            .iter()
            .map(|term| {
                let df = f64::from(self.df.get(term).copied().unwrap_or(0));
                (1.0 + (n - df + 0.5) / (df + 0.5)).ln()
            })
            .collect();
        let mut scores = HashMap::new();
        for idx in candidates {
            let Some(doc) = self.docs.get(&idx) else { continue };
            let length = f64::from(self.lengths[&idx]);
            let (mut i, mut j, mut score, mut hit) = (0, 0, 0.0, false);
            while i < distinct.len() && j < doc.len() {
                match distinct[i].cmp(&doc[j].0) {
                    std::cmp::Ordering::Less => i += 1,
                    std::cmp::Ordering::Greater => j += 1,
                    std::cmp::Ordering::Equal => {
                        let tf = f64::from(doc[j].1);
                        score += idf[i] * tf * (K1 + 1.0) / (tf + K1 * (1.0 - B + B * length / self.average));
                        hit = true;
                        (i, j) = (i + 1, j + 1);
                    }
                }
            }
            if hit {
                scores.insert(idx, score);
            }
        }
        scores
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizer_matches_the_benchmark() {
        let got: Vec<String> = words("def parseHTTPDate(raw_value, x2): return RFC_1123 + 42").collect();
        assert_eq!(got, ["def", "parse", "date", "raw", "value", "x2", "return", "1123", "42"]);
    }

    /// Expected value from the benchmark's formula; drift here means train/serve skew.
    #[test]
    fn default_model_matches_the_benchmark() {
        let features = Features {
            distance: Some(1),
            same_file: true,
            line_gap: 10,
            same_container: true,
            shared_effect: true,
            lexical: 20.0,
            ..Features::default()
        };
        assert_eq!(RelevanceModel::default_model().probability_ppm(&features).0, 595269);
    }

    #[test]
    fn probabilities_rise_with_locality_and_lexical_overlap() {
        let model = RelevanceModel::default_model();
        let far = Features { distance: Some(4), ..Features::default() };
        let near = Features { distance: Some(1), same_file: true, line_gap: 10, lexical: 20.0, ..Features::default() };
        assert!(model.probability_ppm(&near).0 > model.probability_ppm(&far).0);
    }
}
