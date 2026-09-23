//! Reasoned inline suppression (D12):
//! `# slop: allow <rule> — <reason>` (or `--` for the dash).
//! The reason is mandatory — a bare allow is ignored. A suppression
//! anywhere inside an entity's span silences that rule for that entity.

use crate::findings::Finding;
use crate::source::FileFacts;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suppression {
    pub file: String,
    /// 0-based line of the comment.
    pub line: usize,
    pub rule: String,
}

/// Scan raw sources for suppression comments. Files are re-read here (not
/// via FileFacts) because suppressions may sit outside any function.
pub fn scan(repo_root: &std::path::Path, facts: &[FileFacts]) -> Vec<Suppression> {
    let mut out = Vec::new();
    for ff in facts {
        let Ok(source) = std::fs::read_to_string(repo_root.join(&ff.file)) else {
            continue;
        };
        for (line_no, line) in source.lines().enumerate() {
            let Some(rest) = line.split("# slop: allow ").nth(1) else {
                continue;
            };
            let (rule, reason) = match rest.split_once("—").or_else(|| rest.split_once("--")) {
                Some((rule, reason)) => (rule.trim(), reason.trim()),
                None => continue, // no reason, no suppression
            };
            if rule.is_empty() || reason.is_empty() {
                continue;
            }
            out.push(Suppression {
                file: ff.file.clone(),
                line: line_no,
                rule: rule.to_string(),
            });
        }
    }
    out
}

/// Drop findings covered by a suppression within their span.
pub fn filter(findings: Vec<Finding>, suppressions: &[Suppression]) -> Vec<Finding> {
    findings
        .into_iter()
        .filter(|f| {
            !suppressions.iter().any(|s| {
                s.rule == f.rule
                    && s.file == f.file
                    && s.line >= f.lines.0
                    && s.line <= f.lines.1
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::findings::Severity;

    #[test]
    fn suppression_requires_reason_and_matches_span() {
        let sup = Suppression {
            file: "a.py".into(),
            line: 5,
            rule: "purity-lie".into(),
        };
        let finding = |rule: &'static str, lines: (usize, usize)| Finding {
            rule,
            severity: Severity::Warning,
            entity: "a::f".into(),
            file: "a.py".into(),
            lines,
            related: Vec::new(),
            message: String::new(),
            fix_guidance: String::new(),
        };
        let kept = filter(
            vec![
                finding("purity-lie", (3, 8)),  // suppressed: line 5 in span
                finding("purity-lie", (10, 12)), // other function
                finding("dead-island", (3, 8)),  // other rule
            ],
            &[sup],
        );
        assert_eq!(kept.len(), 2);
        assert!(kept.iter().all(|f| f.lines == (10, 12) || f.rule == "dead-island"));
    }
}
