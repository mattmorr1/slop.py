//! Tier-3 semantic redundancy candidates (D4): effect-bucketed prefilter.
//!
//! `(effect signature, arity, returns-value)` is a cheap deterministic
//! semantic bucket key — two date parsers both land in `(Pure, 1, returns)`.
//! Bucketing collapses the O(n^2) all-pairs problem into tiny equivalence
//! classes. The output is *candidates*, never findings: confirmation is the
//! LLM judge's job (advisory, never blocking).

use std::path::Path;

use serde::Serialize;

use crate::build::BuiltGraph;
use crate::source::{self, FileFacts};

#[derive(Debug, Clone, Serialize)]
pub struct CandidateFn {
    pub entity: String,
    pub file: String,
    /// 0-based inclusive.
    pub lines: (usize, usize),
    pub name: String,
    pub docstring: Option<String>,
    /// Body text, truncated for the judge prompt.
    pub snippet: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CandidatePair {
    pub a: CandidateFn,
    pub b: CandidateFn,
    /// Cheap similarity score used for ranking, 0..=1.
    pub score: f64,
}

const MAX_PAIRS: usize = 20;
const MAX_SNIPPET_LINES: usize = 40;

/// Name-token Jaccard similarity: `parse_date` vs `to_datetime` share
/// "date"-ish tokens; `is_valid_email` vs `is_palindrome` don't.
fn name_similarity(a: &str, b: &str) -> f64 {
    let tokens = |s: &str| -> std::collections::HashSet<String> {
        s.split('_')
            .flat_map(|w| {
                // split camelCase too
                let mut parts = Vec::new();
                let mut cur = String::new();
                for ch in w.chars() {
                    if ch.is_uppercase() && !cur.is_empty() {
                        parts.push(cur.to_lowercase());
                        cur = String::new();
                    }
                    cur.push(ch);
                }
                if !cur.is_empty() {
                    parts.push(cur.to_lowercase());
                }
                parts
            })
            .filter(|t| !t.is_empty())
            .collect()
    };
    let (ta, tb) = (tokens(a), tokens(b));
    if ta.is_empty() || tb.is_empty() {
        return 0.0;
    }
    // Prefix-tolerant match: "date" ~ "datetime", "valid" ~ "validate".
    let matches = |x: &str, y: &str| {
        x == y || (x.len() >= 3 && y.len() >= 3 && (x.starts_with(y) || y.starts_with(x)))
    };
    let hit = |set: &std::collections::HashSet<String>,
               other: &std::collections::HashSet<String>| {
        set.iter().filter(|t| other.iter().any(|o| matches(t, o))).count()
    };
    (hit(&ta, &tb) + hit(&tb, &ta)) as f64 / (ta.len() + tb.len()) as f64
}

/// Build ranked candidate pairs from same-bucket functions.
pub fn candidates(
    built: &BuiltGraph,
    facts: &[FileFacts],
    repo_root: &Path,
) -> Vec<CandidatePair> {
    let index = source::location_index(built);

    struct Item {
        entity: String,
        file: String,
        lines: (usize, usize),
        name: String,
        docstring: Option<String>,
        bucket: (Vec<slop_graph::Effect>, u32, bool),
        body_hash: String,
        structural_hash: String,
    }

    let mut items: Vec<Item> = Vec::new();
    for ff in facts {
        for fact in &ff.functions {
            let Some(entity) = source::entity_for(built, &index, &ff.file, fact) else {
                continue;
            };
            // Dunders and tiny bodies produce junk pairs.
            if fact.name.starts_with("__") || fact.significant_tokens < 10 {
                continue;
            }
            items.push(Item {
                entity: entity.id.clone(),
                file: ff.file.clone(),
                lines: (fact.start_line as usize, fact.end_line as usize),
                name: fact.name.clone(),
                docstring: entity.docstring.clone(),
                bucket: (
                    entity.effect_signature.0.clone(),
                    fact.param_count,
                    fact.returns_value,
                ),
                body_hash: fact.body_hash.clone(),
                structural_hash: fact.structural_hash.clone(),
            });
        }
    }

    let mut pairs: Vec<(usize, usize, f64)> = Vec::new();
    for i in 0..items.len() {
        for j in (i + 1)..items.len() {
            let (a, b) = (&items[i], &items[j]);
            if a.bucket != b.bucket {
                continue;
            }
            // Tier-1/Tier-2 already own exact and structural duplicates.
            if !a.body_hash.is_empty()
                && (a.body_hash == b.body_hash || a.structural_hash == b.structural_hash)
            {
                continue;
            }
            let score = name_similarity(&a.name, &b.name);
            if score <= 0.0 {
                continue;
            }
            pairs.push((i, j, score));
        }
    }
    pairs.sort_by(|x, y| y.2.total_cmp(&x.2));
    pairs.truncate(MAX_PAIRS);

    let snippet = |item: &Item| -> String {
        let Ok(text) = std::fs::read_to_string(repo_root.join(&item.file)) else {
            return String::new();
        };
        text.lines()
            .skip(item.lines.0)
            .take((item.lines.1 - item.lines.0 + 1).min(MAX_SNIPPET_LINES))
            .collect::<Vec<_>>()
            .join("\n")
    };

    pairs
        .into_iter()
        .map(|(i, j, score)| CandidatePair {
            a: CandidateFn {
                entity: items[i].entity.clone(),
                file: items[i].file.clone(),
                lines: items[i].lines,
                name: items[i].name.clone(),
                docstring: items[i].docstring.clone(),
                snippet: snippet(&items[i]),
            },
            b: CandidateFn {
                entity: items[j].entity.clone(),
                file: items[j].file.clone(),
                lines: items[j].lines,
                name: items[j].name.clone(),
                docstring: items[j].docstring.clone(),
                snippet: snippet(&items[j]),
            },
            score,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_similarity_orders_sensibly() {
        assert!(name_similarity("parse_date", "to_datetime") > 0.0);
        assert!(
            name_similarity("parse_date", "parse_date_string")
                > name_similarity("parse_date", "send_email")
        );
        assert_eq!(name_similarity("is_valid_email", "run_pipeline"), 0.0);
    }
}
