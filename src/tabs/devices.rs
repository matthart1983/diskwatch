//! Devices tab — port of `dwRenderDevices` from `dw-tabs.jsx`.
//! Real data: name, kind, model (when known), bus hint, total size,
//! used %. Placeholder (`—`): temp, wear, firmware, latency hist —
//! filled in once IOKit / sysfs collectors land.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::app::App;
use crate::collect::{DeviceKind, DeviceTick};
use crate::ui::format::{fmt_size, pad_left, pad_right, usage_color};
use crate::ui::palette as p;

pub fn draw(f: &mut Frame, area: Rect, app: &App) {
    let cols = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),  // filter chips + summary
            Constraint::Min(8),     // device table
            Constraint::Length(12), // detail panel + latency hist
        ])
        .split(area);

    draw_filter_row(f, cols[0], app.devices.len());
    draw_device_table(f, cols[1], app);
    draw_detail(f, cols[2], app);
}

fn draw_filter_row(f: &mut Frame, area: Rect, count: usize) {
    let left = Line::from(vec![
        Span::raw(" "),
        Span::styled("sort", Style::default().fg(p::DIM)),
        Span::raw("  "),
        Span::styled(
            "size",
            Style::default()
                .fg(p::BR_WHITE)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled("io", Style::default().fg(p::FG)),
        Span::raw("  "),
        Span::styled("temp", Style::default().fg(p::FG)),
        Span::raw("  "),
        Span::styled("wear", Style::default().fg(p::FG)),
        Span::raw("  "),
        Span::styled("model", Style::default().fg(p::FG)),
    ]);
    f.render_widget(
        Paragraph::new(left).style(Style::default().bg(p::BG)),
        Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: 1,
        },
    );

    let right_text = format!("{} attached", count);
    let right_w = right_text.chars().count() as u16 + 1;
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            right_text,
            Style::default().fg(p::DIM),
        )))
        .style(Style::default().bg(p::BG)),
        Rect {
            x: area.x + area.width.saturating_sub(right_w),
            y: area.y,
            width: right_w,
            height: 1,
        },
    );
}

fn draw_device_table(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p::FAINT).bg(p::BG))
        .title(Span::styled(
            " BLOCK DEVICES ",
            Style::default().fg(p::CYAN).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(p::BG));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.height < 3 || inner.width < 60 {
        return;
    }

    // Header
    let header =
        "   DEVICE     MODEL                            BUS                 SIZE     USED   KIND";
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

    // Rule under header
    let rule: String = "\u{2500}".repeat(inner.width as usize - 2);
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

    // Rows: two screen rows per device (primary line + serial / state).
    let rows_avail = inner.height.saturating_sub(2);
    let mut y = inner.y + 2;
    for (i, d) in app.devices.iter().enumerate() {
        if y + 1 >= inner.y + inner.height {
            break;
        }
        let selected = i == app.selected_device;
        draw_device_row(
            f,
            inner.x + 1,
            y,
            inner.width.saturating_sub(2),
            d,
            selected,
        );
        y += 2;
        let _ = rows_avail;
    }
}

fn draw_device_row(f: &mut Frame, x: u16, y: u16, w: u16, d: &DeviceTick, selected: bool) {
    let dot_color = if d.idle {
        p::DIM
    } else {
        match d.kind {
            DeviceKind::UsbMassStorage => p::MAGENTA,
            DeviceKind::Hdd => p::GREEN,
            _ => p::GREEN,
        }
    };
    let name_color = if d.idle { p::DIM } else { p::FG };
    let used_pct = if d.size_bytes > 0 {
        (d.used_bytes as f64 / d.size_bytes as f64 * 100.0).round() as u32
    } else {
        0
    };
    let used_color = usage_color(used_pct);

    let primary = Line::from(vec![
        Span::raw(" "),
        Span::styled("\u{25cf}", Style::default().fg(dot_color)),
        Span::raw(" "),
        Span::styled(pad_right(&d.name, 11), Style::default().fg(name_color)),
        Span::styled(pad_right(&d.model, 32), Style::default().fg(p::FG)),
        Span::styled(pad_right(&d.bus, 20), Style::default().fg(p::DIM)),
        Span::styled(
            pad_left(&fmt_size(d.size_bytes), 8),
            Style::default().fg(p::DIM),
        ),
        Span::raw("  "),
        Span::styled(
            pad_left(&format!("{}%", used_pct), 5),
            Style::default().fg(used_color),
        ),
        Span::raw("  "),
        Span::styled(
            d.kind.label().to_string(),
            Style::default().fg(kind_color(d.kind)),
        ),
    ]);

    let area = Rect {
        x,
        y,
        width: w,
        height: 1,
    };
    f.render_widget(
        Paragraph::new(primary).style(Style::default().bg(if selected {
            p::SEL_BG
        } else {
            p::BG
        })),
        area,
    );

    // Second row: serial + state, mirroring the JSX layout.
    let serial = d.serial.as_deref().unwrap_or("—");
    let state = if d.idle {
        ("no usable space reported".to_string(), p::DIM)
    } else {
        match d.smart_ok {
            Some(true) => ("smart verified".to_string(), p::GREEN),
            Some(false) => ("SMART FAILING".to_string(), p::RED),
            None => (
                if d.is_removable {
                    "removable".to_string()
                } else {
                    "—".to_string()
                },
                p::DIM,
            ),
        }
    };
    let sub_line = Line::from(vec![
        Span::raw("       "),
        Span::styled(pad_right(serial, 22), Style::default().fg(p::DIM)),
        Span::styled(state.0, Style::default().fg(state.1)),
    ]);
    let sub_area = Rect {
        x,
        y: y + 1,
        width: w,
        height: 1,
    };
    f.render_widget(
        Paragraph::new(sub_line).style(Style::default().bg(if selected {
            p::SEL_BG
        } else {
            p::BG
        })),
        sub_area,
    );
}

