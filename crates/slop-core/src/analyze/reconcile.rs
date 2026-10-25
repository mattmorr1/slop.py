//! The reconcile gate: provable facts about what changed (an E-sound copy, a `missing` call,
//! an infra-bypass under policy), with ranked matches attached as advice that never blocks.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::analyze::missing::{self, Outcome, Resolver};
use crate::analyze::policy::Policy;
use crate::analyze::precheck::precheck;
use crate::analyze::prewrite::{EquivalenceIndex, PrewriteSidecar, MIN_REUSE_SCORE_PPM};
use crate::analyze::retrieve::Site;
use crate::analyze::session::Change;
use crate::parse::{FunctionFacts, Language};

/// Findings listed in one Stop reason; the rest are counted.
pub const MAX_LISTED: usize = 8;
const MAX_ADVISORY: usize = 6;

/// One provable problem in changed code, with a stable id an agent can acknowledge.
#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub id: String,
    pub file: String,
    /// 0-based.
    pub line: usize,
    pub fact: String,
    pub action: String,
}

#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub findings: Vec<Finding>,
    pub advisory: Vec<String>,
}

fn simple(label: &str) -> &str {
    let last = label.rsplit("::").next().unwrap_or(label);
    last.rsplit(['.', ')']).find(|part| !part.is_empty()).unwrap_or(last)
}

fn parse(language: Language, text: Option<&str>) -> Vec<FunctionFacts> {
    text.and_then(|text| language.parse(text).ok()).unwrap_or_default()
}

/// The first indexed function sharing `hash` that still exists as indexed: a site in a changed file
/// must hold that name and hash both at the baseline and now, so a rename or the new code itself never counts.
fn existing<'i>(index: &'i EquivalenceIndex, hash: &str, live: &impl Fn(&Site, &str) -> bool) -> Option<&'i Site> {
    index.get(hash)?.iter().find(|site| live(site, hash))
}

/// Provable findings and advice for the files a session (or a diff) changed.
pub fn reconcile(repo: &Path, changes: &[Change], sidecar: Option<&PrewriteSidecar>, policy: &Policy) -> Report {
    let mut report = Report::default();
    let mut resolver = Resolver::new(repo);
    let parsed: HashMap<&str, [Vec<FunctionFacts>; 2]> = changes
        .iter()
        .filter_map(|c| Language::from_path(&c.file).map(|lang| (c.file.as_str(), [parse(lang, c.before.as_deref()), parse(lang, Some(&c.after))])))
        .collect();
    let live = |site: &Site, hash: &str| {
        let holds = |fns: &Vec<FunctionFacts>| fns.iter().any(|f| f.name == simple(&site.label) && (f.equiv_hash == hash || f.structural_hash == hash));
        parsed.get(site.file.as_str()).is_none_or(|states| states.iter().all(holds))
    };
    // A move deletes one file and adds another: calls the deleted side already made are not new.
    let moved: HashSet<String> = changes.iter().filter(|c| c.after.is_empty()).flat_map(|c| new_missing(&mut resolver, &c.file, None, c.before.as_deref().unwrap_or(""))).map(|m| m.name).collect();
    for change in changes {
        let Some(language) = Language::from_path(&change.file) else { continue };
        let (file, after) = (change.file.as_str(), change.after.as_str());
        let before_bypass: BTreeSet<_> = change.before.as_deref().map(|b| precheck(language, file, b, policy)).unwrap_or_default().into_iter().map(|f| f.acquired).collect();
        for bypass in precheck(language, file, after, policy).into_iter().filter(|f| !before_bypass.contains(&f.acquired)) {
            let root = bypass.acquired.split('.').next().unwrap_or(&bypass.acquired);
            let line = after.lines().position(|l| l.contains(&bypass.acquired)).or_else(|| after.lines().position(|l| l.contains(root))).unwrap_or(0);
            report.findings.push(Finding {
                id: format!("infra-bypass:{file}:{}", bypass.acquired),
                file: file.to_string(),
                line,
                fact: format!("`{}` acquires {:?} directly, outside the sanctioned channel", bypass.acquired, bypass.effect),
                action: format!("route it through {}", bypass.channels.iter().map(|c| format!("`{c}`")).collect::<Vec<_>>().join(" or ")),
            });
        }
        if language != Language::Python {
            continue;
        }
        let [before, after_fns] = &parsed[file];
        let old = |hash: fn(&FunctionFacts) -> &String| -> HashSet<(&str, &str)> { before.iter().map(|f| (f.name.as_str(), hash(f).as_str())).collect() };
        let (old_sound, old_shape) = (old(|f| &f.equiv_hash), old(|f| &f.structural_hash));
        for function in after_fns {
            let line = function.name_line as usize;
            let dup = sidecar.filter(|_| !function.equiv_hash.is_empty() && !old_sound.contains(&(function.name.as_str(), function.equiv_hash.as_str())))
                .and_then(|s| existing(&s.equivalents, &function.equiv_hash, &live));
            if let Some(site) = dup {
                report.findings.push(Finding {
                    id: format!("duplicate:{}", &function.equiv_hash[..16.min(function.equiv_hash.len())]),
                    file: file.to_string(),
                    line,
                    fact: format!("`{}` is provably equivalent (same behaviour for every input) to existing `{}`", function.name, site.label),
                    action: format!("call `{}` at {}:{} instead of adding a copy", site.label, site.file, site.start + 1),
                });
                continue;
            }
            let Some(sidecar) = sidecar else { continue };
            if !function.structural_hash.is_empty() && !old_shape.contains(&(function.name.as_str(), function.structural_hash.as_str())) {
                if let Some(site) = existing(&sidecar.structural, &function.structural_hash, &live) {
                    report.advisory.push(format!("`{}` ({file}:{}) has the same shape as `{}` ({}:{}); names or literals differ, so review rather than proof", function.name, line + 1, site.label, site.file, site.start + 1));
                    continue;
                }
                let body: String = after.lines().skip(function.start_line as usize).take((function.end_line - function.start_line + 1) as usize).collect::<Vec<_>>().join("\n");
                let query = crate::analyze::retrieve::query_from_source(language, &body);
                if let Some(m) = sidecar.neighborhood.matches(&query, 3).into_iter().find(|m| m.score_ppm >= MIN_REUSE_SCORE_PPM && !(m.file == file && simple(&m.label) == function.name) && parsed.get(m.file.as_str()).is_none_or(|[_, now]| now.iter().any(|f| f.name == simple(&m.label)))) {
                    report.advisory.push(format!("`{}` ({file}:{}) overlaps existing `{}` ({}:{}) through calls [{}]", function.name, line + 1, m.label, m.file, m.line + 1, m.distinctive.join(", ")));
                }
            }
        }
        report.findings.extend(new_missing(&mut resolver, file, change.before.as_deref(), after).iter().filter(|call| !moved.contains(&call.name)).map(|call| missing_finding(file, call, sidecar)));
    }
    report.advisory.truncate(MAX_ADVISORY);
    report
}

