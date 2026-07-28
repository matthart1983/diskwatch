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
use crate::ui::graph;
use crate::ui::palette as p;

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
        .border_style(Style::default().fg(p::faint()).bg(p::bg()))
        .title(Span::styled(
            format!(" {} ", title),
            Style::default().fg(p::dim()),
        ))
        .style(Style::default().bg(p::bg()))
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
                .fg(p::br_white())
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    let line2 = Line::from(Span::styled(
        format!("  {}", sub),
        Style::default().fg(p::dim()),
    ));
    f.render_widget(
        Paragraph::new(vec![line1, line2]).style(Style::default().bg(p::bg())),
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
        p::red()
    } else if pct >= 80 {
        p::yellow()
    } else {
        p::green()
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
        p::yellow()
    } else if rate > 1_000.0 {
        p::green()
    } else {
        p::dim()
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
                (format!("{:.1}ms", us / 1_000.0), p::red())
            } else if us >= 2_000.0 {
                (format!("{:.1}ms", us / 1_000.0), p::yellow())
            } else if us >= 1_000.0 {
                (format!("{:.1}ms", us / 1_000.0), p::green())
            } else if us > 0.0 {
                (format!("{:.0}µs", us), p::green())
            } else {
                ("—".to_string(), p::dim())
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
            render_tile(f, area, "p99 LATENCY", p::dim(), "—", "no IO observed yet");
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
        p::red()
    } else if unknown > 0 {
        p::yellow()
    } else {
        p::green()
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
        p::red()
    } else if warn > 0 {
        p::yellow()
    } else {
        p::cyan()
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
    use crate::app::VisibleColumns;
    let cols = app.visible_columns;
    let show_size = cols.contains(VisibleColumns::SIZE);
    let show_free = cols.contains(VisibleColumns::FREE);
    let show_used = cols.contains(VisibleColumns::USED_PCT);
    let show_temp = cols.contains(VisibleColumns::TEMP);
    let show_smart = cols.contains(VisibleColumns::SMART);

    // Header line — built so we only mention columns the user has on.
    // Pinned prefix (dot + DEVICE + MODEL) is always shown.
    let mut header_spans: Vec<Span> = vec![
        Span::raw("   "),
        Span::styled(pad_right("DEVICE", 11), Style::default().fg(p::dim())),
        Span::styled(pad_right("MODEL", 30), Style::default().fg(p::dim())),
    ];
    if show_size {
        header_spans.push(Span::styled(
            pad_left("SIZE", 8),
            Style::default().fg(p::dim()),
        ));
        header_spans.push(Span::raw("  "));
    }
    if show_free {
        header_spans.push(Span::styled(
            pad_left("FREE", 8),
            Style::default().fg(p::dim()),
        ));
        header_spans.push(Span::raw("  "));
    }
    if show_used {
        header_spans.push(Span::styled(
            pad_left("USED", 4),
            Style::default().fg(p::dim()),
        ));
        header_spans.push(Span::raw("  "));
    }
    if show_temp {
        header_spans.push(Span::styled(
            pad_left("TEMP", 6),
            Style::default().fg(p::dim()),
        ));
        header_spans.push(Span::raw("  "));
    }
    if show_smart {
        header_spans.push(Span::styled("SMART", Style::default().fg(p::dim())));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p::faint()).bg(p::bg()))
        .title(Span::styled(
            format!(" DEVICES  {} attached ", app.devices.len()),
            Style::default().fg(p::cyan()).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(p::bg()));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height < 2 {
        return;
    }
    f.render_widget(
        Paragraph::new(Line::from(header_spans)).style(Style::default().bg(p::bg())),
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
            p::red()
        } else if used_pct >= 80 {
            p::yellow()
        } else {
            p::fg()
        };
        let free_bytes = d.size_bytes.saturating_sub(d.used_bytes);
        let (smart_text, smart_col) = match d.smart_ok {
            Some(true) => ("ok", p::green()),
            Some(false) => ("FAIL", p::red()),
            None => ("—", p::dim()),
        };
        let dot_col = match d.smart_ok {
            Some(true) => p::green(),
            Some(false) => p::red(),
            None => p::dim(),
        };
        // Temperature from the SMART collector (cached, polled per
        // configured interval). Color-coded so a hot drive pops out
        // without the user having to switch tabs. Convert to the
        // display unit configured in app.temp_unit.
        let (temp_text, temp_col) = match app.smart.by_device.get(&d.name) {
            Some(tick) => match tick.temperature_c {
                Some(t) if t >= 70 => (app.temp_unit.format_temp(t), p::red()),
                Some(t) if t >= 55 => (app.temp_unit.format_temp(t), p::yellow()),
                Some(t) => (app.temp_unit.format_temp(t), p::fg()),
                None => ("—".to_string(), p::dim()),
            },
            None => ("—".to_string(), p::dim()),
        };
        let mut line_spans: Vec<Span> = vec![
            Span::raw(" "),
            Span::styled("\u{25cf}", Style::default().fg(dot_col)),
            Span::raw(" "),
            Span::styled(pad_right(&d.name, 11), Style::default().fg(p::fg())),
            Span::styled(pad_right(&d.model, 30), Style::default().fg(p::fg())),
        ];
        if show_size {
            line_spans.push(Span::styled(
                pad_left(&fmt_size(d.size_bytes), 8),
                Style::default().fg(p::dim()),
            ));
            line_spans.push(Span::raw("  "));
        }
        if show_free {
            line_spans.push(Span::styled(
                pad_left(&fmt_size(free_bytes), 8),
                Style::default().fg(p::cyan()),
            ));
            line_spans.push(Span::raw("  "));
        }
        if show_used {
            line_spans.push(Span::styled(
                pad_left(&format!("{}%", used_pct), 4),
                Style::default().fg(used_col),
            ));
            line_spans.push(Span::raw("  "));
        }
        if show_temp {
            line_spans.push(Span::styled(
                pad_left(&temp_text, 6),
                Style::default().fg(temp_col),
            ));
            line_spans.push(Span::raw("  "));
        }
        if show_smart {
            line_spans.push(Span::styled(
                smart_text.to_string(),
                Style::default().fg(smart_col),
            ));
        }
        f.render_widget(
            Paragraph::new(Line::from(line_spans)).style(Style::default().bg(p::bg())),
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
        .border_style(Style::default().fg(p::faint()).bg(p::bg()))
        .title(Span::styled(
            " AGG IO  60s ",
            Style::default().fg(p::cyan()).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(p::bg()));
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
            Style::default().fg(p::cyan()).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled("all devices", Style::default().fg(p::dim())),
    ]);
    f.render_widget(
        Paragraph::new(summary).style(Style::default().bg(p::bg())),
        Rect {
            x: inner.x + 1,
            y: inner.y,
            width: inner.width.saturating_sub(2),
            height: 1,
        },
    );

    // Aggregate chart across all devices. One cell per sample, with a
    // baseline filling any leading cells that don't yet have data
    // (rather than upsampling or padding zeros). Routed through the
    // graph module so it follows the app-wide bars/dots setting.
    let buckets = aggregate_history(app);
    graph::render(
        f.buffer_mut(),
        Rect {
            x: inner.x + 1,
            y: inner.y + 1,
            width: inner.width.saturating_sub(2),
            height: inner.height.saturating_sub(1),
        },
        &buckets,
        p::cyan(),
        graph::opts(),
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
        .border_style(Style::default().fg(p::faint()).bg(p::bg()))
        .title(Span::styled(
            " INSIGHTS ",
            Style::default()
                .fg(p::yellow())
                .add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(p::bg()));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let visible = (inner.height as usize).min(app.insights.len()).min(3);
    for i in 0..visible {
        let ins = &app.insights[i];
        let (badge_fg, badge_bg) = match ins.sev {
            Severity::Crit => (p::red(), p::err_bg()),
            Severity::Warn => (p::yellow(), p::warn_bg()),
            Severity::Info => (p::cyan(), p::ok_bg()),
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
            Span::styled(ins.title.clone(), Style::default().fg(p::fg())),
        ]);
        f.render_widget(
            Paragraph::new(line).style(Style::default().bg(p::bg())),
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
                Style::default().fg(p::dim()),
            )))
            .style(Style::default().bg(p::bg())),
            inner,
        );
    }
}

