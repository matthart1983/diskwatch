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
        Span::styled("device", Style::default().fg(p::dim())),
        Span::raw("  "),
    ];
    for (i, d) in app.devices.iter().enumerate() {
        let selected = i == app.selected_device;
        let badge = match d.smart_ok {
            Some(true) => ("ok", p::green()),
            Some(false) => ("FAIL", p::red()),
            None if app.smart.needs_root(&d.name) => ("needs root", p::dim()),
            None => ("—", p::dim()),
        };
        let label = format!("{} {}", d.name, badge.0);
        if selected {
            spans.push(Span::styled(
                label,
                Style::default()
                    .fg(p::br_white())
                    .bg(p::sel_bg())
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::styled(
                format!("{} ", d.name),
                Style::default().fg(p::fg()),
            ));
            spans.push(Span::styled(
                badge.0.to_string(),
                Style::default().fg(badge.1),
            ));
        }
        spans.push(Span::raw("  "));
    }
    f.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(p::bg())),
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
        .border_style(Style::default().fg(p::faint()).bg(p::bg()))
        .title(Span::styled(
            format!(" {}  SMART ", d.name),
            Style::default().fg(p::cyan()).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(p::bg()));
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Split: top row = root/sudo banner; rest = SMART content.
    let split = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(4)])
        .split(inner);
    draw_root_banner(f, split[0]);

    let tick = app.smart.by_device.get(&d.name);

    if !app.smart.smartctl_available() {
        draw_missing_smartctl_banner(f, split[1], d);
        return;
    }

    let Some(tick) = tick else {
        let secs = app.smart.secs_until_next_refresh();
        let countdown = if app.smart.smartctl_available() {
            format!(
                "  waiting for first SMART poll… next in ~{}s  (press r to refresh now)",
                secs
            )
        } else {
            "  waiting for smartctl probe…".to_string()
        };
        f.render_widget(
            Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(countdown, Style::default().fg(p::dim()))),
            ])
            .style(Style::default().bg(p::bg())),
            split[1],
        );
        return;
    };

    if tick.needs_root {
        draw_needs_root(f, split[1], d);
        return;
    }

    // Layout the SMART body: top half always shows the headline summary
    // (temp / hours / cycles / wear — what the Overview page's TEMP
    // column also shows). Bottom half shows the per-protocol detail:
    // NVMe gets nothing extra (the summary already lists every NVMe
    // metric); ATA gets the full attribute table.
    let body = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(10), Constraint::Min(3)])
        .split(split[1]);
    draw_summary(f, body[0], tick, d, app.temp_unit);
    if !tick.ata_attrs.is_empty() {
        draw_ata_table(f, body[1], tick);
    }
}

fn draw_missing_smartctl_banner(f: &mut Frame, area: Rect, d: &DeviceTick) {
    let smart_summary = match d.smart_ok {
        Some(true) => Line::from(vec![
            Span::styled("  SMART status: ", Style::default().fg(p::dim())),
            Span::styled(
                "verified",
                Style::default().fg(p::green()).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  (via diskutil)", Style::default().fg(p::dim())),
        ]),
        Some(false) => Line::from(vec![
            Span::styled("  SMART status: ", Style::default().fg(p::dim())),
            Span::styled(
                "FAILING",
                Style::default().fg(p::red()).add_modifier(Modifier::BOLD),
            ),
        ]),
        None => Line::from(Span::styled(
            "  SMART status: not reported by this controller",
            Style::default().fg(p::dim()),
        )),
    };

    let lines = vec![
        Line::from(""),
        smart_summary,
        Line::from(""),
        Line::from(Span::styled(
            "  Full SMART attributes (temperature, wear, power-on hours,",
            Style::default().fg(p::fg()),
        )),
        Line::from(Span::styled(
            "  per-attribute thresholds) need `smartctl` on PATH.",
            Style::default().fg(p::fg()),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "    macOS:  brew install smartmontools",
            Style::default().fg(p::cyan()),
        )),
        Line::from(Span::styled(
            "    Linux:  apt install smartmontools  (or pacman / dnf)",
            Style::default().fg(p::cyan()),
        )),
    ];
    f.render_widget(
        Paragraph::new(lines).style(Style::default().bg(p::bg())),
        area,
    );
}

/// smartctl answered, but only to say it was refused the device. A summary
/// of empty fields would look like a drive that reports nothing.
fn draw_needs_root(f: &mut Frame, area: Rect, d: &DeviceTick) {
    let lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(
                format!("  smartctl could not open /dev/{}: ", d.name),
                Style::default().fg(p::dim()),
            ),
            Span::styled(
                "permission denied",
                Style::default()
                    .fg(p::yellow())
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "  Temperature, wear, power-on hours and the attribute table need root.",
            Style::default().fg(p::fg()),
        )),
        Line::from(Span::styled(
            "  Relaunch with:",
            Style::default().fg(p::fg()),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "    sudo diskwatch",
            Style::default().fg(p::cyan()).add_modifier(Modifier::BOLD),
        )),
    ];
    f.render_widget(
        Paragraph::new(lines).style(Style::default().bg(p::bg())),
        area,
    );
}