/// Calls to provably missing names in `after` that `before` did not already make.
pub fn new_missing(resolver: &mut Resolver, file: &str, before: Option<&str>, after: &str) -> Vec<missing::Missing> {
    let was: HashSet<String> = match before.map(|b| missing::check(resolver, file, b)) {
        Some(Outcome::Checked(found)) => found.into_iter().map(|m| m.name).collect(),
        _ => HashSet::new(),
    };
    match missing::check(resolver, file, after) {
        Outcome::Checked(found) => found.into_iter().filter(|m| !was.contains(&m.name)).collect(),
        Outcome::Skipped(_) => Vec::new(),
    }
}

/// A `missing` call as a finding, with up to three "did you mean" suggestions from the sidecar.
pub fn missing_finding(file: &str, call: &missing::Missing, sidecar: Option<&PrewriteSidecar>) -> Finding {
    let leaf = call.name.rsplit('.').next().unwrap_or(&call.name);
    let suggestions = sidecar.map(|s| missing::suggest(&s.defined, &call.name, 3)).unwrap_or_default();
    let mut action = format!("define or import `{leaf}`, or call something that exists");
    if !suggestions.is_empty() {
        let named: Vec<String> = suggestions.iter().map(|d| format!("`{}` ({}:{})", d.label, d.file, d.line + 1)).collect();
        action.push_str(&format!("; did you mean {}", named.join(", ")));
    }
    Finding {
        id: format!("missing:{file}:{}", call.name),
        file: file.to_string(),
        line: call.line,
        fact: format!("`{}(...)` calls a name that does not exist", call.name),
        action,
    }
}

/// The Stop reason: blocking findings first, advice after, then how to acknowledge.
pub fn render(findings: &[Finding], advisory: &[String]) -> String {
    let mut out = format!("slop: the code changed this session has {} unacknowledged provable problem(s):\n", findings.len());
    for (n, f) in findings.iter().take(MAX_LISTED).enumerate() {
        out.push_str(&format!("{}. [{}] {}:{}: {}. Action: {}.\n", n + 1, f.id, f.file, f.line + 1, f.fact, f.action));
    }
    if findings.len() > MAX_LISTED {
        out.push_str(&format!("...and {} more; call the slop `reconcile` tool for the full list.\n", findings.len() - MAX_LISTED));
    }
    if !advisory.is_empty() {
        out.push_str("Advisory (ranked matches, never blocking):\n");
        advisory.iter().for_each(|line| out.push_str(&format!("- {line}\n")));
    }
    out.push_str("If this is intentional, call the slop `acknowledge` tool with the id and a reason.");
    out
}

#[derive(Deserialize)]
struct Ack {
    id: String,
}

fn acks_path(repo: &Path) -> std::path::PathBuf {
    repo.join(".slop/acks.jsonl")
}

/// Every acknowledged id; acks are repository-scoped and persist across sessions.
pub fn acknowledged(repo: &Path) -> Result<HashSet<String>> {
    let path = acks_path(repo);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(HashSet::new()),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .enumerate()
        .map(|(n, line)| serde_json::from_str::<Ack>(line).map(|ack| ack.id).with_context(|| format!("{}:{}", path.display(), n + 1)))
        .collect()
}

