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
use crate::skeleton::{annotation_prefix, skeleton_for, strip_noise};
use crate::source::{entity_for, location_index, FileFacts};
use slop_parse::Language;

/// The leading-whitespace prefix of `line` (spaces or tabs), verbatim.
fn indent_prefix(line: &str) -> &str {
    &line[..line.len() - line.trim_start().len()]
}

/// Prefix every non-empty line of `block` with `pad`, so a skeleton (emitted
/// at column 0) sits at the same column as the code it replaces. Preserves the
/// trailing newline.
fn indent_block(block: &str, pad: &str) -> String {
    if pad.is_empty() {
        return block.to_string();
    }
    let mut out: String = block
        .lines()
        .map(|l| {
            if l.is_empty() {
                "\n".to_string()
            } else {
                format!("{pad}{l}\n")
            }
        })
        .collect();
    if !block.ends_with('\n') {
        out.pop();
    }
    out
}

/// Drop up to `pad.len()` leading whitespace chars from each line, normalizing
/// a signature (whose first line starts at column 0 but whose continuation
/// lines carry absolute indentation) back to column 0 so `indent_block` can
/// re-apply one consistent indent. Without this, a decorated/multi-line method
/// signature double-indents and the compressed view stops parsing.
fn dedent(text: &str, pad: &str) -> String {
    if pad.is_empty() {
        return text.to_string();
    }
    text.lines()
        .map(|l| {
            let strip = l.chars().take(pad.len()).take_while(|c| c.is_whitespace()).count();
            format!("{}\n", &l[strip..])
        })
        .collect()
}

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
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct CompressStats {
    pub total_functions: usize,
    pub skeletonized: usize,
    pub original_chars: usize,
    pub compressed_chars: usize,
}

/// Optional per-skeleton enrichment: given the entity, return a one-line
/// summary to prepend as `# summary: ...`. The read hook passes `None`
/// (deterministic, hot path); an offline caller (e.g. `slop compress
/// --densify`) can pass an LLM-backed closure.
pub type Densifier<'a> = dyn Fn(&slop_graph::CodeEntity) -> Option<String> + 'a;

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
    let lang = Language::from_path(file);
    let plain = |reason_stats: usize| {
        let out = strip_noise(source, lang);
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
            // The function's own indentation (its `def`/decorator column).
            let start = fact.start_line as usize;
            let pad = lines.get(start).map(|l| indent_prefix(l)).unwrap_or("");
            // Normalize the signature to column 0 (its continuation lines carry
            // absolute indent), skeletonize, then re-indent uniformly.
            let sig = dedent(&fact.signature, pad);
            let mut sk = skeleton_for(entity, Some(&sig));
            if sk.is_empty() {
                continue;
            }
            if let Some(summary) = densify.and_then(|f| f(entity)) {
                let summary = summary.trim();
                if !summary.is_empty() {
                    sk = format!("{} summary: {summary}\n{sk}", annotation_prefix(lang));
                }
            }
            regions.push((start, fact.end_line as usize, indent_block(&sk, pad)));
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
            out.push_str(&strip_noise(&lines[cursor..s].join("\n"), lang));
        }
        out.push_str(sk);
        cursor = e + 1;
    }
    if cursor < lines.len() {
        out.push_str(&strip_noise(&lines[cursor..].join("\n"), lang));
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
    fn dedent_then_indent_roundtrips_a_method_signature() {
        // A method's signature: first line at col 0 (slice start), the def line
        // carrying absolute indent. Normalizing then re-indenting must restore
        // consistent 4-space indentation, not double it.
        let sig = "@deco\n    def m(self,\n            x):";
        let norm = dedent(sig, "    ");
        assert_eq!(norm, "@deco\ndef m(self,\n        x):\n");
        let back = indent_block(&norm, "    ");
        assert_eq!(back, "    @deco\n    def m(self,\n            x):\n");
    }

    #[test]
    fn indent_block_leaves_blank_lines_empty() {
        assert_eq!(indent_block("a\n\nb\n", "  "), "  a\n\n  b\n");
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
        assert_eq!(out, strip_noise(&src, Some(Language::Python)));
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
            std::slice::from_ref(&locus),
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
    fn skeletonized_methods_keep_class_indentation() {
        // A method skeletonized inside a class must stay indented, or the
        // compressed view is invalid Python. Regression for the col-0 bug.
        let (built, facts, root) = analysis("toy_repo_slopped");
        // Find a file that has an indented (method-level) function.
        let candidate = facts.iter().find_map(|ff| {
            let src = std::fs::read_to_string(root.join(&ff.file)).ok()?;
            let lines: Vec<&str> = src.lines().collect();
            let has_method = ff.functions.iter().any(|f| {
                lines
                    .get(f.start_line as usize)
                    .is_some_and(|l| l.starts_with(' ') || l.starts_with('\t'))
            });
            has_method.then_some((ff.file.clone(), src))
        });
        let Some((file, src)) = candidate else {
            return; // fixture has no methods; nothing to assert
        };

        // Edit locus far away so the methods skeletonize.
        let locus = built
            .graph
            .entities()
            .find(|(_, e)| e.file != file && e.entity_type == slop_graph::NodeType::Function)
            .map(|(_, e)| e.id.clone());
        let Some(locus) = locus else { return };

        let (out, stats) = compress_file(
            &built,
            &facts,
            &src,
            &file,
            &[locus],
            &CompressConfig { edit_zone_hops: 0, min_lines: 0 },
            None,
        );
        if stats.skeletonized == 0 {
            return;
        }
        // The real integrity check: the compressed view must still parse as
        // Python (a col-0 method skeleton would break the class body).
        assert!(
            slop_parse::analyze_file(&out).is_ok(),
            "compressed view must be parseable Python:\n{out}"
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
        let densify = |_e: &slop_graph::CodeEntity| Some("does a thing".to_string());
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
