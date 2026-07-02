use std::path::PathBuf;
use std::process::Command as Process;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use slop_analyze::{build, detect, diff, effects, policy::Policy};
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
        /// Git ref to diff against (default: HEAD)
        #[arg(long, default_value = "HEAD")]
        base: String,
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
            let facts = slop_analyze::source::parse_repo(&repo, &resolver.files());
            let mut findings = detect::run_all(&built, &policy, &facts);

            if !all {
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
                findings = diff::filter_to_changes(findings, &changed);
            }

            if policy.channels.is_empty() {
                eprintln!(
                    "note: {} has no slop.toml channel policy — infra-bypass checks are silent",
                    repo.display()
                );
            }

            if findings.is_empty() {
                println!("no slop found");
                return Ok(());
            }
            let blocking = findings
                .iter()
                .filter(|f| f.severity == slop_analyze::findings::Severity::Blocking)
                .count();
            for finding in &findings {
                println!("{finding}\n");
            }
            println!(
                "{} finding(s), {} blocking",
                findings.len(),
                blocking
            );
            if blocking > 0 {
                std::process::exit(1);
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
