//! Interactive dashboard for slop findings — what bare `slop` launches in a
//! terminal. Full-screen ratatui: browse the whole-repo audit, run any slop
//! command from a palette (reloading afterward = the verification loop), apply
//! mechanical fixes, and press Enter to open a finding in your editor.
//!
//! Split so the state machine (`app`) is headless-testable; only this event
//! loop and `ui` need a real terminal.

use std::ffi::OsStr;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyEventKind};
use slop_analyze::build::BuiltGraph;
use slop_analyze::{check, query};

pub mod app;
pub mod ui;

pub use app::{Action, App, Cmd, VerifyPanel};

/// Run the dashboard for `repo` until the user quits. Computes the initial
/// audit, then drives the draw/input loop. Restores the terminal even on error.
pub fn run(repo: PathBuf, index: Option<PathBuf>) -> Result<()> {
    // Keep the graph alive for the whole session — the explorer queries it on
    // every focus change.
    let analysis = check::load_analysis(&repo, index.as_deref())?;
    let result = check::audit(&repo, index.as_deref())?;
    let name = repo
        .canonicalize()
        .ok()
        .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_else(|| repo.display().to_string());
    let mut app = App::new(name, result);

    let mut terminal = ratatui::init();
    let outcome = event_loop(&mut terminal, &mut app, &analysis.built, &repo, index.as_deref());
    ratatui::restore();
    outcome
}

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    built: &BuiltGraph,
    repo: &Path,
    index: Option<&Path>,
) -> Result<()> {
    // The slop binary to re-invoke for palette commands — this same process.
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("slop"));

    loop {
        terminal.draw(|frame| ui::draw(frame, app))?;

        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match app.on_key(key.code) {
                Action::Quit => break,
                Action::None => {}
                Action::Reload => {
                    reload(app, repo, index);
                    app.status = Some("reloaded".to_string());
                }
                Action::OpenSelected => open_selected(app, repo),
                Action::FixSelected => {
                    let before = snapshot(app);
                    let args = cmd_args(Cmd::FixApply, repo, index);
                    suspend_run(terminal, exe.as_os_str(), &args, repo)?;
                    reload(app, repo, index);
                    app.status = Some(format!(
                        "fixed (over-commenting + naming) — {}",
                        delta(before, snapshot(app))
                    ));
                }
                Action::RunCommand(Cmd::Test) => {
                    // The project's own test suite — run the real runner, not slop.
                    match test_command(repo) {
                        Some((prog, args)) => {
                            suspend_run(terminal, OsStr::new(&prog), &args, repo)?;
                            reload(app, repo, index);
                            app.status = Some(format!("ran `{prog} {}`", args.join(" ")));
                        }
                        None => {
                            app.status = Some(
                                "no test runner detected (looked for pytest / npm)".to_string(),
                            );
                        }
                    }
                }
                Action::RunCommand(cmd) => {
                    let before = snapshot(app);
                    let args = cmd_args(cmd, repo, index);
                    suspend_run(terminal, exe.as_os_str(), &args, repo)?;
                    reload(app, repo, index);
                    app.status =
                        Some(format!("ran `slop {}` — {}", args.join(" "), delta(before, snapshot(app))));
                }
                Action::Verify => {
                    app.set_verify(run_gate(&exe, repo, index));
                    reload(app, repo, index);
                }
                Action::Explore(id) => match query::subgraph(built, &id, 1, None) {
                    Some(sg) => app.set_explorer_subgraph(sg),
                    None => {
                        app.status = Some(format!("no graph node for {id}"));
                        // Initial open failed — fall back to browsing.
                        if app.explorer.is_none() {
                            app.mode = app::Mode::Normal;
                        }
                    }
                },
            }
        }
    }
    Ok(())
}

/// Re-run the audit in place; a failure keeps the last good state (so a broken
/// reindex mid-session doesn't blank the dashboard).
fn reload(app: &mut App, repo: &Path, index: Option<&Path>) {
    if let Ok(result) = check::audit(repo, index) {
        app.set_result(result);
    }
}

/// Open the selected finding's file at its line in the user's editor. Tries
/// `$SLOP_EDITOR`, then `cursor`, then `code` — all of which accept
/// `-g <file>:<line>` and return immediately (GUI editors). Terminal editors
/// aren't supported from here (use Enter for the path, edit however you like).
fn open_selected(app: &mut App, repo: &Path) {
    let Some(f) = app.selected_finding() else {
        return;
    };
    let target = format!(
        "{}:{}",
        repo.join(&f.finding.file).display(),
        f.finding.lines.0 + 1
    );

    let mut candidates: Vec<String> = Vec::new();
    if let Ok(ed) = std::env::var("SLOP_EDITOR") {
        if !ed.is_empty() {
            candidates.push(ed);
        }
    }
    candidates.push("cursor".to_string());
    candidates.push("code".to_string());

    for ed in &candidates {
        let spawned = Command::new(ed)
            .arg("-g")
            .arg(&target)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        if spawned.is_ok() {
            app.status = Some(format!("opened {target} in {ed}"));
            return;
        }
    }
    app.status = Some(
        "no editor found — set SLOP_EDITOR, or put `code`/`cursor` on PATH".to_string(),
    );
}

