//! Dashboard state and key-driven transitions — the testable core of the TUI.
//! No terminal or ratatui here: `on_key` mutates state and returns an `Action`
//! the run loop performs (open in IDE, run a slop command, reload, quit).
//! `visible` derives the filtered finding list the renderer draws. This is what
//! the headless tests exercise; only `ui`/`lib` need a terminal.

use crossterm::event::KeyCode;
use slop_analyze::check::{AuditFinding, AuditResult};
use slop_analyze::findings::Severity;

/// Rules `slop fix` can repair mechanically (behaviour-safe comment deletion and
/// SCIP-verified renames). Everything else is manual — the finding carries
/// guidance instead.
pub const MECHANICAL_RULES: &[&str] = &["over-commenting", "naming-convention"];

pub fn is_mechanical(rule: &str) -> bool {
    MECHANICAL_RULES.contains(&rule)
}

/// A slop command runnable from the dashboard's palette. The run loop maps each
/// to a real invocation and reloads the audit afterward (the verification loop).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmd {
    Reindex,
    Check,
    InitPolicy,
    Baseline,
    FixDry,
    FixApply,
    Install,
    Gate,
}

impl Cmd {
    /// Palette order.
    pub const ALL: [Cmd; 8] = [
        Cmd::Reindex,
        Cmd::Check,
        Cmd::FixDry,
        Cmd::FixApply,
        Cmd::Baseline,
        Cmd::InitPolicy,
        Cmd::Install,
        Cmd::Gate,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Cmd::Reindex => "re-index          regenerate the SCIP index",
            Cmd::Check => "check             judge the working-tree diff",
            Cmd::FixDry => "fix (dry-run)     preview mechanical repairs",
            Cmd::FixApply => "fix (apply)       write mechanical repairs",
            Cmd::Baseline => "baseline          grandfather current findings",
            Cmd::InitPolicy => "init policy       infer + write slop.toml",
            Cmd::Install => "install harness   wire MCP + hooks into the repo",
            Cmd::Gate => "gate              CI-style pass/fail verdict",
        }
    }
}

/// Which severities the list shows. Cycled with `f`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    All,
    Only(Severity),
}

impl Filter {
    fn matches(self, sev: Severity) -> bool {
        match self {
            Filter::All => true,
            Filter::Only(s) => s == sev,
        }
    }

    fn next(self) -> Filter {
        match self {
            Filter::All => Filter::Only(Severity::Blocking),
            Filter::Only(Severity::Blocking) => Filter::Only(Severity::Warning),
            Filter::Only(Severity::Warning) => Filter::Only(Severity::Advisory),
            Filter::Only(Severity::Advisory) => Filter::All,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Filter::All => "all",
            Filter::Only(Severity::Blocking) => "blocking",
            Filter::Only(Severity::Warning) => "warning",
            Filter::Only(Severity::Advisory) => "advisory",
        }
    }
}

/// Normal browsing vs the command palette overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Palette,
}

/// What a key press asks the run loop to do after state is updated.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Quit,
    Reload,
    /// Open the selected finding's file at its line in the IDE.
    OpenSelected,
    /// Apply the mechanical fix for the selected finding (run loop confirms it
    /// really is mechanical).
    FixSelected,
    /// Run a palette command, then reload.
    RunCommand(Cmd),
}

pub struct App {
    pub repo_name: String,
    findings: Vec<AuditFinding>,
    pub health_all: u32,
    pub health_new: u32,
    pub grandfathered: usize,
    pub policy_is_empty: bool,
    /// Index into the *visible* (filtered) list.
    pub selected: usize,
    pub show_grandfathered: bool,
    pub filter: Filter,
    pub mode: Mode,
    pub palette_selected: usize,
    /// One-line result of the last action, shown in the status bar.
    pub status: Option<String>,
}

impl App {
    pub fn new(repo_name: String, result: AuditResult) -> Self {
        let mut app = Self {
            repo_name,
            findings: Vec::new(),
            health_all: 0,
            health_new: 0,
            grandfathered: 0,
            policy_is_empty: false,
            selected: 0,
            show_grandfathered: true,
            filter: Filter::All,
            mode: Mode::Normal,
            palette_selected: 0,
            status: None,
        };
        app.set_result(result);
        app
    }

