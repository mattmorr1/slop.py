use std::path::{Path, PathBuf};
use std::process::Command as Process;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use slop_analyze::baseline::Baseline;
use slop_analyze::check::{self, CheckRequest};
use slop_analyze::compress::{self, CompressConfig};
use slop_analyze::{detect, gate, harness, infer, policy::Policy, suppress};
use slop_resolve::{Resolver, ScipResolver};

#[derive(Parser)]
#[command(name = "slop", about = "Codebase-relative AI-slop analyzer", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// Hook events slop can handle. Kebab-cased on the CLI by clap:
/// `post-tool-use`, `user-prompt-submit`.
#[derive(Clone, Copy, ValueEnum)]
enum HookEvent {
    PostToolUse,
    UserPromptSubmit,
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
    /// Zoned graph-distance compression of a file (D11): full fidelity within
    /// --hops of the --edit loci, skeletons beyond. Prints the compressed
    /// source; --stats reports the token win. Empty --edit = strip-noise only.
    Compress {
        /// Repo root
        repo: PathBuf,
        /// Repo-relative .py file to compress
        file: String,
        /// Path to index.scip (default: <repo>/index.scip)
        #[arg(long)]
        index: Option<PathBuf>,
        /// Edit-zone entity id (repeatable), e.g. `utils.dates::parse`
        #[arg(long = "edit")]
        edit: Vec<String>,
        /// Edit-zone by file (repeatable): use every entity defined in this
        /// repo-relative file as a locus — mirrors what the read hook does.
        #[arg(long = "edit-file")]
        edit_file: Vec<String>,
        /// Graph hops from the edit zone kept full-fidelity
        #[arg(long, default_value_t = 1)]
        hops: usize,
        /// Add an LLM one-line summary to each skeleton (via ollama;
        /// OLLAMA_MODEL, default qwen2.5:1.5b)
        #[arg(long)]
        densify: bool,
        /// Print compression stats to stderr
        #[arg(long)]
        stats: bool,
    },
    /// ANTHROPIC_BASE_URL reverse proxy (M5): steer + observe any Anthropic
    /// client. Set ANTHROPIC_BASE_URL=http://localhost:<port> to route through it.
    Proxy {
        /// Port to listen on (localhost only)
        #[arg(long, default_value_t = 8787)]
        port: u16,
        /// Upstream Anthropic API base URL
        #[arg(long, default_value = "https://api.anthropic.com")]
        upstream: String,
        /// Repo whose sanctioned-channel policy is injected when --steer is set
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Augment the request's system prompt with the repo's channel policy
        #[arg(long)]
        steer: bool,
        /// Append per-request token usage to this JSONL log
        #[arg(long)]
        log: Option<PathBuf>,
    },
    /// Validation gate (M5): run the check and exit non-zero if anything
    /// blocks, printing a JSON verdict a CI step or agent fix-loop consumes.
    Gate {
        /// Repo root
        repo: PathBuf,
        /// Path to index.scip (default: <repo>/index.scip)
        #[arg(long)]
        index: Option<PathBuf>,
        /// Judge the whole repo instead of the diff
        #[arg(long)]
        all: bool,
        /// Also run advisory Tier-3 semantic-redundancy judging
        #[arg(long)]
        tier3: bool,
        /// Git ref to diff against (default: HEAD)
        #[arg(long, default_value = "HEAD")]
        base: String,
        /// Regenerate the SCIP index (via scip-python) before checking
        #[arg(long)]
        reindex: bool,
        /// Run inside an isolated `git worktree` of the repo
        #[arg(long)]
        worktree: bool,
    },
    /// Agent-host hook (M4d). Reads the hook JSON on stdin, writes the
    /// response on stdout. Register in .claude/settings.json (see docs/harness.md).
    Hook {
        /// Which hook event this invocation handles
        event: HookEvent,
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
        Command::Compress {
            repo,
            file,
            index,
            mut edit,
            edit_file,
            hops,
            densify,
            stats,
        } => {
            let analysis = check::load_analysis(&repo, index.as_deref())?;
            // Expand --edit-file into the entity ids defined in those files.
            if !edit_file.is_empty() {
                for (_, e) in analysis.built.graph.entities() {
                    if edit_file.contains(&e.file) {
                        edit.push(e.id.clone());
                    }
                }
            }
            let source = std::fs::read_to_string(repo.join(&file))
                .with_context(|| format!("reading {file}"))?;

            // --densify: one ollama summary per skeletonized entity, built from
            // its own body sliced out of the source we already hold.
            let src_lines: Vec<&str> = source.lines().collect();
            let ollama = densify.then(slop_llm::ollama::Ollama::from_env);
            let densifier = ollama.as_ref().map(|client| {
                move |e: &slop_graph::CodeEntity| -> Option<String> {
                    let (s, end) = e.source_range;
                    let end = end.min(src_lines.len().saturating_sub(1));
                    let body = src_lines.get(s..=end)?.join("\n");
                    client.summarize(&body).ok().filter(|s| !s.is_empty())
                }
            });
            let densifier_ref: Option<&compress::Densifier> = match &densifier {
                Some(f) => Some(f as &compress::Densifier),
                None => None,
            };

            let (out, st) = compress::compress_file(
                &analysis.built,
                &analysis.facts,
                &source,
                &file,
                &edit,
                &CompressConfig {
                    edit_zone_hops: hops,
                    min_lines: 0,
                },
                densifier_ref,
            );
            print!("{out}");
            if stats {
                let pct = if st.original_chars > 0 {
                    100 - (st.compressed_chars * 100 / st.original_chars)
                } else {
                    0
                };
                eprintln!(
                    "compress: {}/{} functions skeletonized, {} -> {} chars (-{}%)",
                    st.skeletonized, st.total_functions, st.original_chars, st.compressed_chars, pct
                );
            }
        }
        Command::Proxy {
            port,
            upstream,
            repo,
            steer,
            log,
        } => {
            slop_proxy::serve(slop_proxy::ProxyConfig {
                port,
                upstream,
                repo,
                steer,
                log,
            })?;
        }
        Command::Gate {
            repo,
            index,
            all,
            tier3,
            base,
            reindex,
            worktree,
        } => {
            // Optionally run against an isolated worktree of the repo so an
            // agent's in-flight edits are evaluated without touching the tree.
            let worktree_dir = if worktree {
                Some(add_worktree(&repo)?)
            } else {
                None
            };
            let work_repo = worktree_dir.clone().unwrap_or_else(|| repo.clone());
            let index = index.map(|i| {
                if worktree {
                    // A caller-supplied index refers to the original tree;
                    // resolve it relative to the worktree copy instead.
                    work_repo.join(i.file_name().unwrap_or_else(|| i.as_os_str()))
                } else {
                    i
                }
            });

            if reindex {
                run_scip_index(&work_repo, index.as_deref())?;
            }

            let outcome = gate::evaluate(CheckRequest {
                repo: work_repo,
                index,
                policy: None,
                all,
                tier3,
                base,
            });

            if let Some(dir) = worktree_dir {
                remove_worktree(&repo, &dir);
            }

            let outcome = outcome?;
            println!("{}", outcome.json);
            if outcome.blocking > 0 {
                std::process::exit(1);
            }
        }
        Command::Hook { event } => {
            use std::io::Read;
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            let input: serde_json::Value = if buf.trim().is_empty() {
                serde_json::json!({})
            } else {
                serde_json::from_str(&buf)?
            };
            let output = match event {
                HookEvent::PostToolUse => harness::handle_post_tool_use(&input),
                HookEvent::UserPromptSubmit => harness::handle_user_prompt_submit(&input),
            };
            println!("{output}");
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

/// Regenerate the SCIP index for `repo` via `scip-python`, writing to
/// `index` (default `<repo>/index.scip`). Used by `slop gate --reindex` so a
/// re-run after edits sees the new graph.
fn run_scip_index(repo: &Path, index: Option<&Path>) -> Result<()> {
    let out = index
        .map(Path::to_path_buf)
        .unwrap_or_else(|| repo.join("index.scip"));
    let status = Process::new("npx")
        .args(["--yes", "@sourcegraph/scip-python", "index"])
        .arg(repo)
        .args(["--project-name", "slop-gate", "--output"])
        .arg(&out)
        .status()
        .context("running scip-python (is npx on PATH?)")?;
    if !status.success() {
        bail!("scip-python indexing failed for {}", repo.display());
    }
    Ok(())
}

/// Create a detached `git worktree` of `repo` in a temp dir and return its
/// path. The gate evaluates this isolated copy.
fn add_worktree(repo: &Path) -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("slop-gate-{}", std::process::id()));
    let status = Process::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "add", "--detach"])
        .arg(&dir)
        .status()
        .context("running git worktree add")?;
    if !status.success() {
        bail!("git worktree add failed for {}", repo.display());
    }
    Ok(dir)
}

/// Best-effort teardown of a gate worktree. Failures are reported but don't
/// override the gate's own verdict/exit code.
fn remove_worktree(repo: &Path, dir: &Path) {
    let ok = Process::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "remove", "--force"])
        .arg(dir)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        eprintln!("warning: could not remove worktree {}", dir.display());
    }
}
