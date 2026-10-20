//! Volumes tab — port of `dwRenderVolumes`.
//!
//! macOS: APFS container tree with volumes nested under their container.
//! Linux: mdraid arrays with their members, ZFS pools with one row per
//! top-level vdev, and LVM volume groups with their LVs and PVs. With
//! nothing to list this renders a banner explaining what would appear.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::app::App;
use crate::collect::hot_files::PoolHot;
use crate::collect::lvm::{LvmLv, LvmPv, LvmVg};
use crate::collect::volumes::{ApfsContainer, ApfsVolume, VolumeTick};
use crate::collect::zfs::{VdevClass, ZfsPool, ZfsVdev};
use crate::ui::format::{fmt_size, pad_left, pad_right, usage_color};
use crate::ui::palette as p;

pub fn draw(f: &mut Frame, area: Rect, app: &App) {
    let vols = &app.volumes;
    if vols.containers.is_empty()
        && vols.mdraid.is_empty()
        && vols.zfs.is_empty()
        && vols.lvm.is_empty()
    {
        draw_empty(f, area, vols);
        return;
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(8)])
        .split(area);

    draw_filter_row(f, rows[0], vols);
    draw_tree(f, rows[1], app);
}

fn draw_empty(f: &mut Frame, area: Rect, vols: &VolumeTick) {
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
            "  macOS APFS containers, Linux mdraid arrays, ZFS pools and LVM",
            Style::default().fg(p::dim()),
        )),
        Line::from(Span::styled(
            "  volume groups appear here when present.",
            Style::default().fg(p::dim()),
        )),
    ];
    let notes: Vec<&String> = vols.zfs_note.iter().chain(&vols.lvm_note).collect();
    if !notes.is_empty() {
        lines.push(Line::from(""));
    }
    for note in notes {
        lines.push(note_line(note));
    }
    f.render_widget(
        Paragraph::new(lines).style(Style::default().bg(p::bg())),
        inner,
    );
}

/// Shown wherever ZFS or LVM is in use but its tools couldn't be read.
fn note_line(note: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled("  \u{26a0} ", Style::default().fg(p::yellow())),
        Span::styled(note.to_string(), Style::default().fg(p::dim())),
    ])
}

fn draw_filter_row(f: &mut Frame, area: Rect, vols: &VolumeTick) {
    let (containers, mdraid, zfs, lvm) = (&vols.containers, &vols.mdraid, &vols.zfs, &vols.lvm);
    let plural = |n: usize, what: &str| format!("{} {}{}", n, what, if n == 1 { "" } else { "s" });
    let total_vols: usize = containers.iter().map(|c| c.volumes.len()).sum();
    let summary = if mdraid.is_empty() && zfs.is_empty() && lvm.is_empty() {
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
        if !lvm.is_empty() {
            parts.push(plural(lvm.len(), "lvm vg"));
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
        Span::styled("lvm", Style::default().fg(p::fg())),
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

    let hot = app.hot_files.pool_activity(&app.filesystems);
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
        if let Some(h) = hot.get(&pool.name) {
            for line in zfs_hot_lines(h) {
                if y >= max_y {
                    break;
                }
                f.render_widget(
                    Paragraph::new(line).style(Style::default().bg(p::bg())),
                    Rect {
                        x: inner.x + 1,
                        y,
                        width: inner.width.saturating_sub(2),
                        height: 1,
                    },
                );
                y += 1;
            }
        }
    }

    for vg in &app.volumes.lvm {
        let lines = lvm_vg_lines(vg);
        if y >= max_y {
            break;
        }
        for line in lines {
            if y >= max_y {
                break;
            }
            f.render_widget(
                Paragraph::new(line).style(Style::default().bg(p::bg())),
                Rect {
                    x: inner.x + 1,
                    y,
                    width: inner.width.saturating_sub(2),
                    height: 1,
                },
            );
            y += 1;
        }
    }

    // Pools or VGs that couldn't be read are still worth a line when other
    // volumes are listed: their absence would otherwise look like none,
    // and pools kept from an earlier listing would look current.
    y += 1;
    for note in app.volumes.zfs_note.iter().chain(&app.volumes.lvm_note) {
        if y >= max_y {
            break;
        }
        f.render_widget(
            Paragraph::new(note_line(note)).style(Style::default().bg(p::bg())),
            Rect {
                x: inner.x + 1,
                y,
                width: inner.width.saturating_sub(2),
                height: 1,
            },
        );
        y += 1;
    }
}

/// A size, or a dash where sysfs couldn't say.
fn size_or_dash(b: u64) -> String {
    if b == 0 {
        "\u{2014}".to_string()
    } else {
        fmt_size(b)
    }
}

/// One row in the shared column layout: dot, name, kind, size, used,
/// state, then free text.
#[allow(clippy::too_many_arguments)]
fn tree_row(
    dot: ratatui::style::Color,
    name: String,
    bold: bool,
    kind: &str,
    kind_color: ratatui::style::Color,
    size: String,
    used: String,
    used_color: ratatui::style::Color,
    state: (&str, ratatui::style::Color),
    extra: String,
) -> Line<'static> {
    let name_style = if bold {
        Style::default().fg(p::fg()).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(p::fg())
    };
    Line::from(vec![
        Span::raw(" "),
        Span::styled("\u{25cf}", Style::default().fg(dot)),
        Span::raw(" "),
        Span::styled(pad_right(&name, 40), name_style),
        Span::styled(pad_right(kind, 13), Style::default().fg(kind_color)),
        Span::styled(pad_left(&size, 9), Style::default().fg(p::dim())),
        Span::raw("  "),
        Span::styled(pad_left(&used, 5), Style::default().fg(used_color)),
        Span::raw("  "),
        Span::styled(pad_right(state.0, 11), Style::default().fg(state.1)),
        Span::styled(extra, Style::default().fg(p::dim())),
    ])
}

