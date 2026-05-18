//! Overview tab — port of `dwRenderOverview`.
//!
//! Compositional: reads everything App already collected (devices,
//! filesystems, IO history, insights) and presents it as 5 KPI tiles +
//! device summary + aggregate IO sparkline + insights strip + a
//! segmented capacity bar.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::app::App;
use crate::insights::Severity;
use crate::ui::format::{fmt_rate, fmt_size, pad_left, pad_right};
use crate::ui::palette as p;
use crate::ui::sparkline::BaselineSparkline;

pub fn draw(f: &mut Frame, area: Rect, app: &App) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5), // KPI tiles
            Constraint::Min(8),    // devices + IO chart
            Constraint::Length(8), // insights strip + hot files note
            Constraint::Length(4), // capacity bar
        ])
        .split(area);

    draw_tiles(f, rows[0], app);
    draw_middle(f, rows[1], app);
    draw_bottom_strip(f, rows[2], app);
    draw_capacity_bar(f, rows[3], app);
}

// ---------- KPI tiles ----------

fn draw_tiles(f: &mut Frame, area: Rect, app: &App) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Ratio(1, 5),
            Constraint::Ratio(1, 5),
            Constraint::Ratio(1, 5),
            Constraint::Ratio(1, 5),
            Constraint::Ratio(1, 5),
        ])
        .split(area);
    draw_capacity_tile(f, cols[0], app);
    draw_io_tile(f, cols[1], app);
    draw_latency_tile(f, cols[2], app);
    draw_health_tile(f, cols[3], app);
    draw_insights_tile(f, cols[4], app);
}

fn tile_block(title: &'static str) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p::FAINT).bg(p::BG))
        .title(Span::styled(
            format!(" {} ", title),
            Style::default().fg(p::DIM),
        ))
        .style(Style::default().bg(p::BG))
}

