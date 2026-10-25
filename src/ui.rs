use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState};
use unicode_width::UnicodeWidthStr;

use crate::app::{Activity, App};
use crate::lsp::ServerState;

const MAX_INPUT_ROWS: usize = 8;

pub fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let input_height = input_rows(app, area.width) as u16 + 2;
    let [transcript, input, status] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(input_height),
        Constraint::Length(1),
    ])
    .areas(area);

    render_transcript(frame, app, transcript);
    frame.render_widget(&app.input, input);
    render_status(frame, app, status);
}

/// Rows the input needs at `width`, counting soft-wrapped lines.
fn input_rows(app: &App, width: u16) -> usize {
    let inner = usize::from(width.saturating_sub(2)).max(1);
    let rows: usize = app
        .input
        .lines()
        .iter()
        .map(|line| line.width().div_ceil(inner).max(1))
        .sum();
    rows.clamp(1, MAX_INPUT_ROWS)
}

fn render_transcript(frame: &mut Frame, app: &mut App, area: Rect) {
    let height = usize::from(area.height);
    if app.transcript.is_empty() {
        app.transcript_metrics = (0, height);
        let hint = Paragraph::new(vec![
            Line::default(),
            Line::from(Span::styled(
                "  Saddle — the agent explains, you write the code.",
                Style::new().add_modifier(Modifier::DIM),
            )),
        ]);
        frame.render_widget(hint, area);
        return;
    }

    // One column is reserved for the scrollbar so layout is stable as it appears.
    let width = area.width.saturating_sub(1);
    let total = app.transcript.line_count(width);
    app.transcript_metrics = (total, height);
    let visible = app.scroll.visible(total, height);
    let top = visible.start;
    let lines = app.transcript.lines(width, visible);
    frame.render_widget(Paragraph::new(lines), Rect { width, ..area });

    if total > height {
        let mut state = ScrollbarState::new(total.saturating_sub(height)).position(top);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None),
            area,
            &mut state,
        );
    }
}

fn render_status(frame: &mut Frame, app: &App, area: Rect) {
    let base = Style::new().bg(Color::DarkGray).fg(Color::White);
    let status = &app.status;
    let mut spans = vec![
        Span::styled(" saddle ", base.add_modifier(Modifier::BOLD)),
        Span::styled(format!(" {} ", status.project), base.fg(Color::Cyan)),
        Span::styled(format!(" {} ({}) ", status.model, status.api), base),
        Span::styled(
            format!(" sandbox: {} ", status.sandbox),
            base.fg(Color::Yellow),
        ),
    ];
    spans.push(Span::styled(
        format!(
            " context: {}/{} ",
            app.prompt_tokens
                .map(|n| n.to_string())
                .unwrap_or_else(|| "—".into()),
            app.config.context_window
        ),
        base,
    ));
    if let Some(hint) = &status.web_search {
        spans.push(Span::styled(format!(" {hint} "), base.fg(Color::Yellow)));
    }
    if let Some(lsp) = crate::lsp::status_summary(&app.lsp_status) {
        let failing = app.lsp_status.iter().any(|s| {
            matches!(
                s.state,
                ServerState::Failed(_) | ServerState::Restarting { .. }
            )
        });
        let color = if failing { Color::Red } else { Color::Yellow };
        spans.push(Span::styled(format!(" lsp: {lsp} "), base.fg(color)));
    }
    if !app.running_jobs.is_empty() {
        spans.push(Span::styled(
            format!(" jobs: {} running ", app.running_jobs.len()),
            base.fg(Color::LightBlue),
        ));
    }
    match app.activity {
        Activity::Idle => {}
        Activity::Working => spans.push(Span::styled(
            " working… Esc interrupts ",
            base.fg(Color::Green).add_modifier(Modifier::BOLD),
        )),
        Activity::Thinking => spans.push(Span::styled(
            " thinking… Esc interrupts ",
            base.fg(Color::Green).add_modifier(Modifier::BOLD),
        )),
    }
    if !app.scroll.is_following() {
        spans.push(Span::styled(" [scrolled] ", base.fg(Color::Magenta)));
    }

    let hints =
        " Enter send · Shift+Enter newline · PgUp/PgDn scroll · Ctrl+O tools · Ctrl+C quit ";
    let used: usize = spans.iter().map(|s| s.content.width()).sum();
    let free = usize::from(area.width).saturating_sub(used);
    if free > hints.width() {
        spans.push(Span::styled(" ".repeat(free - hints.width()), base));
        spans.push(Span::styled(hints, base.add_modifier(Modifier::DIM)));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)).style(base), area);
}
