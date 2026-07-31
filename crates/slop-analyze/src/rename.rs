//! SCIP-verified identifier rewriting — the mechanical core of the rename
//! auto-fixes (D9 path a, extending `fix.rs` beyond behaviour-inert comment
//! deletion). Given every SCIP occurrence of a symbol and a new name, rewrite
//! each occurrence and re-parse every touched file.
//!
//! Unlike over-commenting (deleting a comment can never change behaviour), a
//! rename is only *verified*-safe, not safe by construction: it rests on SCIP
//! having resolved every reference. Two gates enforce that:
//!
//! 1. **Every occurrence must be locatable.** SCIP columns arrive in an
//!    unspecified unit, so we don't trust them blindly: an occurrence is
//!    located by matching the old name as a whole token at the SCIP column
//!    *or*, failing that, at the one unambiguous word-bounded position on its
//!    line. If any occurrence can't be pinned down, the whole rename aborts —
//!    a partial rewrite would leave a dangling reference.
//! 2. **Every touched file must still parse.** Re-parse after rewriting; abort
//!    if not.
//!
//! The residual risk this can't see is a *dynamically*-dispatched call SCIP
//! never recorded (`getattr`, duck typing). That's why `slop fix` renames are
//! dry-run by default and reported with their reference counts for review.

use std::collections::HashMap;
use std::path::Path;

use petgraph::visit::EdgeRef;
use petgraph::Direction;
use slop_graph::{EdgeKind, NodeType};
use slop_resolve::Resolver;

use crate::build::BuiltGraph;
use crate::findings::Finding;

