use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Command as Process;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use slop_analyze::baseline::Baseline;
use slop_analyze::check::{self, CheckRequest, CheckResult};
use slop_analyze::findings::Severity;
use std::collections::{HashMap, HashSet};

use slop_analyze::compress::{self, CompressConfig};
use slop_analyze::{detect, fix, gate, harness, infer, inline, policy::Policy, rename, suppress};
use slop_resolve::{Resolver, ScipResolver};

#[derive(Parser)]
#[command(name = "slop", about = "Codebase-relative AI-slop analyzer", version)]
#[command(after_help = "Run `slop` with no command in a terminal to open the interactive dashboard.")]
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

/// Severity threshold at which `slop gate` fails (exits non-zero).
#[derive(Clone, Copy, ValueEnum)]
enum FailOn {
    /// Only deterministic blockers (infra-bypass, circular-import). Default.
    Blocking,
    /// ...also Warnings (duplicate-exact, complexity-spike, purity-lie).
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
    /// Apply slop's mechanistic auto-fixes (D9 path a). Two rules:
    /// over-commenting — delete comments that restate the adjacent code
    /// (behaviour-safe: comments are inert); and naming-convention —
    /// rename a camelCase free function to snake_case, rewriting every
    /// SCIP-resolved reference and re-parsing each file (methods and
    /// throwaway-marker names are left to a human). Dry-run unless --write.
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
        /// make the loop act on duplicate-exact / complexity-spike.
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
    /// Open the interactive findings dashboard (full-screen TUI). Browse the
    /// whole-repo audit with arrow keys, filter by severity, toggle
    /// grandfathered findings, reload in place. Bare `slop` in a terminal opens
    /// this on the current directory.
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
    /// Serve slop's MCP tools (validate_change, get_context_envelope,
    /// query_subgraph) over stdio. Launched per-project by an agent host
    /// (e.g. Claude Code); speaks newline-delimited JSON-RPC 2.0.
    Mcp {
        /// Repo root the tools default to (default: current directory)
        #[arg(default_value = ".")]
        repo: PathBuf,
        /// Path to index.scip (default: <repo>/index.scip)
        #[arg(long)]
        index: Option<PathBuf>,
    },
    /// Generate (or regenerate) the SCIP index slop reads. Wraps the per-language
    /// indexer (`scip-python`, `scip-typescript`, `rust-analyzer scip`) and
    /// verifies the result actually has definitions — an indexer can crash
    /// mid-walk and still write a near-empty index that makes every check
    /// silently pass.
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
        #[arg(long, value_enum, default_value_t = Indexer::Auto)]
        indexer: Indexer,
    },
    /// Wire slop's harness into a repo's agent-host config: merge the MCP
    /// server into `<repo>/.mcp.json` and the read/prompt hooks into
    /// `<repo>/.claude/settings.json`, pointing at this binary. Idempotent —
    /// re-running updates slop's own entries and leaves the rest untouched.
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
        // Bare `slop`: open the dashboard in a terminal, else print help (so
        // piping / CI still gets something useful instead of a raw-mode error).
        if std::io::stdout().is_terminal() && std::io::stdin().is_terminal() {
            return slop_tui::run(PathBuf::from("."), None);
        }
        use clap::CommandFactory;
        Cli::command().print_help()?;
        println!();
        return Ok(());
    };
    match command {
        Command::Dash { repo, index } => {
            let index_path = index.clone().unwrap_or_else(|| repo.join("index.scip"));
            ensure_index(&repo, &index_path)?;
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
            let index_path = index.clone().unwrap_or_else(|| repo.join("index.scip"));
            if reindex {
                run_scip_index(&repo, index.as_deref())?;
            } else if ensure_index(&repo, &index_path)? {
                // Just built it, so it cannot be stale.
            } else if let Some(msg) = index_staleness(&repo, &index_path) {
                // Advisory only — the check still runs against the old index.
                eprintln!("warning: {msg}\n  regenerate with `slop check {} --reindex`", repo.display());
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
        Command::Fix { repo, index, write, remove_dead, inline_wrappers } => {
            let analysis = check::load_analysis(&repo, index.as_deref())?;
            let policy = Policy::load(&repo).unwrap_or_default();
            let findings = detect::run_all(&analysis.built, &policy, &analysis.facts);

            // Renames first: they only substitute name tokens (no line-count
            // change), so the over-commenting pass below still sees valid line
            // numbers. The resolver drives SCIP-verified reference rewriting.
            let index_path = index
                .clone()
                .unwrap_or_else(|| repo.join("index.scip"));
            let resolver = ScipResolver::load(&index_path)?;
            let renames = rename::plan_renames(&analysis.built, &resolver, &repo, &findings);
            let mut renamed = 0usize;
            for outcome in &renames.outcomes {
                match outcome {
                    rename::RenameOutcome::Planned(p) => {
                        renamed += 1;
                        let verb = if write { "rename" } else { "would rename" };
                        println!(
                            "{verb} [{}] {} -> {} ({} occurrence(s) across {} file(s))",
                            p.rule, p.entity, p.new_name, p.occurrences, p.file_count
                        );
                    }
                    rename::RenameOutcome::Skipped { entity, reason } => {
                        eprintln!("skip rename {entity}: {reason}");
                    }
                }
            }
            if write {
                for (file, src) in &renames.files {
                    std::fs::write(repo.join(file), src)
                        .with_context(|| format!("writing {file}"))?;
                }
            }

            // Dead-code removal (opt-in). Runs after renames (which don't change
            // line counts, so the parser's spans stay valid) and marks touched
            // files so the over-commenting pass below skips them — its cached
            // line numbers would be stale once whole functions are deleted.
            let mut removed_fns = 0usize;
            let mut dead_files: HashSet<String> = HashSet::new();
            if remove_dead {
                let removals = fix::plan_dead_removals(&analysis.built, &analysis.facts, &findings);
                let mut by_file: HashMap<String, Vec<(u32, u32)>> = HashMap::new();
                for r in &removals {
                    let verb = if write { "remove" } else { "would remove" };
                    println!("{verb} [dead-island] {} ({})", r.entity, r.file);
                    by_file.entry(r.file.clone()).or_default().push(r.lines);
                }
                for (file, ranges) in by_file {
                    dead_files.insert(file.clone());
                    removed_fns += ranges.len();
                    if write {
                        let path = repo.join(&file);
                        let src = std::fs::read_to_string(&path)
                            .with_context(|| format!("reading {file}"))?;
                        let new_src = fix::delete_line_ranges(&src, &ranges);
                        std::fs::write(&path, &new_src)
                            .with_context(|| format!("writing {file}"))?;
                    }
                }
            }

            let mut by_file: HashMap<String, Vec<(usize, usize)>> = HashMap::new();
            for f in findings.iter().filter(|f| f.rule == "over-commenting") {
                if dead_files.contains(&f.file) {
                    continue; // a dead-removal shifted this file's lines; skip
                }
                by_file.entry(f.file.clone()).or_default().push(f.lines);
            }
            let mut files: Vec<_> = by_file.into_iter().collect();
            files.sort_by(|a, b| a.0.cmp(&b.0));

            let mut total = 0usize;
            let mut touched = 0usize;
            for (file, ranges) in files {
                let path = repo.join(&file);
                let Ok(src) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let (new_src, n) = fix::fix_over_commenting(&src, &ranges);
                if n == 0 {
                    continue;
                }
                total += n;
                touched += 1;
                if write {
                    std::fs::write(&path, &new_src)
                        .with_context(|| format!("writing {file}"))?;
                    println!("fixed {file}: removed {n} restating comment(s)");
                } else {
                    println!("would fix {file}: {n} restating comment(s)");
                }
            }
            // Inlining rewrites references and deletes wrapper definitions, so
            // its SCIP occurrence lines are only valid against unmodified files.
            // Under --write it must be the sole writing pass this run; re-index
            // between it and the others. Dry-run reports alongside them safely.
            let mut inlined = 0usize;
            if inline_wrappers {
                if write && (renamed > 0 || removed_fns > 0 || total > 0) {
                    bail!(
                        "--inline-wrappers --write must run alone: other fixes already rewrote files this run, so the index is stale — re-index, then run `slop fix {} --inline-wrappers --write` on its own",
                        repo.display()
                    );
                }
                let wrapper_findings = check::tier3_wrapper_findings(
                    &analysis.built,
                    &policy,
                    &analysis.facts,
                    &repo,
                )?;
                let plan = inline::plan_inlines(
                    &analysis.built,
                    &resolver,
                    &repo,
                    &analysis.facts,
                    &wrapper_findings,
                );
                for outcome in &plan.outcomes {
                    match outcome {
                        inline::InlineOutcome::Planned(p) => {
                            inlined += 1;
                            let verb = if write { "inline" } else { "would inline" };
                            println!(
                                "{verb} [trivial-wrapper] {} -> {} ({} reference(s) across {} file(s))",
                                p.entity, p.callee, p.occurrences, p.file_count
                            );
                        }
                        inline::InlineOutcome::Skipped { entity, reason } => {
                            eprintln!("skip inline {entity}: {reason}");
                        }
                    }
                }
                if write {
                    for (file, src) in &plan.files {
                        std::fs::write(repo.join(file), src)
                            .with_context(|| format!("writing {file}"))?;
                    }
                }
            }

            if total == 0 && renamed == 0 && removed_fns == 0 && inlined == 0 {
                println!("nothing to fix");
            } else if write {
                println!(
                    "applied {renamed} rename(s); removed {total} comment(s) across {touched} file(s); removed {removed_fns} dead function(s); inlined {inlined} wrapper(s)"
                );
                if renamed > 0 || removed_fns > 0 || inlined > 0 {
                    println!(
                        "note: renames/removals/inlines changed the code — re-index (e.g. `slop gate {} --reindex`) before the next check",
                        repo.display()
                    );
                }
            } else {
                let mut summary =
                    format!("dry-run: {renamed} rename(s) + {total} comment(s) across {touched} file(s)");
                if remove_dead {
                    summary.push_str(&format!(" + {removed_fns} dead function(s)"));
                }
                if inline_wrappers {
                    summary.push_str(&format!(" + {inlined} wrapper inline(s)"));
                }
                println!("{summary}");
                println!("apply with: slop fix --write");
                let mut extras = Vec::new();
                if !remove_dead {
                    extras.push("--remove-dead (delete dead free functions)");
                }
                if !inline_wrappers {
                    extras.push("--inline-wrappers (inline confirmed trivial wrappers)");
                }
                if !extras.is_empty() {
                    println!("more fixes available: {}", extras.join(", "));
                }
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

            if reindex {
                run_scip_index(&work_repo, index.as_deref())?;
            } else {
                let path = index
                    .clone()
                    .unwrap_or_else(|| work_repo.join("index.scip"));
                ensure_index(&work_repo, &path)?;
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
            let out = output.unwrap_or_else(|| repo.join("index.scip"));
            let project = project_name.unwrap_or_else(|| default_project_name(&repo));
            run_indexer(indexer, &repo, &out, &project)?;
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
            println!(
                "indexed {} -> {} ({} definitions across {} file(s))",
                repo.display(),
                out.display(),
                defs,
                resolver.files().len()
            );
        }
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
            let policy = Policy::load(&repo)?;
            let check::Analysis { built, facts } = check::load_analysis(&repo, index.as_deref())?;
            let raw = detect::run_all(&built, &policy, &facts);
            let suppressions = suppress::scan(&repo, &facts);
            let findings = suppress::filter(raw, &suppressions);
            let baseline = Baseline::from_findings(&findings).with_effects(&built);
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
        Command::Debug { command } => run_debug(command)?,
    }
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
        return;
    }

    // Group by severity, most severe first, stable within a group.
    let mut counts = [0usize; 3]; // [advisory, warning, blocking] by Severity ordinal
    for sev in [Severity::Blocking, Severity::Warning, Severity::Advisory] {
        let group: Vec<_> = result.findings.iter().filter(|f| f.severity == sev).collect();
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
        "blocking": result.blocking,
        "warning": warning,
        "advisory": advisory,
        "total": result.findings.len(),
        "health": result.health_line,
        "policy_is_empty": result.policy_is_empty,
        "findings": result.findings,
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

/// The slop Claude skill, embedded at build time so `slop install` can drop it
/// into a target repo without needing the slop source tree at runtime.
const SLOP_SKILL: &str = include_str!("../../../skills/slop/SKILL.md");

/// Read a JSON config file into a `Value`, or `Value::Null` if it's absent.
/// Refuses to proceed on an unparseable file (would clobber the user's config)
/// unless `force` is set.
fn read_json_config(path: &Path, force: bool) -> Result<serde_json::Value> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Ok(serde_json::Value::Null);
    };
    if text.trim().is_empty() {
        return Ok(serde_json::Value::Null);
    }
    match serde_json::from_str(&text) {
        Ok(v) => Ok(v),
        Err(e) if force => {
            eprintln!("warning: {} is not valid JSON ({e}); overwriting (--force)", path.display());
            Ok(serde_json::Value::Null)
        }
        Err(e) => bail!(
            "{} is not valid JSON ({e}) — fix it or pass --force to overwrite",
            path.display()
        ),
    }
}

/// Wire slop's MCP server + hooks into `<repo>/.mcp.json` and
/// `<repo>/.claude/settings.json`. Idempotent (see `install::merge_*`).
fn install_harness(repo: &Path, force: bool) -> Result<()> {
    use slop_analyze::install;

    let repo = repo
        .canonicalize()
        .with_context(|| format!("resolving repo path {}", repo.display()))?;
    let exe = std::env::current_exe()
        .context("resolving the slop binary path")?
        .to_string_lossy()
        .into_owned();
    let repo_str = repo.to_string_lossy().into_owned();

    // .mcp.json — the MCP server (validate_change / get_context_envelope /
    // query_subgraph).
    let mcp_path = repo.join(".mcp.json");
    let mcp = install::merge_mcp(read_json_config(&mcp_path, force)?, &exe, &repo_str);
    std::fs::write(&mcp_path, format!("{}\n", serde_json::to_string_pretty(&mcp)?))
        .with_context(|| format!("writing {}", mcp_path.display()))?;
    println!("wrote {} (mcpServers.slop)", mcp_path.display());

    // .claude/settings.json — the read-path + prompt hooks.
    let claude_dir = repo.join(".claude");
    std::fs::create_dir_all(&claude_dir)
        .with_context(|| format!("creating {}", claude_dir.display()))?;
    let settings_path = claude_dir.join("settings.json");
    let settings = install::merge_hooks(read_json_config(&settings_path, force)?, &exe);
    std::fs::write(
        &settings_path,
        format!("{}\n", serde_json::to_string_pretty(&settings)?),
    )
    .with_context(|| format!("writing {}", settings_path.display()))?;
    println!(
        "wrote {} (PostToolUse + UserPromptSubmit hooks)",
        settings_path.display()
    );

    // .claude/skills/slop/SKILL.md — the agent playbook (embedded at build
    // time so install is self-contained). Always refreshed so it tracks the
    // binary; it's a generated doc, not user config.
    let skill_dir = claude_dir.join("skills/slop");
    std::fs::create_dir_all(&skill_dir)
        .with_context(|| format!("creating {}", skill_dir.display()))?;
    let skill_path = skill_dir.join("SKILL.md");
    std::fs::write(&skill_path, SLOP_SKILL)
        .with_context(|| format!("writing {}", skill_path.display()))?;
    println!("wrote {} (slop skill)", skill_path.display());

    println!(
        "\nharness installed. next:\n  \
         - generate a SCIP index:  slop index {repo_str}\n  \
         - optional policy:        slop init {repo_str} --write\n  \
         - gate the fix-loop:      slop gate {repo_str} --reindex\n\
         Restart the agent host to load the new MCP server and hooks."
    );
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

/// The SCIP indexer for a language. slop's graph/effect detectors consume any
/// SCIP index; picking the right indexer per repo is all the multi-language
/// index path needs.
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Indexer {
    /// Detect from project markers (pyproject/setup/*.py vs Cargo.toml vs package.json/tsconfig).
    Auto,
    /// `scip-python` (Python).
    Python,
    /// `scip-typescript` (JavaScript / TypeScript).
    Typescript,
    /// `rust-analyzer scip` (Rust) — the one indexer that needs no npx.
    Rust,
}

impl Indexer {
    /// Resolve `Auto` against the repo's project markers.
    fn resolve(self, repo: &Path) -> Indexer {
        if self != Indexer::Auto {
            return self;
        }
        let has = |f: &str| repo.join(f).exists();
        // Python markers win when several ecosystems are present — slop's
        // parser-based rules cover Python most fully, so it's the richer target.
        // Cargo.toml is checked before the JS markers because a Rust repo often
        // carries a package.json for web assets, but not the reverse.
        if has("pyproject.toml") || has("setup.py") || has("requirements.txt") || has_top_level_ext(repo, "py") {
            Indexer::Python
        } else if has("Cargo.toml") {
            Indexer::Rust
        } else if has("tsconfig.json") || has("package.json") {
            Indexer::Typescript
        } else {
            Indexer::Python
        }
    }
}

/// Is there a top-level file with extension `ext` in `repo`? A cheap language
/// signal for repos without config-file markers.
fn has_top_level_ext(repo: &Path, ext: &str) -> bool {
    std::fs::read_dir(repo)
        .map(|entries| {
            entries.flatten().any(|e| {
                e.path().extension().and_then(|s| s.to_str()) == Some(ext)
            })
        })
        .unwrap_or(false)
}

/// Source-file extensions slop indexes — the staleness check watches these.
const SOURCE_EXTS: &[&str] = &["py", "js", "jsx", "ts", "tsx", "mjs", "cjs", "rs"];

/// Directories never worth walking for source-file mtimes.
const SKIP_DIRS: &[&str] = &[".git", "node_modules", ".venv", "venv", "target", "__pycache__"];

/// A human-readable reason the index at `index_path` is out of date, or `None`
/// if it looks current. "Stale" means a tracked source file has been modified
/// more recently than the index — the check would then judge yesterday's graph.
/// Best-effort: any I/O hiccup yields `None` (never block a check on a mtime we
/// couldn't read).
fn index_staleness(repo: &Path, index_path: &Path) -> Option<String> {
    let index_mtime = std::fs::metadata(index_path).ok()?.modified().ok()?;
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    newest_source_mtime(repo, &mut newest);
    let (src_mtime, src_path) = newest?;
    if src_mtime > index_mtime {
        let rel = src_path.strip_prefix(repo).unwrap_or(&src_path);
        Some(format!(
            "SCIP index {} is older than {} — findings may be out of date",
            index_path.display(),
            rel.display()
        ))
    } else {
        None
    }
}

/// Recursively track the newest-modified source file under `dir`, skipping
/// vendored/build directories. Best-effort — unreadable entries are ignored.
fn newest_source_mtime(dir: &Path, newest: &mut Option<(std::time::SystemTime, PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            let name = entry.file_name();
            if SKIP_DIRS.iter().any(|d| name == *d) {
                continue;
            }
            newest_source_mtime(&path, newest);
        } else if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| SOURCE_EXTS.contains(&e))
        {
            if let Ok(mtime) = entry.metadata().and_then(|m| m.modified()) {
                if newest.as_ref().map(|(t, _)| mtime > *t).unwrap_or(true) {
                    *newest = Some((mtime, path));
                }
            }
        }
    }
}

/// Regenerate the SCIP index for `repo`, writing to `index` (default
/// `<repo>/index.scip`). Used by `slop gate --reindex`; auto-detects the
/// indexer.
fn run_scip_index(repo: &Path, index: Option<&Path>) -> Result<()> {
    let out = index
        .map(Path::to_path_buf)
        .unwrap_or_else(|| repo.join("index.scip"));
    run_indexer(Indexer::Auto, repo, &out, "slop-gate")
}

/// Build the index if it isn't there yet, so no command dead-ends on a missing
/// one. Returns whether it built. Every entry point used to fail with "generate
/// one with: slop index <repo>", which made the first command a new user runs an
/// error telling them to run a different command.
///
/// Only ever builds when the index is *absent* — a stale index still warns
/// rather than silently costing a rebuild on every invocation.
fn ensure_index(repo: &Path, index_path: &Path) -> Result<bool> {
    if index_path.exists() {
        return Ok(false);
    }
    eprintln!("no {} — indexing first...", index_path.display());
    run_indexer(
        Indexer::Auto,
        repo,
        index_path,
        &default_project_name(repo),
    )?;
    Ok(true)
}

/// scip-python's `--project-name` default: the repo directory name.
fn default_project_name(repo: &Path) -> String {
    repo.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".to_string())
}

/// Run the resolved SCIP indexer for `repo`, writing to `out`.
fn run_indexer(indexer: Indexer, repo: &Path, out: &Path, project: &str) -> Result<()> {
    let indexer = indexer.resolve(repo);
    let npx = || {
        let mut cmd = Process::new("npx");
        cmd.arg("--yes");
        cmd
    };
    let (tool, install_hint, mut cmd) = match indexer {
        Indexer::Rust => {
            // rust-analyzer emits SCIP natively, so Rust needs no Node toolchain.
            let mut cmd = Process::new("rust-analyzer");
            cmd.arg("scip").arg(repo).arg("--output").arg(out);
            ("rust-analyzer scip", "install it with: rustup component add rust-analyzer", cmd)
        }
        Indexer::Typescript => {
            // scip-typescript reads the project's tsconfig from its cwd and
            // takes just an output path.
            let mut cmd = npx();
            cmd.args(["@sourcegraph/scip-typescript", "index", "--output"])
                .arg(out)
                .current_dir(repo);
            ("scip-typescript", "is npx on PATH?", cmd)
        }
        Indexer::Python | Indexer::Auto => {
            // scip-python resolves the project from its *cwd*, not from the path
            // argument, so without this it indexes wherever slop was invoked
            // from — silently analyzing the wrong codebase for any
            // `slop check <other-repo>`.
            let mut cmd = npx();
            cmd.current_dir(repo);
            cmd.args(["@sourcegraph/scip-python", "index"])
                .arg(repo)
                .args(["--project-name", project, "--output"])
                .arg(out);
            ("scip-python", "is npx on PATH?", cmd)
        }
    };
    let status = cmd
        .status()
        .with_context(|| format!("running {tool} ({install_hint})"))?;
    if !status.success() {
        bail!("{tool} indexing failed for {}", repo.display());
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
