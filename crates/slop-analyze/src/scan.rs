//! Scope, evidence attribution, and coverage for one repository Judgment.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command as Process;

use anyhow::{bail, Context, Result};
use serde::Serialize;

use crate::diff::{self, ChangedLines};
use crate::findings::Finding;
use crate::snapshot::RepositorySnapshot;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScanScope {
    Repository,
    Worktree { base: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct CoverageReport {
    pub repository_files: usize,
    pub indexed_files: usize,
    pub parsed_files: usize,
    pub graph_joined_functions: usize,
    pub by_language: BTreeMap<String, usize>,
    pub unindexed_files: Vec<String>,
    pub parse_failed_files: Vec<String>,
}

pub fn coverage(snapshot: &RepositorySnapshot) -> CoverageReport {
    let parsed: BTreeSet<&str> = snapshot.facts.iter().map(|facts| facts.file.as_str()).collect();
    let mut by_language = BTreeMap::new();
    for file in snapshot.sources.keys() {
        let language = match Path::new(file).extension().and_then(|value| value.to_str()) {
            Some("py") => "python",
            Some("js" | "jsx") => "javascript",
            Some("ts" | "tsx") => "typescript",
            Some("rs") => "rust",
            _ => "unknown",
        };
        *by_language.entry(language.to_string()).or_default() += 1;
    }
    let locations = crate::source::location_index(&snapshot.built);
    let graph_joined_functions = snapshot
        .facts
        .iter()
        .flat_map(|file| {
            file.functions.iter().filter(|function| {
                crate::source::entity_for(&snapshot.built, &locations, &file.file, function).is_some()
            })
        })
        .count();
    CoverageReport {
        repository_files: snapshot.sources.len(),
        indexed_files: snapshot.indexed_files().len(),
        parsed_files: snapshot.facts.len(),
        graph_joined_functions,
        by_language,
        unindexed_files: snapshot
            .sources
            .keys()
            .filter(|file| !snapshot.indexed_files().contains(*file))
            .cloned()
            .collect(),
        parse_failed_files: snapshot
            .sources
            .keys()
            .filter(|file| !parsed.contains(file.as_str()))
            .cloned()
            .collect(),
    }
}

pub fn worktree_changes(snapshot: &RepositorySnapshot, base: &str) -> Result<ChangedLines> {
    let output = Process::new("git")
        .args(["-C"])
        .arg(snapshot.repo())
        .args(["diff", "-U0", "--no-color", base, "--"])
        .output()
        .context("running git diff (use --all for a non-git tree)")?;
    if !output.status.success() {
        bail!(
            "git diff failed: {} — use --all to judge the whole repository",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let mut changed = diff::parse_unified_diff(&String::from_utf8_lossy(&output.stdout));
    let untracked = Process::new("git")
        .args(["-C"])
        .arg(snapshot.repo())
        .args(["ls-files", "--others", "--exclude-standard", "-z"])
        .output()
        .context("listing untracked source files")?;
    if !untracked.status.success() {
        bail!("git ls-files failed: {}", String::from_utf8_lossy(&untracked.stderr).trim());
    }
    for bytes in untracked.stdout.split(|byte| *byte == 0).filter(|path| !path.is_empty()) {
        let file = String::from_utf8_lossy(bytes).replace('\\', "/");
        let Some(source) = snapshot.sources.get(&file) else { continue };
        let end = source.lines().count().saturating_sub(1);
        changed.entry(file).or_default().push((0, end));
    }
    Ok(changed)
}

pub fn filter_to_changes(findings: Vec<Finding>, changed: &ChangedLines) -> Vec<Finding> {
    findings
        .into_iter()
        .filter(|finding| {
            finding.evidence_loci().iter().any(|locus| {
                changed.get(&locus.file).is_some_and(|ranges| {
                    ranges
                        .iter()
                        .any(|&(start, end)| locus.lines.0 <= end && start <= locus.lines.1)
                })
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::findings::{EvidenceLocus, Severity};

    #[test]
    fn relational_finding_is_kept_when_a_related_locus_changes() {
        let finding = Finding {
            rule: "duplicate-exact",
            severity: Severity::Warning,
            entity: "old::canonical".into(),
            file: "old.py".into(),
            lines: (0, 4),
            related: vec![EvidenceLocus {
                entity: "new::copy".into(),
                file: "new.py".into(),
                lines: (10, 14),
            }],
            message: String::new(),
            fix_guidance: String::new(),
        };
        let changed = HashMap::from([("new.py".to_string(), vec![(12, 12)])]);
        assert_eq!(filter_to_changes(vec![finding], &changed).len(), 1);
    }
}
