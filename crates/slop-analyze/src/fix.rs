//! Mechanistic auto-fixes (D9 path a): behavior-safe, deterministic repairs
//! slop applies itself, no agent required. Currently one rule —
//! `over-commenting`: delete full-line comments that merely restate the
//! adjacent line of code.
//!
//! Deleting a comment can never change Python behavior, so this is safe *by
//! construction*. The only judgment is which comments are pure restatement;
//! that's kept deliberately conservative (high word-overlap required) so it
//! favors keeping a comment over wrongly deleting an explanatory one.

use std::collections::HashSet;

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

/// Does `comment` (a `# ...` line) merely restate `code`?
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
pub fn fix_over_commenting(source: &str, ranges: &[(usize, usize)]) -> (String, usize) {
    let lines: Vec<&str> = source.lines().collect();
    let trailing_newline = source.ends_with('\n');

    // The next code line at or after `i` (skipping blanks and comments).
    let next_code = |i: usize| -> Option<&str> {
        lines[i + 1..]
            .iter()
            .map(|l| l.trim())
            .find(|t| !t.is_empty() && !t.starts_with('#'))
    };

    let mut remove: HashSet<usize> = HashSet::new();
    for &(s, e) in ranges {
        let end = e.min(lines.len().saturating_sub(1));
        for i in s..=end {
            let Some(line) = lines.get(i) else { continue };
            let t = line.trim_start();
            if !t.starts_with('#') {
                continue;
            }
            let comment = t.trim_start_matches('#');
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_pure_restatement() {
        let src = "def f():\n    # return the result\n    return result\n";
        let (out, n) = fix_over_commenting(src, &[(0, 2)]);
        assert_eq!(n, 1);
        assert!(!out.contains('#'), "{out}");
        assert!(out.contains("return result"));
    }

    #[test]
    fn keeps_explanatory_comments() {
        // Domain rationale the code can't express — must survive.
        let src = "def f():\n    # WHOOP weights HRV at 65 percent\n    x = 0.65 * hrv\n";
        let (out, n) = fix_over_commenting(src, &[(0, 2)]);
        assert_eq!(n, 0);
        assert!(out.contains("WHOOP"));
    }

    #[test]
    fn keeps_comments_outside_ranges() {
        let src = "# module header restating module\ndef f():\n    pass\n";
        let (out, n) = fix_over_commenting(src, &[(1, 2)]);
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
