//! The `check` flow shared by the CLI (`slop check`) and the MCP
//! `validate_change` tool (M4c): build the graph, run detectors (+
//! optional Tier-3), suppress, baseline, then either judge the whole repo
//! (`--all`) or just the diff against a git ref (D7's diff-relative mode).

use std::path::{Path, PathBuf};
use std::process::Command as Process;

use anyhow::{bail, Context, Result};
use slop_resolve::{Resolver, ScipResolver};

use crate::baseline::Baseline;
use crate::findings::{Finding, Severity};
use crate::policy::Policy;
use crate::{build, diff, effects, health, source, suppress};

pub struct CheckRequest {
    pub repo: PathBuf,
    pub index: Option<PathBuf>,
    pub policy: Option<PathBuf>,
    /// Judge the whole repo instead of the diff (audit-lite).
    pub all: bool,
    /// Run Tier-3 semantic-redundancy via the Claude API (needs
    /// `ANTHROPIC_API_KEY`). Slow and network-bound — off by default.
    pub tier3: bool,
    /// Git ref to diff against in diff-relative mode.
    pub base: String,
}

impl Default for CheckRequest {
    fn default() -> Self {
        Self {
            repo: PathBuf::new(),
            index: None,
            policy: None,
            all: false,
            tier3: false,
            base: "HEAD".to_string(),
        }
    }
}

pub struct CheckResult {
    pub findings: Vec<Finding>,
    pub health_line: String,
    pub blocking: usize,
    /// True when the repo has no slop.toml — infra-bypass stays silent.
    pub policy_is_empty: bool,
}

fn tier3_findings(
    built: &build::BuiltGraph,
    facts: &[source::FileFacts],
    repo: &Path,
) -> Result<Vec<Finding>> {
    use slop_llm::{judge_from_env, JudgeInput};

    let candidates = crate::tier3::candidates(built, facts, repo);
    if candidates.is_empty() {
        eprintln!("tier3: no semantic-redundancy candidates");
        return Ok(Vec::new());
    }
    let judge = judge_from_env()?;
    eprintln!(
        "tier3: judging {} semantic-redundancy pair(s) via {}",
        candidates.len(),
        judge.model()
    );
    let render = |c: &crate::tier3::CandidateFn| {
        format!(
            "# {} ({}:{})\n# docstring: {}\n{}",
            c.entity,
            c.file,
            c.lines.0 + 1,
            c.docstring.as_deref().unwrap_or("<none>"),
            c.snippet
        )
    };
    let inputs: Vec<JudgeInput> = candidates
        .iter()
        .enumerate()
        .map(|(i, pair)| JudgeInput {
            index: i,
            a_label: pair.a.entity.clone(),
            a_context: render(&pair.a),
            b_label: pair.b.entity.clone(),
            b_context: render(&pair.b),
        })
        .collect();
    let verdicts = judge.judge(&inputs)?;

    let mut findings = Vec::new();
    for verdict in verdicts.into_iter().filter(|v| v.redundant) {
        let Some(pair) = candidates.get(verdict.index) else {
            continue;
        };
        findings.push(Finding {
            rule: "semantic-redundancy",
            severity: Severity::Advisory,
            entity: pair.a.entity.clone(),
            file: pair.a.file.clone(),
            lines: pair.a.lines,
            message: format!(
                "`{}` and `{}` appear to serve the same purpose ({} confidence): {}",
                pair.a.entity, pair.b.entity, verdict.confidence, verdict.reason
            ),
            fix_guidance: format!(
                "Unify `{}` and `{}` behind one implementation",
                pair.a.entity, pair.b.entity
            ),
        });
    }
    Ok(findings)
}

/// Confirms or demotes Tier-2 `duplicate-structural` findings: matching
/// shape is a weak signal on its own (see `detect::shape_groups`), so when
/// `--tier3` is on, ask the judge whether each group's canonical member and
/// its first peer actually serve the same purpose. Returns
/// `(canonical entity, confirmed redundant, reason)` for `run` to apply
/// against the findings `detect::run_all` already produced.
fn tier3_structural_verdicts(
    built: &build::BuiltGraph,
    facts: &[source::FileFacts],
    repo: &Path,
) -> Result<Vec<(String, bool, String)>> {
    use slop_llm::{judge_from_env, JudgeInput};

    let candidates = crate::tier3::structural_candidates(built, facts, repo);
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let judge = judge_from_env()?;
    eprintln!(
        "tier3: confirming {} duplicate-structural group(s) via {}",
        candidates.len(),
        judge.model()
    );
    let render = |c: &crate::tier3::CandidateFn| {
        format!(
            "# {} ({}:{})\n# docstring: {}\n{}",
            c.entity,
            c.file,
            c.lines.0 + 1,
            c.docstring.as_deref().unwrap_or("<none>"),
            c.snippet
        )
    };
    let inputs: Vec<JudgeInput> = candidates
        .iter()
        .enumerate()
        .map(|(i, pair)| JudgeInput {
            index: i,
            a_label: pair.a.entity.clone(),
            a_context: render(&pair.a),
            b_label: pair.b.entity.clone(),
            b_context: render(&pair.b),
        })
        .collect();
    let verdicts = judge.judge(&inputs)?;

    Ok(verdicts
        .into_iter()
        .filter_map(|v| {
            let pair = candidates.get(v.index)?;
            Some((pair.a.entity.clone(), v.redundant, v.reason))
        })
        .collect())
}