pub fn acknowledge(repo: &Path, session_id: Option<&str>, id: &str, reason: &str) -> Result<()> {
    use std::io::Write;
    let path = acks_path(repo);
    std::fs::create_dir_all(path.parent().context("acks path has no parent")?)?;
    let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let line = json!({ "ts": at, "session_id": session_id, "id": id, "reason": reason });
    let mut log = std::fs::OpenOptions::new().create(true).append(true).open(&path).with_context(|| format!("opening {}", path.display()))?;
    writeln!(log, "{line}").with_context(|| format!("appending to {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze::snapshot::{CaptureRequest, Freshness, RepositorySnapshot};
    use crate::analyze::test_support::TempFixture;

    const COPY: &str = "from datetime import datetime\n\n\ndef parse_iso(raw: str) -> datetime:\n    \"\"\"Parse an ISO-8601 date string.\"\"\"\n    cleaned = raw.strip()\n    if cleaned.endswith(\"Z\"):\n        cleaned = cleaned[:-1] + \"+00:00\"\n    return datetime.fromisoformat(cleaned)\n";

    fn ids(report: &Report) -> Vec<&str> {
        report.findings.iter().map(|f| f.id.as_str()).collect()
    }

    #[test]
    fn duplicates_need_a_site_that_held_its_name_and_hash_before_and_after() {
        let repo = TempFixture::new("toy_repo_slopped");
        let snapshot = RepositorySnapshot::capture(CaptureRequest { repo: &repo, index: None, policy: None, freshness: Freshness::Warn }).unwrap();
        let path = snapshot.write_prewrite_sidecar().unwrap();
        let mut sidecar = crate::analyze::prewrite::load_sidecar(&repo).unwrap().unwrap();
        let _ = std::fs::remove_file(path);
        let hash = Language::Python.parse(COPY).unwrap()[0].equiv_hash.clone();
        let policy = Policy::default();
        let added = Change { file: "utils/iso.py".into(), before: None, after: COPY.into() };
        assert!(ids(&reconcile(&repo, std::slice::from_ref(&added), Some(&sidecar), &policy))[0].starts_with("duplicate:"));

        // A sidecar rebuilt after the write indexes the new copy itself; that site must not count.
        sidecar.equivalents.insert(hash.clone(), vec![Site { label: "utils.iso::parse_iso".into(), file: "utils/iso.py".into(), start: 3, end: 8 }]);
        assert!(reconcile(&repo, &[added], Some(&sidecar), &policy).findings.is_empty());

        // Renaming the original in place leaves nothing to call, so it is not a copy.
        let original = std::fs::read_to_string(repo.join("utils/dates.py")).unwrap();
        sidecar.equivalents.insert(hash, vec![Site { label: "utils.dates::parse_date".into(), file: "utils/dates.py".into(), start: 5, end: 10 }]);
        let renamed = Change { file: "utils/dates.py".into(), after: original.replace("def parse_date", "def parse_iso_date"), before: Some(original) };
        assert!(ids(&reconcile(&repo, &[renamed], Some(&sidecar), &policy)).iter().all(|id| !id.starts_with("duplicate:")));
    }

    #[test]
    fn a_moved_file_is_neither_a_copy_of_itself_nor_new_missing_calls() {
        let repo = TempFixture::new("toy_repo_slopped");
        let snapshot = RepositorySnapshot::capture(CaptureRequest { repo: &repo, index: None, policy: None, freshness: Freshness::Warn }).unwrap();
        let path = snapshot.write_prewrite_sidecar().unwrap();
        let sidecar = crate::analyze::prewrite::load_sidecar(&repo).unwrap().unwrap();
        let _ = std::fs::remove_file(path);
        std::fs::write(repo.join("utils/legacy.py"), "def run(step):\n    return goto(step)\n").unwrap();
        let git = |args: &[&str]| assert!(std::process::Command::new("git").arg("-C").arg(&*repo).args(args).output().unwrap().status.success(), "git {args:?}");
        git(&["init", "-q"]);
        git(&["add", "."]);
        git(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "init"]);
        crate::analyze::session::start(&repo, "moved").unwrap();
        std::fs::create_dir_all(repo.join("utils/moved")).unwrap();
        for file in ["dates.py", "legacy.py"] {
            std::fs::rename(repo.join("utils").join(file), repo.join("utils/moved").join(file)).unwrap();
        }
        let session = crate::analyze::session::load(&repo, "moved").unwrap();
        let changes = crate::analyze::session::changes(&repo, &session).unwrap();
        assert!(changes.iter().any(|c| c.file == "utils/dates.py" && c.after.is_empty()), "a deleted file is a change");
        let report = reconcile(&repo, &changes, Some(&sidecar), &Policy::default());
        assert!(report.findings.is_empty(), "{:?}", ids(&report));
        assert!(report.advisory.iter().all(|a| !a.contains("utils/dates.py")), "{:?}", report.advisory);
    }
}