/// A VG, then its LVs, then the PVs it sits on.
fn lvm_vg_lines(vg: &LvmVg) -> Vec<Line<'static>> {
    let partial = vg.is_partial();
    let (used, used_pct) = if vg.size_bytes > 0 {
        let p = pct(
            vg.size_bytes - vg.free_bytes.min(vg.size_bytes),
            vg.size_bytes,
        );
        (format!("{p}%"), p)
    } else {
        ("\u{2014}".to_string(), 0)
    };
    let state = if partial {
        ("PARTIAL", p::red())
    } else {
        ("active", p::green())
    };
    let lv_count = format!(
        "{} LV{}",
        vg.lvs.len(),
        if vg.lvs.len() == 1 { "" } else { "s" }
    );
    let extra = if vg.size_bytes > 0 {
        format!("{lv_count} \u{b7} {} free", fmt_size(vg.free_bytes))
    } else {
        lv_count
    };
    let mut lines = vec![tree_row(
        state.1,
        format!("\u{25be} {}", vg.name),
        true,
        "lvm vg",
        p::cyan(),
        size_or_dash(vg.size_bytes),
        used,
        usage_color(used_pct),
        state,
        extra,
    )];
    let n = vg.lvs.len() + vg.pvs.len();
    let glyph = |i: usize| {
        if i + 1 == n {
            "  \u{2514}"
        } else {
            "  \u{251c}"
        }
    };
    for (i, lv) in vg.lvs.iter().enumerate() {
        lines.push(lvm_lv_line(lv, glyph(i)));
    }
    for (i, pv) in vg.pvs.iter().enumerate() {
        lines.push(lvm_pv_line(pv, glyph(vg.lvs.len() + i)));
    }
    lines
}

/// STATE for an LV: lvm2's health word when there is one (shortened to
/// fit), else whether it's active and in sync.
fn lv_state(lv: &LvmLv) -> (&'static str, ratatui::style::Color) {
    if lv.is_invalid_snapshot() {
        return ("invalid", p::red());
    }
    match lv.health.as_str() {
        "" => {}
        "partial" => return ("partial", p::red()),
        "failed" => return ("failed", p::red()),
        "out of data" => return ("out of data", p::red()),
        "refresh needed" => return ("refresh", p::yellow()),
        "mismatches exist" => return ("mismatches", p::yellow()),
        "metadata read only" => return ("meta ro", p::red()),
        _ => return ("unhealthy", p::yellow()),
    }
    if !lv.is_active() {
        return ("inactive", p::dim());
    }
    if lv.sync_percent.is_some_and(|s| s < 100.0) {
        return ("syncing", p::yellow());
    }
    ("active", p::green())
}