fn draw_hot_files_note(f: &mut Frame, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p::faint()).bg(p::bg()))
        .title(Span::styled(
            " HOT FILES ",
            Style::default().fg(p::dim()).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(p::bg()));
    let inner = block.inner(area);
    f.render_widget(block, area);
    f.render_widget(
        Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled(
                "  per-process write rate deferred",
                Style::default().fg(p::dim()),
            )),
            Line::from(Span::styled(
                "  see [7] for what's needed",
                Style::default().fg(p::dim()),
            )),
        ])
        .style(Style::default().bg(p::bg())),
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
        .border_style(Style::default().fg(p::faint()).bg(p::bg()))
        .title(Span::styled(
            format!(
                " CAPACITY  {} used / {} ",
                fmt_size(used_sum),
                fmt_size(total)
            ),
            Style::default().fg(p::cyan()).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(p::bg()));
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
            p::magenta()
        } else if matches!(d.kind, crate::collect::DeviceKind::Nvme) {
            p::cyan()
        } else {
            p::green()
        };
        let block: String = "\u{2588}".repeat(seg_w);
        spans.push(Span::styled(block, Style::default().fg(color).bg(p::bg())));
        consumed_cells += seg_w;
    }
    if consumed_cells < bar_w {
        let free_w = bar_w - consumed_cells;
        let block: String = "\u{2591}".repeat(free_w);
        spans.push(Span::styled(
            block,
            Style::default().fg(p::faint()).bg(p::bg()),
        ));
    }
    f.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(p::bg())),
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
            p::magenta()
        } else if matches!(d.kind, crate::collect::DeviceKind::Nvme) {
            p::cyan()
        } else {
            p::green()
        };
        legend.push(Span::raw("  "));
        legend.push(Span::styled("\u{25fc} ", Style::default().fg(color)));
        legend.push(Span::styled(
            format!("{} used {}", d.name, fmt_size(d.used_bytes)),
            Style::default().fg(p::dim()),
        ));
    }
    legend.push(Span::raw("  "));
    legend.push(Span::styled("\u{25fc} ", Style::default().fg(p::faint())));
    legend.push(Span::styled(
        format!("free {}", fmt_size(free)),
        Style::default().fg(p::dim()),
    ));
    if inner.height >= 2 {
        f.render_widget(
            Paragraph::new(Line::from(legend)).style(Style::default().bg(p::bg())),
            Rect {
                x: inner.x,
                y: inner.y + 1,
                width: inner.width,
                height: 1,
            },
        );
    }
}
