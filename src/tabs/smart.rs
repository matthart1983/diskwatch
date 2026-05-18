//! SMART tab — port of `dwRenderSmart`.
//!
//! Two paths:
//! - smartctl present + queried: render full SMART attribute table
//!   (NVMe headline values when the underlying log is NVMe, ATA-style
//!   table when it isn't).
//! - smartctl absent or no data yet: fall back to the basic
//!   verified/failing flag pulled from `diskutil` via DeviceTick.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::app::App;
use crate::collect::smart::SmartTick;
use crate::collect::DeviceTick;
use crate::ui::format::{fmt_size, pad_left, pad_right};
use crate::ui::palette as p;

pub fn draw(f: &mut Frame, area: Rect, app: &App) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(8)])
        .split(area);

    draw_device_picker(f, rows[0], app);
    draw_attribute_panel(f, rows[1], app);
}

fn draw_device_picker(f: &mut Frame, area: Rect, app: &App) {
    let mut spans: Vec<Span> = vec![
        Span::raw(" "),
        Span::styled("device", Style::default().fg(p::DIM)),
        Span::raw("  "),
    ];
    for (i, d) in app.devices.iter().enumerate() {
        let selected = i == app.selected_device;
        let badge = match d.smart_ok {
            Some(true) => ("ok", p::GREEN),
            Some(false) => ("FAIL", p::RED),
            None => ("—", p::DIM),
        };
        let label = format!("{} {}", d.name, badge.0);
        if selected {
            spans.push(Span::styled(
                label,
                Style::default()
                    .fg(p::BR_WHITE)
                    .bg(p::SEL_BG)
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::styled(
                format!("{} ", d.name),
                Style::default().fg(p::FG),
            ));
            spans.push(Span::styled(
                badge.0.to_string(),
                Style::default().fg(badge.1),
            ));
        }
        spans.push(Span::raw("  "));
    }
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

fn draw_attribute_panel(f: &mut Frame, area: Rect, app: &App) {
    let Some(d) = app.devices.get(app.selected_device) else {
        return;
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p::FAINT).bg(p::BG))
        .title(Span::styled(
            format!(" {}  SMART ", d.name),
            Style::default().fg(p::CYAN).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(p::BG));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let tick = app.smart.by_device.get(&d.name);

    if !app.smart.smartctl_available() {
        draw_missing_smartctl_banner(f, inner, d);
        return;
    }

    let Some(tick) = tick else {
        f.render_widget(
            Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(
                    "  waiting for first SMART poll (runs every 5 min)…",
                    Style::default().fg(p::DIM),
                )),
            ])
            .style(Style::default().bg(p::BG)),
            inner,
        );
        return;
    };

    if tick.ata_attrs.is_empty() {
        draw_nvme_summary(f, inner, tick, d);
    } else {
        draw_ata_table(f, inner, tick);
    }
}

