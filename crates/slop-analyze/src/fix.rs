//! Mechanistic auto-fixes (D9 path a): behavior-safe, deterministic repairs
//! slop applies itself, no agent required. Currently one rule —
//! `over-commenting`: delete full-line comments that merely restate the
//! adjacent line of code.
//!
//! Deleting a comment can never change behavior, so this is safe *by
//! construction* — but only once "is a comment" is answered per language. A
//! `#`-prefixed line is a comment in Python and an attribute in Rust, and doc
//! comments are a contract rather than noise, so both are left alone. The only
//! remaining judgment is which comments are pure restatement; that's kept
//! deliberately conservative (high word-overlap required) so it favors keeping
//! a comment over wrongly deleting an explanatory one.

use std::collections::HashSet;

use crate::build::BuiltGraph;
use crate::findings::{Finding, Severity};
use crate::source::{self, FileFacts};
use slop_parse::Language;

/// Common words that carry no restatement signal.
const STOPWORDS: &[&str] = &[
    "the", "a", "an", "to", "of", "for", "and", "or", "is", "are", "this", "that", "it", "in",
    "on", "with", "be", "by", "as", "from", "we", "will", "then", "now", "if", "so", "into",
    "our", "its", "these", "those", "each", "all", "any", "here",
];

/// Fraction of a comment's content words that must appear in the adjacent code
/// line for it to count as restatement.
const RESTATE_THRESHOLD: f64 = 0.6;

/// Lowercased word set: split on non-alphanumeric and on camelCase
/// boundaries, so `getUserName` / `get_user_name` both yield {get,user,name}.
fn words(s: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for c in s.chars() {
        if c.is_alphanumeric() {
            if c.is_uppercase() && prev_lower && !cur.is_empty() {
                out.insert(cur.to_lowercase());
                cur.clear();
            }
            cur.push(c);
            prev_lower = c.is_lowercase();
        } else {
            if !cur.is_empty() {
                out.insert(std::mem::take(&mut cur).to_lowercase());
            }
            prev_lower = false;
        }
    }
    if !cur.is_empty() {
        out.insert(cur.to_lowercase());
    }
    out
}

/// Does `comment` (a comment line, marker already stripped) restate `code`?
fn restates(comment: &str, code: &str) -> bool {
    let stop: HashSet<&str> = STOPWORDS.iter().copied().collect();
    let cw: HashSet<String> = words(comment)
        .into_iter()
        .filter(|w| !stop.contains(w.as_str()) && w.len() > 1)
        .collect();
    if cw.is_empty() {
        return false;
    }
    let code_words = words(code);
    let overlap = cw.iter().filter(|w| code_words.contains(*w)).count();
    overlap as f64 / cw.len() as f64 >= RESTATE_THRESHOLD
}

/// Remove restating full-line comments inside each `(start, end)` line range
/// (0-based, inclusive). Returns the rewritten source and the count removed.
pub fn fix_over_commenting(
    source: &str,
    ranges: &[(usize, usize)],
    lang: Option<Language>,
) -> (String, usize) {
    // No parser for the file means no reliable comment syntax, and a wrong
    // guess deletes code.
    let Some((marker, keep)) = lang.map(Language::line_comment) else {
        return (source.to_string(), 0);
    };
    let is_comment = |t: &str| t.starts_with(marker) && !keep.iter().any(|k| t.starts_with(k));
    let lines: Vec<&str> = source.lines().collect();
    let trailing_newline = source.ends_with('\n');

    // The next code line at or after `i` (skipping blanks and comments).
    let next_code = |i: usize| -> Option<&str> {
        lines[i + 1..]
            .iter()
            .map(|l| l.trim())
            .find(|t| !t.is_empty() && !is_comment(t))
    };

    let mut remove: HashSet<usize> = HashSet::new();
    for &(s, e) in ranges {
        let end = e.min(lines.len().saturating_sub(1));
        for i in s..=end {
            let Some(line) = lines.get(i) else { continue };
            let t = line.trim_start();
            if !is_comment(t) {
                continue;
            }
            let comment = t.trim_start_matches(marker);
            if let Some(code) = next_code(i) {
                if restates(comment, code) {
                    remove.insert(i);
                }
            }
        }
    }

    if remove.is_empty() {
        return (source.to_string(), 0);
    }
    let kept: Vec<&str> = lines
        .iter()
        .enumerate()
        .filter(|(i, _)| !remove.contains(i))
        .map(|(_, l)| *l)
        .collect();
    let mut out = kept.join("\n");
    if trailing_newline {
        out.push('\n');
    }
    (out, remove.len())
}

/// A dead free function slated for removal: its 0-based inclusive line span
/// (from the parser's `FunctionFacts`, which is 0-based and covers decorators).
#[derive(Debug, Clone)]
pub struct DeadRemoval {
    pub file: String,
    pub entity: String,
    pub lines: (u32, u32),
}