fn lvm_lv_line(lv: &LvmLv, glyph: &str) -> Line<'static> {
    let state = lv_state(lv);
    let (used, used_color) = match lv.fullest_percent() {
        Some(pc) => {
            let pc = pc.round() as u32;
            (format!("{pc}%"), usage_color(pc))
        }
        None => ("\u{2014}".to_string(), p::dim()),
    };
    let mut extra: Vec<String> = Vec::new();
    if let Some(d) = &lv.dev {
        extra.push(d.clone());
    }
    if lv.is_thin_pool() {
        if let (Some(d), Some(m)) = (lv.data_percent, lv.metadata_percent) {
            extra.push(format!("data {d:.0}% \u{b7} meta {m:.0}%"));
        }
    }
    if let Some(s) = lv.sync_percent.filter(|s| *s < 100.0) {
        extra.push(format!("sync {s:.0}%"));
    }
    if let Some(pool) = &lv.pool {
        extra.push(format!("in {pool}"));
    }
    if let Some(origin) = &lv.origin {
        extra.push(format!("of {origin}"));
    }
    let kind = lv.kind();
    tree_row(
        state.1,
        format!("{glyph} {}", lv.name),
        false,
        &kind,
        p::cyan(),
        fmt_size(lv.size_bytes),
        used,
        used_color,
        state,
        extra.join(" \u{b7} "),
    )
}

fn lvm_pv_line(pv: &LvmPv, glyph: &str) -> Line<'static> {
    let state = if pv.missing {
        ("missing", p::red())
    } else {
        ("ok", p::green())
    };
    let used = if pv.size_bytes > 0 {
        format!(
            "{}%",
            pct(
                pv.size_bytes - pv.free_bytes.min(pv.size_bytes),
                pv.size_bytes
            )
        )
    } else {
        "\u{2014}".to_string()
    };
    tree_row(
        state.1,
        format!("{glyph} {}", pv.path),
        false,
        "pv",
        p::dim(),
        size_or_dash(pv.size_bytes),
        used,
        p::fg(),
        state,
        if pv.size_bytes > 0 {
            format!("{} free", fmt_size(pv.free_bytes))
        } else {
            String::new()
        },
    )
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

