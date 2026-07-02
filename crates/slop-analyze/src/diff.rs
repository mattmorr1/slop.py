//! Diff-relative mode (D7): the repo is the baseline; the diff is what gets
//! judged. Parses `git diff -U0` output into changed line ranges and filters
//! findings to entities whose body overlaps a change.

use std::collections::HashMap;

use crate::findings::Finding;

/// file -> changed line ranges on the new side, 0-based inclusive.
pub type ChangedLines = HashMap<String, Vec<(usize, usize)>>;

/// Parse unified diff text (produced with `-U0` for exact ranges).
pub fn parse_unified_diff(diff_text: &str) -> ChangedLines {
    let mut changed: ChangedLines = HashMap::new();
    let mut current_file: Option<String> = None;

    for line in diff_text.lines() {
        if let Some(path) = line.strip_prefix("+++ b/") {
            current_file = Some(path.to_string());
        } else if line.starts_with("+++ ") {
            current_file = None; // deleted file (`+++ /dev/null`)
        } else if let Some(hunk) = line.strip_prefix("@@ ") {
            let Some(file) = &current_file else { continue };
            // Hunk header: `-a[,b] +c[,d] @@ ...` — we want the new side.
            let Some(new_side) = hunk.split(' ').find(|p| p.starts_with('+')) else {
                continue;
            };
            let mut nums = new_side[1..].splitn(2, ',');
            let start_1based: usize = match nums.next().and_then(|n| n.parse().ok()) {
                Some(n) => n,
                None => continue,
            };
            let count: usize = nums.next().and_then(|n| n.parse().ok()).unwrap_or(1);
            if count == 0 {
                continue; // pure deletion: no new-side lines to judge
            }
            let start = start_1based.saturating_sub(1);
            changed
                .entry(file.clone())
                .or_default()
                .push((start, start + count - 1));
        }
    }
    changed
}

/// Keep only findings whose entity body overlaps a changed line range.
pub fn filter_to_changes(findings: Vec<Finding>, changed: &ChangedLines) -> Vec<Finding> {
    findings
        .into_iter()
        .filter(|f| {
            changed.get(&f.file).is_some_and(|ranges| {
                ranges
                    .iter()
                    .any(|&(start, end)| f.lines.0 <= end && start <= f.lines.1)
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::findings::Severity;

    const DIFF: &str = "\
diff --git a/services/alerts.py b/services/alerts.py
--- a/services/alerts.py
+++ b/services/alerts.py
@@ -0,0 +1,12 @@
+new file body
diff --git a/utils/dates.py b/utils/dates.py
--- a/utils/dates.py
+++ b/utils/dates.py
@@ -7 +7,2 @@ def parse_date
+    tweak
+    tweak
";

    fn finding(file: &str, lines: (usize, usize)) -> Finding {
        Finding {
            rule: "infra-bypass",
            severity: Severity::Blocking,
            entity: "x".into(),
            file: file.into(),
            lines,
            message: String::new(),
            fix_guidance: String::new(),
        }
    }

    #[test]
    fn parses_new_side_ranges() {
        let changed = parse_unified_diff(DIFF);
        assert_eq!(changed["services/alerts.py"], vec![(0, 11)]);
        assert_eq!(changed["utils/dates.py"], vec![(6, 7)]);
    }

    #[test]
    fn filters_findings_to_overlapping_entities() {
        let changed = parse_unified_diff(DIFF);
        let kept = filter_to_changes(
            vec![
                finding("services/alerts.py", (6, 11)), // overlaps new file
                finding("core/http_client.py", (12, 22)), // untouched file
                finding("utils/dates.py", (10, 12)),    // touched file, other fn
            ],
            &changed,
        );
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].file, "services/alerts.py");
    }
}
