//! Interactive dashboard for slop findings — what bare `slop` launches in a
//! terminal. Full-screen ratatui: browse the whole-repo audit, run any slop
//! command from a palette (reloading afterward = the verification loop), apply
//! mechanical fixes, and press Enter to open a finding in your editor.
//!
//! Split so the state machine (`app`) is headless-testable; only this event
//! loop and `ui` need a real terminal.

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

pub use app::{Action, App, Cmd};

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
                    let args = cmd_args(Cmd::FixApply, repo, index);
                    suspend_run(terminal, &exe, &args)?;
                    reload(app, repo, index);
                    app.status = Some(
                        "applied mechanical fixes (over-commenting + naming) and reloaded"
                            .to_string(),
                    );
                }
                Action::RunCommand(cmd) => {
                    let args = cmd_args(cmd, repo, index);
                    suspend_run(terminal, &exe, &args)?;
                    reload(app, repo, index);
                    app.status = Some(format!("ran `slop {}` and reloaded", args.join(" ")));
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

/// Leave the alt-screen, run `slop <args>` with inherited stdio (so the user
/// sees real output — `index`/`gate` are verbose and may prompt), wait for a
/// keypress, then re-enter the dashboard.
fn suspend_run(terminal: &mut ratatui::DefaultTerminal, exe: &Path, args: &[String]) -> Result<()> {
    ratatui::restore();
    println!("\n$ slop {}\n", args.join(" "));
    match Command::new(exe).args(args).status() {
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
    }
}
