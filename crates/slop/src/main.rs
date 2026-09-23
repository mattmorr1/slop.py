use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::process::Command as Process;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use slop_analyze::baseline::Baseline;
use slop_analyze::check::{self, CheckRequest, CheckResult};
use slop_analyze::findings::{FindingId, Severity};

use slop_analyze::compress::{self, CompressConfig};
use slop_analyze::index::{
    default_project_name, ensure_index, run_indexer, run_scip_index, Indexer,
};
use slop_analyze::{gate, harness, infer, repair, retrieve};
use slop_resolve::{IndexSet, Resolver, ScipResolver};

mod setup;

#[derive(Parser)]
#[command(name = "slop", about = "Codebase-relative AI-slop analyzer", version)]
#[command(
    after_help = "Start with `slop setup`, then run `slop check`. The dashboard is available as `slop dash`."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

/// Hook events slop can handle. Kebab-cased on the CLI by clap:
/// `post-tool-use`, `user-prompt-submit`.
#[derive(Clone, Copy, ValueEnum)]
enum HookEvent {
    PreToolUse,
    PostToolUse,
    UserPromptSubmit,
    SessionStart,
    SubagentStart,
}

/// Which SCIP indexer `slop index` runs. Mirrors [`Indexer`]; separate only
/// because deriving clap's `ValueEnum` on the library type would put clap in
/// the analysis crate.
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum IndexerArg {
    /// Detect from the repo's project markers.
    Auto,
    /// `scip-python`.
    Python,
    /// `scip-typescript`.
    Typescript,
    /// `rust-analyzer scip`.
    Rust,
}

impl From<IndexerArg> for Indexer {
    fn from(a: IndexerArg) -> Self {
        match a {
            IndexerArg::Auto => Indexer::Auto,
            IndexerArg::Python => Indexer::Python,
            IndexerArg::Typescript => Indexer::Typescript,
            IndexerArg::Rust => Indexer::Rust,
        }
    }
}

/// Severity threshold at which `slop gate` fails (exits non-zero).
#[derive(Clone, Copy, ValueEnum)]
enum FailOn {
    /// Only deterministic blockers (infra-bypass, circular-import). Default.
    Blocking,
    /// ...also Warnings (duplicate-exact, duplicate-equivalent, complexity-spike, purity-lie).
    Warning,
    /// ...also Advisories (everything).
    Advisory,
}

impl From<FailOn> for slop_analyze::findings::Severity {
    fn from(f: FailOn) -> Self {
        use slop_analyze::findings::Severity;
        match f {
            FailOn::Blocking => Severity::Blocking,
            FailOn::Warning => Severity::Warning,
            FailOn::Advisory => Severity::Advisory,
        }
    }
}

