//! Volumes tab — port of `dwRenderVolumes`.
//!
//! macOS: APFS container tree with volumes nested under their container.
//! Linux mdraid/zpool/LVM are deferred — on non-macOS this renders a
//! banner explaining what's missing.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::app::App;
use crate::collect::volumes::{ApfsContainer, ApfsVolume, MdRaidArray};
use crate::ui::format::{fmt_size, pad_left, pad_right};
use crate::ui::palette as p;

pub fn draw(f: &mut Frame, area: Rect, app: &App) {
    if app.volumes.containers.is_empty() && app.volumes.mdraid.is_empty() {
        draw_empty(f, area);
        return;
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(8)])
        .split(area);

    draw_filter_row(f, rows[0], &app.volumes.containers, &app.volumes.mdraid);
    draw_tree(f, rows[1], app);
}

fn draw_empty(f: &mut Frame, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p::FAINT).bg(p::BG))
        .title(Span::styled(
            " VOLUMES ",
            Style::default().fg(p::CYAN).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(p::BG));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            "  No managed volumes found.",
            Style::default().fg(p::DIM),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "  macOS APFS containers, Linux mdraid arrays, and (later)",
            Style::default().fg(p::DIM),
        )),
        Line::from(Span::styled(
            "  ZFS pools / LVM volume groups appear here when present.",
            Style::default().fg(p::DIM),
        )),
    ];
    f.render_widget(
        Paragraph::new(lines).style(Style::default().bg(p::BG)),
        inner,
    );
}