/// A single occurrence to rewrite: repo-relative file and the 0-based
/// (line, col) SCIP start position of the name token.
#[derive(Debug, Clone)]
pub struct NameOccurrence {
    pub file: String,
    pub line: u32,
    pub col: u32,
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Byte offset within `line` where `old` sits as a whole token, given the SCIP
/// `col` hint. Trust the column if it lands on `old` word-bounded; otherwise
/// fall back to the single unambiguous word-bounded match. `None` when the
/// name isn't there or appears ambiguously (both abort the rename).
fn locate(line: &str, col: usize, old: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    let bounded = |start: usize| -> bool {
        let end = start + old.len();
        end <= line.len()
            && &line[start..end] == old
            && (start == 0 || !is_ident_byte(bytes[start - 1]))
            && (end == line.len() || !is_ident_byte(bytes[end]))
    };
    if col <= line.len() && bounded(col) {
        return Some(col);
    }
    let mut found = None;
    let mut from = 0;
    while let Some(rel) = line[from..].find(old) {
        let abs = from + rel;
        if bounded(abs) {
            if found.is_some() {
                return None; // ambiguous — refuse to guess
            }
            found = Some(abs);
        }
        from = abs + 1;
    }
    found
}

/// 0-based line start byte offsets.
fn line_starts(src: &str) -> Vec<usize> {
    let mut starts = vec![0];
    for (i, b) in src.bytes().enumerate() {
        if b == b'\n' {
            starts.push(i + 1);
        }
    }
    starts
}

/// Rewrite `old_name` -> `new_name` at every occurrence, grouped per file,
/// with the two safety gates above. Returns the rewritten source for each
/// touched file, or an `Err` describing why the rename was refused (so the
/// caller can skip it and report, never write a partial rename).
pub fn rewrite_occurrences(
    sources: &HashMap<String, String>,
    occurrences: &[NameOccurrence],
    old_name: &str,
    new_name: &str,
) -> Result<HashMap<String, String>, String> {
    if new_name.is_empty() || new_name == old_name {
        return Err("new name is empty or unchanged".to_string());
    }
    let mut by_file: HashMap<&str, Vec<&NameOccurrence>> = HashMap::new();
    for occ in occurrences {
        by_file.entry(occ.file.as_str()).or_default().push(occ);
    }

    let mut rewritten = HashMap::new();
    for (file, occs) in by_file {
        let src = sources
            .get(file)
            .ok_or_else(|| format!("no source loaded for {file}"))?;
        let starts = line_starts(src);

        // Resolve each occurrence to an absolute byte span, deduping repeats.
        let mut spans: Vec<(usize, usize)> = Vec::new();
        for occ in &occs {
            let line_start = *starts
                .get(occ.line as usize)
                .ok_or_else(|| format!("{file}: occurrence line {} out of range", occ.line))?;
            let line = src[line_start..].split('\n').next().unwrap_or("");
            let rel = locate(line, occ.col as usize, old_name).ok_or_else(|| {
                format!(
                    "{file}:{}: could not locate `{old_name}` unambiguously — rename aborted",
                    occ.line + 1
                )
            })?;
            let start = line_start + rel;
            let span = (start, start + old_name.len());
            if !spans.contains(&span) {
                spans.push(span);
            }
        }

        // Splice right-to-left so earlier offsets stay valid.
        spans.sort_by(|a, b| b.0.cmp(&a.0));
        let mut out = src.clone();
        for (start, end) in spans {
            out.replace_range(start..end, new_name);
        }

        // Re-parse in the file's own language. This used to always run the
        // Python parser, so the guard was meaningless off Python: it rejected
        // every valid `.ts`/`.rs` rewrite and would have accepted anything the
        // Python grammar happened to admit.
        let reparses = match slop_parse::Language::from_path(file) {
            Some(lang) => lang.parse(&out).is_ok(),
            None => false,
        };
        if !reparses {
            return Err(format!(
                "rewriting {file} produced source that no longer parses — rename aborted"
            ));
        }
        rewritten.insert(file.to_string(), out);
    }
    Ok(rewritten)
}

/// camelCase / PascalCase -> snake_case. A `_` is inserted only before an
/// uppercase letter that follows a lowercase letter or digit, so runs of
/// capitals (acronyms like `HTTP`) collapse to lowercase rather than exploding
/// into `h_t_t_p`.
pub fn to_snake_case(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    let mut prev_lower_or_digit = false;
    for ch in name.chars() {
        if ch.is_uppercase() && prev_lower_or_digit {
            out.push('_');
        }
        out.extend(ch.to_lowercase());
        prev_lower_or_digit = ch.is_lowercase() || ch.is_ascii_digit();
    }
    out
}

/// The mechanical target name for a `naming-convention` finding: the
/// snake_case form, or `None` when it's a no-op.
///
/// Deliberately narrow. Only case conversion is deterministic and correct.
/// `slop-name` markers are *not* mechanized: `_v2` is a version marker but
/// `when_finding_is_new` legitimately ends in `_new`, and the finding's whole
/// point is that a human should choose a *descriptive* replacement, not that a
/// marker be blindly stripped.
fn target_name(old: &str) -> Option<String> {
    let new = to_snake_case(old);
    (!new.is_empty() && new != old && !new.starts_with(|c: char| c.is_ascii_digit()))
        .then_some(new)
}

/// True when `node` is a method (contained by a Class). Methods are excluded
/// from mechanical rename: they can be dispatched dynamically (SCIP misses the
/// call) and many are framework contracts invoked by name — `setUp`,
/// `tearDown`, dunder overrides — where a snake_case rename silently breaks the
/// framework. Renaming those is a human call.
fn is_method(built: &BuiltGraph, node: petgraph::graph::NodeIndex) -> bool {
    built
        .graph
        .graph
        .edges_directed(node, Direction::Incoming)
        .any(|e| {
            *e.weight() == EdgeKind::Contains
                && built.graph.entity(e.source()).entity_type == NodeType::Class
        })
}

/// A rename slop is ready to apply, with counts for the reviewer.
pub struct RenamePlan {
    pub rule: &'static str,
    pub entity: String,
    pub old_name: String,
    pub new_name: String,
    /// Total occurrences rewritten (definition + references).
    pub occurrences: usize,
    /// Distinct files touched.
    pub file_count: usize,
}

pub enum RenameOutcome {
    Planned(RenamePlan),
    Skipped { entity: String, reason: String },
}

/// The result of planning every rename: the per-finding outcomes plus the
/// accumulated final source for each file a successful rename touched. Renames
/// compose — two renames in one module both land in that file's entry — so the
/// caller writes each file in `files` exactly once.
pub struct Renames {
    pub outcomes: Vec<RenameOutcome>,
    pub files: HashMap<String, String>,
}

/// The SCIP symbol whose definition node carries `entity` id, if any.
pub(crate) fn symbol_for<'a>(built: &'a BuiltGraph, entity: &str) -> Option<&'a str> {
    let node = built.graph.node(entity)?;
    built
        .by_symbol
        .iter()
        .find(|(_, &idx)| idx == node)
        .map(|(sym, _)| sym.as_str())
}

