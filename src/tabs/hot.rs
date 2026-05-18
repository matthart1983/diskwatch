//! Hot Files tab — port of `dwRenderHot`, FSEvents-backed on macOS.
//!
//! What we have via FSEvents: file path, event kind, event count per
//! path. What the design promised but we can't supply from FSEvents
//! alone: bytes/sec (FSEvents doesn't carry byte counts) and the
//! originating pid (only the Endpoint Security framework does, which
//! is entitlement-gated). The footer banner says so honestly.

use std::time::Instant;

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::app::App;
use crate::collect::hot_files::{ActivityKind, FileActivity};
use crate::ui::format::{pad_left, pad_right};
use crate::ui::palette as p;

const VISIBLE_ROWS: usize = 15;

pub fn draw(f: &mut Frame, area: Rect, app: &App) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // summary line
            Constraint::Min(6),    // table
            Constraint::Length(4), // banner
        ])
        .split(area);

    draw_summary(f, rows[0], app);
    draw_table(f, rows[1], app);
    draw_banner(f, rows[2], app);
}

fn draw_summary(f: &mut Frame, area: Rect, app: &App) {
    let (total_events, roots, err) = app.hot_files.snapshot_meta();
    let active = {
        let s = app.hot_files.state.lock().unwrap();
        s.activity.len()
    };

    let mut spans = vec![
        Span::raw(" "),
        Span::styled("watch", Style::default().fg(p::DIM)),
        Span::raw("  "),
    ];
    if let Some(e) = &err {
        spans.push(Span::styled(
            format!("ERROR: {}", e),
            Style::default().fg(p::RED).add_modifier(Modifier::BOLD),
        ));
    } else if roots.is_empty() {
        spans.push(Span::styled("(no roots)", Style::default().fg(p::DIM)));
    } else {
        let joined: Vec<String> = roots.iter().map(|p| p.display().to_string()).collect();
        spans.push(Span::styled(joined.join("  "), Style::default().fg(p::FG)));
    }
    spans.push(Span::raw("   "));
    spans.push(Span::styled(
        format!(
            "{} active paths  {} events since start",
            active, total_events
        ),
        Style::default().fg(p::DIM),
    ));
    f.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(p::BG)),
        Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: 1,
        },
    );
}

fn draw_table(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p::FAINT).bg(p::BG))
        .title(Span::styled(
            " HOT FILES  by event rate ",
            Style::default().fg(p::CYAN).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(p::BG));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height < 3 {
        return;
    }

    let header =
        "   PATH                                                                EV/s    TOTAL  AGE   KIND";
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            header.to_string(),
            Style::default().fg(p::DIM),
        )))
        .style(Style::default().bg(p::BG)),
        Rect {
            x: inner.x + 1,
            y: inner.y,
            width: inner.width.saturating_sub(1),
            height: 1,
        },
    );
    let rule: String = "\u{2500}".repeat(inner.width.saturating_sub(2) as usize);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            rule,
            Style::default().fg(p::FAINT).bg(p::BG),
        ))),
        Rect {
            x: inner.x + 1,
            y: inner.y + 1,
            width: inner.width.saturating_sub(2),
            height: 1,
        },
    );

    let visible = ((inner.height as usize).saturating_sub(2))
        .min(VISIBLE_ROWS)
        .min(app.devices.len() + VISIBLE_ROWS);
    let top = app.hot_files.top(visible);
    if top.is_empty() {
        let s = app.hot_files.state.lock().unwrap();
        let msg = if s.error.is_some() {
            "  watcher not running — see banner below"
        } else {
            "  waiting for filesystem activity…"
        };
        drop(s);
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(msg, Style::default().fg(p::DIM))))
                .style(Style::default().bg(p::BG)),
            Rect {
                x: inner.x + 1,
                y: inner.y + 2,
                width: inner.width.saturating_sub(2),
                height: 1,
            },
        );
        return;
    }

    let now = Instant::now();
    for (i, fa) in top.iter().enumerate() {
        if i + 2 >= inner.height as usize {
            break;
        }
        draw_row(
            f,
            inner.x + 1,
            inner.y + 2 + i as u16,
            inner.width.saturating_sub(2),
            fa,
            now,
            i == 0,
        );
    }
}