/// The "hot files" rows under a pool: a summary line, then its busiest
/// files. A pool nothing watches says so rather than reading as idle.
fn zfs_hot_lines(h: &PoolHot) -> Vec<Line<'static>> {
    let label = |text: String, color| {
        vec![
            Span::raw("   "),
            Span::styled(pad_right("hot files", 10), Style::default().fg(p::dim())),
            Span::styled(text, Style::default().fg(color)),
        ]
    };
    if !h.watched {
        return vec![Line::from(label(
            "not watched \u{2014} pass --watch <mountpoint>".to_string(),
            p::dim(),
        ))];
    }
    if h.top.is_empty() {
        return vec![Line::from(label("idle".to_string(), p::dim()))];
    }
    let mut lines = vec![Line::from(label(
        format!("{:.1} ev/s", h.events_per_sec),
        p::fg(),
    ))];
    let n = h.top.len();
    for (i, (path, rate)) in h.top.iter().enumerate() {
        let branch = if i + 1 == n { "\u{2514}" } else { "\u{251c}" };
        lines.push(Line::from(vec![
            Span::raw("   "),
            Span::styled(format!("{branch} "), Style::default().fg(p::dim())),
            Span::styled(
                pad_left(&format!("{rate:.1} ev/s"), 12),
                Style::default().fg(p::fg()),
            ),
            Span::raw("  "),
            Span::styled(path.display().to_string(), Style::default().fg(p::dim())),
        ]));
    }
    lines
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

        // Pools kept from the last listing are marked as such.
        let note = "zpool list did not answer within 2s; showing pools as last listed";
        let out = render(VolumeTick {
            zfs: fixtures::pools(fixtures::UBUNTU),
            zfs_note: Some(note.to_string()),
            ..Default::default()
        });
        assert!(out.contains("\u{25be} rpool"), "{out}");
        assert!(out.contains(note), "{out}");
    }

    #[test]
    fn lvm_vgs_list_their_lvs_and_pvs() {
        let out = render(VolumeTick {
            lvm: crate::collect::lvm::fixtures::vgs(),
            ..Default::default()
        });
        assert!(out.contains("(2 lvm vgs)"), "{out}");
        let data = row(&out, "\u{25be} data");
        for want in ["lvm vg", "1.0 TB", "46%", "active", "5 LVs", "537 GB free"] {
            assert!(data.contains(want), "{want:?} missing from {data:?}");
        }
        let pool = row(&out, "\u{251c} pool");
        for want in ["thin pool", "215 GB", "61%", "data 61% \u{b7} meta 12%"] {
            assert!(pool.contains(want), "{want:?} missing from {pool:?}");
        }
        assert!(row(&out, "\u{251c} vm1").contains("in pool"));
        let snap = row(&out, "\u{251c} home_snap");
        assert!(
            snap.contains("snapshot") && snap.contains("of home"),
            "{snap:?}"
        );
        let mirror = row(&out, "\u{251c} mirror");
        assert!(
            mirror.contains("raid1") && mirror.contains("syncing") && mirror.contains("sync 42%"),
            "{mirror:?}"
        );
        assert!(row(&out, "\u{251c} /dev/sda").contains("pv"));
        assert!(row(&out, "\u{2514} /dev/sdb").contains("ok"));
        assert!(row(&out, "\u{2514} /dev/nvme0n1p3").contains("254 GB"));
        assert!(!out.contains("sdc1"), "a PV in no VG isn't listed");
    }

    #[test]
    fn a_degraded_vg_shows_what_broke() {
        let out = render(VolumeTick {
            lvm: crate::collect::lvm::fixtures::degraded(),
            ..Default::default()
        });
        assert!(row(&out, "\u{25be} data").contains("PARTIAL"));
        assert!(row(&out, "\u{251c} mirror").contains("partial"));
        assert!(row(&out, "\u{251c} old_snap").contains("invalid"));
        assert!(row(&out, "\u{2514} [unknown]").contains("missing"));
        assert!(row(&out, "\u{251c} pool").contains("96%"));
    }

    #[test]
    fn lvs_from_sysfs_say_what_is_missing() {
        let note =
            "lvm2 reports need root; showing active LVs from sysfs, without free space or health";
        let mut vg = crate::collect::lvm::LvmVg {
            name: "fedora".into(),
            ..Default::default()
        };
        vg.lvs.push(crate::collect::lvm::LvmLv {
            name: "root".into(),
            size_bytes: 64_000_000_000,
            dev: Some("dm-1".into()),
            ..Default::default()
        });
        vg.pvs.push(crate::collect::lvm::LvmPv {
            path: "/dev/nvme0n1p3".into(),
            ..Default::default()
        });
        let out = render(VolumeTick {
            lvm: vec![vg],
            lvm_note: Some(note.into()),
            ..Default::default()
        });
        let vg = row(&out, "\u{25be} fedora");
        assert!(vg.contains("\u{2014}") && vg.contains("1 LV"), "{vg:?}");
        assert!(!vg.contains("free"), "{vg:?}");
        let root = row(&out, "\u{251c} root");
        assert!(root.contains("64 GB") && root.contains("dm-1") && root.contains("active"));
        assert!(out.contains(note), "{out}");
    }

    #[test]
    fn the_empty_state_lists_lvm() {
        let out = render(VolumeTick::default());
        assert!(out.contains("LVM"), "{out}");
        assert!(!out.contains("not listed yet"), "{out}");
    }

    fn text(lines: Vec<Line<'static>>) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_pool_lists_its_busiest_files() {
        let h = PoolHot {
            events_per_sec: 50.0,
            top: vec![
                (std::path::PathBuf::from("/tank/db.sqlite"), 40.0),
                (std::path::PathBuf::from("/tank/log"), 10.0),
            ],
            watched: true,
        };
        let out = text(zfs_hot_lines(&h));
        assert!(out.contains("50.0 ev/s"), "{out}");
        assert!(
            out.contains("40.0 ev/s") && out.contains("/tank/db.sqlite"),
            "{out}"
        );
    }

    #[test]
    fn an_unwatched_pool_says_so_instead_of_idle() {
        let h = PoolHot::default();
        assert!(text(zfs_hot_lines(&h)).contains("not watched"));
        let h = PoolHot {
            watched: true,
            ..PoolHot::default()
        };
        assert!(text(zfs_hot_lines(&h)).contains("idle"));
    }
}