fn draw_summary(
    f: &mut Frame,
    area: Rect,
    tick: &SmartTick,
    d: &DeviceTick,
    unit: crate::app::TempUnit,
) {
    let temp = tick
        .temperature_c
        .map(|t| unit.format_temp(t))
        .unwrap_or_else(|| "—".to_string());
    let temp_color = match tick.temperature_c {
        Some(t) if t >= 70 => p::red(),
        Some(t) if t >= 55 => p::yellow(),
        Some(_) => p::fg(),
        None => p::dim(),
    };
    let wear = tick
        .percentage_used
        .map(|n| format!("{}%", n))
        .unwrap_or_else(|| "—".to_string());
    let wear_color = match tick.percentage_used {
        Some(n) if n >= 80 => p::red(),
        Some(n) if n >= 50 => p::yellow(),
        Some(_) => p::fg(),
        None => p::dim(),
    };
    let spare = tick
        .available_spare
        .map(|n| format!("{}%", n))
        .unwrap_or_else(|| "—".to_string());
    let spare_color = match tick.available_spare {
        Some(n) if n <= 10 => p::red(),
        Some(n) if n <= 30 => p::yellow(),
        Some(_) => p::green(),
        None => p::dim(),
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
        kv("device", &d.name, p::fg()),
        kv("model", &d.model, p::fg()),
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
            p::fg(),
        ),
        kv(
            "power cycles",
            &tick
                .power_cycles
                .map(|c| c.to_string())
                .unwrap_or_else(|| "—".to_string()),
            p::fg(),
        ),
        Line::from(""),
        kv("host writes", &host_writes, p::fg()),
        kv("host reads", &host_reads, p::fg()),
    ];
    f.render_widget(
        Paragraph::new(lines).style(Style::default().bg(p::bg())),
        area,
    );
}

fn draw_ata_table(f: &mut Frame, area: Rect, tick: &SmartTick) {
    let header = "   ID  ATTRIBUTE                         VALUE   WORST   THRESH  RAW";
    let mut lines = vec![Line::from(Span::styled(
        header.to_string(),
        Style::default().fg(p::dim()),
    ))];
    let rule: String = "\u{2500}".repeat(area.width.saturating_sub(2) as usize);
    lines.push(Line::from(Span::styled(
        rule,
        Style::default().fg(p::faint()),
    )));
    for a in &tick.ata_attrs {
        let critical = matches!(a.id, 5 | 10 | 187 | 196 | 197 | 198);
        let warn = critical && a.value < a.worst;
        let row_color = if warn { p::yellow() } else { p::fg() };
        let dot_color = if warn { p::yellow() } else { p::green() };
        lines.push(Line::from(vec![
            Span::raw(" "),
            Span::styled("\u{25cf}", Style::default().fg(dot_color)),
            Span::raw(" "),
            Span::styled(
                pad_left(&format!("{:02X}", a.id), 3),
                Style::default().fg(p::dim()),
            ),
            Span::raw("  "),
            Span::styled(pad_right(&a.name, 32), Style::default().fg(row_color)),
            Span::styled(
                pad_left(&a.value.to_string(), 5),
                Style::default().fg(p::fg()),
            ),
            Span::raw("  "),
            Span::styled(
                pad_left(&a.worst.to_string(), 5),
                Style::default().fg(p::dim()),
            ),
            Span::raw("  "),
            Span::styled(
                pad_left(
                    &a.thresh
                        .map(|t| t.to_string())
                        .unwrap_or_else(|| "—".into()),
                    5,
                ),
                Style::default().fg(p::dim()),
            ),
            Span::raw("  "),
            Span::styled(a.raw.clone(), Style::default().fg(p::fg())),
        ]));
    }
    f.render_widget(
        Paragraph::new(lines).style(Style::default().bg(p::bg())),
        area,
    );
}