fn draw_row(f: &mut Frame, x: u16, y: u16, w: u16, fa: &FileActivity, now: Instant, leader: bool) {
    let rate_str = if fa.events_per_sec >= 1.0 {
        format!("{:.1}", fa.events_per_sec)
    } else if fa.events_per_sec > 0.01 {
        format!("{:.2}", fa.events_per_sec)
    } else {
        "—".to_string()
    };
    let rate_color = if fa.events_per_sec >= 20.0 {
        p::YELLOW
    } else if fa.events_per_sec >= 5.0 {
        p::BR_CYAN
    } else if fa.events_per_sec >= 1.0 {
        p::FG
    } else {
        p::DIM
    };
    let dot = if leader { p::YELLOW } else { p::GREEN };
    let kind_color = match fa.last_kind {
        ActivityKind::Created => p::GREEN,
        ActivityKind::Modified => p::CYAN,
        ActivityKind::Removed => p::RED,
        ActivityKind::Renamed => p::MAGENTA,
        _ => p::DIM,
    };
    let path = display_path(&fa.path.display().to_string(), 68);
    let age = age_label(now.duration_since(fa.last_seen));

    let row_bg = if leader { p::SEL_BG } else { p::BG };
    f.render_widget(
        Paragraph::new("").style(Style::default().bg(row_bg)),
        Rect {
            x,
            y,
            width: w,
            height: 1,
        },
    );
    let line = Line::from(vec![
        Span::raw(" "),
        Span::styled("\u{25cf}", Style::default().fg(dot)),
        Span::raw(" "),
        Span::styled(pad_right(&path, 68), Style::default().fg(p::FG)),
        Span::raw(" "),
        Span::styled(
            pad_left(&rate_str, 6),
            Style::default().fg(rate_color).add_modifier(if leader {
                Modifier::BOLD
            } else {
                Modifier::empty()
            }),
        ),
        Span::raw("  "),
        Span::styled(
            pad_left(&fa.total_events.to_string(), 6),
            Style::default().fg(p::DIM),
        ),
        Span::raw("  "),
        Span::styled(pad_left(&age, 4), Style::default().fg(p::DIM)),
        Span::raw("  "),
        Span::styled(
            fa.last_kind.label().to_string(),
            Style::default().fg(kind_color),
        ),
    ]);
    f.render_widget(
        Paragraph::new(line).style(Style::default().bg(row_bg)),
        Rect {
            x,
            y,
            width: w,
            height: 1,
        },
    );
}

fn draw_banner(f: &mut Frame, area: Rect, _app: &App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p::FAINT).bg(p::BG))
        .style(Style::default().bg(p::BG));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let lines = vec![
        Line::from(vec![
            Span::styled(" note  ", Style::default().fg(p::YELLOW).add_modifier(Modifier::BOLD)),
            Span::styled(
                "FSEvents reports paths + kinds, not bytes or pids.",
                Style::default().fg(p::FG),
            ),
        ]),
        Line::from(Span::styled(
            "       Per-byte attribution needs fs_usage (root) or eBPF; per-process needs Endpoint Security entitlement.",
            Style::default().fg(p::DIM),
        )),
    ];
    f.render_widget(
        Paragraph::new(lines).style(Style::default().bg(p::BG)),
        inner,
    );
}

fn display_path(s: &str, w: usize) -> String {
    let len = s.chars().count();
    if len <= w {
        return s.to_string();
    }
    // Right-truncate with leading ellipsis so the filename is visible.
    let tail: String = s.chars().skip(len - (w - 1)).collect();
    format!("…{}", tail)
}

fn age_label(d: std::time::Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{}s", s)
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else {
        format!("{}h", s / 3600)
    }
}
