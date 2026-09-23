//! Exact, snapshot-bound repair planning and transactional application.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{bail, Context, Result};
use serde::Serialize;

use crate::findings::{Finding, FindingId};
use crate::fix;
use crate::inline::{self, InlineOutcome};
use crate::rename::{self, RenameOutcome};
use crate::snapshot::{RepositorySnapshot, SnapshotId};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
pub const REPAIR_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SafetyClass {
    Provable,
    IndexVerified,
    Advisory,
}

#[derive(Debug, Clone)]
pub enum RepairSelection {
    All,
    Finding(FindingId),
}

#[derive(Debug, Clone, Serialize)]
pub struct RepairAction {
    pub finding: FindingId,
    pub rule: &'static str,
    pub entity: String,
    pub safety: SafetyClass,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SkippedRepair {
    pub entity: String,
    pub reason: String,
}

#[derive(Debug)]
pub struct RepairPlan {
    pub schema_version: u32,
    pub snapshot: SnapshotId,
    pub actions: Vec<RepairAction>,
    pub skipped: Vec<SkippedRepair>,
    pub files: BTreeMap<String, String>,
}

#[derive(Serialize)]
pub struct RepairPreview<'a> {
    pub schema_version: u32,
    pub snapshot: &'a SnapshotId,
    pub actions: &'a [RepairAction],
    pub skipped: &'a [SkippedRepair],
    pub files: Vec<&'a str>,
}

