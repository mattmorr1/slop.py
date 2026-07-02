use std::path::PathBuf;
use std::process::Command as Process;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use slop_analyze::baseline::Baseline;
use slop_analyze::{build, detect, diff, effects, health, infer, policy::Policy, source, suppress};
use slop_resolve::{Resolver, ScipResolver};

#[derive(Parser)]
#[command(name = "slop", about = "Codebase-relative AI-slop analyzer", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Analyze a repo. Default: judge only the working-tree diff (vs HEAD).
    Check {
        /// Repo root (must contain slop.toml for policy-gated checks)
        repo: PathBuf,
        /// Path to index.scip (default: <repo>/index.scip)
        #[arg(long)]
        index: Option<PathBuf>,
        /// Judge the whole repo instead of the diff (audit-lite)
        #[arg(long)]
        all: bool,
        /// Path to a slop.toml policy (default: <repo>/slop.toml)
        #[arg(long)]
        policy: Option<PathBuf>,
        /// Tier-3: judge semantic-redundancy candidates via the Claude API
        /// (requires ANTHROPIC_API_KEY; findings are Advisory only)
        #[arg(long)]
        tier3: bool,
        /// Git ref to diff against (default: HEAD)
        #[arg(long, default_value = "HEAD")]
        base: String,
    },
    /// Record current findings as the grandfathered baseline.
    Baseline {
        /// Repo root
        repo: PathBuf,
        /// Path to index.scip (default: <repo>/index.scip)
        #[arg(long)]
        index: Option<PathBuf>,
    },
    /// Infer sanctioned channels from dominant patterns; print (and
    /// optionally write) a slop.toml.
    Init {
        /// Repo root
        repo: PathBuf,
        /// Path to index.scip (default: <repo>/index.scip)
        #[arg(long)]
        index: Option<PathBuf>,
        /// Write <repo>/slop.toml (refuses to overwrite an existing one)
        #[arg(long)]
        write: bool,
    },
    /// Debug: load a SCIP index and print what the resolver sees.
    IndexInfo {
        /// Path to index.scip
        index: PathBuf,
    },
    /// Debug: list all occurrences recorded in a file.
    Occurrences {
        /// Path to index.scip
        index: PathBuf,
        /// Repo-relative file path as recorded in the index
        file: String,
    },
    /// Debug: resolve the reference at file:line:col (0-based) to its definition.
    Resolve {
        /// Path to index.scip
        index: PathBuf,
        /// Repo-relative file path as recorded in the index
        file: String,
        line: u32,
        col: u32,
    },
}

