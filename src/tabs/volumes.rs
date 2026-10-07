//! Volumes tab — port of `dwRenderVolumes`.
//!
//! macOS: APFS container tree with volumes nested under their container.
//! Linux: mdraid arrays with their members, and ZFS pools with one row per
//! top-level vdev. LVM is deferred. With nothing to list this renders a
//! banner explaining what would appear.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::app::App;
use crate::collect::volumes::{ApfsContainer, ApfsVolume, VolumeTick};
use crate::collect::zfs::{VdevClass, ZfsPool, ZfsVdev};
use crate::ui::format::{fmt_size, pad_left, pad_right, usage_color};
use crate::ui::palette as p;

pub fn draw(f: &mut Frame, area: Rect, app: &App) {
    let vols = &app.volumes;
    if vols.containers.is_empty() && vols.mdraid.is_empty() && vols.zfs.is_empty() {
        draw_empty(f, area, vols.zfs_note.as_deref());
        return;
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(8)])
        .split(area);

    draw_filter_row(f, rows[0], vols);
    draw_tree(f, rows[1], app);
}

fn draw_empty(f: &mut Frame, area: Rect, zfs_note: Option<&str>) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p::faint()).bg(p::bg()))
        .title(Span::styled(
            " VOLUMES ",
            Style::default().fg(p::cyan()).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(p::bg()));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let mut lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            "  No managed volumes found.",
            Style::default().fg(p::dim()),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "  macOS APFS containers, Linux mdraid arrays and ZFS pools",
            Style::default().fg(p::dim()),
        )),
        Line::from(Span::styled(
            "  appear here when present. LVM volume groups are not listed yet.",
            Style::default().fg(p::dim()),
        )),
    ];
    if let Some(note) = zfs_note {
        lines.push(Line::from(""));
        lines.push(zfs_note_line(note));
    }
    f.render_widget(
        Paragraph::new(lines).style(Style::default().bg(p::bg())),
        inner,
    );
}

/// Shown wherever ZFS is in use but `zpool list` couldn't be read.
fn zfs_note_line(note: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled("  \u{26a0} ", Style::default().fg(p::yellow())),
        Span::styled(note.to_string(), Style::default().fg(p::dim())),
    ])
}

fn draw_filter_row(f: &mut Frame, area: Rect, vols: &VolumeTick) {
    let (containers, mdraid, zfs) = (&vols.containers, &vols.mdraid, &vols.zfs);
    let plural = |n: usize, what: &str| format!("{} {}{}", n, what, if n == 1 { "" } else { "s" });
    let total_vols: usize = containers.iter().map(|c| c.volumes.len()).sum();
    let summary = if mdraid.is_empty() && zfs.is_empty() {
        format!(
            "{}, {}",
            plural(containers.len(), "container"),
            plural(total_vols, "volume")
        )
    } else {
        let mut parts = Vec::new();
        if !containers.is_empty() {
            parts.push(format!("{} apfs", containers.len()));
        }
        if !mdraid.is_empty() {
            parts.push(plural(mdraid.len(), "mdraid array"));
        }
        if !zfs.is_empty() {
            parts.push(plural(zfs.len(), "zfs pool"));
        }
        parts.join(" / ")
    };
    let line = Line::from(vec![
        Span::raw(" "),
        Span::styled("show", Style::default().fg(p::dim())),
        Span::raw("  "),
        Span::styled(
            "all",
            Style::default()
                .fg(p::br_white())
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled("apfs", Style::default().fg(p::fg())),
        Span::raw("  "),
        Span::styled("mdraid", Style::default().fg(p::fg())),
        Span::raw("  "),
        Span::styled("zfs", Style::default().fg(p::fg())),
        Span::raw("  "),
        Span::styled(format!("({})", summary), Style::default().fg(p::dim())),
    ]);
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

fn draw_tree(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p::faint()).bg(p::bg()))
        .title(Span::styled(
            " VOLUMES ",
            Style::default().fg(p::cyan()).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(p::bg()));
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
            Style::default().fg(p::dim()),
        )))
        .style(Style::default().bg(p::bg())),
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
            Style::default().fg(p::faint()).bg(p::bg()),
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

    for pool in &app.volumes.zfs {
        if y >= max_y {
            break;
        }
        draw_zfs_pool_row(f, inner.x + 1, y, inner.width.saturating_sub(2), pool);
        y += 1;
        let n = pool.vdevs.len();
        for (i, vdev) in pool.vdevs.iter().enumerate() {
            if y >= max_y {
                break;
            }
            let last = i + 1 == n;
            draw_zfs_vdev_row(f, inner.x + 1, y, inner.width.saturating_sub(2), vdev, last);
            y += 1;
        }
    }

    // Pools that couldn't be read are still worth a line when other
    // volumes are listed: their absence would otherwise look like none.
    if let Some(note) = &app.volumes.zfs_note {
        if y + 1 < max_y {
            f.render_widget(
                Paragraph::new(zfs_note_line(note)).style(Style::default().bg(p::bg())),
                Rect {
                    x: inner.x + 1,
                    y: y + 1,
                    width: inner.width.saturating_sub(2),
                    height: 1,
                },
            );
        }
    }
}

