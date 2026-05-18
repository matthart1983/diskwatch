//! IO tab — port of `dwRenderIo`.
//!
//! 4-up grid of per-device panels. Each shows read/write rates,
//! sparkline history, and inline summary stats. Latency / queue depth
//! are deferred; the panel shows "—" for those slots.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::app::App;
use crate::collect::{DeviceHistory, IoTick};
use crate::ui::format::fmt_rate;
use crate::ui::palette as p;
use crate::ui::sparkline::BaselineSparkline;

pub fn draw(f: &mut Frame, area: Rect, app: &App) {
    if app.io.latest.is_empty() {
        draw_empty(f, area);
        return;
    }

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(8)])
        .split(area);

    draw_summary_line(f, rows[0], app);
    draw_panel_grid(f, rows[1], app);
}

fn draw_empty(f: &mut Frame, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p::FAINT).bg(p::BG))
        .title(Span::styled(
            " IO ",
            Style::default().fg(p::CYAN).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(p::BG));
    let inner = block.inner(area);
    f.render_widget(block, area);
    f.render_widget(
        Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled(
                "  No IO data yet — sampling begins on the first tick.",
                Style::default().fg(p::DIM),
            )),
        ])
        .style(Style::default().bg(p::BG)),
        inner,
    );
}

fn draw_summary_line(f: &mut Frame, area: Rect, app: &App) {
    let (agg, write) = crate::collect::io::aggregate(&app.io.latest);
    let any_split = app.io.latest.iter().any(|t| t.split.is_some());
    let read = agg - write;
    let mut spans = vec![
        Span::raw(" "),
        Span::styled("aggregate", Style::default().fg(p::DIM)),
        Span::raw("  "),
        Span::styled(
            fmt_rate(agg),
            Style::default().fg(p::CYAN).add_modifier(Modifier::BOLD),
        ),
    ];
    if any_split {
        spans.push(Span::raw("   "));
        spans.push(Span::styled("read ", Style::default().fg(p::DIM)));
        spans.push(Span::styled(
            fmt_rate(read),
            Style::default().fg(p::GREEN).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw("   "));
        spans.push(Span::styled("write ", Style::default().fg(p::DIM)));
        spans.push(Span::styled(
            fmt_rate(write),
            Style::default().fg(p::CYAN).add_modifier(Modifier::BOLD),
        ));
    } else {
        spans.push(Span::raw("   "));
        spans.push(Span::styled(
            "(read+write combined; split pending IOKit Statistics)",
            Style::default().fg(p::DIM),
        ));
    }
    spans.push(Span::raw("   "));
    spans.push(Span::styled(
        format!(
            "{} device{}",
            app.io.latest.len(),
            if app.io.latest.len() == 1 { "" } else { "s" }
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

fn draw_panel_grid(f: &mut Frame, area: Rect, app: &App) {
    let panels = panel_areas(area, app.io.latest.len());
    for (panel_area, tick) in panels.into_iter().zip(app.io.latest.iter()) {
        let history = app.io.history.get(&tick.device);
        draw_panel(f, panel_area, tick, history);
    }
}

fn panel_areas(area: Rect, n: usize) -> Vec<Rect> {
    // Up to a 2x2 grid; for >4 devices, render the first 4 (rare on
    // workstations).
    let n = n.min(4);
    if n == 0 {
        return Vec::new();
    }
    let cols = if n == 1 { 1 } else { 2 };
    let rows = n.div_ceil(cols);

    let row_constraints: Vec<Constraint> = (0..rows)
        .map(|_| Constraint::Ratio(1, rows as u32))
        .collect();
    let row_areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints(row_constraints)
        .split(area);

    let mut out = Vec::with_capacity(n);
    for (r, row_area) in row_areas.iter().enumerate() {
        let in_row = if r == rows - 1 && n % cols != 0 {
            n % cols
        } else {
            cols
        };
        let col_constraints: Vec<Constraint> = (0..in_row)
            .map(|_| Constraint::Ratio(1, in_row as u32))
            .collect();
        let col_areas = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(col_constraints)
            .split(*row_area);
        for c in col_areas.iter() {
            out.push(*c);
            if out.len() == n {
                return out;
            }
        }
    }
    out
}

fn latency_line(tick: &IoTick) -> Line<'static> {
    let Some(pct) = tick.latency_pct else {
        return Line::from(Span::styled(
            "lat   no samples yet",
            Style::default().fg(p::DIM),
        ));
    };
    let color = |us: f64| {
        if us >= 10_000.0 {
            p::RED
        } else if us >= 2_000.0 {
            p::YELLOW
        } else if us > 0.0 {
            p::FG
        } else {
            p::DIM
        }
    };
    let lbl = |us: f64| {
        if us <= 0.0 {
            "—".to_string()
        } else if us >= 1_000.0 {
            format!("{:.1}ms", us / 1_000.0)
        } else {
            format!("{:.0}µs", us)
        }
    };
    Line::from(vec![
        Span::styled("lat ", Style::default().fg(p::DIM)),
        Span::styled("r ", Style::default().fg(p::DIM)),
        Span::styled("p50 ", Style::default().fg(p::DIM)),
        Span::styled(lbl(pct.p50_r), Style::default().fg(color(pct.p50_r))),
        Span::raw(" "),
        Span::styled("p99 ", Style::default().fg(p::DIM)),
        Span::styled(
            lbl(pct.p99_r),
            Style::default()
                .fg(color(pct.p99_r))
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("   "),
        Span::styled("w ", Style::default().fg(p::DIM)),
        Span::styled("p50 ", Style::default().fg(p::DIM)),
        Span::styled(lbl(pct.p50_w), Style::default().fg(color(pct.p50_w))),
        Span::raw(" "),
        Span::styled("p99 ", Style::default().fg(p::DIM)),
        Span::styled(
            lbl(pct.p99_w),
            Style::default()
                .fg(color(pct.p99_w))
                .add_modifier(Modifier::BOLD),
        ),
    ])
}

fn draw_panel(f: &mut Frame, area: Rect, tick: &IoTick, history: Option<&DeviceHistory>) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p::FAINT).bg(p::BG))
        .title(Span::styled(
            format!(" {} ", tick.device),
            Style::default().fg(p::CYAN).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(p::BG));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.height < 5 || inner.width < 20 {
        return;
    }

    // Top line: per-direction rates.
    let mut rate_spans: Vec<Span> = Vec::new();
    if let Some((r, w)) = tick.split {
        rate_spans.push(Span::styled("read ", Style::default().fg(p::DIM)));
        rate_spans.push(Span::styled(
            fmt_rate(r),
            Style::default().fg(p::GREEN).add_modifier(Modifier::BOLD),
        ));
        rate_spans.push(Span::raw("   "));
        rate_spans.push(Span::styled("write ", Style::default().fg(p::DIM)));
        rate_spans.push(Span::styled(
            fmt_rate(w),
            Style::default().fg(p::CYAN).add_modifier(Modifier::BOLD),
        ));
    } else {
        rate_spans.push(Span::styled("rate ", Style::default().fg(p::DIM)));
        rate_spans.push(Span::styled(
            fmt_rate(tick.bps),
            Style::default().fg(p::CYAN).add_modifier(Modifier::BOLD),
        ));
    }
    let summary = Line::from(rate_spans);
    f.render_widget(
        Paragraph::new(summary).style(Style::default().bg(p::BG)),
        Rect {
            x: inner.x + 1,
            y: inner.y,
            width: inner.width.saturating_sub(2),
            height: 1,
        },
    );

    // Second line: latency percentiles.
    let lat_line = latency_line(tick);
    f.render_widget(
        Paragraph::new(lat_line).style(Style::default().bg(p::BG)),
        Rect {
            x: inner.x + 1,
            y: inner.y + 1,
            width: inner.width.saturating_sub(2),
            height: 1,
        },
    );

    // Single sparkline of combined throughput. Baseline-aware so the
    // panel is visually filled from the first sample.
    if let Some(h) = history {
        let panel_inner_h = inner.height.saturating_sub(3);
        let data: Vec<f64> = h.combined.iter().copied().collect();
        f.render_widget(
            BaselineSparkline::new(&data).style(Style::default().fg(p::CYAN).bg(p::BG)),
            Rect {
                x: inner.x + 1,
                y: inner.y + 2,
                width: inner.width.saturating_sub(2),
                height: panel_inner_h,
            },
        );
    }

    // Footer note explains the percentile semantics so the user knows
    // what they're looking at.
    if inner.height >= 6 {
        let lat = Line::from(vec![Span::styled(
            "  p50/p99 of per-tick averages — micro-spikes invisible without eBPF/IOReport",
            Style::default().fg(p::DIM),
        )]);
        f.render_widget(
            Paragraph::new(lat).style(Style::default().bg(p::BG)),
            Rect {
                x: inner.x + 1,
                y: inner.y + inner.height - 1,
                width: inner.width.saturating_sub(2),
                height: 1,
            },
        );
    }
}