fn draw_filter_row(
    f: &mut Frame,
    area: Rect,
    containers: &[ApfsContainer],
    mdraid: &[MdRaidArray],
) {
    let total_vols: usize = containers.iter().map(|c| c.volumes.len()).sum();
    let summary = if mdraid.is_empty() {
        format!(
            "{} container{}, {} volume{}",
            containers.len(),
            if containers.len() == 1 { "" } else { "s" },
            total_vols,
            if total_vols == 1 { "" } else { "s" }
        )
    } else if containers.is_empty() {
        format!(
            "{} mdraid array{}",
            mdraid.len(),
            if mdraid.len() == 1 { "" } else { "s" }
        )
    } else {
        format!("{} apfs / {} mdraid", containers.len(), mdraid.len())
    };
    let line = Line::from(vec![
        Span::raw(" "),
        Span::styled("show", Style::default().fg(p::DIM)),
        Span::raw("  "),
        Span::styled(
            "all",
            Style::default()
                .fg(p::BR_WHITE)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled("apfs", Style::default().fg(p::FG)),
        Span::raw("  "),
        Span::styled("mdraid", Style::default().fg(p::FG)),
        Span::raw("  "),
        Span::styled(format!("({})", summary), Style::default().fg(p::DIM)),
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

fn draw_tree(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p::FAINT).bg(p::BG))
        .title(Span::styled(
            " VOLUMES ",
            Style::default().fg(p::CYAN).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(p::BG));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.height < 2 {
        return;
    }

    // Header
    let header = "   VOLUME / MEMBER                          KIND          SIZE       USED      STATE       MOUNT";
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

    let mut y = inner.y + 2;
    let max_y = inner.y + inner.height;

    for container in &app.volumes.containers {
        if y >= max_y {
            break;
        }
        draw_container_row(f, inner.x + 1, y, inner.width.saturating_sub(2), container);
        y += 1;

        let n = container.volumes.len();
        for (i, v) in container.volumes.iter().enumerate() {
            if y >= max_y {
                break;
            }
            let last = i + 1 == n;
            draw_volume_row(f, inner.x + 1, y, inner.width.saturating_sub(2), v, last);
            y += 1;
        }
    }

    for arr in &app.volumes.mdraid {
        if y >= max_y {
            break;
        }
        draw_mdraid_row(f, inner.x + 1, y, inner.width.saturating_sub(2), arr);
        y += 1;
        let n = arr.members.len();
        for (i, m) in arr.members.iter().enumerate() {
            if y >= max_y {
                break;
            }
            let last = i + 1 == n;
            draw_member_row(f, inner.x + 1, y, inner.width.saturating_sub(2), m, last);
            y += 1;
        }
        if let Some(prog) = &arr.progress {
            if y < max_y {
                draw_progress_row(f, inner.x + 1, y, inner.width.saturating_sub(2), prog);
                y += 1;
            }
        }
    }
}

fn draw_container_row(f: &mut Frame, x: u16, y: u16, w: u16, c: &ApfsContainer) {
    let used_pct = if c.size_bytes > 0 {
        (c.used_bytes as f64 / c.size_bytes as f64 * 100.0).round() as u32
    } else {
        0
    };
    let line = Line::from(vec![
        Span::raw(" "),
        Span::styled("\u{25cf}", Style::default().fg(p::GREEN)),
        Span::raw(" "),
        Span::styled(
            pad_right(&format!("\u{25be} {} (apfs)", c.bsd), 40),
            Style::default().fg(p::FG).add_modifier(Modifier::BOLD),
        ),
        Span::styled(pad_right("apfs ctr", 13), Style::default().fg(p::CYAN)),
        Span::styled(
            pad_left(&fmt_size(c.size_bytes), 9),
            Style::default().fg(p::DIM),
        ),
        Span::raw("  "),
        Span::styled(
            pad_left(&format!("{}%", used_pct), 5),
            Style::default().fg(p::FG),
        ),
        Span::raw("  "),
        Span::styled(pad_right("mounted", 11), Style::default().fg(p::GREEN)),
        Span::styled(
            c.physical_store
                .as_deref()
                .map(|s| format!("on {}", s))
                .unwrap_or_default(),
            Style::default().fg(p::DIM),
        ),
    ]);
    f.render_widget(
        Paragraph::new(line).style(Style::default().bg(p::BG)),
        Rect {
            x,
            y,
            width: w,
            height: 1,
        },
    );
}

fn draw_mdraid_row(
    f: &mut Frame,
    x: u16,
    y: u16,
    w: u16,
    arr: &crate::collect::volumes::MdRaidArray,
) {
    let healthy = arr.members_present == arr.members_total && !arr.member_state.contains('_');
    let dot_color = if !healthy { p::YELLOW } else { p::GREEN };
    let state_label = if !healthy {
        "DEGRADED".to_string()
    } else {
        arr.state.clone()
    };
    let state_color = if !healthy { p::YELLOW } else { p::GREEN };

    let line = Line::from(vec![
        Span::raw(" "),
        Span::styled("\u{25cf}", Style::default().fg(dot_color)),
        Span::raw(" "),
        Span::styled(
            pad_right(&format!("\u{25be} /dev/{}", arr.name), 40),
            Style::default().fg(p::FG).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            pad_right(&format!("mdraid {}", arr.level), 13),
            Style::default().fg(p::CYAN),
        ),
        Span::styled(
            pad_left(&fmt_size(arr.size_bytes), 9),
            Style::default().fg(p::DIM),
        ),
        Span::raw("  "),
        Span::styled(
            pad_left(&format!("{}/{}", arr.members_present, arr.members_total), 5),
            Style::default().fg(p::FG),
        ),
        Span::raw("  "),
        Span::styled(
            pad_right(&state_label, 11),
            Style::default().fg(state_color),
        ),
        Span::styled(
            format!("[{}]", arr.member_state),
            Style::default().fg(p::DIM),
        ),
    ]);
    f.render_widget(
        Paragraph::new(line).style(Style::default().bg(p::BG)),
        Rect {
            x,
            y,
            width: w,
            height: 1,
        },
    );
}

fn draw_member_row(
    f: &mut Frame,
    x: u16,
    y: u16,
    w: u16,
    m: &crate::collect::volumes::MdRaidMember,
    last: bool,
) {
    let glyph = if last { "  \u{2514}" } else { "  \u{251c}" };
    let (flag_text, flag_color) = match m.flag.as_deref() {
        Some("(F)") => ("failed", p::RED),
        Some("(S)") => ("spare", p::DIM),
        Some("(W)") => ("write-mostly", p::YELLOW),
        Some(other) => (other.trim_matches(|c: char| c == '(' || c == ')'), p::DIM),
        None => ("in_sync", p::GREEN),
    };
    let dot_color = if flag_text == "failed" {
        p::RED
    } else if flag_text == "write-mostly" {
        p::YELLOW
    } else {
        p::GREEN
    };
    let line = Line::from(vec![
        Span::raw(" "),
        Span::styled("\u{25cf}", Style::default().fg(dot_color)),
        Span::raw(" "),
        Span::styled(
            pad_right(
                &format!("{} /dev/{}  (idx {})", glyph, m.device, m.index),
                40,
            ),
            Style::default().fg(p::FG),
        ),
        Span::styled(pad_right("member", 13), Style::default().fg(p::DIM)),
        Span::styled(pad_left("—", 9), Style::default().fg(p::DIM)),
        Span::raw("  "),
        Span::styled(pad_left("—", 5), Style::default().fg(p::DIM)),
        Span::raw("  "),
        Span::styled(pad_right(flag_text, 11), Style::default().fg(flag_color)),
        Span::raw(""),
    ]);
    f.render_widget(
        Paragraph::new(line).style(Style::default().bg(p::BG)),
        Rect {
            x,
            y,
            width: w,
            height: 1,
        },
    );
}

fn draw_progress_row(
    f: &mut Frame,
    x: u16,
    y: u16,
    w: u16,
    prog: &crate::collect::volumes::MdRaidProgress,
) {
    let bar_w = 24;
    let filled = ((prog.percent / 100.0) * bar_w as f32).round() as usize;
    let bar: String = (0..bar_w)
        .map(|i| if i < filled { '\u{2588}' } else { '\u{2591}' })
        .collect();
    let line = Line::from(vec![
        Span::raw("      "),
        Span::styled(
            format!("{} {:.1}%  ", prog.op, prog.percent),
            Style::default().fg(p::CYAN),
        ),
        Span::styled(bar, Style::default().fg(p::CYAN)),
        Span::raw("  "),
        Span::styled(
            format!("eta {}  speed {}", prog.eta, prog.speed),
            Style::default().fg(p::DIM),
        ),
    ]);
    f.render_widget(
        Paragraph::new(line).style(Style::default().bg(p::BG)),
        Rect {
            x,
            y,
            width: w,
            height: 1,
        },
    );
}

fn draw_volume_row(f: &mut Frame, x: u16, y: u16, w: u16, v: &ApfsVolume, last: bool) {
    let glyph = if last { "  \u{2514}" } else { "  \u{251c}" };
    let display_name = if v.name.is_empty() {
        v.bsd.clone()
    } else {
        format!("{}  ({})", v.name, v.bsd)
    };
    let mount = v.mount_point.as_deref().unwrap_or("(not mounted)");
    let role_col = if v.role.is_empty() { p::DIM } else { p::CYAN };
    let state = if v.mount_point.is_some() {
        ("mounted", p::GREEN)
    } else {
        ("offline", p::DIM)
    };
    let line = Line::from(vec![
        Span::raw(" "),
        Span::styled(
            "\u{25cf}",
            Style::default().fg(if v.mount_point.is_some() {
                p::GREEN
            } else {
                p::DIM
            }),
        ),
        Span::raw(" "),
        Span::styled(
            pad_right(&format!("{} {}", glyph, display_name), 40),
            Style::default().fg(if v.mount_point.is_some() {
                p::FG
            } else {
                p::DIM
            }),
        ),
        Span::styled(
            pad_right(
                if v.role.is_empty() {
                    "apfs vol".to_string()
                } else {
                    format!("apfs {}", v.role.to_ascii_lowercase())
                }
                .as_str(),
                13,
            ),
            Style::default().fg(role_col),
        ),
        Span::styled(
            pad_left(&fmt_size(v.consumed_bytes), 9),
            Style::default().fg(p::DIM),
        ),
        Span::raw("  "),
        Span::styled(pad_left("—", 5), Style::default().fg(p::DIM)),
        Span::raw("  "),
        Span::styled(pad_right(state.0, 11), Style::default().fg(state.1)),
        Span::styled(mount.to_string(), Style::default().fg(p::DIM)),
    ]);
    f.render_widget(
        Paragraph::new(line).style(Style::default().bg(p::BG)),
        Rect {
            x,
            y,
            width: w,
            height: 1,
        },
    );
}