/// Plan every mechanical rename implied by the `naming-convention` /
/// `slop-name` findings: compute the target name, gate on collision and SCIP
/// symbol resolution, gather all occurrences, and rewrite (with the re-parse /
/// locatability gates in `rewrite_occurrences`). Each finding becomes either a
/// ready `RenamePlan` or a `Skipped` with the reason — the caller applies or
/// reports. Each entity is planned at most once.
pub fn plan_renames(
    built: &BuiltGraph,
    resolver: &dyn Resolver,
    repo: &Path,
    findings: &[Finding],
) -> Renames {
    let mut outcomes = Vec::new();
    let mut done: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Accumulates edits so successive renames in the same file compose instead
    // of the last write clobbering earlier ones. Only files that a rename
    // actually rewrote end up in `touched` (and thus get written).
    let mut cache: HashMap<String, String> = HashMap::new();
    let mut touched: std::collections::HashSet<String> = std::collections::HashSet::new();
    // New ids already claimed this run — so two renames can't converge on the
    // same name (e.g. `foo_v2` and `foo_new` both -> `foo`) and silently
    // shadow each other; the graph check only sees pre-existing definitions.
    let mut claimed: std::collections::HashSet<String> = std::collections::HashSet::new();

    for finding in findings.iter().filter(|f| f.rule == "naming-convention") {
        if !done.insert(finding.entity.clone()) {
            continue;
        }
        let old_name = finding.entity.rsplit("::").next().unwrap_or(&finding.entity);
        let skip = |reason: String| RenameOutcome::Skipped {
            entity: finding.entity.clone(),
            reason,
        };

        let Some(new_name) = target_name(old_name) else {
            continue; // no-op after case conversion
        };

        let Some(node) = built.graph.node(&finding.entity) else {
            continue;
        };
        if is_method(built, node) {
            outcomes.push(skip(
                "method — rename by hand (dynamic dispatch / framework contracts can't be verified mechanically)".to_string(),
            ));
            continue;
        }

        // Collision: a sibling scope already owns, or another rename this run
        // already claimed, the new name.
        let prefix = finding.entity.rsplit_once("::").map(|(p, _)| p).unwrap_or("");
        let new_id = if prefix.is_empty() {
            new_name.clone()
        } else {
            format!("{prefix}::{new_name}")
        };
        if built.graph.node(&new_id).is_some() || claimed.contains(&new_id) {
            outcomes.push(skip(format!(
                "would collide with `{new_id}` — resolve by hand"
            )));
            continue;
        }

        let Some(symbol) = symbol_for(built, &finding.entity) else {
            outcomes.push(skip("no SCIP symbol — can't verify references".to_string()));
            continue;
        };
        let symbol = symbol.to_string();

        // Every occurrence of the symbol across the repo.
        let occs: Vec<NameOccurrence> = resolver
            .files()
            .iter()
            .flat_map(|file| {
                resolver
                    .occurrences_in(file)
                    .iter()
                    .filter(|o| o.symbol == symbol)
                    .map(move |o| NameOccurrence {
                        file: file.to_string(),
                        line: o.range.start_line,
                        col: o.range.start_col,
                    })
            })
            .collect();
        if occs.is_empty() {
            outcomes.push(skip("no occurrences recorded in the index".to_string()));
            continue;
        }

        // Ensure the current (possibly already-rewritten) source of each
        // touched file is in the cache, reading pristine on first touch.
        let mut file_set: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut read_failed = None;
        for occ in &occs {
            file_set.insert(&occ.file);
            if cache.contains_key(&occ.file) {
                continue;
            }
            match std::fs::read_to_string(repo.join(&occ.file)) {
                Ok(s) => {
                    cache.insert(occ.file.clone(), s);
                }
                Err(e) => {
                    read_failed = Some(format!("reading {}: {e}", occ.file));
                    break;
                }
            }
        }
        if let Some(reason) = read_failed {
            outcomes.push(skip(reason));
            continue;
        }

        match rewrite_occurrences(&cache, &occs, old_name, &new_name) {
            Ok(files) => {
                let file_count = files.len();
                for (f, src) in files {
                    touched.insert(f.clone());
                    cache.insert(f, src);
                }
                claimed.insert(new_id);
                outcomes.push(RenameOutcome::Planned(RenamePlan {
                    rule: finding.rule,
                    entity: finding.entity.clone(),
                    old_name: old_name.to_string(),
                    new_name,
                    occurrences: occs.len(),
                    file_count,
                }));
            }
            Err(reason) => outcomes.push(skip(reason)),
        }
    }

    let files = cache
        .into_iter()
        .filter(|(f, _)| touched.contains(f))
        .collect();
    Renames { outcomes, files }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_name_is_case_conversion_only() {
        assert_eq!(target_name("fetchData").as_deref(), Some("fetch_data"));
        assert_eq!(target_name("already_ok"), None);
    }

    fn sources(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(f, s)| (f.to_string(), s.to_string())).collect()
    }

    #[test]
    fn snake_case_handles_acronyms() {
        assert_eq!(to_snake_case("fetchData"), "fetch_data");
        assert_eq!(to_snake_case("HTTPServer"), "httpserver");
        assert_eq!(to_snake_case("getHTTPResponse"), "get_httpresponse");
        assert_eq!(to_snake_case("already_snake"), "already_snake");
    }

    #[test]
    fn rewrites_def_and_all_references() {
        // Definition + a call on another line + a call sharing a line with an
        // unrelated identifier of a different name.
        let src = "def fetchData():\n    return 1\n\nx = fetchData()\ny = fetchData() + other\n";
        let occs = vec![
            NameOccurrence { file: "m.py".into(), line: 0, col: 4 },
            NameOccurrence { file: "m.py".into(), line: 3, col: 4 },
            NameOccurrence { file: "m.py".into(), line: 4, col: 4 },
        ];
        let out = rewrite_occurrences(&sources(&[("m.py", src)]), &occs, "fetchData", "fetch_data")
            .unwrap();
        let got = &out["m.py"];
        assert!(!got.contains("fetchData"), "{got}");
        assert_eq!(got.matches("fetch_data").count(), 3);
        assert!(got.contains("+ other"), "unrelated identifier untouched: {got}");
    }

    #[test]
    fn column_hint_wrong_but_name_unique_still_locates() {
        // Simulate a bogus/mismatched column: the unique word-bounded fallback
        // still finds the token.
        let src = "result = compute()\n";
        let occs = vec![NameOccurrence { file: "m.py".into(), line: 0, col: 999 }];
        let out =
            rewrite_occurrences(&sources(&[("m.py", src)]), &occs, "compute", "calc").unwrap();
        assert_eq!(out["m.py"], "result = calc()\n");
    }

    #[test]
    fn aborts_when_occurrence_not_locatable() {
        // SCIP claims an occurrence on a line where the name doesn't appear as
        // a token — abort rather than leave a dangling reference.
        let src = "def foo():\n    pass\n";
        let occs = vec![NameOccurrence { file: "m.py".into(), line: 1, col: 4 }];
        assert!(rewrite_occurrences(&sources(&[("m.py", src)]), &occs, "foo", "bar").is_err());
    }

    #[test]
    fn aborts_when_result_does_not_parse() {
        // Renaming to a Python keyword breaks the parse — must abort.
        let src = "def foo():\n    return foo()\n";
        let occs = vec![
            NameOccurrence { file: "m.py".into(), line: 0, col: 4 },
            NameOccurrence { file: "m.py".into(), line: 1, col: 11 },
        ];
        assert!(rewrite_occurrences(&sources(&[("m.py", src)]), &occs, "foo", "return").is_err());
    }

    #[test]
    fn rewrites_across_multiple_files() {
        let def = "def oldName():\n    return 1\n";
        let user = "from m import oldName\nprint(oldName())\n";
        let out = rewrite_occurrences(
            &sources(&[("m.py", def), ("u.py", user)]),
            &[
                NameOccurrence { file: "m.py".into(), line: 0, col: 4 },
                NameOccurrence { file: "u.py".into(), line: 0, col: 14 },
                NameOccurrence { file: "u.py".into(), line: 1, col: 6 },
            ],
            "oldName",
            "new_name",
        )
        .unwrap();
        assert!(out["m.py"].contains("def new_name()"));
        assert!(out["u.py"].contains("from m import new_name"));
        assert!(out["u.py"].contains("print(new_name())"));
    }
}