fn render_tile(
    f: &mut Frame,
    area: Rect,
    title: &'static str,
    dot_color: ratatui::style::Color,
    value: &str,
    sub: &str,
) {
    let block = tile_block(title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height < 3 {
        return;
    }
    let line1 = Line::from(vec![
        Span::styled(" \u{25cf}  ", Style::default().fg(dot_color)),
        Span::styled(
            value.to_string(),
            Style::default()
                .fg(p::BR_WHITE)
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    let line2 = Line::from(Span::styled(
        format!("  {}", sub),
        Style::default().fg(p::DIM),
    ));
    f.render_widget(
        Paragraph::new(vec![line1, line2]).style(Style::default().bg(p::BG)),
        inner,
    );
}

fn draw_capacity_tile(f: &mut Frame, area: Rect, app: &App) {
    let total: u64 = app.devices.iter().map(|d| d.size_bytes).sum();
    let used: u64 = app.devices.iter().map(|d| d.used_bytes).sum();
    let pct = if total > 0 {
        (used as f64 / total as f64 * 100.0).round() as u32
    } else {
        0
    };
    let color = if pct >= 90 {
        p::RED
    } else if pct >= 80 {
        p::YELLOW
    } else {
        p::GREEN
    };
    render_tile(
        f,
        area,
        "CAPACITY",
        color,
        &format!("{}%", pct),
        &format!("used {} / {}", fmt_size(used), fmt_size(total)),
    );
}

fn draw_io_tile(f: &mut Frame, area: Rect, app: &App) {
    let (rate, _) = crate::collect::io::aggregate(&app.io.latest);
    let active = app.io.latest.iter().filter(|t| t.bps > 1_000.0).count();
    let color = if rate > 50_000_000.0 {
        p::YELLOW
    } else if rate > 1_000.0 {
        p::GREEN
    } else {
        p::DIM
    };
    render_tile(
        f,
        area,
        "IO",
        color,
        fmt_rate(rate).trim(),
        &format!("{} of {} active", active, app.io.latest.len()),
    );
}

fn draw_latency_tile(f: &mut Frame, area: Rect, app: &App) {
    match crate::collect::io::worst_p99_us(&app.io.latest) {
        Some(us) => {
            let (value, color) = if us >= 10_000.0 {
                (format!("{:.1}ms", us / 1_000.0), p::RED)
            } else if us >= 2_000.0 {
                (format!("{:.1}ms", us / 1_000.0), p::YELLOW)
            } else if us >= 1_000.0 {
                (format!("{:.1}ms", us / 1_000.0), p::GREEN)
            } else if us > 0.0 {
                (format!("{:.0}µs", us), p::GREEN)
            } else {
                ("—".to_string(), p::DIM)
            };
            render_tile(
                f,
                area,
                "p99 LATENCY",
                color,
                &value,
                "max across devices  60s window",
            );
        }
        None => {
            render_tile(f, area, "p99 LATENCY", p::DIM, "—", "no IO observed yet");
        }
    }
}

fn draw_health_tile(f: &mut Frame, area: Rect, app: &App) {
    let total = app.devices.len();
    let healthy = app
        .devices
        .iter()
        .filter(|d| matches!(d.smart_ok, Some(true)))
        .count();
    let failing = app
        .devices
        .iter()
        .filter(|d| matches!(d.smart_ok, Some(false)))
        .count();
    let unknown = total.saturating_sub(healthy).saturating_sub(failing);
    let color = if failing > 0 {
        p::RED
    } else if unknown > 0 {
        p::YELLOW
    } else {
        p::GREEN
    };
    render_tile(
        f,
        area,
        "HEALTH",
        color,
        &format!("{}/{}", healthy, total),
        &format!("{} failing  {} unknown", failing, unknown),
    );
}

fn draw_insights_tile(f: &mut Frame, area: Rect, app: &App) {
    let crit = app
        .insights
        .iter()
        .filter(|i| i.sev == Severity::Crit)
        .count();
    let warn = app
        .insights
        .iter()
        .filter(|i| i.sev == Severity::Warn)
        .count();
    let total = app.insights.len();
    let color = if crit > 0 {
        p::RED
    } else if warn > 0 {
        p::YELLOW
    } else {
        p::CYAN
    };
    render_tile(
        f,
        area,
        "INSIGHTS",
        color,
        &total.to_string(),
        &format!("{} crit  {} warn", crit, warn),
    );
}

// ---------- middle: devices + IO sparkline ----------

fn draw_middle(f: &mut Frame, area: Rect, app: &App) {
    let split = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(area);
    draw_devices_summary(f, split[0], app);
    draw_io_sparkline(f, split[1], app);
}

fn draw_devices_summary(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p::FAINT).bg(p::BG))
        .title(Span::styled(
            format!(" DEVICES  {} attached ", app.devices.len()),
            Style::default().fg(p::CYAN).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(p::BG));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height < 2 {
        return;
    }
    let header = "   DEVICE     MODEL                         SIZE     USED   SMART";
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
    let visible = ((inner.height as usize).saturating_sub(1)).min(app.devices.len());
    for i in 0..visible {
        let d = &app.devices[i];
        let used_pct = if d.size_bytes > 0 {
            (d.used_bytes as f64 / d.size_bytes as f64 * 100.0).round() as u32
        } else {
            0
        };
        let used_col = if used_pct >= 90 {
            p::RED
        } else if used_pct >= 80 {
            p::YELLOW
        } else {
            p::FG
        };
        let (smart_text, smart_col) = match d.smart_ok {
            Some(true) => ("ok", p::GREEN),
            Some(false) => ("FAIL", p::RED),
            None => ("—", p::DIM),
        };
        let dot_col = match d.smart_ok {
            Some(true) => p::GREEN,
            Some(false) => p::RED,
            None => p::DIM,
        };
        let line = Line::from(vec![
            Span::raw(" "),
            Span::styled("\u{25cf}", Style::default().fg(dot_col)),
            Span::raw(" "),
            Span::styled(pad_right(&d.name, 11), Style::default().fg(p::FG)),
            Span::styled(pad_right(&d.model, 30), Style::default().fg(p::FG)),
            Span::styled(
                pad_left(&fmt_size(d.size_bytes), 8),
                Style::default().fg(p::DIM),
            ),
            Span::raw("  "),
            Span::styled(
                pad_left(&format!("{}%", used_pct), 4),
                Style::default().fg(used_col),
            ),
            Span::raw("  "),
            Span::styled(smart_text.to_string(), Style::default().fg(smart_col)),
        ]);
        f.render_widget(
            Paragraph::new(line).style(Style::default().bg(p::BG)),
            Rect {
                x: inner.x + 1,
                y: inner.y + 1 + i as u16,
                width: inner.width.saturating_sub(2),
                height: 1,
            },
        );
    }
}

fn draw_io_sparkline(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p::FAINT).bg(p::BG))
        .title(Span::styled(
            " AGG IO  60s ",
            Style::default().fg(p::CYAN).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(p::BG));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height < 3 {
        return;
    }
    let (agg, _) = crate::collect::io::aggregate(&app.io.latest);
    let summary = Line::from(vec![
        Span::raw(" "),
        Span::styled(
            fmt_rate(agg),
            Style::default().fg(p::CYAN).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled("all devices", Style::default().fg(p::DIM)),
    ]);
    f.render_widget(
        Paragraph::new(summary).style(Style::default().bg(p::BG)),
        Rect {
            x: inner.x + 1,
            y: inner.y,
            width: inner.width.saturating_sub(2),
            height: 1,
        },
    );

    // Aggregate sparkline across all devices. One cell per sample,
    // with a `▁` baseline filling any leading cells that don't yet
    // have data (rather than upsampling or padding zeros).
    let buckets = aggregate_history(app);
    f.render_widget(
        BaselineSparkline::new(&buckets).style(Style::default().fg(p::CYAN).bg(p::BG)),
        Rect {
            x: inner.x + 1,
            y: inner.y + 1,
            width: inner.width.saturating_sub(2),
            height: inner.height.saturating_sub(1),
        },
    );
}

fn aggregate_history(app: &App) -> Vec<f64> {
    // Sum every device's per-tick rates index-wise into a single
    // aggregate series. Length matches the underlying ring; the
    // baseline sparkline widget handles the case where the area is
    // wider than the data.
    let mut buckets: Vec<f64> = Vec::new();
    for h in app.io.history.values() {
        for (i, v) in h.combined.iter().enumerate() {
            if i >= buckets.len() {
                buckets.push(0.0);
            }
            buckets[i] += v;
        }
    }
    buckets
}

// ---------- bottom strip: insights + hot files note ----------

fn draw_bottom_strip(f: &mut Frame, area: Rect, app: &App) {
    let split = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);
    draw_insights_summary(f, split[0], app);
    draw_hot_files_note(f, split[1]);
}

fn draw_insights_summary(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p::FAINT).bg(p::BG))
        .title(Span::styled(
            " INSIGHTS ",
            Style::default().fg(p::YELLOW).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(p::BG));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let visible = (inner.height as usize).min(app.insights.len()).min(3);
    for i in 0..visible {
        let ins = &app.insights[i];
        let (badge_fg, badge_bg) = match ins.sev {
            Severity::Crit => (p::RED, p::ERR_BG),
            Severity::Warn => (p::YELLOW, p::WARN_BG),
            Severity::Info => (p::CYAN, p::OK_BG),
        };
        let line = Line::from(vec![
            Span::raw(" "),
            Span::styled(
                format!(" {} ", ins.sev.label()),
                Style::default()
                    .fg(badge_fg)
                    .bg(badge_bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(ins.title.clone(), Style::default().fg(p::FG)),
        ]);
        f.render_widget(
            Paragraph::new(line).style(Style::default().bg(p::BG)),
            Rect {
                x: inner.x,
                y: inner.y + i as u16,
                width: inner.width,
                height: 1,
            },
        );
    }
    if visible == 0 {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  no insights yet",
                Style::default().fg(p::DIM),
            )))
            .style(Style::default().bg(p::BG)),
            inner,
        );
    }
}