fn pct(part: u64, whole: u64) -> u32 {
    if whole == 0 {
        return 0;
    }
    (part as f64 / whole as f64 * 100.0).round() as u32
}

/// Pool and vdev states as `zpool` names them.
fn zfs_health_color(health: &str) -> ratatui::style::Color {
    match health {
        "ONLINE" => p::green(),
        // Redundancy reduced, or a spare already covering for a failure.
        "DEGRADED" | "OFFLINE" | "INUSE" => p::yellow(),
        // An idle hot spare.
        "AVAIL" => p::dim(),
        // FAULTED, UNAVAIL, REMOVED, SUSPENDED.
        _ => p::red(),
    }
}

fn draw_zfs_pool_row(f: &mut Frame, x: u16, y: u16, w: u16, pool: &ZfsPool) {
    let used_pct = pct(pool.alloc_bytes, pool.size_bytes);
    let health = zfs_health_color(&pool.health);
    let line = Line::from(vec![
        Span::raw(" "),
        Span::styled("\u{25cf}", Style::default().fg(health)),
        Span::raw(" "),
        Span::styled(
            pad_right(&format!("\u{25be} {}", pool.name), 40),
            Style::default().fg(p::fg()).add_modifier(Modifier::BOLD),
        ),
        Span::styled(pad_right("zfs pool", 13), Style::default().fg(p::cyan())),
        Span::styled(
            pad_left(&fmt_size(pool.size_bytes), 9),
            Style::default().fg(p::dim()),
        ),
        Span::raw("  "),
        Span::styled(
            pad_left(&format!("{}%", used_pct), 5),
            Style::default().fg(usage_color(used_pct)),
        ),
        Span::raw("  "),
        Span::styled(pad_right(&pool.health, 11), Style::default().fg(health)),
        Span::styled(
            format!(
                "{} alloc \u{b7} {} free",
                fmt_size(pool.alloc_bytes),
                fmt_size(pool.free_bytes)
            ),
            Style::default().fg(p::dim()),
        ),
    ]);
    f.render_widget(
        Paragraph::new(line).style(Style::default().bg(p::bg())),
        Rect {
            x,
            y,
            width: w,
            height: 1,
        },
    );
}

/// `mirror · nvme0n1p3, nvme1n1p3`, with any member that isn't ONLINE
/// carrying its state: `mirror · nvme0n1p3 OFFLINE, nvme1n1p3`. A lone
/// member's state is the vdev's, already in the STATE column.
fn zfs_layout_summary(v: &ZfsVdev) -> String {
    let members: Vec<String> = v
        .members
        .iter()
        .map(|m| {
            if m.health == "ONLINE" || v.members.len() == 1 {
                m.display().to_string()
            } else {
                format!("{} {}", m.display(), m.health)
            }
        })
        .collect();
    format!("{} \u{b7} {}", v.layout(), members.join(", "))
}

