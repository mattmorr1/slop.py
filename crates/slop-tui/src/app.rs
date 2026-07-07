//! Dashboard state and key-driven transitions — the testable core of the TUI.
//! No terminal or ratatui here: `on_key` mutates state (or asks the run loop to
//! reload/quit), and `visible` derives the filtered finding list the renderer
//! draws. This is what the headless tests exercise; only `ui`/`lib` need a
//! terminal.

use crossterm::event::KeyCode;
use slop_analyze::check::{AuditFinding, AuditResult};
use slop_analyze::findings::Severity;

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

    /// All → Blocking → Warning → Advisory → All.
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

/// What a key press asks the run loop to do after state is updated.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Quit,
    Reload,
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
    /// Include grandfathered findings in the list (toggle `g`). Default on: the
    /// dashboard is for exploring the whole repo, and grandfathered rows are
    /// marked, not hidden — the opposite of `slop check`'s silent drop.
    pub show_grandfathered: bool,
    pub filter: Filter,
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
        };
        app.set_result(result);
        app
    }

    /// Replace the findings (e.g. after a reload), keeping the view settings and
    /// clamping the selection into the new list.
    pub fn set_result(&mut self, result: AuditResult) {
        self.findings = result.findings;
        self.health_all = result.health_all;
        self.health_new = result.health_new;
        self.grandfathered = result.grandfathered;
        self.policy_is_empty = result.policy_is_empty;
        self.clamp();
    }

    /// The findings currently shown, honoring the grandfathered toggle and the
    /// severity filter. Order is `audit`'s (most-severe first).
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

    /// Total findings and per-severity counts over the *whole* set (not the
    /// filtered view) — the header summary.
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

    /// Handle a key; returns what the run loop should do next.
    pub fn on_key(&mut self, key: KeyCode) -> Action {
        match key {
            KeyCode::Char('q') | KeyCode::Esc => return Action::Quit,
            KeyCode::Char('r') => return Action::Reload,
            KeyCode::Down | KeyCode::Char('j') => self.move_by(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_by(-1),
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = self.visible().len().saturating_sub(1),
            KeyCode::Char('g') => {
                self.show_grandfathered = !self.show_grandfathered;
                self.clamp();
            }
            KeyCode::Char('f') => {
                self.filter = self.filter.next();
                self.clamp();
            }
            _ => {}
        }
        Action::None
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

    /// Keep `selected` inside the current visible list.
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
        assert_eq!(app.selected, 0);
        app.on_key(KeyCode::Up); // already at top
        assert_eq!(app.selected, 0);
        app.on_key(KeyCode::Down);
        assert_eq!(app.selected, 1);
        app.on_key(KeyCode::Down); // past the end
        assert_eq!(app.selected, 1);
    }

    #[test]
    fn grandfathered_toggle_filters_and_reclamps() {
        let mut app = app_with(vec![
            (finding("a", Severity::Blocking, "x"), false),
            (finding("b", Severity::Warning, "y"), true),
        ]);
        assert_eq!(app.visible().len(), 2); // default shows grandfathered
        app.on_key(KeyCode::End); // select last (the grandfathered one)
        assert_eq!(app.selected, 1);
        app.on_key(KeyCode::Char('g')); // hide grandfathered
        assert_eq!(app.visible().len(), 1);
        assert_eq!(app.selected, 0); // reclamped
    }

    #[test]
    fn severity_filter_cycles() {
        let mut app = app_with(vec![
            (finding("a", Severity::Blocking, "x"), false),
            (finding("b", Severity::Warning, "y"), false),
            (finding("c", Severity::Advisory, "z"), false),
        ]);
        app.on_key(KeyCode::Char('f')); // -> blocking only
        assert_eq!(app.filter.label(), "blocking");
        assert_eq!(app.visible().len(), 1);
        assert_eq!(app.selected_finding().unwrap().finding.rule, "a");
    }

    #[test]
    fn q_quits_r_reloads() {
        let mut app = app_with(vec![(finding("a", Severity::Blocking, "x"), false)]);
        assert_eq!(app.on_key(KeyCode::Char('r')), Action::Reload);
        assert_eq!(app.on_key(KeyCode::Char('q')), Action::Quit);
    }

    #[test]
    fn counts_are_over_full_set_not_filtered() {
        let mut app = app_with(vec![
            (finding("a", Severity::Blocking, "x"), false),
            (finding("b", Severity::Warning, "y"), false),
        ]);
        app.on_key(KeyCode::Char('f')); // filter to blocking only
        assert_eq!(app.counts(), (2, 1, 1, 0)); // still counts both
    }
}
