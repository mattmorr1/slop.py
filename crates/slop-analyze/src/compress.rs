//! Zoned, graph-distance compression of a source file (D10/D11): full
//! fidelity inside the *edit zone* (entities within `edit_zone_hops` of an
//! edited locus, over Contains/Calls/Imports edges), deterministic skeletons
//! beyond it. This is what makes the read-path harness token-efficient — the
//! agent keeps the code it's actually working near, and pays only a
//! signature + effect-signature + docstring contract for everything else.
//!
//! Falls back to a plain noise-strip when there's no edit zone yet or no
//! graph coverage, so it never over-compresses a cold read.

use crate::build::BuiltGraph;
use crate::envelope::proximity_distances;
use crate::skeleton::{skeleton_for, strip_noise};
use crate::source::{entity_for, location_index, FileFacts};

pub struct CompressConfig {
    /// Graph hops from the edit zone that stay full-fidelity.
    pub edit_zone_hops: usize,
    /// Files shorter than this (in lines) skip skeletonization entirely —
    /// not worth a graph walk, and the token win is negligible.
    pub min_lines: usize,
}

impl Default for CompressConfig {
    fn default() -> Self {
        Self {
            edit_zone_hops: 1,
            min_lines: 40,
        }
    }
}

/// A per-function decision, for reporting/measurement.
pub struct CompressStats {
    pub total_functions: usize,
    pub skeletonized: usize,
    pub original_chars: usize,
    pub compressed_chars: usize,
}

/// Optional per-skeleton enrichment: `densify(entity_id) -> one-line summary`.
/// The read hook passes `None` (deterministic, hot path); an offline caller can
/// pass an LLM-backed closure to add a `# summary:` line to each skeleton.
pub type Densifier<'a> = dyn Fn(&str) -> Option<String> + 'a;

