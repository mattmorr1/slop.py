//! Rendering: turn `App` state into a ratatui frame. Pure drawing — all the
//! decision logic lives in `app`. Layout: a header band, a findings list beside
//! a detail pane, and a footer of key hints.

use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;
use slop_analyze::findings::Severity;

use crate::app::App;

fn severity_color(sev: Severity) -> Color {
    match sev {
        Severity::Blocking => Color::Red,
        Severity::Warning => Color::Yellow,
        Severity::Advisory => Color::DarkGray,
    }
}

fn severity_tag(sev: Severity) -> &'static str {
    match sev {
        Severity::Blocking => "BLOCK",
        Severity::Warning => "WARN",
        Severity::Advisory => "ADVIS",
    }
}

pub fn draw(frame: &mut Frame, app: &App) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(6),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    let [list_area, detail_area] =
        Layout::horizontal([Constraint::Percentage(46), Constraint::Percentage(54)]).areas(body);

    draw_header(frame, header, app);
    draw_list(frame, list_area, app);
    draw_detail(frame, detail_area, app);
    frame.render_widget(
        Line::from(
            " ↑/↓ move · f filter · g grandfathered · r reload · q quit",
        )
        .style(Style::new().add_modifier(Modifier::DIM)),
        footer,
    );
}

fn draw_header(frame: &mut Frame, area: ratatui::layout::Rect, app: &App) {
    let (total, b, w, a) = app.counts();
    let mut lines = vec![
        Line::from(vec![
            Span::styled("slop", Style::new().bold().fg(Color::Cyan)),
            Span::raw(format!(" · {}", app.repo_name)),
        ]),
        Line::from(format!(
            "health  {}/100 all   {}/100 new",
            app.health_all, app.health_new
        )),
        Line::from(vec![
            Span::raw(format!("{total} findings  ")),
            Span::styled(format!("{b} blocking"), Style::new().fg(Color::Red)),
            Span::raw(" · "),
            Span::styled(format!("{w} warning"), Style::new().fg(Color::Yellow)),
            Span::raw(" · "),
            Span::styled(format!("{a} advisory"), Style::new().fg(Color::DarkGray)),
            Span::raw(format!(" · {} grandfathered", app.grandfathered)),
        ]),
        Line::from(format!(
            "filter: {}   grandfathered: {}",
            app.filter.label(),
            if app.show_grandfathered { "shown" } else { "hidden" }
        )),
    ];
    if app.policy_is_empty {
        lines.push(Line::from(
            Span::styled(
                "no slop.toml — infra-bypass checks silent (run `slop init`)",
                Style::new().fg(Color::DarkGray),
            ),
        ));
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines)).block(Block::bordered()),
        area,
    );
}

fn draw_list(frame: &mut Frame, area: ratatui::layout::Rect, app: &App) {
    let visible = app.visible();
    let block = Block::bordered().title(format!(" findings ({}) ", visible.len()));
    if visible.is_empty() {
        frame.render_widget(
            Paragraph::new("no findings match this filter (f/g to change)")
                .style(Style::new().add_modifier(Modifier::DIM))
                .block(block),
            area,
        );
        return;
    }
    let items: Vec<ListItem> = visible
        .iter()
        .map(|f| {
            let sev = f.finding.severity;
            let mut spans = vec![
                Span::styled(
                    format!("{:<6}", severity_tag(sev)),
                    Style::new().fg(severity_color(sev)).bold(),
                ),
                Span::styled(f.finding.rule, Style::new().bold()),
                Span::raw(" "),
                Span::raw(f.finding.entity.clone()),
            ];
            if f.grandfathered {
                spans.push(Span::styled(
                    " ·grandfathered",
                    Style::new().add_modifier(Modifier::DIM),
                ));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▸ ");
    let mut state = ListState::default();
    state.select(Some(app.selected));
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_detail(frame: &mut Frame, area: ratatui::layout::Rect, app: &App) {
    let block = Block::bordered().title(" detail ");
    let Some(f) = app.selected_finding() else {
        frame.render_widget(
            Paragraph::new("nothing selected").block(block),
            area,
        );
        return;
    };
    let sev = f.finding.severity;
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                severity_tag(sev),
                Style::new().fg(severity_color(sev)).bold(),
            ),
            Span::raw("  "),
            Span::styled(f.finding.rule, Style::new().bold()),
        ]),
        Line::from(f.finding.entity.clone()),
        Line::from(Span::styled(
            format!("{}:{}", f.finding.file, f.finding.lines.0 + 1),
            Style::new().add_modifier(Modifier::DIM),
        )),
    ];
    if f.grandfathered {
        lines.push(Line::from(Span::styled(
            "grandfathered by baseline (not counted in `new` health)",
            Style::new().fg(Color::DarkGray),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(f.finding.message.clone()));
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("fix: ", Style::new().bold().fg(Color::Green)),
        Span::raw(f.finding.fix_guidance.clone()),
    ]));
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(block)
            .wrap(Wrap { trim: true }),
        area,
    );
}