fn draw_zfs_vdev_row(f: &mut Frame, x: u16, y: u16, w: u16, v: &ZfsVdev, last: bool) {
    let glyph = if last { "  \u{2514}" } else { "  \u{251c}" };
    // A single-disk vdev is named by its device path; the member's short
    // name says the same thing in a column that fits.
    let name = match (v.layout(), v.members.first()) {
        ("disk", Some(m)) => m.display(),
        _ => v.name.as_str(),
    };
    let used = match v.alloc_bytes {
        Some(a) if v.size_bytes > 0 => format!("{}%", pct(a, v.size_bytes)),
        _ => "\u{2014}".to_string(),
    };
    let health = zfs_health_color(&v.health);
    let class_color = match v.class {
        VdevClass::Cache | VdevClass::Spare => p::dim(),
        _ => p::cyan(),
    };
    let line = Line::from(vec![
        Span::raw(" "),
        Span::styled("\u{25cf}", Style::default().fg(health)),
        Span::raw(" "),
        Span::styled(
            pad_right(&format!("{} {}", glyph, name), 40),
            Style::default().fg(p::fg()),
        ),
        Span::styled(
            pad_right(v.class.label(), 13),
            Style::default().fg(class_color),
        ),
        Span::styled(
            pad_left(&fmt_size(v.size_bytes), 9),
            Style::default().fg(p::dim()),
        ),
        Span::raw("  "),
        Span::styled(pad_left(&used, 5), Style::default().fg(p::fg())),
        Span::raw("  "),
        Span::styled(pad_right(&v.health, 11), Style::default().fg(health)),
        Span::styled(zfs_layout_summary(v), Style::default().fg(p::dim())),
    ]);
    f.render_widget(
        Paragraph::new(line).style(Style::default().bg(p::bg())),
        Rect {
            x,
            y,
            width: w,
            height: 1,
        },
    );
}