fn kv(key: &str, val: &str, val_color: ratatui::style::Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!(" {:<16}", key), Style::default().fg(p::dim())),
        Span::styled(val.to_string(), Style::default().fg(val_color)),
    ])
}

/// One-line banner at the top of the SMART panel. Tells the user whether
/// they have full SMART access (root via sudo) or whether they need to
/// relaunch with sudo to see temperature / wear / power-on hours.
fn draw_root_banner(f: &mut Frame, area: Rect) {
    if area.height < 1 || area.width < 10 {
        return;
    }
    let line = if running_as_root() {
        Line::from(vec![
            Span::styled(" ✓ running as root — ", Style::default().fg(p::green())),
            Span::styled("full SMART data available", Style::default().fg(p::dim())),
        ])
    } else {
        Line::from(vec![
            Span::styled(" ⚠ ", Style::default().fg(p::yellow())),
            Span::styled(
                "for SMART statistics (temperature, wear, hours), launch with: ",
                Style::default().fg(p::dim()),
            ),
            Span::styled(
                "sudo diskwatch",
                Style::default().fg(p::cyan()).add_modifier(Modifier::BOLD),
            ),
        ])
    };
    f.render_widget(
        Paragraph::new(line).style(Style::default().bg(p::bg())),
        Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: 1,
        },
    );
}

#[cfg(unix)]
fn running_as_root() -> bool {
    // SAFETY: libc::geteuid is async-signal-safe and has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

#[cfg(not(unix))]
fn running_as_root() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use crate::app::{App, ViewMode};
    use crate::collect::smart::SmartTick;
    use crate::collect::{DeviceKind, DeviceTick};
    use crate::tabs::TabId;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// Issue #25's two NVMe disks, polled by a normal user.
    fn refused_app(tab: TabId) -> App {
        let mut app = App::new_for_test(tab, ViewMode::Full);
        app.devices = ["nvme0n1", "nvme1n1"]
            .iter()
            .map(|name| DeviceTick {
                name: name.to_string(),
                kind: DeviceKind::Nvme,
                model: "Sabrent".into(),
                bus: "PCIe / NVMe".into(),
                size_bytes: 1_000_204_886_016,
                used_bytes: 0,
                is_removable: false,
                firmware: None,
                serial: None,
                smart_ok: None,
                idle: false,
            })
            .collect();
        app.selected_device = 0;
        app.smart.assume_smartctl();
        app.smart.by_device.clear();
        for d in &app.devices {
            app.smart.by_device.insert(
                d.name.clone(),
                SmartTick {
                    device: d.name.clone(),
                    needs_root: true,
                    ..Default::default()
                },
            );
        }
        app
    }

    fn render(app: &App) -> String {
        let mut term = Terminal::new(TestBackend::new(170, 60)).expect("terminal");
        term.draw(|f| crate::tabs::draw(f, f.area(), app))
            .expect("draw");
        let buf = term.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn smart_tab_says_root_is_why_it_is_empty() {
        let out = render(&refused_app(TabId::Smart));
        assert!(
            out.contains("smartctl could not open /dev/nvme0n1: permission denied"),
            "{out}"
        );
        assert!(out.contains("sudo diskwatch"), "{out}");
        assert!(out.contains("nvme1n1 needs root"), "picker badge\n{out}");
    }

    #[test]
    fn overview_and_devices_say_needs_root_instead_of_a_dash() {
        let out = render(&refused_app(TabId::Overview));
        let row = out.lines().find(|l| l.contains("nvme1n1")).unwrap();
        assert!(row.contains("needs root"), "{row:?}");

        let out = render(&refused_app(TabId::Devices));
        assert!(out.contains("SMART needs root"), "{out}");
        let detail = out.lines().find(|l| l.contains("SMART    ")).unwrap();
        assert!(detail.contains("needs root"), "{detail:?}");
    }
}
