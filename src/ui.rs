//! Ratatui rendering: line list (with gutter, selection, match highlight,
//! horizontal scroll), status bar, and the search bar.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::{App, Focus, InputState};

fn base_style() -> Style {
    Style::default().fg(Color::Gray)
}

fn hl_style() -> Style {
    Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD)
}

fn gutter_style() -> Style {
    Style::default().fg(Color::DarkGray)
}

/// Build one visible row: dimmed absolute line number + content, with
/// matched chars highlighted and horizontal scroll applied by skipping
/// `hscroll` chars. Content is clipped to `width` display cells (approx:
/// measured in chars, which is exact for the ASCII-heavy text this tool
/// usually sees).
fn render_row(
    app: &App,
    row: usize,
    gutter_w: usize,
    width: usize,
    hscroll: usize,
) -> Option<Line<'static>> {
    let abs = app.abs_at(row)?;
    let text = app.line(abs)?; // ring if hot, spill file otherwise
    let selected = row == app.selected();

    let mut spans = Vec::new();
    let gutter = format!("{:>width$} ", abs + 1, width = gutter_w);
    spans.push(Span::styled(gutter, gutter_style()));

    let hl = app.highlight_for_text(&text); // sorted char indices
    let is_hl = |i: usize| hl.as_ref().is_some_and(|v| v.binary_search(&i).is_ok());

    let mut buf = String::new();
    let mut cur_hl = false;
    let mut taken = 0usize;
    for (i, ch) in text.chars().enumerate() {
        if i < hscroll {
            continue;
        }
        if taken >= width {
            break;
        }
        taken += 1;
        let ch_hl = is_hl(i);
        if taken > 1 && ch_hl != cur_hl {
            spans.push(Span::styled(
                std::mem::take(&mut buf),
                if cur_hl { hl_style() } else { base_style() },
            ));
        }
        cur_hl = ch_hl;
        buf.push(ch);
    }
    if !buf.is_empty() {
        spans.push(Span::styled(
            buf,
            if cur_hl { hl_style() } else { base_style() },
        ));
    }
    if text.chars().count() > hscroll + width {
        spans.push(Span::styled("→", gutter_style()));
    }

    let mut line = Line::from(spans);
    if selected {
        line = line.style(
            Style::default()
                .bg(Color::Rgb(40, 44, 60))
                .add_modifier(Modifier::BOLD),
        );
    }
    Some(line)
}

fn status_bar(app: &App) -> Line<'static> {
    let mut spans = vec![Span::styled(
        " logtui ",
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )];

    let mut count = format!(" {} lines", thousands(app.total_lines()));
    // With the spill, evicted lines are still reachable — nothing is dropped.
    if !app.spill_enabled() && app.buffer().dropped() > 0 {
        count.push_str(&format!(" ({} dropped)", thousands(app.buffer().dropped())));
    }
    spans.push(Span::styled(count, Style::default().fg(Color::White)));

    if let Some(e) = app.spill_error() {
        spans.push(Span::styled(
            format!("  │  ⚠ spill failed: {e}"),
            Style::default().fg(Color::Red),
        ));
    }

    if let Some(f) = app.filter() {
        let mode = f.effective_mode().label();
        let mut s = format!(
            "  │  filter \"{}\" [{}]  {} matches",
            f.query(),
            mode,
            thousands(app.view_len() as u64)
        );
        if f.is_regex_fallback() {
            s.push_str(" (invalid regex → substr)");
        }
        spans.push(Span::styled(s, Style::default().fg(Color::Yellow)));
    }

    let (state, color) = match app.input_state() {
        InputState::Live => ("  │  ● LIVE", Color::Green),
        InputState::Eof => ("  │  ■ EOF", Color::Yellow),
        InputState::Error => ("  │  ■ READ ERROR", Color::Red),
    };
    spans.push(Span::styled(state, Style::default().fg(color)));
    if let Some(msg) = app.read_error() {
        spans.push(Span::styled(
            format!(" ({msg})"),
            Style::default().fg(Color::Red),
        ));
    }

    if app.follow() {
        spans.push(Span::styled("  FOLLOW", Style::default().fg(Color::Green)));
    }

    let len = app.view_len();
    let pos = if len == 0 {
        "  │  0/0".to_string()
    } else {
        format!("  │  {}/{}", app.selected() + 1, thousands(len as u64))
    };
    spans.push(Span::styled(pos, Style::default().fg(Color::DarkGray)));
    spans.push(Span::styled(
        "  /:filter n/N:match g/G:top/bottom r:mode q:quit",
        Style::default().fg(Color::DarkGray),
    ));
    Line::from(spans)
}

fn search_bar(app: &App) -> Line<'static> {
    let mode = app.requested_mode().label();
    let mut spans = vec![
        Span::styled("/", Style::default().fg(Color::Cyan)),
        Span::raw(app.query().to_string()),
        Span::styled(format!("  [{}]", mode), Style::default().fg(Color::DarkGray)),
    ];
    if app.filter().is_some_and(|f| f.is_regex_fallback()) {
        spans.push(Span::styled(
            "  invalid regex — substring fallback",
            Style::default().fg(Color::Red),
        ));
    }
    Line::from(spans)
}

fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let show_search = app.focus() == Focus::Search || !app.query().is_empty();
    let constraints: Vec<Constraint> = if show_search {
        vec![Constraint::Min(1), Constraint::Length(1), Constraint::Length(1)]
    } else {
        vec![Constraint::Min(1), Constraint::Length(1)]
    };
    let chunks = Layout::vertical(constraints).split(f.area());
    let list_area: Rect = chunks[0];

    app.set_viewport(list_area.height as usize);
    app.ensure_visible();

    let gutter_w = {
        let total = app.total_lines();
        if total == 0 {
            4
        } else {
            format!("{total}").len().max(4)
        }
    };
    let content_w = list_area.width.saturating_sub(gutter_w as u16 + 1) as usize;
    let hscroll = app.hscroll() as usize;

    let len = app.view_len();
    let top = app.scroll();
    let bottom = (top + list_area.height as usize).min(len);

    let mut lines: Vec<Line> = Vec::with_capacity(list_area.height as usize);
    if len == 0 {
        let msg = match (app.input_state(), app.filter().is_some()) {
            (InputState::Live, false) => "waiting for input on stdin…",
            (InputState::Eof, false) => "input is empty (EOF)",
            (InputState::Error, false) => "reading stdin failed — see status bar",
            (_, true) => "no matches — edit the filter or press Esc to clear",
        };
        lines.push(Line::from(Span::styled(
            format!("  {msg}"),
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for row in top..bottom {
            if let Some(line) = render_row(app, row, gutter_w, content_w, hscroll) {
                lines.push(line);
            }
            // Rows whose lines were evicted under a live filter are skipped.
        }
    }

    f.render_widget(Paragraph::new(lines), list_area);
    f.render_widget(
        Paragraph::new(status_bar(app)).style(Style::default().bg(Color::Rgb(24, 26, 36))),
        chunks[1],
    );
    if show_search {
        f.render_widget(Paragraph::new(search_bar(app)), chunks[2]);
        if app.focus() == Focus::Search {
            // cursor sits right after the query text
            let x = chunks[2].x + 1 + app.cursor() as u16;
            f.set_cursor_position((x.min(chunks[2].right() - 1), chunks[2].y));
        }
    }
}
