//! Filesystems tab — port of `dwRenderFs`.
//!
//! Real: mount path, device, fs type, size, used %, inline usage bar,
//! removable flag. Placeholders (`—`): inode %, 7-day growth — these
//! need statvfs (inode) and a snapshot history (growth) we haven't
//! built yet.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::app::App;
use crate::collect::FsTick;
use crate::ui::format::{fmt_size, pad_left, pad_right, usage_bar_color, usage_color};
use crate::ui::palette as p;

pub fn draw(f: &mut Frame, area: Rect, app: &App) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),  // filter chips
            Constraint::Min(8),     // mounts table
            Constraint::Length(10), // detail
        ])
        .split(area);

    draw_filter_row(f, rows[0], &app.filesystems);
    draw_mounts_table(f, rows[1], app);
    draw_detail(f, rows[2], app);
}

fn draw_filter_row(f: &mut Frame, area: Rect, fs: &[FsTick]) {
    let mounted = fs.len();
    let system = fs.iter().filter(|m| m.is_system).count();
    let user = mounted.saturating_sub(system);
    let removable = fs.iter().filter(|m| m.is_removable).count();
    let line = Line::from(vec![
        Span::raw(" "),
        Span::styled("show", Style::default().fg(p::DIM)),
        Span::raw("  "),
        Span::styled(
            format!("mounted {}", mounted),
            Style::default()
                .fg(p::BR_WHITE)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(format!("system {}", system), Style::default().fg(p::FG)),
        Span::raw("  "),
        Span::styled(format!("user {}", user), Style::default().fg(p::FG)),
        Span::raw("  "),
        Span::styled(
            format!("removable {}", removable),
            Style::default().fg(p::FG),
        ),
    ]);
    f.render_widget(
        Paragraph::new(line).style(Style::default().bg(p::BG)),
        Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: 1,
        },
    );
}

fn draw_mounts_table(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p::FAINT).bg(p::BG))
        .title(Span::styled(
            " MOUNTS ",
            Style::default().fg(p::CYAN).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(p::BG));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.height < 3 {
        return;
    }

    let header =
        "   MOUNT                          DEVICE              FS        SIZE     USAGE                USED    INODES";
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

    let rows_avail = inner.height.saturating_sub(2);
    let visible = (rows_avail as usize).min(app.filesystems.len());
    for i in 0..visible {
        let y = inner.y + 2 + i as u16;
        let selected = i == app.selected_fs;
        draw_fs_row(
            f,
            inner.x + 1,
            y,
            inner.width.saturating_sub(2),
            &app.filesystems[i],
            selected,
        );
    }
}