/// Compress `source` (the text of `file`) around `edit_loci` (entity ids in
/// the edit zone). Returns the rewritten text and stats.
pub fn compress_file(
    built: &BuiltGraph,
    facts: &[FileFacts],
    source: &str,
    file: &str,
    edit_loci: &[String],
    config: &CompressConfig,
    densify: Option<&Densifier>,
) -> (String, CompressStats) {
    let lines: Vec<&str> = source.lines().collect();
    let plain = |reason_stats: usize| {
        let out = strip_noise(source);
        (
            out.clone(),
            CompressStats {
                total_functions: reason_stats,
                skeletonized: 0,
                original_chars: source.len(),
                compressed_chars: out.len(),
            },
        )
    };

    if edit_loci.is_empty() || lines.len() < config.min_lines {
        return plain(0);
    }
    let starts: Vec<_> = edit_loci
        .iter()
        .filter_map(|id| built.graph.node(id))
        .collect();
    if starts.is_empty() {
        return plain(0);
    }
    let dist = proximity_distances(built, &starts);
    let by_loc = location_index(built);

    // Collect (start, end, skeleton) for each out-of-zone function in this file.
    let mut regions: Vec<(usize, usize, String)> = Vec::new();
    let mut total = 0usize;
    for ff in facts.iter().filter(|f| f.file == file) {
        for fact in &ff.functions {
            let Some(entity) = entity_for(built, &by_loc, &ff.file, fact) else {
                continue;
            };
            total += 1;
            let Some(idx) = built.graph.node(&entity.id) else {
                continue;
            };
            let in_zone = dist.get(&idx).is_some_and(|d| *d <= config.edit_zone_hops);
            if in_zone {
                continue;
            }
            let mut sk = skeleton_for(entity, Some(&fact.signature));
            if sk.is_empty() {
                continue;
            }
            if let Some(summary) = densify.and_then(|f| f(&entity.id)) {
                sk = format!("# summary: {}\n{sk}", summary.trim());
            }
            regions.push((fact.start_line as usize, fact.end_line as usize, sk));
        }
    }

    if regions.is_empty() {
        return plain(total);
    }

    // Prune nested/overlapping regions, keeping the outermost (a skeletonized
    // class body already subsumes its methods).
    regions.sort_by_key(|r| (r.0, std::cmp::Reverse(r.1)));
    let mut pruned: Vec<(usize, usize, String)> = Vec::new();
    let mut covered_to: Option<usize> = None;
    for (s, e, sk) in regions {
        if covered_to.is_some_and(|c| s <= c) {
            continue;
        }
        covered_to = Some(e);
        pruned.push((s, e, sk));
    }

    let last = lines.len().saturating_sub(1);
    let mut out = String::new();
    let mut cursor = 0usize;
    let skeletonized = pruned.len();
    for (s, e, sk) in &pruned {
        let s = *s;
        let e = (*e).min(last);
        if s > cursor && s <= lines.len() {
            out.push_str(&strip_noise(&lines[cursor..s].join("\n")));
        }
        out.push_str(sk);
        cursor = e + 1;
    }
    if cursor < lines.len() {
        out.push_str(&strip_noise(&lines[cursor..].join("\n")));
    }

    let stats = CompressStats {
        total_functions: total,
        skeletonized,
        original_chars: source.len(),
        compressed_chars: out.len(),
    };
    (out, stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use slop_resolve::{Resolver, ScipResolver};

    fn analysis(fixture: &str) -> (BuiltGraph, Vec<FileFacts>, PathBuf) {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures")
            .join(fixture);
        let resolver = ScipResolver::load(&root.join("index.scip")).expect("index");
        let mut built = crate::build::build_graph(&resolver);
        crate::effects::infer_effects(&mut built);
        let facts = crate::source::parse_repo(&root, &resolver.files());
        (built, facts, root)
    }

    #[test]
    fn empty_edit_zone_falls_back_to_strip_noise() {
        let (built, facts, root) = analysis("toy_repo_slopped");
        let file = "utils/cleaning.py";
        let src = std::fs::read_to_string(root.join(file)).unwrap();
        let (out, stats) = compress_file(
            &built,
            &facts,
            &src,
            file,
            &[],
            &CompressConfig::default(),
            None,
        );
        assert_eq!(stats.skeletonized, 0);
        assert_eq!(out, strip_noise(&src));
    }

    #[test]
    fn out_of_zone_functions_become_skeletons() {
        let (built, facts, root) = analysis("toy_repo_slopped");
        let file = "utils/cleaning.py";
        let src = std::fs::read_to_string(root.join(file)).unwrap();

        // Pick a real function in this file as the edit locus, force everything
        // to be out of zone with hops=0, and low min_lines so it engages.
        let locus = built
            .graph
            .entities()
            .find(|(_, e)| e.file == file && e.entity_type == slop_graph::NodeType::Function)
            .map(|(_, e)| e.id.clone())
            .expect("a function in the file");

        let (out, stats) = compress_file(
            &built,
            &facts,
            &src,
            file,
            &[locus.clone()],
            &CompressConfig {
                edit_zone_hops: 0,
                min_lines: 0,
            },
            None,
        );

        assert!(stats.total_functions > 1, "fixture should have >1 function");
        assert!(stats.skeletonized >= 1, "expected some skeletons");
        // The locus itself (hop 0) stays full; skeletons carry the `...` marker.
        assert!(out.contains("..."), "skeletons present: {out}");
        assert!(
            stats.compressed_chars <= stats.original_chars,
            "compression should not grow the file"
        );
    }

    #[test]
    fn densifier_adds_summary_lines() {
        let (built, facts, root) = analysis("toy_repo_slopped");
        let file = "utils/cleaning.py";
        let src = std::fs::read_to_string(root.join(file)).unwrap();
        let locus = built
            .graph
            .entities()
            .find(|(_, e)| e.file == file && e.entity_type == slop_graph::NodeType::Function)
            .map(|(_, e)| e.id.clone())
            .unwrap();
        let densify = |_id: &str| Some("does a thing".to_string());
        let (out, stats) = compress_file(
            &built,
            &facts,
            &src,
            file,
            &[locus],
            &CompressConfig {
                edit_zone_hops: 0,
                min_lines: 0,
            },
            Some(&densify),
        );
        if stats.skeletonized > 0 {
            assert!(out.contains("# summary: does a thing"), "{out}");
        }
    }
}