    /// Replace the findings (e.g. after a reload), keeping view settings and
    /// clamping the selection into the new list.
    pub fn set_result(&mut self, result: AuditResult) {
        self.findings = result.findings;
        self.health_all = result.health_all;
        self.health_new = result.health_new;
        self.grandfathered = result.grandfathered;
        self.policy_is_empty = result.policy_is_empty;
        self.clamp();
    }

    pub fn visible(&self) -> Vec<&AuditFinding> {
        self.findings
            .iter()
            .filter(|f| self.show_grandfathered || !f.grandfathered)
            .filter(|f| self.filter.matches(f.finding.severity))
            .collect()
    }

    pub fn selected_finding(&self) -> Option<&AuditFinding> {
        self.visible().into_iter().nth(self.selected)
    }

    /// Is the selected finding mechanically fixable?
    pub fn selected_is_mechanical(&self) -> bool {
        self.selected_finding()
            .map(|f| is_mechanical(f.finding.rule))
            .unwrap_or(false)
    }

    pub fn counts(&self) -> (usize, usize, usize, usize) {
        let mut b = 0;
        let mut w = 0;
        let mut a = 0;
        for f in &self.findings {
            match f.finding.severity {
                Severity::Blocking => b += 1,
                Severity::Warning => w += 1,
                Severity::Advisory => a += 1,
            }
        }
        (self.findings.len(), b, w, a)
    }

    pub fn on_key(&mut self, key: KeyCode) -> Action {
        match self.mode {
            Mode::Palette => self.on_key_palette(key),
            Mode::Normal => self.on_key_normal(key),
        }
    }

