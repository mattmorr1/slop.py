use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::process::Command as Process;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use slop_core::analyze::baseline::Baseline;
use slop_core::analyze::check::{self, CheckRequest, CheckResult};
use slop_core::analyze::findings::Severity;

use slop_core::analyze::index::{
    default_project_name, ensure_index, run_indexer, run_scip_index, Indexer,
};
use slop_core::analyze::{gate, harness, infer, retrieve};
use slop_core::resolve::{IndexSet, Resolver, ScipResolver};

mod setup;

#[derive(Parser)]
#[command(name = "slop", about = "Codebase-relative AI-slop analyzer", version)]
#[command(after_help = "Start with `slop setup`, then run `slop check`.")]
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
    /// Only deterministic blockers (infra-bypass). Default.
    Blocking,
    /// ...also Warnings (duplicate-exact, duplicate-equivalent, purity-lie).
    Warning,
    /// ...also Advisories (everything).
    Advisory,
}

impl From<FailOn> for slop_core::analyze::findings::Severity {
    fn from(f: FailOn) -> Self {
        use slop_core::analyze::findings::Severity;
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
        /// Git ref to diff against (default: HEAD)
        #[arg(long, default_value = "HEAD")]
        base: String,
        /// Severity at or above which the gate fails. Lower it to `warning` to
        /// make the loop act on duplicate-exact / duplicate-equivalent / purity-lie.
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
    /// Serve slop's MCP tools (find_capability, assess_write, validate_change,
    /// query_subgraph) over stdio. Launched by an agent host
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
    /// Research adapter for the equivalence benchmark: each JSONL line
    /// `{"a": src, "b": src}` (one Python function each) yields every tier's verdict.
    #[command(hide = true)]
    Equiv {
        pairs: PathBuf,
        /// egglog saturation rounds (requires the `egraph` feature).
        #[arg(long, default_value_t = 12)]
        iterations: usize,
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
        Command::Check {
            repo,
            index,
            all,
            policy,
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
        Command::Gate {
            repo,
            index,
            all,
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
            let started = std::time::Instant::now();
            let (name, output) = match event {
                HookEvent::PreToolUse => ("PreToolUse", harness::handle_pre_tool_use(&input)),
                HookEvent::PostToolUse => ("PostToolUse", harness::handle_post_tool_use(&input)),
                HookEvent::UserPromptSubmit => ("UserPromptSubmit", harness::handle_user_prompt_submit(&input)),
                HookEvent::SessionStart => ("SessionStart", harness::handle_session_start(&input, "SessionStart")),
                HookEvent::SubagentStart => ("SubagentStart", harness::handle_session_start(&input, "SubagentStart")),
            };
            println!("{output}");
            harness::log_hook(&input, name, &output, started.elapsed());
        }
        Command::Lsp { repo, index } => {
            slop_cli::lsp::serve_stdio(repo, index)?;
        }
        Command::Mcp { repo, index } => {
            slop_cli::mcp::serve_stdio(repo, index)?;
        }
        Command::Index {
            repo,
            output,
            project_name,
            indexer,
        } => {
            if indexer == IndexerArg::Auto && output.is_none() {
                run_scip_index(&repo, None)?;
                let paths = slop_core::analyze::index::index_paths(&repo, None);
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
        Command::Equiv { pairs, iterations } => run_equiv(&pairs, iterations)?,
        Command::Baseline { repo, index } => {
            let analysis = check::load_analysis(&repo, index.as_deref())?;
            let findings = check::baseline_findings(&analysis);
            let baseline = Baseline::from_findings(&findings).with_effects(&analysis.built);
            let count = baseline.findings.len();
            baseline.save(&repo)?;
            println!(
                "baselined {count} finding(s) into {}",
                repo.join(slop_core::analyze::baseline::BASELINE_FILE).display()
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
            .and_then(slop_core::parse::Language::from_path)
            .unwrap_or(slop_core::parse::Language::Python);
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
        if slop_core::analyze::source::is_test_file(&ff.file) {
            continue;
        }
        let Some(lang) = slop_core::parse::Language::from_path(&ff.file) else {
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
                .is_some_and(slop_core::analyze::source::is_test_entity)
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
/// help so the everyday surface (check / gate / …) stays legible.
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
    term: slop_core::parse::equiv::Term,
}

fn equiv_side(source: &str) -> Result<EquivSide> {
    let facts = slop_core::parse::Language::Python.parse(source)?;
    let first = facts.first().context("no function in source")?;
    let equiv = slop_core::parse::equiv::equivalence_facts(source)?
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
        for (key, tier) in [("egg_sound", slop_core::parse::equiv::Tier::Sound), ("egg_graded", slop_core::parse::equiv::Tier::Graded)] {
            let start = std::time::Instant::now();
            let proved = slop_core::parse::egraph::equal(&a.term, &b.term, tier, iterations)?;
            verdict[key] = serde_json::json!(proved);
            verdict[format!("{key}_us")] = serde_json::json!(start.elapsed().as_micros() as u64);
        }
        #[cfg(not(feature = "egraph"))]
        let _ = iterations;
        println!("{verdict}");
    }
    Ok(())
}