#[derive(Subcommand)]
enum Command {
    /// Analyze a repo. Default: judge only the working-tree diff (vs HEAD).
    #[command(visible_alias = "analyze")]
    Check {
        /// Repo root (default: current directory; must contain slop.toml for
        /// policy-gated checks)
        #[arg(default_value = ".")]
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
        /// Emit findings as a JSON object (for editors / tooling) instead of
        /// the human report.
        #[arg(long)]
        json: bool,
        /// Regenerate the SCIP index before checking (via the auto-detected
        /// indexer). Without it, a stale index only earns a warning.
        #[arg(long)]
        reindex: bool,
    },
    /// Preview or transactionally apply snapshot-bound repairs. Index-verified
    /// renames are enabled by default; graded repairs require an opt-in flag.
    Fix {
        /// Repo root (default: current directory)
        #[arg(default_value = ".")]
        repo: PathBuf,
        /// Path to index.scip (default: <repo>/index.scip)
        #[arg(long)]
        index: Option<PathBuf>,
        /// Apply changes to disk (default: report only)
        #[arg(long)]
        write: bool,
        /// Repair exactly one finding ID from `slop check --json`.
        #[arg(long)]
        finding: Option<String>,
        /// Include graded restating-comment repairs. Tooling directives remain
        /// protected, but the classification is heuristic and requires review.
        #[arg(long)]
        allow_advisory: bool,
        /// Also remove dead free functions (Warning `dead-island`). Opt-in even
        /// with --write: deleting code is riskier than the comment/rename fixes
        /// (SCIP can miss a dynamic caller), so review the dry-run first.
        #[arg(long)]
        remove_dead: bool,
        /// Also inline judge-confirmed trivial wrappers (`trivial-wrapper`):
        /// rewrite every reference to the callee and delete the wrapper. Runs
        /// the Tier-3 judge (needs an API key) and touches multiple files, so
        /// it is opt-in; review the dry-run first.
        #[arg(long)]
        inline_wrappers: bool,
    },
    /// Zoned graph-distance compression of a file (D11): full fidelity within
    /// --hops of the --edit loci, skeletons beyond. Prints the compressed
    /// source; --stats reports the token win. Empty --edit = strip-noise only.
    Compress {
        /// Repo root
        repo: PathBuf,
        /// Repo-relative source file to compress (Python, JS/TS, or Rust)
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
        /// Maximum simultaneous client connections; excess receives HTTP 503.
        #[arg(long, default_value_t = 32)]
        max_connections: usize,
        /// Maximum request body size in MiB.
        #[arg(long, default_value_t = 16)]
        max_request_mib: usize,
        /// Maximum response bytes retained for usage extraction in MiB.
        #[arg(long, default_value_t = 2)]
        max_capture_mib: usize,
        /// Client/upstream I/O timeout in seconds.
        #[arg(long, default_value_t = 300)]
        timeout_seconds: u64,
    },
    /// Validation gate (M5): run the check and exit non-zero if anything
    /// blocks, printing a JSON verdict a CI step or agent fix-loop consumes.
    Gate {
        /// Repo root (default: current directory)
        #[arg(default_value = ".")]
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
        /// Severity at or above which the gate fails. Lower it to `warning` to
        /// make the loop act on duplicate-exact / duplicate-equivalent / complexity-spike.
        #[arg(long, value_enum, default_value_t = FailOn::Blocking)]
        fail_on: FailOn,
        /// Regenerate the SCIP index (auto-detected indexer) before checking
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
    /// Open the optional interactive findings dashboard (full-screen TUI).
    Dash {
        /// Repo root (default: current directory)
        #[arg(default_value = ".")]
        repo: PathBuf,
        /// Path to index.scip (default: <repo>/index.scip)
        #[arg(long)]
        index: Option<PathBuf>,
    },
    /// Serve slop as a language server (LSP over stdio): publishes findings as
    /// editor diagnostics on open/save. Editor-agnostic — point VS Code,
    /// Neovim, Zed, or JetBrains at `slop lsp` (see editors/vscode for a shim).
    Lsp {
        /// Repo root the server analyzes (default: current directory)
        #[arg(default_value = ".")]
        repo: PathBuf,
        /// Path to index.scip (default: <repo>/index.scip)
        #[arg(long)]
        index: Option<PathBuf>,
    },
    /// Serve slop's MCP tools (assess_write, validate_change,
    /// get_context_envelope, query_subgraph) over stdio. Launched by an agent host
    /// (e.g. Claude Code); speaks newline-delimited JSON-RPC 2.0.
    Mcp {
        /// Repo root the tools default to (default: current directory)
        #[arg(default_value = ".")]
        repo: PathBuf,
        /// Path to index.scip (default: <repo>/index.scip)
        #[arg(long)]
        index: Option<PathBuf>,
    },
    /// Generate SCIP artifacts for every detected language. Explicit
    /// --indexer/--output retains the single-artifact compatibility seam.
    Index {
        /// Repo root to index (default: current directory)
        #[arg(default_value = ".")]
        repo: PathBuf,
        /// Output path (default: <repo>/index.scip)
        #[arg(long)]
        output: Option<PathBuf>,
        /// scip-python project name (default: the repo directory name)
        #[arg(long)]
        project_name: Option<String>,
        /// Which SCIP indexer to run (default: auto-detect from project markers)
        #[arg(long, value_enum, default_value_t = IndexerArg::Auto)]
        indexer: IndexerArg,
    },
    /// Configure slop for an agent host and editor, then install the repo-local
    /// harness transactionally. Preferences are user-local; harness files are
    /// project-local and can be removed with `slop uninstall`.
    Setup {
        /// Repo root to configure (default: current directory)
        #[arg(default_value = ".")]
        repo: PathBuf,
        /// Preferred AI host. Required when stdin is not interactive unless a
        /// preference was saved by an earlier setup.
        #[arg(long, value_enum)]
        ai: Option<setup::AgentHost>,
        /// Preferred editor to open from `slop launch`.
        #[arg(long, value_enum)]
        editor: Option<setup::Editor>,
        /// Replace invalid or conflicting slop-owned generated files.
        #[arg(long)]
        force: bool,
    },
    /// Verify the saved AI/editor, harness receipt, project markers, and index.
    Doctor {
        /// Repo root to diagnose (default: current directory)
        #[arg(default_value = ".")]
        repo: PathBuf,
    },
    /// Remove only entries and generated files owned by `slop setup`.
    Uninstall {
        /// Repo root to disconnect (default: current directory)
        #[arg(default_value = ".")]
        repo: PathBuf,
        /// Remove generated files even if their content changed after setup.
        #[arg(long)]
        force: bool,
    },
    /// Open the preferred editor and run the preferred AI in this repository.
    Launch {
        /// Repo root (default: current directory)
        #[arg(default_value = ".")]
        repo: PathBuf,
        /// Override the saved AI for this launch.
        #[arg(long, value_enum)]
        ai: Option<setup::AgentHost>,
        /// Override the saved editor for this launch.
        #[arg(long, value_enum)]
        editor: Option<setup::Editor>,
        /// Do not refresh the repo-local harness before launching.
        #[arg(long)]
        no_setup: bool,
        /// Do not open the configured editor.
        #[arg(long)]
        no_editor: bool,
        /// Args passed through to the AI host (everything after `--`).
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Internal adapter used by the dashboard to delegate one scoped repair to
    /// the AI selected by `slop setup`.
    #[command(hide = true)]
    Delegate {
        #[arg(default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        prompt: String,
    },
    /// Internal adapter used by the dashboard to open a finding in the editor
    /// selected by `slop setup`.
    #[command(hide = true)]
    Open { target: String },
    /// Research adapter for the context benchmark: one capture, then a context
    /// artifact per (target, budget), after one line with the shared cost catalog.
    #[command(hide = true)]
    ContextBench {
        repo: PathBuf,
        /// One target entity id per line.
        targets: PathBuf,
        #[arg(long, value_delimiter = ',', default_value = "1000,2000,4000,8000,16000")]
        budgets: Vec<usize>,
        /// `coverage` (default) or `ranked` (the ablation baseline).
        #[arg(long, default_value = "coverage")]
        selection: String,
    },
    /// Research adapter for the equivalence benchmark: each JSONL line
    /// `{"a": src, "b": src}` (one Python function each) yields every tier's verdict.
    #[command(hide = true)]
    Equiv {
        pairs: PathBuf,
        /// egglog saturation rounds (requires the `egraph` feature).
        #[arg(long, default_value_t = 12)]
        iterations: usize,
    },
    /// Wire slop's harness into a repo's agent-host config: merge the MCP
    /// server into `<repo>/.mcp.json` and the read/prompt hooks into
    /// `<repo>/.claude/settings.json`, pointing at this binary. Idempotent —
    /// re-running updates slop's own entries and leaves the rest untouched.
    #[command(hide = true)]
    Install {
        /// Repo root to install into (default: current directory)
        #[arg(default_value = ".")]
        repo: PathBuf,
        /// Overwrite even if an existing config file fails to parse as JSON
        #[arg(long)]
        force: bool,
    },
    /// Launch Claude Code with slop's context layer active. Idempotently wires
    /// the harness (MCP tools + read/prompt hooks + skill) into the repo, then
    /// starts `claude` there so it loads them. `--proxy` also routes the session
    /// through slop's steering + token-observability proxy. Args after `--` pass
    /// through to claude (e.g. `slop claude -- --resume`).
    #[command(hide = true)]
    Claude {
        /// Repo root (default: current directory)
        #[arg(default_value = ".")]
        repo: PathBuf,
        /// Skip the idempotent harness install (assume it's already wired)
        #[arg(long)]
        no_install: bool,
        /// Also start `slop proxy --steer` and point this claude session at it
        #[arg(long)]
        proxy: bool,
        /// Port for the steering proxy when --proxy is set
        #[arg(long, default_value_t = 8787)]
        proxy_port: u16,
        /// Args passed through to claude (everything after `--`)
        #[arg(last = true)]
        claude_args: Vec<String>,
    },
    /// Record current findings as the grandfathered baseline.
    Baseline {
        /// Repo root (default: current directory)
        #[arg(default_value = ".")]
        repo: PathBuf,
        /// Path to index.scip (default: <repo>/index.scip)
        #[arg(long)]
        index: Option<PathBuf>,
    },
    /// Infer sanctioned channels from dominant patterns; print (and
    /// optionally write) a slop.toml.
    Init {
        /// Repo root (default: current directory)
        #[arg(default_value = ".")]
        repo: PathBuf,
        /// Path to index.scip (default: <repo>/index.scip)
        #[arg(long)]
        index: Option<PathBuf>,
        /// Write <repo>/slop.toml (refuses to overwrite an existing one)
        #[arg(long)]
        write: bool,
    },
    /// Where does proposed code belong? Reads the content on stdin and names
    /// existing functions whose callee neighborhood it overlaps.
    Suggest {
        /// Repo root (default: current directory)
        #[arg(default_value = ".")]
        repo: PathBuf,
        /// Path to index.scip (default: <repo>/index.scip)
        #[arg(long)]
        index: Option<PathBuf>,
        /// Path the content is destined for; sets the language (default: Python)
        #[arg(long)]
        file: Option<String>,
        #[arg(long, default_value_t = 5)]
        limit: usize,
        /// Leave-one-out over every indexed function instead of reading stdin:
        /// feed each existing body back as if proposed, hiding its own entry.
        #[arg(long)]
        eval: bool,
    },
    /// Low-level SCIP index introspection (index-info / occurrences / resolve).
    Debug {
        #[command(subcommand)]
        command: DebugCommand,
    },
}

#[derive(Subcommand)]
enum DebugCommand {
    /// Load a SCIP index and print what the resolver sees.
    IndexInfo {
        /// Path to index.scip
        index: PathBuf,
    },
    /// List all occurrences recorded in a file.
    Occurrences {
        /// Path to index.scip
        index: PathBuf,
        /// Repo-relative file path as recorded in the index
        file: String,
    },
    /// Resolve the reference at file:line:col (0-based) to its definition.
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
    let Some(command) = cli.command else {
        use clap::CommandFactory;
        Cli::command().print_help()?;
        println!();
        return Ok(());
    };
    match command {
        Command::Dash { repo, index } => {
            if let Some(index_path) = index.as_deref() {
                ensure_index(&repo, index_path)?;
            }
            slop_tui::run(repo, index)?;
        }
        Command::Check {
            repo,
            index,
            all,
            policy,
            tier3,
            base,
            json,
            reindex,
        } => {
            // `check::run` reindexes a stale index itself; --reindex forces
            // one even when the mtimes say it is current.
            if reindex {
                run_scip_index(&repo, index.as_deref())?;
            }
            let result = check::run(CheckRequest {
                repo: repo.clone(),
                index,
                policy,
                all,
                tier3,
                base,
            })?;

            if json {
                print_check_json(&result)?;
            } else {
                print_check_human(&repo, &result);
            }
            if result.blocking > 0 {
                std::process::exit(1);
            }
        }
        Command::Fix {
            repo,
            index,
            write,
            finding,
            allow_advisory,
            remove_dead,
            inline_wrappers,
        } => {
            let analysis = check::load_analysis(&repo, index.as_deref())?;
            let mut findings = check::effective_findings(&analysis, false)?;
            if inline_wrappers {
                findings.extend(check::tier3_wrapper_findings(
                    &analysis.built,
                    &analysis.policy,
                    &analysis.facts,
                    &repo,
                )?);
            }
            let selection = match finding {
                Some(value) => repair::RepairSelection::Finding(
                    value.parse::<FindingId>().map_err(anyhow::Error::msg)?,
                ),
                None => repair::RepairSelection::All,
            };
            let eligible: Vec<_> = findings
                .into_iter()
                .filter(|finding| match finding.rule {
                    "naming-convention" => true,
                    "over-commenting" => allow_advisory,
                    "dead-island" => remove_dead,
                    "trivial-wrapper" => inline_wrappers,
                    _ => false,
                })
                .collect();
            let safety = if allow_advisory || remove_dead || inline_wrappers {
                repair::SafetyClass::Advisory
            } else {
                repair::SafetyClass::IndexVerified
            };
            let plan = repair::plan(&analysis, &eligible, selection, safety)?;
            for action in &plan.actions {
                println!(
                    "{} [{}] {} — {} ({})",
                    if write { "apply" } else { "would apply" },
                    action.rule,
                    action.entity,
                    action.summary,
                    action.finding
                );
            }
            for skipped in &plan.skipped {
                eprintln!("skip {}: {}", skipped.entity, skipped.reason);
            }
            if plan.files.is_empty() {
                println!("nothing to fix");
            } else if write {
                let action_ids: std::collections::HashSet<_> = plan
                    .actions
                    .iter()
                    .map(|action| action.finding.clone())
                    .collect();
                let pending = repair::apply(&analysis, &plan)?;
                let verification = (|| -> Result<_> {
                    run_scip_index(&repo, index.as_deref())?;
                    let verified = check::load_analysis_fresh(
                        &repo,
                        index.as_deref(),
                        check::Freshness::Warn,
                    )?;
                    let remaining = check::effective_findings(&verified, false)?
                        .into_iter()
                        .filter(|finding| action_ids.contains(&finding.id()))
                        .count();
                    if remaining > 0 {
                        bail!("verification left {remaining} selected finding(s)");
                    }
                    verified.write_prewrite_sidecar()?;
                    Ok(verified)
                })();
                let verified = match verification {
                    Ok(verified) => verified,
                    Err(verification_error) => {
                        pending.rollback().with_context(|| {
                            format!("repair verification failed ({verification_error:#}) and rollback failed")
                        })?;
                        if let Err(index_error) = run_scip_index(&repo, index.as_deref()) {
                            bail!(
                                "repair verification failed and source was rolled back, but restoring the index failed: {verification_error:#}; {index_error:#}"
                            );
                        }
                        let restore_evidence = (|| -> Result<()> {
                            let restored = check::load_analysis_fresh(
                                &repo,
                                index.as_deref(),
                                check::Freshness::Warn,
                            )?;
                            restored.write_prewrite_sidecar()?;
                            Ok(())
                        })();
                        if let Err(restored_error) = restore_evidence {
                            bail!(
                                "repair verification failed and source/index were restored, but rebuilding prewrite evidence failed: {verification_error:#}; {restored_error:#}"
                            );
                        }
                        return Err(verification_error
                            .context("repair verification failed; source and index rolled back"));
                    }
                };
                let receipt = pending.commit()?;
                println!(
                    "applied {} repair(s) across {} file(s); verified snapshot {}",
                    receipt.actions,
                    receipt.files.len(),
                    verified.id()
                );
            } else {
                println!(
                    "dry-run: {} repair(s) across {} file(s)",
                    plan.actions.len(),
                    plan.files.len()
                );
                println!("apply with: slop fix --write");
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
                let pct = 100usize.saturating_sub(
                    st.compressed_chars
                        .saturating_mul(100)
                        .checked_div(st.original_chars)
                        .unwrap_or(100),
                );
                eprintln!(
                    "compress: {}/{} functions skeletonized, {} -> {} chars (-{}%)",
                    st.skeletonized,
                    st.total_functions,
                    st.original_chars,
                    st.compressed_chars,
                    pct
                );
            }
        }
        Command::Proxy {
            port,
            upstream,
            repo,
            steer,
            log,
            max_connections,
            max_request_mib,
            max_capture_mib,
            timeout_seconds,
        } => {
            slop_proxy::serve(slop_proxy::ProxyConfig {
                port,
                upstream,
                repo,
                steer,
                log,
                max_connections,
                max_request_bytes: max_request_mib.saturating_mul(1024 * 1024),
                max_capture_bytes: max_capture_mib.saturating_mul(1024 * 1024),
                timeout: std::time::Duration::from_secs(timeout_seconds),
            })?;
        }
        Command::Gate {
            repo,
            index,
            all,
            tier3,
            base,
            fail_on,
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

            // The gate runs through `check::run`, which builds a missing index
            // and refreshes a stale one; --reindex forces one regardless.
            if reindex {
                run_scip_index(&work_repo, index.as_deref())?;
            }

            let outcome = gate::evaluate(
                CheckRequest {
                    repo: work_repo,
                    index,
                    policy: None,
                    all,
                    tier3,
                    base,
                },
                fail_on.into(),
            );

            if let Some(dir) = worktree_dir {
                remove_worktree(&repo, &dir);
            }

            let outcome = outcome?;
            println!("{}", outcome.json);
            if outcome.failing > 0 {
                std::process::exit(1);
            }
        }
        Command::Hook { event } => {
            use std::io::Read;
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            // Hooks run on every read/write, so payload we can't parse is a
            // silent no-op rather than an error: a non-zero exit here would
            // surface as a hook failure in the agent host on each tool call.
            // Same discipline as the read-remap, which passes a read through
            // untouched rather than risk corrupting it.
            let input: serde_json::Value =
                serde_json::from_str(&buf).unwrap_or_else(|_| serde_json::json!({}));
            let output = match event {
                HookEvent::PreToolUse => harness::handle_pre_tool_use(&input),
                HookEvent::PostToolUse => harness::handle_post_tool_use(&input),
                HookEvent::UserPromptSubmit => harness::handle_user_prompt_submit(&input),
                HookEvent::SessionStart => harness::handle_session_start(&input, "SessionStart"),
                HookEvent::SubagentStart => harness::handle_session_start(&input, "SubagentStart"),
            };
            println!("{output}");
        }
        Command::Lsp { repo, index } => {
            slop_lsp::serve_stdio(repo, index)?;
        }
        Command::Mcp { repo, index } => {
            slop_mcp::serve_stdio(repo, index)?;
        }
        Command::Index {
            repo,
            output,
            project_name,
            indexer,
        } => {
            if indexer == IndexerArg::Auto && output.is_none() {
                run_scip_index(&repo, None)?;
                let paths = slop_analyze::index::index_paths(&repo, None);
                let resolver = IndexSet::load(&paths)?;
                if resolver.definition_count() == 0 {
                    bail!(
                        "indexed {} but the result has no definitions",
                        repo.display()
                    );
                }
                let snapshot = check::load_analysis_fresh(&repo, None, check::Freshness::Warn)?;
                snapshot.write_prewrite_sidecar()?;
                println!(
                    "indexed {} language artifact(s), {} definitions across {} file(s)",
                    paths.len(),
                    resolver.definition_count(),
                    resolver.files().len()
                );
                return Ok(());
            }
            let out = output.unwrap_or_else(|| repo.join("index.scip"));
            let project = project_name.unwrap_or_else(|| default_project_name(&repo));
            run_indexer(indexer.into(), &repo, &out, &project)?;
            // Verify it's usable, not just present (scip-python can exit 0 with
            // a broken, definition-less index).
            let resolver = ScipResolver::load(&out)?;
            let defs = resolver.definition_count();
            if defs == 0 {
                bail!(
                    "indexed {} but the result has no definitions — the indexer likely failed. Check its output above.",
                    repo.display()
                );
            }
            let snapshot = check::load_analysis_fresh(&repo, Some(&out), check::Freshness::Warn)?;
            snapshot.write_prewrite_sidecar()?;
            println!(
                "indexed {} -> {} ({} definitions across {} file(s))",
                repo.display(),
                out.display(),
                defs,
                resolver.files().len()
            );
        }
        Command::Setup {
            repo,
            ai,
            editor,
            force,
        } => {
            let preferences = setup::configure(&repo, ai, editor, force)?;
            println!(
                "configured {:?} + {:?}; run `slop doctor {}` to verify",
                preferences.ai,
                preferences.editor,
                repo.display()
            );
        }
        Command::Doctor { repo } => {
            if !setup::doctor(&repo)? {
                std::process::exit(1);
            }
        }
        Command::Uninstall { repo, force } => {
            let changed = setup::uninstall(&repo, force)?;
            println!("removed slop integration from {} file(s)", changed.len());
        }
        Command::Launch {
            repo,
            ai,
            editor,
            no_setup,
            no_editor,
            args,
        } => {
            setup::launch(&repo, ai, editor, no_setup, no_editor, &args)?;
        }
        Command::Delegate { repo, prompt } => {
            setup::delegate(&repo, &prompt)?;
        }
        Command::Open { target } => {
            setup::open_target(&target)?;
        }
        Command::ContextBench { repo, targets, budgets, selection } => run_context_bench(&repo, &targets, &budgets, &selection)?,
        Command::Equiv { pairs, iterations } => run_equiv(&pairs, iterations)?,
        Command::Install { repo, force } => {
            install_harness(&repo, force)?;
        }
        Command::Claude {
            repo,
            no_install,
            proxy,
            proxy_port,
            claude_args,
        } => {
            launch_claude(&repo, no_install, proxy, proxy_port, &claude_args)?;
        }
        Command::Baseline { repo, index } => {
            let analysis = check::load_analysis(&repo, index.as_deref())?;
            let findings = check::baseline_findings(&analysis);
            let baseline = Baseline::from_findings(&findings).with_effects(&analysis.built);
            let count = baseline.findings.len();
            baseline.save(&repo)?;
            println!(
                "baselined {count} finding(s) into {}",
                repo.join(slop_analyze::baseline::BASELINE_FILE).display()
            );
        }
        Command::Init { repo, index, write } => {
            let index_path = index.clone().unwrap_or_else(|| repo.join("index.scip"));
            ensure_index(&repo, &index_path)?;
            let analysis = check::load_analysis(&repo, index.as_deref())?;
            let built = &analysis.built;
            let proposals = infer::infer_channels(built);
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
        Command::Suggest {
            repo,
            index,
            file,
            limit,
            eval,
        } => run_suggest(&repo, index.as_deref(), file.as_deref(), limit, eval)?,
        Command::Debug { command } => run_debug(command)?,
    }
    Ok(())
}

/// Prospective retrieval: name the existing functions a piece of proposed code
/// overlaps. `--eval` runs it leave-one-out over the whole repo, which is how the
/// signal gets measured before it is wired into anything.
fn run_suggest(
    repo: &Path,
    index: Option<&Path>,
    file: Option<&str>,
    limit: usize,
    eval: bool,
) -> Result<()> {
    let analysis = check::load_analysis_fresh(repo, index, check::Freshness::Warn)?;
    let hood = analysis.neighborhood();
    if !eval {
        let mut source = String::new();
        std::io::stdin().read_to_string(&mut source)?;
        let lang = file
            .and_then(slop_parse::Language::from_path)
            .unwrap_or(slop_parse::Language::Python);
        let query = retrieve::query_from_source(lang, &source);
        let matches = hood.matches(&query, None, limit);
        if matches.is_empty() {
            println!(
                "nothing in {} shares this code's neighborhood",
                repo.display()
            );
            return Ok(());
        }
        println!(
            "this code calls {} known things; closest existing homes:",
            query.len()
        );
        for m in &matches {
            let score = (m.score_ppm as f64 / 1_000_000.0).sqrt();
            println!("  {}  ({}:{})", m.label, m.file, m.line + 1);
            println!(
                "      score {:.2}, {} distinctive of {} shared: {}",
                score,
                m.distinctive.len(),
                m.shared,
                m.distinctive.join(", ")
            );
        }
        return Ok(());
    }

    let mut hits = 0usize;
    let mut probed = 0usize;
    for ff in &analysis.facts {
        if slop_analyze::source::is_test_file(&ff.file) {
            continue;
        }
        let Some(lang) = slop_parse::Language::from_path(&ff.file) else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(repo.join(&ff.file)) else {
            continue;
        };
        let lines: Vec<&str> = text.lines().collect();
        for fact in &ff.functions {
            let (a, b) = (
                fact.start_line as usize,
                (fact.end_line as usize).min(lines.len() - 1),
            );
            if a > b {
                continue;
            }
            let body = lines[a..=b].join("\n");
            let query = retrieve::query_from_source(lang, &body);
            // The label the graph knows this function by, so it can be hidden.
            let label = analysis
                .built
                .graph
                .entities()
                .find(|(_, e)| e.file == ff.file && e.source_range.0 == a)
                .map(|(_, e)| e.id.clone());
            // Inline `mod tests` fixture builders are excluded as candidates by
            // `Neighborhood::build`; without this they still leak in as queries.
            if label
                .as_deref()
                .is_some_and(slop_analyze::source::is_test_entity)
            {
                continue;
            }
            probed += 1;
            let matches = hood.matches(&query, label.as_deref(), limit);
            if matches.is_empty() {
                continue;
            }
            hits += 1;
            println!(
                "{}:{}  {}",
                ff.file,
                a + 1,
                label.unwrap_or_else(|| fact.name.clone())
            );
            for m in &matches {
                let score = (m.score_ppm as f64 / 1_000_000.0).sqrt();
                println!(
                    "    -> {}  ({}:{})  {:.2} / {} distinctive: {}",
                    m.label,
                    m.file,
                    m.line + 1,
                    score,
                    m.distinctive.len(),
                    m.distinctive.join(", ")
                );
            }
        }
    }
    println!(
        "\n{hits} of {probed} functions retrieved an existing neighbor ({:.1}%)",
        100.0 * hits as f64 / probed.max(1) as f64
    );
    Ok(())
}

/// Low-level SCIP introspection behind `slop debug`. Kept off the top-level
/// help so the everyday surface (check / fix / gate / …) stays legible.
fn run_debug(command: DebugCommand) -> Result<()> {
    match command {
        DebugCommand::IndexInfo { index } => {
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
        DebugCommand::Occurrences { index, file } => {
            let resolver = ScipResolver::load(&index)?;
            for occ in resolver.occurrences_in(&file) {
                let role = if occ.is_definition { "def" } else { "ref" };
                let known = resolver.definition_of(&occ.symbol).is_some()
                    || resolver.local_definition_of(&file, &occ.symbol).is_some();
                let resolved = if known { "" } else { "  [no definition]" };
                // The enclosing range is the graph's `source_range`, and what
                // parser facts join against — print it or the join is opaque.
                let span = occ
                    .enclosing_range
                    .map(|r| format!(" encl={}-{}", r.start_line, r.end_line))
                    .unwrap_or_default();
                println!(
                    "{}:{}-{}:{} {role}{span} {}{resolved}",
                    occ.range.start_line,
                    occ.range.start_col,
                    occ.range.end_line,
                    occ.range.end_col,
                    occ.symbol
                );
            }
        }
        DebugCommand::Resolve {
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

/// ANSI colors, but only when stdout is a real terminal and `NO_COLOR` is
/// unset (https://no-color.org). Piping `slop check` into a file or another
/// program yields clean, uncolored text.
fn use_color() -> bool {
    std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
}

/// Wrap `s` in an ANSI SGR code when `color` is on; otherwise return it plain.
fn paint(s: &str, code: &str, color: bool) -> String {
    if color {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

/// The color a severity renders in: blocking = red, warning = yellow,
/// advisory = dim. Shared by the header and the summary line.
fn severity_code(sev: Severity) -> &'static str {
    match sev {
        Severity::Blocking => "1;31", // bold red
        Severity::Warning => "1;33",  // bold yellow
        Severity::Advisory => "2",    // dim
    }
}

/// The human report for `slop check`: findings grouped by severity
/// (blocking → advisory), colorized when writing to a terminal, then a one-line
/// summary and the health delta.
fn print_check_human(repo: &Path, result: &CheckResult) {
    let color = use_color();

    if result.policy_is_empty {
        eprintln!(
            "note: {} has no slop.toml channel policy — infra-bypass checks are silent (run `slop init`)",
            repo.display()
        );
    }

    if result.findings.is_empty() {
        println!("{}", paint("no slop found", "1;32", color)); // bold green
        println!("{}", result.health_line);
        println!(
            "coverage: {}/{} indexed · {}/{} parsed",
            result.coverage.indexed_files,
            result.coverage.repository_files,
            result.coverage.parsed_files,
            result.coverage.repository_files
        );
        return;
    }

    // Group by severity, most severe first, stable within a group.
    let mut counts = [0usize; 3]; // [advisory, warning, blocking] by Severity ordinal
    for sev in [Severity::Blocking, Severity::Warning, Severity::Advisory] {
        let group: Vec<_> = result
            .findings
            .iter()
            .filter(|f| f.severity == sev)
            .collect();
        if group.is_empty() {
            continue;
        }
        counts[sev as usize] = group.len();
        let header = format!("{} ({})", sev, group.len());
        println!("{}", paint(&header, severity_code(sev), color));
        for f in group {
            let loc = format!("{}:{}", f.file, f.lines.0 + 1);
            println!(
                "  {} {}  {}",
                paint(&format!("[{}]", f.rule), "1", color), // bold rule
                f.entity,
                paint(&loc, "2", color), // dim location
            );
            println!("    {}", f.message);
            println!("    {} {}", paint("fix:", "2", color), f.fix_guidance);
        }
        println!();
    }

    let summary = format!(
        "{} finding(s) — {} blocking, {} warning, {} advisory",
        result.findings.len(),
        counts[Severity::Blocking as usize],
        counts[Severity::Warning as usize],
        counts[Severity::Advisory as usize],
    );
    println!("{summary}");
    println!("{}", result.health_line);
    println!(
        "coverage: {}/{} indexed · {}/{} parsed",
        result.coverage.indexed_files,
        result.coverage.repository_files,
        result.coverage.parsed_files,
        result.coverage.repository_files
    );
}

/// The `--json` report for `slop check`: the same verdict shape `slop gate`
/// emits, so an editor or CI step can consume either interchangeably.
fn print_check_json(result: &CheckResult) -> Result<()> {
    let warning = result
        .findings
        .iter()
        .filter(|f| f.severity == Severity::Warning)
        .count();
    let advisory = result
        .findings
        .iter()
        .filter(|f| f.severity == Severity::Advisory)
        .count();
    let out = serde_json::json!({
        "schema_version": result.schema_version,
        "snapshot": result.snapshot,
        "freshness": result.freshness,
        "scope": result.scope,
        "coverage": result.coverage,
        "blocking": result.blocking,
        "warning": warning,
        "advisory": advisory,
        "total": result.findings.len(),
        "health": result.health_line,
        "current_health": result.current_health,
        "policy_is_empty": result.policy_is_empty,
        "findings": result.findings,
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

/// Compatibility shim for the original Claude-only command.
fn install_harness(repo: &Path, force: bool) -> Result<()> {
    let _ = setup::configure_claude(repo, force)?;
    println!("Claude harness installed; `slop setup` configures other hosts and editors");
    Ok(())
}

/// Launch Claude Code in `repo` with slop's context layer active. Wires the
/// harness (unless `no_install`), optionally starts the steering proxy and
/// points the session at it via `ANTHROPIC_BASE_URL`, then runs `claude` with
/// the repo as its working directory so it loads `.mcp.json` + the hooks.
fn launch_claude(
    repo: &Path,
    no_install: bool,
    proxy: bool,
    proxy_port: u16,
    claude_args: &[String],
) -> Result<()> {
    let repo = repo
        .canonicalize()
        .with_context(|| format!("resolving repo path {}", repo.display()))?;

    if !no_install {
        install_harness(&repo, false)?;
        eprintln!();
    }

    // Optional steering/observability proxy: start it, wait for the port, and
    // set ANTHROPIC_BASE_URL so this claude session routes through it. Killed
    // when claude exits.
    let mut proxy_child = None;
    let mut envs: Vec<(String, String)> = Vec::new();
    if proxy {
        if !repo.join("slop.toml").exists() {
            eprintln!(
                "note: {} has no slop.toml — the proxy will log tokens but inject no steering (run `slop init`)",
                repo.display()
            );
        }
        let exe = std::env::current_exe().context("resolving the slop binary path")?;
        let log = repo.join(".slop-proxy.jsonl");
        let child = Process::new(&exe)
            .arg("proxy")
            .args(["--port", &proxy_port.to_string()])
            .arg("--repo")
            .arg(&repo)
            .arg("--steer")
            .arg("--log")
            .arg(&log)
            .spawn()
            .context("starting slop proxy")?;
        wait_for_port(proxy_port)?;
        envs.push((
            "ANTHROPIC_BASE_URL".to_string(),
            format!("http://localhost:{proxy_port}"),
        ));
        eprintln!(
            "slop proxy on :{proxy_port} — this claude session routes through it (usage log: {})",
            log.display()
        );
        proxy_child = Some(child);
    }

    // Hand the terminal to claude (a full TUI); wait for it to exit.
    let status = Process::new("claude")
        .current_dir(&repo)
        .args(claude_args)
        .envs(envs)
        .status()
        .context("launching claude (is Claude Code on PATH?)");

    if let Some(mut child) = proxy_child {
        let _ = child.kill();
    }
    let status = status?;
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

/// Block until `127.0.0.1:port` accepts a connection (the proxy has bound), or
/// give up after ~5s.
fn wait_for_port(port: u16) -> Result<()> {
    use std::net::{TcpStream, ToSocketAddrs};
    use std::time::{Duration, Instant};
    let addr = ("127.0.0.1", port)
        .to_socket_addrs()?
        .next()
        .context("resolving proxy address")?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    bail!("slop proxy did not come up on :{port}");
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

/// One side's verdict inputs: the three token hashes plus the E-equivalence term.
struct EquivSide {
    exact: String,
    structural: String,
    alpha: String,
    sound: String,
    graded: String,
    #[cfg_attr(not(feature = "egraph"), allow(dead_code))]
    term: slop_parse::equiv::Term,
}

fn equiv_side(source: &str) -> Result<EquivSide> {
    let facts = slop_parse::Language::Python.parse(source)?;
    let first = facts.first().context("no function in source")?;
    let equiv = slop_parse::equiv::equivalence_facts(source)?
        .into_iter()
        .next()
        .context("no function in source")?;
    Ok(EquivSide {
        exact: first.body_hash.clone(),
        structural: first.structural_hash.clone(),
        alpha: first.alpha_hash.clone(),
        sound: equiv.sound,
        graded: equiv.graded,
        term: equiv.term,
    })
}

fn run_equiv(pairs: &Path, iterations: usize) -> Result<()> {
    let text = std::fs::read_to_string(pairs).with_context(|| format!("reading {}", pairs.display()))?;
    for (index, line) in text.lines().enumerate().filter(|(_, line)| !line.trim().is_empty()) {
        let pair: serde_json::Value = serde_json::from_str(line).with_context(|| format!("pair {index}"))?;
        let side = |key: &str| pair[key].as_str().context("pair needs string fields a and b").and_then(equiv_side);
        let start = std::time::Instant::now();
        let (a, b) = match side("a").and_then(|a| Ok((a, side("b")?))) {
            Ok(sides) => sides,
            Err(error) => {
                println!("{}", serde_json::json!({ "index": index, "error": format!("{error:#}") }));
                continue;
            }
        };
        let normalize_us = start.elapsed().as_micros() as u64 / 2;
        // An empty token hash means the body is below the significance floor: abstain.
        let same = |x: &str, y: &str| (!x.is_empty() && !y.is_empty()).then_some(x == y);
        #[cfg_attr(not(feature = "egraph"), allow(unused_mut))]
        let mut verdict = serde_json::json!({
            "index": index,
            "exact": same(&a.exact, &b.exact),
            "structural": same(&a.structural, &b.structural),
            "alpha": same(&a.alpha, &b.alpha),
            "sound": a.sound == b.sound,
            "graded": a.graded == b.graded,
            "normalize_us": normalize_us,
        });
        #[cfg(feature = "egraph")]
        for (key, tier) in [("egg_sound", slop_parse::equiv::Tier::Sound), ("egg_graded", slop_parse::equiv::Tier::Graded)] {
            let start = std::time::Instant::now();
            let proved = slop_parse::egraph::equal(&a.term, &b.term, tier, iterations)?;
            verdict[key] = serde_json::json!(proved);
            verdict[format!("{key}_us")] = serde_json::json!(start.elapsed().as_micros() as u64);
        }
        #[cfg(not(feature = "egraph"))]
        let _ = iterations;
        println!("{verdict}");
    }
    Ok(())
}

fn run_context_bench(repo: &Path, targets: &Path, budgets: &[usize], selection: &str) -> Result<()> {
    use slop_analyze::context::ContextRequest;
    use slop_analyze::envelope::{self, Selection};
    let selection = match selection {
        "coverage" => Selection::Coverage,
        "ranked" => Selection::Ranked,
        other => bail!("unknown selection {other:?}: use coverage or ranked"),
    };
    let snapshot = check::load_analysis(repo, None)?;
    let catalog = envelope::catalog(&snapshot.built, &snapshot.facts);
    println!("{}", serde_json::json!({ "snapshot": snapshot.id(), "catalog": catalog }));
    let text = std::fs::read_to_string(targets).with_context(|| format!("reading {}", targets.display()))?;
    for target in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        println!(
            "{}",
            serde_json::json!({
                "target": target,
                "neighbors": envelope::neighbors(&snapshot.built, target),
                "distances": envelope::distances(&snapshot.built, target, 4),
            })
        );
        for &budget in budgets {
            let start = std::time::Instant::now();
            let artifact = snapshot.context(ContextRequest { target_entity: target, token_budget: budget, edit_zone_hops: 1, selection });
            let items: Vec<serde_json::Value> = artifact
                .items
                .iter()
                .map(|item| serde_json::json!({ "entity": item.entity, "fidelity": item.fidelity, "tokens": item.text.len() / 4 + 1 }))
                .collect();
            println!(
                "{}",
                serde_json::json!({
                    "target": target,
                    "budget": budget,
                    "items": items,
                    "fallback": artifact.fallback.map(|fallback| fallback.reason),
                    "micros": start.elapsed().as_micros() as u64,
                })
            );
        }
    }
    Ok(())
}