    fn on_key_normal(&mut self, key: KeyCode) -> Action {
        match key {
            KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
            KeyCode::Char('r') => Action::Reload,
            KeyCode::Char('c') => {
                self.mode = Mode::Palette;
                self.palette_selected = 0;
                Action::None
            }
            KeyCode::Enter => {
                if self.selected_finding().is_some() {
                    Action::OpenSelected
                } else {
                    Action::None
                }
            }
            KeyCode::Char('x') => {
                match self.selected_finding() {
                    Some(f) if is_mechanical(f.finding.rule) => Action::FixSelected,
                    Some(f) => {
                        self.status = Some(format!(
                            "{} isn't auto-fixable — press Enter to open it and follow the fix guidance",
                            f.finding.rule
                        ));
                        Action::None
                    }
                    None => Action::None,
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_by(1);
                Action::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_by(-1);
                Action::None
            }
            KeyCode::Home => {
                self.selected = 0;
                Action::None
            }
            KeyCode::End => {
                self.selected = self.visible().len().saturating_sub(1);
                Action::None
            }
            KeyCode::Char('g') => {
                self.show_grandfathered = !self.show_grandfathered;
                self.clamp();
                Action::None
            }
            KeyCode::Char('f') => {
                self.filter = self.filter.next();
                self.clamp();
                Action::None
            }
            _ => Action::None,
        }
    }

    fn on_key_palette(&mut self, key: KeyCode) -> Action {
        match key {
            KeyCode::Esc | KeyCode::Char('c') | KeyCode::Char('q') => {
                self.mode = Mode::Normal;
                Action::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.palette_selected = (self.palette_selected + 1).min(Cmd::ALL.len() - 1);
                Action::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.palette_selected = self.palette_selected.saturating_sub(1);
                Action::None
            }
            KeyCode::Enter => {
                let cmd = Cmd::ALL[self.palette_selected];
                self.mode = Mode::Normal;
                Action::RunCommand(cmd)
            }
            _ => Action::None,
        }
    }

    fn move_by(&mut self, delta: isize) {
        let len = self.visible().len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        let last = len - 1;
        self.selected = match delta {
            d if d < 0 => self.selected.saturating_sub((-d) as usize),
            d => (self.selected + d as usize).min(last),
        };
    }

    fn clamp(&mut self) {
        let len = self.visible().len();
        if len == 0 {
            self.selected = 0;
        } else if self.selected >= len {
            self.selected = len - 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slop_analyze::findings::Finding;

    fn finding(rule: &'static str, sev: Severity, entity: &str) -> Finding {
        Finding {
            rule,
            severity: sev,
            entity: entity.to_string(),
            file: "f.py".to_string(),
            lines: (0, 1),
            message: "m".to_string(),
            fix_guidance: "fix".to_string(),
        }
    }

    fn app_with(items: Vec<(Finding, bool)>) -> App {
        let findings: Vec<AuditFinding> = items
            .into_iter()
            .map(|(finding, grandfathered)| AuditFinding { finding, grandfathered })
            .collect();
        let grandfathered = findings.iter().filter(|f| f.grandfathered).count();
        App::new(
            "repo".into(),
            AuditResult {
                findings,
                health_all: 50,
                health_new: 90,
                grandfathered,
                policy_is_empty: false,
            },
        )
    }

    #[test]
    fn navigation_clamps_at_both_ends() {
        let mut app = app_with(vec![
            (finding("a", Severity::Blocking, "x"), false),
            (finding("b", Severity::Warning, "y"), false),
        ]);
        app.on_key(KeyCode::Up);
        assert_eq!(app.selected, 0);
        app.on_key(KeyCode::Down);
        assert_eq!(app.selected, 1);
        app.on_key(KeyCode::Down);
        assert_eq!(app.selected, 1);
    }

    #[test]
    fn grandfathered_toggle_filters_and_reclamps() {
        let mut app = app_with(vec![
            (finding("a", Severity::Blocking, "x"), false),
            (finding("b", Severity::Warning, "y"), true),
        ]);
        assert_eq!(app.visible().len(), 2);
        app.on_key(KeyCode::End);
        assert_eq!(app.selected, 1);
        app.on_key(KeyCode::Char('g'));
        assert_eq!(app.visible().len(), 1);
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn severity_filter_cycles() {
        let mut app = app_with(vec![
            (finding("a", Severity::Blocking, "x"), false),
            (finding("b", Severity::Warning, "y"), false),
            (finding("c", Severity::Advisory, "z"), false),
        ]);
        app.on_key(KeyCode::Char('f'));
        assert_eq!(app.filter.label(), "blocking");
        assert_eq!(app.visible().len(), 1);
        assert_eq!(app.selected_finding().unwrap().finding.rule, "a");
    }

    #[test]
    fn enter_opens_and_q_quits() {
        let mut app = app_with(vec![(finding("a", Severity::Blocking, "x"), false)]);
        assert_eq!(app.on_key(KeyCode::Enter), Action::OpenSelected);
        assert_eq!(app.on_key(KeyCode::Char('q')), Action::Quit);
    }

    #[test]
    fn x_fixes_mechanical_but_only_advises_on_manual() {
        // over-commenting is mechanical -> FixSelected
        let mut mech = app_with(vec![(finding("over-commenting", Severity::Advisory, "x"), false)]);
        assert_eq!(mech.on_key(KeyCode::Char('x')), Action::FixSelected);
        // infra-bypass is manual -> no action, but a status hint is set
        let mut manual = app_with(vec![(finding("infra-bypass", Severity::Blocking, "y"), false)]);
        assert_eq!(manual.on_key(KeyCode::Char('x')), Action::None);
        assert!(manual.status.as_ref().unwrap().contains("isn't auto-fixable"));
    }

    #[test]
    fn palette_opens_navigates_and_runs() {
        let mut app = app_with(vec![(finding("a", Severity::Blocking, "x"), false)]);
        assert_eq!(app.on_key(KeyCode::Char('c')), Action::None);
        assert_eq!(app.mode, Mode::Palette);
        app.on_key(KeyCode::Down); // move to second command (Check)
        assert_eq!(app.on_key(KeyCode::Enter), Action::RunCommand(Cmd::Check));
        assert_eq!(app.mode, Mode::Normal); // palette closed
    }

    #[test]
    fn palette_esc_cancels_without_running() {
        let mut app = app_with(vec![(finding("a", Severity::Blocking, "x"), false)]);
        app.on_key(KeyCode::Char('c'));
        assert_eq!(app.on_key(KeyCode::Esc), Action::None);
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn counts_are_over_full_set_not_filtered() {
        let mut app = app_with(vec![
            (finding("a", Severity::Blocking, "x"), false),
            (finding("b", Severity::Warning, "y"), false),
        ]);
        app.on_key(KeyCode::Char('f'));
        assert_eq!(app.counts(), (2, 1, 1, 0));
    }
}