/// Plan removals for `dead-island` findings. Only **Warning**-severity ones —
/// i.e. free functions with no resolved caller and not an entry point. Methods
/// stay Advisory (SCIP misses dynamic dispatch) and are never auto-removed. The
/// span comes from the parser's `FunctionFacts` (covers decorators), joined to
/// the finding by graph entity.
pub fn plan_dead_removals(
    built: &BuiltGraph,
    facts: &[FileFacts],
    findings: &[Finding],
) -> Vec<DeadRemoval> {
    let dead: HashSet<&str> = findings
        .iter()
        .filter(|f| f.rule == "dead-island" && f.severity == Severity::Warning)
        .map(|f| f.entity.as_str())
        .collect();
    if dead.is_empty() {
        return Vec::new();
    }
    let index = source::location_index(built);
    let mut out = Vec::new();
    for ff in facts {
        for fact in &ff.functions {
            let Some(entity) = source::entity_for(built, &index, &ff.file, fact) else {
                continue;
            };
            if dead.contains(entity.id.as_str()) {
                out.push(DeadRemoval {
                    file: ff.file.clone(),
                    entity: entity.id.clone(),
                    lines: (fact.start_line, fact.end_line),
                });
            }
        }
    }
    out
}

/// Delete the given **0-based** inclusive line ranges from `source`, then
/// collapse any run of 3+ blank lines the deletion left behind to two blanks
/// (PEP8 top-level spacing). Returns the rewritten source.
pub fn delete_line_ranges(source: &str, ranges: &[(u32, u32)]) -> String {
    let lines: Vec<&str> = source.lines().collect();
    let trailing_newline = source.ends_with('\n');
    let mut remove: HashSet<usize> = HashSet::new();
    for &(s, e) in ranges {
        let start = s as usize;
        let end = (e as usize).min(lines.len().saturating_sub(1));
        for i in start..=end {
            remove.insert(i);
        }
    }
    let kept: Vec<&str> = lines
        .iter()
        .enumerate()
        .filter(|(i, _)| !remove.contains(i))
        .map(|(_, l)| *l)
        .collect();

    // Collapse 3+ consecutive blanks (a removed function often leaves a gap).
    let mut out_lines: Vec<&str> = Vec::with_capacity(kept.len());
    let mut blanks = 0;
    for l in kept {
        if l.trim().is_empty() {
            blanks += 1;
            if blanks > 2 {
                continue;
            }
        } else {
            blanks = 0;
        }
        out_lines.push(l);
    }
    let mut out = out_lines.join("\n");
    if trailing_newline {
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deletes_line_ranges_and_collapses_gap() {
        let src = "def a():\n    pass\n\n\ndef dead():\n    pass\n\n\ndef b():\n    pass\n";
        // Delete `dead` (lines 4-5, 0-based: `def dead():` + `    pass`).
        let out = delete_line_ranges(src, &[(4, 5)]);
        assert!(!out.contains("dead"), "{out}");
        assert!(out.contains("def a") && out.contains("def b"));
        // At most 2 blank lines (PEP8 spacing) — never a 3+ blank run.
        assert!(!out.contains("\n\n\n\n"), "{out:?}");
    }

    #[test]
    fn removes_pure_restatement() {
        let src = "def f():\n    # return the result\n    return result\n";
        let (out, n) = fix_over_commenting(src, &[(0, 2)], Some(Language::Python));
        assert_eq!(n, 1);
        assert!(!out.contains('#'), "{out}");
        assert!(out.contains("return result"));
    }

    #[test]
    fn keeps_explanatory_comments() {
        // Domain rationale the code can't express — must survive.
        let src = "def f():\n    # WHOOP weights HRV at 65 percent\n    x = 0.65 * hrv\n";
        let (out, n) = fix_over_commenting(src, &[(0, 2)], Some(Language::Python));
        assert_eq!(n, 0);
        assert!(out.contains("WHOOP"));
    }

    #[test]
    fn rust_attributes_and_doc_comments_are_never_touched() {
        // `#[derive]` is code; `///` is the contract. Only the plain restating
        // `//` line may go.
        let src = "#[derive(Debug)]\n/// Returns the result.\nfn f() -> R {\n    // return result\n    return result;\n}\n";
        let (out, n) = fix_over_commenting(src, &[(0, 5)], Some(Language::Rust));
        assert_eq!(n, 1, "{out}");
        assert!(out.contains("#[derive(Debug)]"), "{out}");
        assert!(out.contains("/// Returns the result."), "{out}");
        assert!(!out.contains("// return result"), "{out}");
    }

    #[test]
    fn an_unknown_language_is_left_alone() {
        let src = "# could be anything\nvalue\n";
        assert_eq!(fix_over_commenting(src, &[(0, 1)], None), (src.to_string(), 0));
    }

    #[test]
    fn keeps_comments_outside_ranges() {
        let src = "# module header restating module\ndef f():\n    pass\n";
        let (out, n) = fix_over_commenting(src, &[(1, 2)], Some(Language::Python));
        assert_eq!(n, 0);
        assert_eq!(out, src);
    }

    #[test]
    fn camelcase_and_snake_case_match() {
        assert!(restates(" set user name", "self.setUserName(x)"));
        assert!(restates(" compute risk score", "risk_score = compute()"));
        assert!(!restates(" guard against overflow", "return a + b"));
    }
}
