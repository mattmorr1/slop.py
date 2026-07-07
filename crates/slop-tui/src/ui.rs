//! Rendering: turn `App` state into a ratatui frame. Pure drawing — all the
//! decision logic lives in `app`. Layout: a header band, a findings list beside
//! a detail pane, a status line, and a footer of key hints. The command palette
//! renders as a centered overlay when open.

use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;
use slop_analyze::findings::Severity;

use crate::app::{App, Cmd, Mode};

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
    let [header, body, status, footer] = Layout::vertical([
        Constraint::Length(6),
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    let [list_area, detail_area] =
        Layout::horizontal([Constraint::Percentage(46), Constraint::Percentage(54)]).areas(body);

    draw_header(frame, header, app);
    if app.mode == Mode::Explorer {
        draw_explorer(frame, body, app);
    } else {
        draw_list(frame, list_area, app);
        draw_detail(frame, detail_area, app);
    }
    draw_status(frame, status, app);
    draw_footer(frame, footer, app);

    if app.mode == Mode::Palette {
        draw_palette(frame, body, app);
    }
    if app.mode == Mode::Verify {
        draw_verify(frame, body, app);
    }
}

fn draw_verify(frame: &mut Frame, area: Rect, app: &App) {
    let Some(panel) = &app.verify else {
        return;
    };
    let height = (panel.lines.len() as u16) + 4;
    let [v] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(area);
    let [popup] = Layout::horizontal([Constraint::Percentage(60)])
        .flex(Flex::Center)
        .areas(v);

    let (verdict, color) = if panel.passed {
        ("GATE PASS", Color::Green)
    } else {
        ("GATE FAIL", Color::Red)
    };
    let mut lines = vec![
        Line::from(Span::styled(verdict, Style::new().bold().fg(color))),
        Line::from(""),
    ];
    for l in &panel.lines {
        lines.push(Line::from(l.clone()));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "any key to dismiss",
        Style::new().add_modifier(Modifier::DIM),
    )));
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(Text::from(lines)).block(Block::bordered().title(" verification ")),
        popup,
    );
}

fn draw_explorer(frame: &mut Frame, area: Rect, app: &App) {
    let [focus_area, list_area] =
        Layout::vertical([Constraint::Length(4), Constraint::Min(0)]).areas(area);

    let Some(sg) = &app.explorer else {
        frame.render_widget(
            Paragraph::new("loading subgraph…").block(Block::bordered().title(" explore ")),
            area,
        );
        return;
    };

    let effects = if sg.effects.is_empty() {
        "none".to_string()
    } else {
        sg.effects
            .iter()
            .map(|e| format!("{e:?}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let focus = Paragraph::new(Text::from(vec![
        Line::from(vec![
            Span::styled(sg.target.clone(), Style::new().bold().fg(Color::Cyan)),
            Span::styled(format!("  [{}]", sg.entity_type), Style::new().add_modifier(Modifier::DIM)),
        ]),
        Line::from(vec![
            Span::styled("effects: ", Style::new().add_modifier(Modifier::DIM)),
            Span::styled(effects, Style::new().fg(Color::Magenta)),
        ]),
    ]))
    .block(Block::bordered().title(" explore "));
    frame.render_widget(focus, focus_area);

    let block = Block::bordered().title(format!(" edges ({}) ", sg.neighbors.len()));
    if sg.neighbors.is_empty() {
        frame.render_widget(
            Paragraph::new("no edges at depth 1")
                .style(Style::new().add_modifier(Modifier::DIM))
                .block(block),
            list_area,
        );
        return;
    }
    let items: Vec<ListItem> = sg
        .neighbors
        .iter()
        .map(|n| {
            // out = this entity depends on the neighbor; in = neighbor depends on it.
            let (arrow, verb) = if n.direction == "out" {
                ("→", n.edge.clone())
            } else {
                ("←", format!("{} by", n.edge))
            };
            ListItem::new(Line::from(vec![
                Span::raw(format!("{arrow} ")),
                Span::styled(format!("{verb:<12}"), Style::new().add_modifier(Modifier::DIM)),
                Span::raw(n.id.clone()),
            ]))
        })
        .collect();
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▸ ");
    let mut state = ListState::default();
    state.select(Some(app.explorer_selected));
    frame.render_stateful_widget(list, list_area, &mut state);
}

fn draw_header(frame: &mut Frame, area: Rect, app: &App) {
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
        lines.push(Line::from(Span::styled(
            "no slop.toml — infra-bypass checks silent (press c → init policy)",
            Style::new().fg(Color::DarkGray),
        )));
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines)).block(Block::bordered()),
        area,
    );
}

fn draw_list(frame: &mut Frame, area: Rect, app: &App) {
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

fn draw_detail(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::bordered().title(" detail ");
    let Some(f) = app.selected_finding() else {
        frame.render_widget(Paragraph::new("nothing selected").block(block), area);
        return;
    };
    let sev = f.finding.severity;
    let mut lines = vec![
        Line::from(vec![
            Span::styled(severity_tag(sev), Style::new().fg(severity_color(sev)).bold()),
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
    // Fixability hint.
    if crate::app::is_mechanical(f.finding.rule) {
        lines.push(Line::from(Span::styled(
            "auto-fixable — press x to apply the mechanical repair",
            Style::new().fg(Color::Green),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            "manual fix — press Enter to open in your editor",
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

fn draw_status(frame: &mut Frame, area: Rect, app: &App) {
    if let Some(msg) = &app.status {
        frame.render_widget(
            Line::from(format!(" {msg}")).style(Style::new().fg(Color::Cyan)),
            area,
        );
    }
}

fn draw_footer(frame: &mut Frame, area: Rect, app: &App) {
    let hint = match app.mode {
        Mode::Palette => "↑/↓ move · enter run · esc cancel",
        Mode::Explorer => "↑/↓ move · enter jump · backspace back · esc close",
        Mode::Verify => "any key to dismiss",
        Mode::Normal => {
            " enter open · e explore · x fix · v verify · c commands · f filter · g grand · r reload · q quit"
        }
    };
    frame.render_widget(
        Line::from(hint).style(Style::new().add_modifier(Modifier::DIM)),
        area,
    );
}

fn draw_palette(frame: &mut Frame, area: Rect, app: &App) {
    // Centered overlay sized to the command list.
    let height = (Cmd::ALL.len() as u16) + 2;
    let [v] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(area);
    let [popup] = Layout::horizontal([Constraint::Percentage(70)])
        .flex(Flex::Center)
        .areas(v);

    let items: Vec<ListItem> = Cmd::ALL
        .iter()
        .map(|c| ListItem::new(Line::from(c.label())))
        .collect();
    let list = List::new(items)
        .block(Block::bordered().title(" run a command "))
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▸ ");
    let mut state = ListState::default();
    state.select(Some(app.palette_selected));
    frame.render_widget(Clear, popup); // clear what's underneath
    frame.render_stateful_widget(list, popup, &mut state);
}