fn draw_hot_files_note(f: &mut Frame, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p::FAINT).bg(p::BG))
        .title(Span::styled(
            " HOT FILES ",
            Style::default().fg(p::DIM).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(p::BG));
    let inner = block.inner(area);
    f.render_widget(block, area);
    f.render_widget(
        Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled(
                "  per-process write rate deferred",
                Style::default().fg(p::DIM),
            )),
            Line::from(Span::styled(
                "  see [7] for what's needed",
                Style::default().fg(p::DIM),
            )),
        ])
        .style(Style::default().bg(p::BG)),
        inner,
    );
}

// ---------- bottom capacity bar ----------

fn draw_capacity_bar(f: &mut Frame, area: Rect, app: &App) {
    let total: u64 = app.devices.iter().map(|d| d.size_bytes).sum();
    let used_sum: u64 = app.devices.iter().map(|d| d.used_bytes).sum();
    let free = total.saturating_sub(used_sum);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p::FAINT).bg(p::BG))
        .title(Span::styled(
            format!(
                " CAPACITY  {} used / {} ",
                fmt_size(used_sum),
                fmt_size(total)
            ),
            Style::default().fg(p::CYAN).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(p::BG));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height < 2 || inner.width < 10 || total == 0 {
        return;
    }
    // Each device contributes a colored segment proportional to its
    // **used** bytes; the remaining slice is faint "free". This matches
    // the JSX design — the bar shows where capacity is being consumed,
    // not how big each disk is.
    let bar_w = inner.width as usize;
    let mut spans: Vec<Span> = Vec::with_capacity(app.devices.len() + 1);
    let mut consumed_cells = 0usize;
    for d in &app.devices {
        if d.used_bytes == 0 || consumed_cells >= bar_w {
            continue;
        }
        let seg_w = ((d.used_bytes as f64 / total as f64) * bar_w as f64).round() as usize;
        let seg_w = seg_w.max(1).min(bar_w - consumed_cells);
        let color = if d.is_removable {
            p::MAGENTA
        } else if matches!(d.kind, crate::collect::DeviceKind::Nvme) {
            p::CYAN
        } else {
            p::GREEN
        };
        let block: String = "\u{2588}".repeat(seg_w);
        spans.push(Span::styled(block, Style::default().fg(color).bg(p::BG)));
        consumed_cells += seg_w;
    }
    if consumed_cells < bar_w {
        let free_w = bar_w - consumed_cells;
        let block: String = "\u{2591}".repeat(free_w);
        spans.push(Span::styled(block, Style::default().fg(p::FAINT).bg(p::BG)));
    }
    f.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(p::BG)),
        Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: 1,
        },
    );

    // Legend: each device's used bytes + a "free" entry.
    let mut legend: Vec<Span> = Vec::new();
    for d in &app.devices {
        let color = if d.is_removable {
            p::MAGENTA
        } else if matches!(d.kind, crate::collect::DeviceKind::Nvme) {
            p::CYAN
        } else {
            p::GREEN
        };
        legend.push(Span::raw("  "));
        legend.push(Span::styled("\u{25fc} ", Style::default().fg(color)));
        legend.push(Span::styled(
            format!("{} used {}", d.name, fmt_size(d.used_bytes)),
            Style::default().fg(p::DIM),
        ));
    }
    legend.push(Span::raw("  "));
    legend.push(Span::styled("\u{25fc} ", Style::default().fg(p::FAINT)));
    legend.push(Span::styled(
        format!("free {}", fmt_size(free)),
        Style::default().fg(p::DIM),
    ));
    if inner.height >= 2 {
        f.render_widget(
            Paragraph::new(Line::from(legend)).style(Style::default().bg(p::BG)),
            Rect {
                x: inner.x,
                y: inner.y + 1,
                width: inner.width,
                height: 1,
            },
        );
    }
}