/// Tier-3: build effect-bucketed candidates, judge via the Claude API, map
/// confirmed pairs to Advisory findings (never blocking — D4).
fn tier3_findings(
    built: &build::BuiltGraph,
    facts: &[source::FileFacts],
    repo: &std::path::Path,
) -> Result<Vec<slop_analyze::findings::Finding>> {
    use slop_analyze::findings::{Finding, Severity};
    use slop_llm::{ClaudeJudge, Judge, JudgeInput};

    let candidates = slop_analyze::tier3::candidates(built, facts, repo);
    if candidates.is_empty() {
        eprintln!("tier3: no semantic-redundancy candidates");
        return Ok(Vec::new());
    }
    let judge = ClaudeJudge::from_env()?;
    eprintln!(
        "tier3: judging {} candidate pair(s) via {}",
        candidates.len(),
        judge.model
    );
    let render = |c: &slop_analyze::tier3::CandidateFn| {
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

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Check {
            repo,
            index,
            all,
            policy,
            tier3,
            base,
        } => {
            let index_path = index.unwrap_or_else(|| repo.join("index.scip"));
            if !index_path.exists() {
                bail!(
                    "no SCIP index at {} — generate one with:\n  npx --yes @sourcegraph/scip-python index {} --project-name <name> --output {}",
                    index_path.display(),
                    repo.display(),
                    index_path.display(),
                );
            }
            let resolver = ScipResolver::load(&index_path)?;
            let policy = match policy {
                Some(path) => Policy::load_file(&path)?,
                None => Policy::load(&repo)?,
            };

            let mut built = build::build_graph(&resolver);
            effects::infer_effects(&mut built);
            let facts = source::parse_repo(&repo, &resolver.files());
            let mut raw = detect::run_all(&built, &policy, &facts);
            if tier3 {
                raw.extend(tier3_findings(&built, &facts, &repo)?);
            }
            let suppressions = suppress::scan(&repo, &facts);
            let unsuppressed = suppress::filter(raw, &suppressions);
            let baseline = Baseline::load(&repo)?;
            let effective = baseline.filter(unsuppressed);

            let (findings, health_line) = if all {
                let s = health::score(&effective, &built);
                (effective, format!("health: {s}/100"))
            } else {
                let output = Process::new("git")
                    .args(["-C"])
                    .arg(&repo)
                    .args(["diff", "-U0", "--no-color", &base, "--", "*.py"])
                    .output()
                    .context("running git diff (use --all for a non-git tree)")?;
                if !output.status.success() {
                    bail!(
                        "git diff failed: {} — use --all to judge the whole repo",
                        String::from_utf8_lossy(&output.stderr).trim()
                    );
                }
                let changed = diff::parse_unified_diff(&String::from_utf8_lossy(&output.stdout));
                let new = diff::filter_to_changes(effective.clone(), &changed);
                let new_keys: std::collections::HashSet<(&str, String)> = new
                    .iter()
                    .map(|f| (f.rule, f.entity.clone()))
                    .collect();
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

            if policy.channels.is_empty() {
                eprintln!(
                    "note: {} has no slop.toml channel policy — infra-bypass checks are silent (run `slop init`)",
                    repo.display()
                );
            }

            if findings.is_empty() {
                println!("no slop found");
                println!("{health_line}");
                return Ok(());
            }
            let blocking = findings
                .iter()
                .filter(|f| f.severity == slop_analyze::findings::Severity::Blocking)
                .count();
            for finding in &findings {
                println!("{finding}\n");
            }
            println!("{} finding(s), {} blocking", findings.len(), blocking);
            println!("{health_line}");
            if blocking > 0 {
                std::process::exit(1);
            }
        }
        Command::Baseline { repo, index } => {
            let index_path = index.unwrap_or_else(|| repo.join("index.scip"));
            let resolver = ScipResolver::load(&index_path)?;
            let policy = Policy::load(&repo)?;
            let mut built = build::build_graph(&resolver);
            effects::infer_effects(&mut built);
            let facts = source::parse_repo(&repo, &resolver.files());
            let raw = detect::run_all(&built, &policy, &facts);
            let suppressions = suppress::scan(&repo, &facts);
            let findings = suppress::filter(raw, &suppressions);
            let baseline = Baseline::from_findings(&findings);
            let count = baseline.findings.len();
            baseline.save(&repo)?;
            println!(
                "baselined {count} finding(s) into {}",
                repo.join(slop_analyze::baseline::BASELINE_FILE).display()
            );
        }
        Command::Init { repo, index, write } => {
            let index_path = index.unwrap_or_else(|| repo.join("index.scip"));
            let resolver = ScipResolver::load(&index_path)?;
            let mut built = build::build_graph(&resolver);
            effects::infer_effects(&mut built);
            let proposals = infer::infer_channels(&built);
            if proposals.is_empty() {
                println!("no dominant effect channels found — nothing to propose");
                return Ok(());
            }
            let snippet = infer::to_toml(&proposals);
            println!("proposed policy (confirm before enforcing):\n\n{snippet}");
            if write {
                let path = repo.join("slop.toml");
                if path.exists() {
                    bail!("{} already exists — merge manually", path.display());
                }
                std::fs::write(&path, &snippet)?;
                println!("wrote {}", path.display());
            }
        }
        Command::IndexInfo { index } => {
            let resolver = ScipResolver::load(&index)?;
            println!("definitions: {}", resolver.definition_count());
            for file in resolver.files() {
                let occs = resolver.occurrences_in(file);
                let defs = occs.iter().filter(|o| o.is_definition).count();
                println!(
                    "{file}: {} occurrences ({} definitions, {} references)",
                    occs.len(),
                    defs,
                    occs.len() - defs
                );
            }
        }
        Command::Occurrences { index, file } => {
            let resolver = ScipResolver::load(&index)?;
            for occ in resolver.occurrences_in(&file) {
                let role = if occ.is_definition { "def" } else { "ref" };
                let known = resolver.definition_of(&occ.symbol).is_some()
                    || resolver.local_definition_of(&file, &occ.symbol).is_some();
                let resolved = if known { "" } else { "  [no definition]" };
                println!(
                    "{}:{}-{}:{} {role} {}{resolved}",
                    occ.range.start_line,
                    occ.range.start_col,
                    occ.range.end_line,
                    occ.range.end_col,
                    occ.symbol
                );
            }
        }
        Command::Resolve {
            index,
            file,
            line,
            col,
        } => {
            let resolver = ScipResolver::load(&index)?;
            match resolver.resolve(&file, line, col) {
                Some(def) => {
                    println!("symbol:  {}", def.symbol);
                    println!("name:    {}", def.display_name);
                    match (&def.file, &def.range) {
                        (Some(f), Some(r)) => {
                            println!("defined: {f}:{}:{}", r.start_line, r.start_col)
                        }
                        _ => println!("defined: <external>"),
                    }
                }
                None => println!("no definition found at {file}:{line}:{col}"),
            }
        }
    }
    Ok(())
}
