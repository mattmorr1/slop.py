//! Interactive dashboard for slop findings — what bare `slop` launches in a
//! terminal. Full-screen ratatui: browse the whole-repo audit with arrow keys,
//! filter by severity, toggle grandfathered findings, reload in place.
//!
//! Split so the state machine (`app`) is headless-testable; only this event
//! loop and `ui` need a real terminal (the untested-by-nature surface, kept
//! thin — every decision lives in `app`).

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyEventKind};
use slop_analyze::check;

pub mod app;
pub mod ui;

pub use app::{Action, App};

/// Run the dashboard for `repo` until the user quits. Computes the initial
/// audit, then drives the draw/input loop. Restores the terminal even on error.
pub fn run(repo: PathBuf, index: Option<PathBuf>) -> Result<()> {
    let result = check::audit(&repo, index.as_deref())?;
    let name = repo
        .canonicalize()
        .ok()
        .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_else(|| repo.display().to_string());
    let mut app = App::new(name, result);

    let mut terminal = ratatui::init();
    let outcome = event_loop(&mut terminal, &mut app, &repo, index.as_deref());
    ratatui::restore();
    outcome
}

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    repo: &Path,
    index: Option<&Path>,
) -> Result<()> {
    loop {
        terminal.draw(|frame| ui::draw(frame, app))?;

        // Poll so the UI stays responsive; only key *presses* act (ignore key
        // releases/repeats on terminals that report them).
        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match app.on_key(key.code) {
                Action::Quit => break,
                Action::Reload => {
                    // A failed reindex/analysis shouldn't kill the session —
                    // keep showing the last good audit.
                    if let Ok(result) = check::audit(repo, index) {
                        app.set_result(result);
                    }
                }
                Action::None => {}
            }
        }
    }
    Ok(())
}
