use std::path::PathBuf;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use slop_analyze::baseline::Baseline;
use slop_analyze::check::{self, CheckRequest};
use slop_analyze::{detect, infer, policy::Policy, suppress};
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
    /// Serve slop's MCP tools (validate_change, get_context_envelope,
    /// query_subgraph) over stdio. Launched per-project by an agent host
    /// (e.g. Claude Code); speaks newline-delimited JSON-RPC 2.0.
    Mcp {
        /// Repo root the tools default to
        repo: PathBuf,
        /// Path to index.scip (default: <repo>/index.scip)
        #[arg(long)]
        index: Option<PathBuf>,
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
            let result = check::run(CheckRequest {
                repo: repo.clone(),
                index,
                policy,
                all,
                tier3,
                base,
            })?;

            if result.policy_is_empty {
                eprintln!(
                    "note: {} has no slop.toml channel policy — infra-bypass checks are silent (run `slop init`)",
                    repo.display()
                );
            }

            if result.findings.is_empty() {
                println!("no slop found");
                println!("{}", result.health_line);
                return Ok(());
            }
            for finding in &result.findings {
                println!("{finding}\n");
            }
            println!(
                "{} finding(s), {} blocking",
                result.findings.len(),
                result.blocking
            );
            println!("{}", result.health_line);
            if result.blocking > 0 {
                std::process::exit(1);
            }
        }
        Command::Mcp { repo, index } => {
            slop_mcp::serve_stdio(repo, index)?;
        }
        Command::Baseline { repo, index } => {
            let policy = Policy::load(&repo)?;
            let check::Analysis { built, facts } = check::load_analysis(&repo, index.as_deref())?;
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
            let check::Analysis { built, .. } = check::load_analysis(&repo, index.as_deref())?;
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