fn draw_container_row(f: &mut Frame, x: u16, y: u16, w: u16, c: &ApfsContainer) {
    let used_pct = if c.size_bytes > 0 {
        (c.used_bytes as f64 / c.size_bytes as f64 * 100.0).round() as u32
    } else {
        0
    };
    let line = Line::from(vec![
        Span::raw(" "),
        Span::styled("\u{25cf}", Style::default().fg(p::green())),
        Span::raw(" "),
        Span::styled(
            pad_right(&format!("\u{25be} {} (apfs)", c.bsd), 40),
            Style::default().fg(p::fg()).add_modifier(Modifier::BOLD),
        ),
        Span::styled(pad_right("apfs ctr", 13), Style::default().fg(p::cyan())),
        Span::styled(
            pad_left(&fmt_size(c.size_bytes), 9),
            Style::default().fg(p::dim()),
        ),
        Span::raw("  "),
        Span::styled(
            pad_left(&format!("{}%", used_pct), 5),
            Style::default().fg(p::fg()),
        ),
        Span::raw("  "),
        Span::styled(pad_right("mounted", 11), Style::default().fg(p::green())),
        Span::styled(
            c.physical_store
                .as_deref()
                .map(|s| format!("on {}", s))
                .unwrap_or_default(),
            Style::default().fg(p::dim()),
        ),
    ]);
    f.render_widget(
        Paragraph::new(line).style(Style::default().bg(p::bg())),
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
    let dot_color = if !healthy { p::yellow() } else { p::green() };
    let state_label = if !healthy {
        "DEGRADED".to_string()
    } else {
        arr.state.clone()
    };
    let state_color = if !healthy { p::yellow() } else { p::green() };

    let line = Line::from(vec![
        Span::raw(" "),
        Span::styled("\u{25cf}", Style::default().fg(dot_color)),
        Span::raw(" "),
        Span::styled(
            pad_right(&format!("\u{25be} /dev/{}", arr.name), 40),
            Style::default().fg(p::fg()).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            pad_right(&format!("mdraid {}", arr.level), 13),
            Style::default().fg(p::cyan()),
        ),
        Span::styled(
            pad_left(&fmt_size(arr.size_bytes), 9),
            Style::default().fg(p::dim()),
        ),
        Span::raw("  "),
        Span::styled(
            pad_left(&format!("{}/{}", arr.members_present, arr.members_total), 5),
            Style::default().fg(p::fg()),
        ),
        Span::raw("  "),
        Span::styled(
            pad_right(&state_label, 11),
            Style::default().fg(state_color),
        ),
        Span::styled(
            format!("[{}]", arr.member_state),
            Style::default().fg(p::dim()),
        ),
    ]);
    f.render_widget(
        Paragraph::new(line).style(Style::default().bg(p::bg())),
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
        Some("(F)") => ("failed", p::red()),
        Some("(S)") => ("spare", p::dim()),
        Some("(W)") => ("write-mostly", p::yellow()),
        Some(other) => (other.trim_matches(|c: char| c == '(' || c == ')'), p::dim()),
        None => ("in_sync", p::green()),
    };
    let dot_color = if flag_text == "failed" {
        p::red()
    } else if flag_text == "write-mostly" {
        p::yellow()
    } else {
        p::green()
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
            Style::default().fg(p::fg()),
        ),
        Span::styled(pad_right("member", 13), Style::default().fg(p::dim())),
        Span::styled(pad_left("—", 9), Style::default().fg(p::dim())),
        Span::raw("  "),
        Span::styled(pad_left("—", 5), Style::default().fg(p::dim())),
        Span::raw("  "),
        Span::styled(pad_right(flag_text, 11), Style::default().fg(flag_color)),
        Span::raw(""),
    ]);
    f.render_widget(
        Paragraph::new(line).style(Style::default().bg(p::bg())),
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
            Style::default().fg(p::cyan()),
        ),
        Span::styled(bar, Style::default().fg(p::cyan())),
        Span::raw("  "),
        Span::styled(
            format!("eta {}  speed {}", prog.eta, prog.speed),
            Style::default().fg(p::dim()),
        ),
    ]);
    f.render_widget(
        Paragraph::new(line).style(Style::default().bg(p::bg())),
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
    let role_col = if v.role.is_empty() {
        p::dim()
    } else {
        p::cyan()
    };
    let state = if v.mount_point.is_some() {
        ("mounted", p::green())
    } else {
        ("offline", p::dim())
    };
    let line = Line::from(vec![
        Span::raw(" "),
        Span::styled(
            "\u{25cf}",
            Style::default().fg(if v.mount_point.is_some() {
                p::green()
            } else {
                p::dim()
            }),
        ),
        Span::raw(" "),
        Span::styled(
            pad_right(&format!("{} {}", glyph, display_name), 40),
            Style::default().fg(if v.mount_point.is_some() {
                p::fg()
            } else {
                p::dim()
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
            Style::default().fg(p::dim()),
        ),
        Span::raw("  "),
        Span::styled(pad_left("—", 5), Style::default().fg(p::dim())),
        Span::raw("  "),
        Span::styled(pad_right(state.0, 11), Style::default().fg(state.1)),
        Span::styled(mount.to_string(), Style::default().fg(p::dim())),
    ]);
    f.render_widget(
        Paragraph::new(line).style(Style::default().bg(p::bg())),
        Rect {
            x,
            y,
            width: w,
            height: 1,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::ViewMode;
    use crate::collect::zfs::fixtures;
    use crate::tabs::TabId;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn render(vols: VolumeTick) -> String {
        let mut app = App::new_for_test(TabId::Volumes, ViewMode::Full);
        app.volumes = vols;
        let mut term = Terminal::new(TestBackend::new(170, 24)).expect("terminal");
        term.draw(|f| draw(f, f.area(), &app)).expect("draw");
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

    fn row<'a>(out: &'a str, needle: &str) -> &'a str {
        out.lines()
            .find(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("no row with {needle:?} in\n{out}"))
    }

    #[test]
    fn zfs_pools_list_with_their_layout() {
        let out = render(VolumeTick {
            zfs: fixtures::pools(fixtures::UBUNTU),
            ..Default::default()
        });
        assert!(out.contains("(2 zfs pools)"), "{out}");
        let rpool = row(&out, "\u{25be} rpool");
        for want in [
            "zfs pool",
            "991 GB",
            "6%",
            "ONLINE",
            "58 GB alloc",
            "933 GB free",
        ] {
            assert!(rpool.contains(want), "{want:?} missing from {rpool:?}");
        }
        let vdev = row(&out, "mirror \u{b7} nvme0n1p3, nvme1n1p3");
        assert!(
            vdev.contains("mirror-0") && vdev.contains("data"),
            "{vdev:?}"
        );
        assert!(out.contains("mirror \u{b7} nvme0n1p2, nvme1n1p2"), "{out}");
    }

    #[test]
    fn every_vdev_class_gets_a_row() {
        let out = render(VolumeTick {
            zfs: fixtures::pools(fixtures::CLASSES),
            ..Default::default()
        });
        assert!(row(&out, "mirror \u{b7} nvme0n1p1, nvme1n1p1").contains("special"));
        assert!(row(&out, "disk \u{b7} nvme2n1p1").contains("log"));
        assert!(row(&out, "disk \u{b7} nvme2n1p2").contains("cache"));
        let spare = row(&out, "disk \u{b7} sdc1");
        assert!(
            spare.contains("spare") && spare.contains("AVAIL"),
            "{spare:?}"
        );
    }

    #[test]
    fn a_degraded_member_is_named() {
        let out = render(VolumeTick {
            zfs: fixtures::pools(fixtures::DEGRADED),
            ..Default::default()
        });
        assert!(row(&out, "\u{25be} rpool").contains("DEGRADED"));
        assert!(
            out.contains("mirror \u{b7} nvme0n1p3 OFFLINE, nvme1n1p3"),
            "{out}"
        );
        assert!(
            out.contains("raidz1 \u{b7} sdb1, 2817390427383737393 UNAVAIL, sdd1 FAULTED, sde1"),
            "{out}"
        );
    }

    #[test]
    fn pools_sit_alongside_mdraid() {
        let out = render(VolumeTick {
            mdraid: crate::collect::volumes::parse_mdstat(
                "md0 : active raid1 sde1[0] sdf1[1]\n      1953382464 blocks super 1.2 [2/2] [UU]\n",
            ),
            zfs: fixtures::pools(fixtures::RAIDZ1),
            ..Default::default()
        });
        assert!(out.contains("(1 mdraid array / 1 zfs pool)"), "{out}");
        assert!(
            out.contains("/dev/md0") && out.contains("\u{25be} media"),
            "{out}"
        );
    }

    #[test]
    fn the_empty_state_no_longer_says_zfs_is_coming_later() {
        let out = render(VolumeTick::default());
        assert!(out.contains("No managed volumes found."), "{out}");
        assert!(out.contains("ZFS pools"), "{out}");
        assert!(!out.contains("(later)"), "{out}");
        assert!(
            !out.contains('\u{26a0}'),
            "no zpool, no ZFS: nothing to warn about"
        );
    }

    #[test]
    fn a_zpool_that_could_not_be_read_says_so() {
        let note = "ZFS is loaded but zpool was not found; pools not shown";
        let out = render(VolumeTick {
            zfs_note: Some(note.to_string()),
            ..Default::default()
        });
        assert!(out.contains(note), "{out}");

        // Still said when other volumes fill the tab.
        let out = render(VolumeTick {
            mdraid: crate::collect::volumes::parse_mdstat(
                "md0 : active raid1 sde1[0] sdf1[1]\n      1953382464 blocks super 1.2 [2/2] [UU]\n",
            ),
            zfs_note: Some(note.to_string()),
            ..Default::default()
        });
        assert!(out.contains(note), "{out}");
    }
}