impl RepairPlan {
    pub fn preview(&self) -> RepairPreview<'_> {
        RepairPreview {
            schema_version: self.schema_version,
            snapshot: &self.snapshot,
            actions: &self.actions,
            skipped: &self.skipped,
            files: self.files.keys().map(String::as_str).collect(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RepairReceipt {
    pub schema_version: u32,
    pub snapshot: SnapshotId,
    pub actions: usize,
    pub files: Vec<String>,
}

#[derive(Debug, Serialize)]
struct JournalEntry {
    target: String,
    staged: String,
    backup: String,
}

#[derive(Serialize)]
struct RepairJournal<'a> {
    schema_version: u32,
    state: &'static str,
    snapshot: &'a SnapshotId,
    entries: &'a [JournalEntry],
}

#[must_use = "a pending repair must be committed after verification or rolled back"]
pub struct PendingRepair {
    receipt: RepairReceipt,
    journal_path: Option<PathBuf>,
    entries: Vec<JournalEntry>,
}

impl PendingRepair {
    pub fn receipt(&self) -> &RepairReceipt {
        &self.receipt
    }

    pub fn commit(self) -> Result<RepairReceipt> {
        let Some(journal_path) = &self.journal_path else {
            return Ok(self.receipt);
        };
        std::fs::remove_file(journal_path).with_context(|| {
            format!(
                "finalizing verified repair journal {}",
                journal_path.display()
            )
        })?;
        let mut failures = Vec::new();
        for entry in &self.entries {
            if let Err(error) = std::fs::remove_file(&entry.backup) {
                failures.push(format!("removing {}: {error}", entry.backup));
            }
        }
        if !failures.is_empty() {
            bail!(
                "repair committed but backup cleanup left orphaned files: {}",
                failures.join("; ")
            );
        }
        Ok(self.receipt)
    }

    pub fn rollback(self) -> Result<RepairReceipt> {
        rollback(&self.entries)?;
        if let Some(journal_path) = &self.journal_path {
            std::fs::remove_file(journal_path).with_context(|| {
                format!(
                    "removing rolled-back repair journal {}",
                    journal_path.display()
                )
            })?;
        }
        Ok(self.receipt)
    }
}

fn selected(finding: &Finding, selection: &RepairSelection) -> bool {
    match selection {
        RepairSelection::All => true,
        RepairSelection::Finding(id) => finding.id() == *id,
    }
}

fn source_for(
    snapshot: &RepositorySnapshot,
    files: &BTreeMap<String, String>,
    file: &str,
) -> Result<String> {
    files
        .get(file)
        .cloned()
        .or_else(|| snapshot.sources.get(file).map(ToString::to_string))
        .with_context(|| format!("source {file} is not part of snapshot {}", snapshot.id()))
}

pub fn plan(
    snapshot: &RepositorySnapshot,
    findings: &[Finding],
    selection: RepairSelection,
    safety_ceiling: SafetyClass,
) -> Result<RepairPlan> {
    let selected: Vec<Finding> = findings
        .iter()
        .filter(|finding| selected(finding, &selection))
        .cloned()
        .collect();
    if matches!(selection, RepairSelection::Finding(_)) && selected.is_empty() {
        bail!(
            "selected finding does not exist in snapshot {}",
            snapshot.id()
        );
    }

    let mut actions = Vec::new();
    let mut skipped = Vec::new();
    let renames = rename::plan_renames(
        &snapshot.built,
        &snapshot.resolver,
        snapshot.repo(),
        &selected,
    );
    let mut files: BTreeMap<String, String> = renames.files.into_iter().collect();
    for outcome in renames.outcomes {
        match outcome {
            RenameOutcome::Planned(rename) if safety_ceiling >= SafetyClass::IndexVerified => {
                if let Some(finding) = selected
                    .iter()
                    .find(|finding| finding.entity == rename.entity)
                {
                    actions.push(RepairAction {
                        finding: finding.id(),
                        rule: rename.rule,
                        entity: rename.entity,
                        safety: SafetyClass::IndexVerified,
                        summary: format!(
                            "rename {} to {} across {} occurrence(s) in {} file(s)",
                            rename.old_name, rename.new_name, rename.occurrences, rename.file_count
                        ),
                    });
                }
            }
            RenameOutcome::Planned(rename) => skipped.push(SkippedRepair {
                entity: rename.entity,
                reason: "repair exceeds selected safety ceiling".to_string(),
            }),
            RenameOutcome::Skipped { entity, reason } => {
                skipped.push(SkippedRepair { entity, reason })
            }
        }
    }

    let dead = if safety_ceiling >= SafetyClass::Advisory {
        fix::plan_dead_removals(&snapshot.built, &snapshot.facts, &selected)
    } else {
        Vec::new()
    };
    let mut dead_by_file: BTreeMap<String, Vec<(u32, u32)>> = BTreeMap::new();
    for removal in dead {
        let finding = selected
            .iter()
            .find(|finding| finding.rule == "dead-island" && finding.entity == removal.entity)
            .expect("dead removal originates from selected finding");
        actions.push(RepairAction {
            finding: finding.id(),
            rule: finding.rule,
            entity: removal.entity,
            safety: SafetyClass::Advisory,
            summary: format!("remove dead free function from {}", removal.file),
        });
        dead_by_file
            .entry(removal.file)
            .or_default()
            .push(removal.lines);
    }
    for (file, ranges) in &dead_by_file {
        let source = source_for(snapshot, &files, file)?;
        files.insert(file.clone(), fix::delete_line_ranges(&source, ranges));
    }

    if safety_ceiling >= SafetyClass::Advisory {
        let mut comments: BTreeMap<String, Vec<(usize, usize)>> = BTreeMap::new();
        for finding in selected
            .iter()
            .filter(|finding| finding.rule == "over-commenting")
        {
            if !dead_by_file.contains_key(&finding.file) {
                comments
                    .entry(finding.file.clone())
                    .or_default()
                    .push(finding.lines);
            }
        }
        for (file, ranges) in comments {
            let source = source_for(snapshot, &files, &file)?;
            let (rewritten, removed) =
                fix::fix_over_commenting(&source, &ranges, slop_parse::Language::from_path(&file));
            if removed == 0 {
                continue;
            }
            for finding in selected
                .iter()
                .filter(|finding| finding.rule == "over-commenting" && finding.file == file)
            {
                actions.push(RepairAction {
                    finding: finding.id(),
                    rule: finding.rule,
                    entity: finding.entity.clone(),
                    safety: SafetyClass::Advisory,
                    summary: format!("remove {removed} restating comment(s) from {file}"),
                });
            }
            files.insert(file, rewritten);
        }
    }

    let wrapper_findings: Vec<Finding> = selected
        .iter()
        .filter(|finding| finding.rule == "trivial-wrapper")
        .cloned()
        .collect();
    if !wrapper_findings.is_empty() {
        if !files.is_empty() {
            skipped.extend(
                wrapper_findings.into_iter().map(|finding| SkippedRepair {
                    entity: finding.entity,
                    reason: "wrapper inlining must be applied in a separate snapshot-bound plan"
                        .to_string(),
                }),
            );
        } else if safety_ceiling < SafetyClass::Advisory {
            skipped.extend(wrapper_findings.into_iter().map(|finding| SkippedRepair {
                entity: finding.entity,
                reason: "repair exceeds selected safety ceiling".to_string(),
            }));
        } else {
            let inlines = inline::plan_inlines(
                &snapshot.built,
                &snapshot.resolver,
                snapshot.repo(),
                &snapshot.facts,
                &wrapper_findings,
            );
            for outcome in inlines.outcomes {
                match outcome {
                    InlineOutcome::Planned(inline) => {
                        let finding = wrapper_findings
                            .iter()
                            .find(|finding| finding.entity == inline.entity)
                            .expect("inline originates from selected finding");
                        actions.push(RepairAction {
                            finding: finding.id(),
                            rule: finding.rule,
                            entity: inline.entity,
                            safety: SafetyClass::Advisory,
                            summary: format!(
                                "inline wrapper into {} reference(s) across {} file(s)",
                                inline.occurrences, inline.file_count
                            ),
                        });
                    }
                    InlineOutcome::Skipped { entity, reason } => {
                        skipped.push(SkippedRepair { entity, reason });
                    }
                }
            }
            files.extend(inlines.files);
        }
    }

    files.retain(|file, rewritten| {
        snapshot
            .sources
            .get(file)
            .is_some_and(|original| original.as_ref() != rewritten)
    });
    for (file, rewritten) in &files {
        let Some(language) = slop_parse::Language::from_path(file) else {
            bail!("repair produced unsupported source file {file}");
        };
        language
            .parse(rewritten)
            .map_err(|error| anyhow::anyhow!("repair made {file} unparsable: {error}"))?;
    }
    Ok(RepairPlan {
        schema_version: REPAIR_SCHEMA_VERSION,
        snapshot: snapshot.id().clone(),
        actions,
        skipped,
        files,
    })
}

fn temporary_path(target: &Path, kind: &str) -> Result<PathBuf> {
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let name = target
        .file_name()
        .and_then(|value| value.to_str())
        .context("repair target has no UTF-8 file name")?;
    Ok(target.with_file_name(format!(
        ".{name}.slop-{}-{sequence}.{kind}",
        std::process::id()
    )))
}

fn rollback(committed: &[JournalEntry]) -> Result<()> {
    let mut failures = Vec::new();
    for entry in committed.iter().rev() {
        let target = Path::new(&entry.target);
        let backup = Path::new(&entry.backup);
        if target.exists() {
            if let Err(error) = std::fs::remove_file(target) {
                failures.push(format!("removing {}: {error}", target.display()));
                continue;
            }
        }
        if let Err(error) = std::fs::rename(backup, target) {
            failures.push(format!("restoring {}: {error}", target.display()));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        bail!("repair rollback failed: {}", failures.join("; "))
    }
}

fn cleanup_staged(entries: &[JournalEntry]) {
    for entry in entries {
        let _ = std::fs::remove_file(&entry.staged);
    }
}

pub fn apply(snapshot: &RepositorySnapshot, plan: &RepairPlan) -> Result<PendingRepair> {
    if snapshot.id() != &plan.snapshot {
        bail!(
            "repair plan belongs to snapshot {}, not {}",
            plan.snapshot,
            snapshot.id()
        );
    }
    if plan.files.is_empty() {
        return Ok(PendingRepair {
            receipt: RepairReceipt {
                schema_version: REPAIR_SCHEMA_VERSION,
                snapshot: plan.snapshot.clone(),
                actions: 0,
                files: Vec::new(),
            },
            journal_path: None,
            entries: Vec::new(),
        });
    }
    let state_dir = snapshot.repo().join(".slop");
    std::fs::create_dir_all(&state_dir)
        .with_context(|| format!("creating {}", state_dir.display()))?;
    let journal_path = state_dir.join("repair-journal.json");
    if journal_path.exists() {
        bail!(
            "unfinished repair journal at {} — inspect or restore it before applying another repair",
            journal_path.display()
        );
    }

    for relative in plan.files.keys() {
        let target = snapshot.repo().join(relative);
        let expected = snapshot
            .sources
            .get(relative)
            .context("repair source missing from snapshot")?;
        let current = std::fs::read_to_string(&target)
            .with_context(|| format!("reading repair target {}", target.display()))?;
        if current != expected.as_ref() {
            bail!(
                "{} changed after snapshot {}; rebuild the repair plan",
                relative,
                plan.snapshot
            );
        }
    }

    let mut entries = Vec::with_capacity(plan.files.len());
    for (relative, rewritten) in &plan.files {
        let target = snapshot.repo().join(relative);
        let staged = temporary_path(&target, "staged")?;
        let backup = temporary_path(&target, "backup")?;
        let entry = JournalEntry {
            target: target.to_string_lossy().into_owned(),
            staged: staged.to_string_lossy().into_owned(),
            backup: backup.to_string_lossy().into_owned(),
        };
        let stage_result = (|| -> Result<()> {
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&staged)
                .with_context(|| format!("creating {}", staged.display()))?;
            output
                .write_all(rewritten.as_bytes())
                .with_context(|| format!("writing {}", staged.display()))?;
            output
                .sync_all()
                .with_context(|| format!("syncing {}", staged.display()))?;
            std::fs::set_permissions(&staged, std::fs::metadata(&target)?.permissions())?;
            Ok(())
        })();
        if let Err(error) = stage_result {
            let _ = std::fs::remove_file(&staged);
            cleanup_staged(&entries);
            return Err(error);
        }
        entries.push(entry);
    }

    let journal_staged = temporary_path(&journal_path, "staged")?;
    let journal_result = (|| -> Result<()> {
        let mut journal = File::create(&journal_staged)?;
        serde_json::to_writer_pretty(
            &mut journal,
            &RepairJournal {
                schema_version: REPAIR_SCHEMA_VERSION,
                state: "pending_verification",
                snapshot: &plan.snapshot,
                entries: &entries,
            },
        )?;
        journal.write_all(b"\n")?;
        journal.sync_all()?;
        std::fs::rename(&journal_staged, &journal_path)?;
        Ok(())
    })();
    if let Err(error) = journal_result {
        let _ = std::fs::remove_file(&journal_staged);
        cleanup_staged(&entries);
        return Err(error);
    }

    let mut committed = Vec::with_capacity(entries.len());
    for entry in &entries {
        if let Err(error) = std::fs::rename(&entry.target, &entry.backup) {
            rollback(&committed)?;
            cleanup_staged(&entries);
            let _ = std::fs::remove_file(&journal_path);
            bail!("staging backup for {}: {error}", entry.target);
        }
        if let Err(error) = std::fs::rename(&entry.staged, &entry.target) {
            let restore = std::fs::rename(&entry.backup, &entry.target);
            rollback(&committed)?;
            restore.with_context(|| format!("restoring {} after failed commit", entry.target))?;
            cleanup_staged(&entries);
            let _ = std::fs::remove_file(&journal_path);
            bail!("committing {}: {error}", entry.target);
        }
        committed.push(JournalEntry {
            target: entry.target.clone(),
            staged: entry.staged.clone(),
            backup: entry.backup.clone(),
        });
    }
    Ok(PendingRepair {
        receipt: RepairReceipt {
            schema_version: REPAIR_SCHEMA_VERSION,
            snapshot: plan.snapshot.clone(),
            actions: plan.actions.len(),
            files: plan.files.keys().cloned().collect(),
        },
        journal_path: Some(journal_path),
        entries: committed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn copy_tree(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let target = to.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_tree(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), target).unwrap();
            }
        }
    }

    fn repair_fixture(tag: &str) -> (PathBuf, std::sync::Arc<RepositorySnapshot>) {
        let source =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/toy_repo");
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let repo = std::env::temp_dir().join(format!(
            "slop-repair-{tag}-{}-{sequence}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&repo);
        copy_tree(&source, &repo);
        let snapshot = RepositorySnapshot::capture(crate::snapshot::CaptureRequest {
            repo: &repo,
            index: None,
            policy: None,
            freshness: crate::snapshot::Freshness::Warn,
        })
        .unwrap();
        (repo, snapshot)
    }

    fn marker_plan(snapshot: &RepositorySnapshot) -> RepairPlan {
        let file = "utils/dates.py";
        let rewritten = format!("{}\nX_REPAIR_MARKER = 1\n", snapshot.sources[file]);
        RepairPlan {
            schema_version: REPAIR_SCHEMA_VERSION,
            snapshot: snapshot.id().clone(),
            actions: Vec::new(),
            skipped: Vec::new(),
            files: BTreeMap::from([(file.to_string(), rewritten)]),
        }
    }

    #[test]
    fn finding_selection_is_exact() {
        let finding = Finding {
            rule: "over-commenting",
            severity: crate::findings::Severity::Advisory,
            entity: "m::f".into(),
            file: "m.py".into(),
            lines: (0, 1),
            related: Vec::new(),
            message: String::new(),
            fix_guidance: String::new(),
        };
        assert!(selected(&finding, &RepairSelection::Finding(finding.id())));
    }

    #[test]
    fn pending_repair_rolls_back_after_failed_verification() {
        let (repo, snapshot) = repair_fixture("rollback");
        let file = repo.join("utils/dates.py");
        let original = std::fs::read_to_string(&file).unwrap();
        let pending = apply(&snapshot, &marker_plan(&snapshot)).unwrap();
        assert_ne!(std::fs::read_to_string(&file).unwrap(), original);
        assert!(repo.join(".slop/repair-journal.json").exists());
        pending.rollback().unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), original);
        assert!(!repo.join(".slop/repair-journal.json").exists());
        std::fs::remove_dir_all(repo).unwrap();
    }

    #[test]
    fn verified_repair_commits_and_removes_recovery_state() {
        let (repo, snapshot) = repair_fixture("commit");
        let file = repo.join("utils/dates.py");
        let original = std::fs::read_to_string(&file).unwrap();
        let pending = apply(&snapshot, &marker_plan(&snapshot)).unwrap();
        let receipt = pending.commit().unwrap();
        assert_eq!(receipt.schema_version, REPAIR_SCHEMA_VERSION);
        assert_ne!(std::fs::read_to_string(&file).unwrap(), original);
        assert!(!repo.join(".slop/repair-journal.json").exists());
        std::fs::remove_dir_all(repo).unwrap();
    }
}