/// Leave the alt-screen, run `program <args>` in `cwd` with inherited stdio (so
/// the user sees real output — `index`/`gate`/tests are verbose and may prompt),
/// wait for a keypress, then re-enter the dashboard.
fn suspend_run(
    terminal: &mut ratatui::DefaultTerminal,
    program: &OsStr,
    args: &[String],
    cwd: &Path,
) -> Result<()> {
    ratatui::restore();
    println!("\n$ {} {}\n", program.to_string_lossy(), args.join(" "));
    match Command::new(program).args(args).current_dir(cwd).status() {
        Ok(s) => println!(
            "\n[slop] exit {} — press Enter to return to the dashboard…",
            s.code().unwrap_or(-1)
        ),
        Err(e) => println!("\n[slop] failed to run ({e}) — press Enter to return…"),
    }
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
    *terminal = ratatui::init();
    Ok(())
}

/// (blocking count, health-all) — the pair a before/after verification delta
/// compares.
fn snapshot(app: &App) -> (usize, u32) {
    (app.counts().1, app.health_all)
}

/// Human before→after line for the status bar.
fn delta(before: (usize, u32), after: (usize, u32)) -> String {
    format!(
        "blocking {}→{} · health {}→{}",
        before.0, after.0, before.1, after.1
    )
}

/// Run `slop gate --all` and parse its JSON verdict into a panel. This is the
/// canonical CI check — the honest "does this pass right now" answer.
fn run_gate(exe: &Path, repo: &Path, index: Option<&Path>) -> VerifyPanel {
    let mut args = vec!["gate".to_string(), repo.display().to_string(), "--all".to_string()];
    push_index(&mut args, index);
    match Command::new(exe).args(&args).output() {
        Ok(out) => {
            let v: serde_json::Value =
                serde_json::from_slice(&out.stdout).unwrap_or_else(|_| serde_json::json!({}));
            let passed = v["passed"].as_bool().unwrap_or(false);
            VerifyPanel {
                passed,
                lines: vec![
                    v["health"].as_str().unwrap_or("health: ?").to_string(),
                    format!(
                        "failing {} · blocking {} · total {}",
                        v["failing"], v["blocking"], v["total"]
                    ),
                ],
            }
        }
        Err(e) => VerifyPanel {
            passed: false,
            lines: vec![format!("gate failed to run: {e}")],
        },
    }
}

/// The project's test command, by ecosystem marker. `None` if we can't tell.
fn test_command(repo: &Path) -> Option<(String, Vec<String>)> {
    if repo.join("package.json").exists() {
        return Some(("npm".to_string(), vec!["test".to_string()]));
    }
    if repo.join("pyproject.toml").exists()
        || repo.join("setup.py").exists()
        || repo.join("pytest.ini").exists()
        || repo.join("tests").is_dir()
    {
        return Some(("pytest".to_string(), Vec::new()));
    }
    None
}

/// Append `--index <path>` when the dashboard was launched with an explicit one.
fn push_index(args: &mut Vec<String>, index: Option<&Path>) {
    if let Some(i) = index {
        args.push("--index".to_string());
        args.push(i.display().to_string());
    }
}

/// The CLI arguments each palette command maps to, including the repo path and
/// (where the command accepts it) the explicit index.
fn cmd_args(cmd: Cmd, repo: &Path, index: Option<&Path>) -> Vec<String> {
    let repo_s = repo.display().to_string();
    match cmd {
        Cmd::Reindex => {
            let mut a = vec!["index".to_string(), repo_s];
            if let Some(i) = index {
                a.push("--output".to_string());
                a.push(i.display().to_string());
            }
            a
        }
        Cmd::Check => {
            let mut a = vec!["check".to_string(), repo_s, "--all".to_string()];
            push_index(&mut a, index);
            a
        }
        Cmd::FixDry => {
            let mut a = vec!["fix".to_string(), repo_s];
            push_index(&mut a, index);
            a
        }
        Cmd::FixApply => {
            let mut a = vec!["fix".to_string(), repo_s, "--write".to_string()];
            push_index(&mut a, index);
            a
        }
        Cmd::Baseline => {
            let mut a = vec!["baseline".to_string(), repo_s];
            push_index(&mut a, index);
            a
        }
        Cmd::InitPolicy => {
            let mut a = vec!["init".to_string(), repo_s, "--write".to_string()];
            push_index(&mut a, index);
            a
        }
        Cmd::Install => vec!["install".to_string(), repo_s],
        Cmd::Gate => {
            let mut a = vec!["gate".to_string(), repo_s, "--all".to_string()];
            push_index(&mut a, index);
            a
        }
        // Test runs the project's own runner, not slop — handled before this.
        Cmd::Test => Vec::new(),
    }
}