fn draw_detail(f: &mut Frame, area: Rect, app: &App) {
    let split = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(40), Constraint::Length(50)])
        .split(area);

    let sel = app.devices.get(app.selected_device);

    // Left: per-device detail
    let left_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p::FAINT).bg(p::BG))
        .title(Span::styled(
            match sel {
                Some(d) => format!(" {}  DETAIL ", d.name),
                None => " DETAIL ".to_string(),
            },
            Style::default().fg(p::CYAN).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(p::BG));
    let left_inner = left_block.inner(split[0]);
    f.render_widget(left_block, split[0]);

    if let Some(d) = sel {
        let used_pct = if d.size_bytes > 0 {
            (d.used_bytes as f64 / d.size_bytes as f64 * 100.0).round() as u32
        } else {
            0
        };
        let used_color = usage_color(used_pct);
        let (smart_text, smart_color) = match d.smart_ok {
            Some(true) => ("verified".to_string(), p::GREEN),
            Some(false) => ("FAILING".to_string(), p::RED),
            None => ("—".to_string(), p::DIM),
        };
        let mut rows = vec![
            kv("device", &d.name, p::FG),
            kv("kind", d.kind.label(), kind_color(d.kind)),
            kv("model", &d.model, p::FG),
            kv("bus", &d.bus, p::FG),
            kv(
                "serial",
                d.serial.as_deref().unwrap_or("—"),
                if d.serial.is_some() { p::FG } else { p::DIM },
            ),
            kv(
                "firmware",
                d.firmware.as_deref().unwrap_or("—"),
                if d.firmware.is_some() { p::FG } else { p::DIM },
            ),
            kv("capacity", &fmt_size(d.size_bytes), p::FG),
            kv(
                "used",
                &format!("{} ({}%)", fmt_size(d.used_bytes), used_pct),
                used_color,
            ),
            kv(
                "removable",
                if d.is_removable { "yes" } else { "no" },
                p::FG,
            ),
            kv("SMART", &smart_text, smart_color),
        ];
        rows.push(Line::from(""));
        rows.push(Line::from(Span::styled(
            " temp / wear / power-on hours pending smartctl",
            Style::default().fg(p::DIM),
        )));
        f.render_widget(
            Paragraph::new(rows).style(Style::default().bg(p::BG)),
            left_inner,
        );
    }

    // Right: latency histogram placeholder
    let right_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p::FAINT).bg(p::BG))
        .title(Span::styled(
            " LATENCY hist  read ",
            Style::default().fg(p::CYAN).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(p::BG));
    let right_inner = right_block.inner(split[1]);
    f.render_widget(right_block, split[1]);
    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            "  awaiting per-device IO sampler",
            Style::default().fg(p::DIM),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "  Linux: eBPF biolatency (CAP_BPF)",
            Style::default().fg(p::DIM),
        )),
        Line::from(Span::styled(
            "  macOS: IOKit IOStorageDeviceCharacteristics",
            Style::default().fg(p::DIM),
        )),
    ];
    f.render_widget(
        Paragraph::new(lines).style(Style::default().bg(p::BG)),
        right_inner,
    );
}

fn kv(key: &str, val: &str, val_color: ratatui::style::Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!(" {:<14}", key), Style::default().fg(p::DIM)),
        Span::styled(val.to_string(), Style::default().fg(val_color)),
    ])
}

fn kind_color(k: DeviceKind) -> ratatui::style::Color {
    match k {
        DeviceKind::Nvme => p::CYAN,
        DeviceKind::Ssd => p::GREEN,
        DeviceKind::Hdd => p::GREEN,
        DeviceKind::UsbMassStorage => p::MAGENTA,
        DeviceKind::Unknown => p::DIM,
    }
}