fn draw_missing_smartctl_banner(f: &mut Frame, area: Rect, d: &DeviceTick) {
    let smart_summary = match d.smart_ok {
        Some(true) => Line::from(vec![
            Span::styled("  SMART status: ", Style::default().fg(p::DIM)),
            Span::styled(
                "verified",
                Style::default().fg(p::GREEN).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  (via diskutil)", Style::default().fg(p::DIM)),
        ]),
        Some(false) => Line::from(vec![
            Span::styled("  SMART status: ", Style::default().fg(p::DIM)),
            Span::styled(
                "FAILING",
                Style::default().fg(p::RED).add_modifier(Modifier::BOLD),
            ),
        ]),
        None => Line::from(Span::styled(
            "  SMART status: not reported by this controller",
            Style::default().fg(p::DIM),
        )),
    };

    let lines = vec![
        Line::from(""),
        smart_summary,
        Line::from(""),
        Line::from(Span::styled(
            "  Full SMART attributes (temperature, wear, power-on hours,",
            Style::default().fg(p::FG),
        )),
        Line::from(Span::styled(
            "  per-attribute thresholds) need `smartctl` on PATH.",
            Style::default().fg(p::FG),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "    macOS:  brew install smartmontools",
            Style::default().fg(p::CYAN),
        )),
        Line::from(Span::styled(
            "    Linux:  apt install smartmontools  (or pacman / dnf)",
            Style::default().fg(p::CYAN),
        )),
    ];
    f.render_widget(
        Paragraph::new(lines).style(Style::default().bg(p::BG)),
        area,
    );
}

fn draw_nvme_summary(f: &mut Frame, area: Rect, tick: &SmartTick, d: &DeviceTick) {
    let temp = tick
        .temperature_c
        .map(|t| format!("{}°C", t))
        .unwrap_or_else(|| "—".to_string());
    let temp_color = match tick.temperature_c {
        Some(t) if t >= 70 => p::RED,
        Some(t) if t >= 55 => p::YELLOW,
        Some(_) => p::FG,
        None => p::DIM,
    };
    let wear = tick
        .percentage_used
        .map(|n| format!("{}%", n))
        .unwrap_or_else(|| "—".to_string());
    let wear_color = match tick.percentage_used {
        Some(n) if n >= 80 => p::RED,
        Some(n) if n >= 50 => p::YELLOW,
        Some(_) => p::FG,
        None => p::DIM,
    };
    let spare = tick
        .available_spare
        .map(|n| format!("{}%", n))
        .unwrap_or_else(|| "—".to_string());
    let spare_color = match tick.available_spare {
        Some(n) if n <= 10 => p::RED,
        Some(n) if n <= 30 => p::YELLOW,
        Some(_) => p::GREEN,
        None => p::DIM,
    };
    let units_to_bytes = |units: u64| units.saturating_mul(512_000);
    let host_writes = tick
        .data_units_written
        .map(|u| fmt_size(units_to_bytes(u)))
        .unwrap_or_else(|| "—".to_string());
    let host_reads = tick
        .data_units_read
        .map(|u| fmt_size(units_to_bytes(u)))
        .unwrap_or_else(|| "—".to_string());

    let lines = vec![
        kv("device", &d.name, p::FG),
        kv("model", &d.model, p::FG),
        Line::from(""),
        kv("temperature", &temp, temp_color),
        kv("wear (used%)", &wear, wear_color),
        kv("avail spare", &spare, spare_color),
        Line::from(""),
        kv(
            "power-on hours",
            &tick
                .power_on_hours
                .map(|h| format!("{} ({:.1} days)", h, h as f64 / 24.0))
                .unwrap_or_else(|| "—".to_string()),
            p::FG,
        ),
        kv(
            "power cycles",
            &tick
                .power_cycles
                .map(|c| c.to_string())
                .unwrap_or_else(|| "—".to_string()),
            p::FG,
        ),
        Line::from(""),
        kv("host writes", &host_writes, p::FG),
        kv("host reads", &host_reads, p::FG),
    ];
    f.render_widget(
        Paragraph::new(lines).style(Style::default().bg(p::BG)),
        area,
    );
}

fn draw_ata_table(f: &mut Frame, area: Rect, tick: &SmartTick) {
    let header = "   ID  ATTRIBUTE                         VALUE   WORST   THRESH  RAW";
    let mut lines = vec![Line::from(Span::styled(
        header.to_string(),
        Style::default().fg(p::DIM),
    ))];
    let rule: String = "\u{2500}".repeat(area.width.saturating_sub(2) as usize);
    lines.push(Line::from(Span::styled(
        rule,
        Style::default().fg(p::FAINT),
    )));
    for a in &tick.ata_attrs {
        let critical = matches!(a.id, 5 | 10 | 187 | 196 | 197 | 198);
        let warn = critical && a.value < a.worst;
        let row_color = if warn { p::YELLOW } else { p::FG };
        let dot_color = if warn { p::YELLOW } else { p::GREEN };
        lines.push(Line::from(vec![
            Span::raw(" "),
            Span::styled("\u{25cf}", Style::default().fg(dot_color)),
            Span::raw(" "),
            Span::styled(
                pad_left(&format!("{:02X}", a.id), 3),
                Style::default().fg(p::DIM),
            ),
            Span::raw("  "),
            Span::styled(pad_right(&a.name, 32), Style::default().fg(row_color)),
            Span::styled(
                pad_left(&a.value.to_string(), 5),
                Style::default().fg(p::FG),
            ),
            Span::raw("  "),
            Span::styled(
                pad_left(&a.worst.to_string(), 5),
                Style::default().fg(p::DIM),
            ),
            Span::raw("  "),
            Span::styled(
                pad_left(
                    &a.thresh
                        .map(|t| t.to_string())
                        .unwrap_or_else(|| "—".into()),
                    5,
                ),
                Style::default().fg(p::DIM),
            ),
            Span::raw("  "),
            Span::styled(a.raw.clone(), Style::default().fg(p::FG)),
        ]));
    }
    f.render_widget(
        Paragraph::new(lines).style(Style::default().bg(p::BG)),
        area,
    );
}

fn kv(key: &str, val: &str, val_color: ratatui::style::Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!(" {:<16}", key), Style::default().fg(p::DIM)),
        Span::styled(val.to_string(), Style::default().fg(val_color)),
    ])
}