fn draw_fs_row(f: &mut Frame, x: u16, y: u16, w: u16, m: &FsTick, selected: bool) {
    let used_pct = if m.size_bytes > 0 {
        (m.used_bytes as f64 / m.size_bytes as f64 * 100.0).round() as u32
    } else {
        0
    };
    let used_col = usage_color(used_pct);
    let dot_col = if m.size_bytes == 0 {
        p::DIM
    } else if used_pct >= 90 {
        p::RED
    } else if used_pct >= 80 {
        p::YELLOW
    } else {
        p::GREEN
    };
    let dev_short = m.device.trim_start_matches("/dev/");
    let row_bg = if selected { p::SEL_BG } else { p::BG };

    // First paint the whole row bg so the gaps between sub-paragraphs
    // share the selection color.
    f.render_widget(
        Paragraph::new("").style(Style::default().bg(row_bg)),
        Rect {
            x,
            y,
            width: w,
            height: 1,
        },
    );

    let prefix = Line::from(vec![
        Span::raw(" "),
        Span::styled("\u{25cf}", Style::default().fg(dot_col)),
        Span::raw(" "),
        Span::styled(
            pad_right(&m.mount, 30),
            Style::default().fg(if m.size_bytes == 0 { p::DIM } else { p::FG }),
        ),
        Span::styled(pad_right(dev_short, 18), Style::default().fg(p::DIM)),
        Span::styled(pad_right(&m.fs_type, 8), Style::default().fg(p::CYAN)),
        Span::raw(" "),
        Span::styled(
            pad_left(&fmt_size(m.size_bytes), 8),
            Style::default().fg(p::DIM),
        ),
        Span::raw(" "),
    ]);
    f.render_widget(
        Paragraph::new(prefix).style(Style::default().bg(row_bg)),
        Rect {
            x,
            y,
            width: w.min(70),
            height: 1,
        },
    );

    // Usage bar at x+70, 18 wide.
    let bar_x = x + 70;
    let bar_w = 18u16;
    let filled = ((used_pct as f64 / 100.0) * bar_w as f64).round() as u16;
    let bar_col = usage_bar_color(used_pct);
    let bar_spans: Vec<Span> = (0..bar_w)
        .map(|i| {
            if i < filled {
                Span::styled("\u{2588}", Style::default().fg(bar_col).bg(row_bg))
            } else {
                Span::styled("\u{2584}", Style::default().fg(p::FAINT).bg(row_bg))
            }
        })
        .collect();
    if bar_x + bar_w <= x + w {
        f.render_widget(
            Paragraph::new(Line::from(bar_spans)),
            Rect {
                x: bar_x,
                y,
                width: bar_w,
                height: 1,
            },
        );
    }

    let tail = Line::from(vec![
        Span::raw("  "),
        Span::styled(
            pad_left(&format!("{}%", used_pct), 4),
            Style::default().fg(used_col),
        ),
        Span::raw("  "),
        Span::styled(
            pad_left(
                &m.inode_pct
                    .map(|n| format!("{}%", n))
                    .unwrap_or_else(|| "—".to_string()),
                5,
            ),
            Style::default().fg(p::DIM),
        ),
    ]);
    let tail_x = bar_x + bar_w;
    if tail_x < x + w {
        f.render_widget(
            Paragraph::new(tail).style(Style::default().bg(row_bg)),
            Rect {
                x: tail_x,
                y,
                width: (x + w).saturating_sub(tail_x),
                height: 1,
            },
        );
    }
}

fn draw_detail(f: &mut Frame, area: Rect, app: &App) {
    let split = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(40), Constraint::Length(50)])
        .split(area);

    let sel = app.filesystems.get(app.selected_fs);

    let left_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p::FAINT).bg(p::BG))
        .title(Span::styled(
            match sel {
                Some(m) => format!(" {}  DETAIL ", m.mount),
                None => " DETAIL ".to_string(),
            },
            Style::default().fg(p::CYAN).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(p::BG));
    let inner = left_block.inner(split[0]);
    f.render_widget(left_block, split[0]);

    if let Some(m) = sel {
        let used_pct = if m.size_bytes > 0 {
            (m.used_bytes as f64 / m.size_bytes as f64 * 100.0).round() as u32
        } else {
            0
        };
        let lines = vec![
            kv("mount", &m.mount, p::FG),
            kv("device", m.device.trim_start_matches("/dev/"), p::FG),
            kv("fs", &m.fs_type, p::CYAN),
            kv("size", &fmt_size(m.size_bytes), p::FG),
            kv(
                "used",
                &format!("{} ({}%)", fmt_size(m.used_bytes), used_pct),
                usage_color(used_pct),
            ),
            kv("free", &fmt_size(m.avail_bytes), p::FG),
            kv("kind", if m.is_system { "system" } else { "user" }, p::DIM),
            kv(
                "removable",
                if m.is_removable { "yes" } else { "no" },
                p::FG,
            ),
            Line::from(""),
            Line::from(Span::styled(
                " inode usage + 7d growth pending statvfs + history",
                Style::default().fg(p::DIM),
            )),
        ];
        f.render_widget(
            Paragraph::new(lines).style(Style::default().bg(p::BG)),
            inner,
        );
    }

    let right_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p::FAINT).bg(p::BG))
        .title(Span::styled(
            " 7d USAGE ",
            Style::default().fg(p::CYAN).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(p::BG));
    let inner_r = right_block.inner(split[1]);
    f.render_widget(right_block, split[1]);
    f.render_widget(
        Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled(
                "  awaiting snapshot history",
                Style::default().fg(p::DIM),
            )),
        ])
        .style(Style::default().bg(p::BG)),
        inner_r,
    );
}

fn kv(key: &str, val: &str, val_color: ratatui::style::Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!(" {:<12}", key), Style::default().fg(p::DIM)),
        Span::styled(val.to_string(), Style::default().fg(val_color)),
    ])
}