/// The graph + parsed source facts every consumer starts from: the
/// `SCIP load → build → infer effects → parse` sequence, in one place.
/// `check::run`, the MCP tools, `slop baseline`, and `slop init` all share
/// it rather than re-deriving the graph three slightly different ways.
pub struct Analysis {
    pub built: build::BuiltGraph,
    pub facts: Vec<source::FileFacts>,
}

/// Resolve `index` (default `<repo>/index.scip`), build the effect graph, and
/// parse the repo's source facts. Errors with the `scip-python` hint when the
/// index is missing.
pub fn load_analysis(repo: &Path, index: Option<&Path>) -> Result<Analysis> {
    let index_path = index
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| repo.join("index.scip"));
    if !index_path.exists() {
        bail!(
            "no SCIP index at {} — generate one with:\n  npx --yes @sourcegraph/scip-python index {} --project-name <name> --output {}",
            index_path.display(),
            repo.display(),
            index_path.display(),
        );
    }
    let resolver = ScipResolver::load(&index_path)?;
    let mut built = build::build_graph(&resolver);
    effects::infer_effects(&mut built);
    let facts = source::parse_repo(repo, &resolver.files());
    Ok(Analysis { built, facts })
}

pub fn run(req: CheckRequest) -> Result<CheckResult> {
    let policy = match &req.policy {
        Some(path) => Policy::load_file(path)?,
        None => Policy::load(&req.repo)?,
    };

    let Analysis { built, facts } = load_analysis(&req.repo, req.index.as_deref())?;
    let mut raw = crate::detect::run_all(&built, &policy, &facts);
    if req.tier3 {
        for (entity, redundant, reason) in tier3_structural_verdicts(&built, &facts, &req.repo)? {
            let Some(f) = raw.iter_mut().find(|f| f.rule == "duplicate-structural" && f.entity == entity)
            else {
                continue;
            };
            if redundant {
                // Shape match corroborated by the judge — promote the
                // candidate from Advisory to a Warning worth acting on.
                f.severity = Severity::Warning;
                f.message = format!("{} — confirmed by semantic review: {reason}", f.message);
            } else {
                f.message = format!(
                    "{} (unconfirmed — semantic review found a different purpose: {reason})",
                    f.message
                );
            }
        }
        raw.extend(tier3_findings(&built, &facts, &req.repo)?);
    }
    let suppressions = suppress::scan(&req.repo, &facts);
    let unsuppressed = suppress::filter(raw, &suppressions);
    let baseline = Baseline::load(&req.repo)?;
    let effective = baseline.filter(unsuppressed);

    let (findings, health_line) = if req.all {
        let s = health::score(&effective, &built);
        (effective, format!("health: {s}/100"))
    } else {
        let output = Process::new("git")
            .args(["-C"])
            .arg(&req.repo)
            .args(["diff", "-U0", "--no-color", &req.base, "--", "*.py"])
            .output()
            .context("running git diff (use all=true for a non-git tree)")?;
        if !output.status.success() {
            bail!(
                "git diff failed: {} — use all=true to judge the whole repo",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let changed = diff::parse_unified_diff(&String::from_utf8_lossy(&output.stdout));
        let new = diff::filter_to_changes(effective.clone(), &changed);
        let new_keys: std::collections::HashSet<(&str, String)> =
            new.iter().map(|f| (f.rule, f.entity.clone())).collect();
        let before: Vec<_> = effective
            .iter()
            .filter(|f| !new_keys.contains(&(f.rule, f.entity.clone())))
            .cloned()
            .collect();
        let line = format!(
            "health: {} -> {}",
            health::score(&before, &built),
            health::score(&effective, &built)
        );
        (new, line)
    };

    let blocking = findings.iter().filter(|f| f.severity == Severity::Blocking).count();
    Ok(CheckResult {
        findings,
        health_line,
        blocking,
        policy_is_empty: policy.channels.is_empty(),
    })
}
