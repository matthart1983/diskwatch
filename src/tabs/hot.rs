//! Hot Files tab — port of `dwRenderHot`, FSEvents-backed on macOS.
//!
//! What we have from the watcher: file path, event kind, event count per
//! path. Not bytes — neither FSEvents nor inotify carries them.
//!
//! The PROCESS column does not come from the watcher at all. It is a
//! join against `collect::processes`, which pairs each hot path with the
//! processes holding it open and picks the busiest. That is an inference
//! from two sampled readings, and unprivileged it only sees your own
//! uid, so the banner reports the coverage rather than letting a
//! half-empty column speak for itself.

use std::time::Instant;

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::app::App;
use crate::collect::hot_files::{ActivityKind, FileActivity};
use crate::collect::ProcessTick;
use crate::ui::format::{fmt_rate_compact, pad_left, pad_right};
use crate::ui::palette as p;

/// Width of the PROCESS column. Wide enough for `systemd-journald` plus
/// a five-digit pid, which is about as long as a real comm gets.
const PROC_W: usize = 22;

/// PATH takes whatever the other columns leave. It shrinks before the
/// process column does: a truncated path is still recognisable from its
/// tail, whereas half a process name is not an answer to anything.
///
/// No upper cap: a wide terminal should show more of the path rather than
/// a fixed-width column with empty space beside it (#12). 18 is still the
/// floor a narrow terminal needs to keep the row readable.
fn path_width(inner_w: u16) -> usize {
    // Mirrors `draw_row` span for span: 3 lead, gap, PROC_W, gap, rate 6,
    // gap, total 6, gap, age 4, gap, kind 6. Getting this wrong doesn't
    // wrap — it shears the last column off the right edge.
    const FIXED: usize = 3 + 2 + PROC_W + 2 + 6 + 2 + 6 + 2 + 4 + 2 + 6;
    (inner_w as usize).saturating_sub(FIXED).max(18)
}

/// Rows the table can show at `area` (the tab's whole content area, before
/// the summary/table/banner split below) — what `clamp_hot_scroll` needs to
/// keep the cursor's scroll window correct, computed the same way `draw`
/// arrives at the table's actual height.
pub fn visible_rows(app: &App, area: Rect) -> u16 {
    let table_h = area.height.saturating_sub(2 + banner_height(app));
    // 2 box border rows + the header line + its rule.
    table_h.saturating_sub(4)
}

pub fn draw(f: &mut Frame, area: Rect, app: &App) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),                  // summary line
            Constraint::Min(6),                     // table
            Constraint::Length(banner_height(app)), // banner
        ])
        .split(area);

    draw_summary(f, rows[0], app);
    draw_table(f, rows[1], app);
    draw_banner(f, rows[2], app);
}

/// The banner is three lines plus a border, and a fourth when part of the
/// process table is hidden. Sizing it statically clipped that last line,
/// which is the one line on the tab a user can act on.
fn banner_height(app: &App) -> u16 {
    if app.processes.coverage().is_partial() {
        6
    } else {
        5
    }
}

fn draw_summary(f: &mut Frame, area: Rect, app: &App) {
    let (total_events, roots, err) = app.hot_files.snapshot_meta();
    let active = {
        let s = app.hot_files.state.lock().unwrap();
        s.activity.len()
    };

    let mut spans = vec![
        Span::raw(" "),
        Span::styled("watch", Style::default().fg(p::dim())),
        Span::raw("  "),
    ];
    // Roots and errors are drawn together, not either/or. One bad path in
    // a configured list used to hide every good one behind its error,
    // which read as "nothing is being watched" when most of it was.
    if roots.is_empty() && app.hot_files.installing() {
        spans.push(Span::styled(
            "installing watches…",
            Style::default().fg(p::dim()),
        ));
    } else if roots.is_empty() {
        spans.push(Span::styled("(no roots)", Style::default().fg(p::dim())));
    } else {
        let joined: Vec<String> = roots.iter().map(|p| p.display().to_string()).collect();
        spans.push(Span::styled(
            joined.join("  "),
            Style::default().fg(p::fg()),
        ));
    }
    if let Some(e) = &err {
        spans.push(Span::raw("   "));
        spans.push(Span::styled(
            format!("ERROR: {}", e),
            Style::default().fg(p::red()).add_modifier(Modifier::BOLD),
        ));
    }
    spans.push(Span::raw("   "));
    spans.push(Span::styled(
        format!(
            "{} active paths  {} events since start",
            active, total_events
        ),
        Style::default().fg(p::dim()),
    ));
    // The busiest process overall, deliberately not filtered to
    // processes holding a hot path: the fd scan misses short-lived
    // writers, and this reading still catches them.
    //
    // Dropped rather than clipped below the width where it fits — a
    // half-written process name in the summary reads as a rendering bug.
    let busiest = if area.width >= 110 {
        app.processes.top(1).into_iter().next()
    } else {
        None
    };
    if let Some(busiest) = busiest {
        spans.push(Span::raw("   "));
        spans.push(Span::styled("busiest", Style::default().fg(p::dim())));
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!(
                "{} ({})  {}",
                busiest.name,
                busiest.pid,
                fmt_rate_compact(busiest.total_bps())
            ),
            Style::default().fg(p::fg()),
        ));
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

fn draw_table(f: &mut Frame, area: Rect, app: &App) {
    let total = app.hot_files.active_count();
    // 2 box border rows + header + rule — matches `visible_rows`, which
    // computes the same figure from the tab's outer content area instead
    // of this already-sliced table area.
    let visible = (area.height.saturating_sub(4) as usize).max(1);
    // Defensive: `app.hot_offset`/`hot_selected` are clamped after every key
    // press in `clamp_hot_scroll`, but the count can shrink between key
    // presses (decay, path removal) — clamp again here so a stale offset
    // can't slice past what's actually tracked.
    let offset = app.hot_offset.min(total.saturating_sub(1));

    let title = if total > visible {
        format!(
            " HOT FILES  by event rate  {}-{} of {} ",
            offset + 1,
            (offset + visible).min(total),
            total
        )
    } else {
        " HOT FILES  by event rate ".to_string()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p::faint()).bg(p::bg()))
        .title(Span::styled(
            title,
            Style::default().fg(p::cyan()).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(p::bg()));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height < 3 {
        return;
    }

    // The row is drawn at `inner.x + 1` with width `inner.width - 2`, so
    // that, not the block's inner width, is what the columns have to fit
    // inside. Handing this the wider figure sheared two characters off
    // the KIND column on every row while the header still fit.
    let path_w = path_width(inner.width.saturating_sub(2));
    let header = format!(
        "   {}  {}  {}  {}  {}  {}",
        pad_right("PATH", path_w),
        pad_right("PROCESS", PROC_W),
        pad_left("EV/s", 6),
        pad_left("TOTAL", 6),
        pad_left("AGE", 4),
        "KIND",
    );
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            header,
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

    // Sorted rank, not just this page: `top` truncates to `offset + visible`
    // so the busiest-overall ("leader") marker stays correct even once
    // scrolled away from rank 0.
    let ranked = app.hot_files.top(offset + visible);
    let page = ranked.get(offset..).unwrap_or(&[]);
    if page.is_empty() {
        let s = app.hot_files.state.lock().unwrap();
        let msg = if s.installing {
            "  installing watches…"
        } else if s.error.is_some() {
            "  watcher not running — see banner below"
        } else {
            "  waiting for filesystem activity…"
        };
        drop(s);
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(msg, Style::default().fg(p::dim()))))
                .style(Style::default().bg(p::bg())),
            Rect {
                x: inner.x + 1,
                y: inner.y + 2,
                width: inner.width.saturating_sub(2),
                height: 1,
            },
        );
        return;
    }

    let now = Instant::now();
    for (i, fa) in page.iter().take(visible).enumerate() {
        if i + 2 >= inner.height as usize {
            break;
        }
        let rank = offset + i;
        draw_row(
            f,
            inner.x + 1,
            inner.y + 2 + i as u16,
            inner.width.saturating_sub(2),
            fa,
            app.processes.likely_owner(&fa.path),
            path_w,
            now,
            rank == 0,
            rank == app.hot_selected,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_row(
    f: &mut Frame,
    x: u16,
    y: u16,
    w: u16,
    fa: &FileActivity,
    owner: Option<&ProcessTick>,
    path_w: usize,
    now: Instant,
    leader: bool,
    selected: bool,
) {
    let rate_str = if fa.events_per_sec >= 1.0 {
        format!("{:.1}", fa.events_per_sec)
    } else if fa.events_per_sec > 0.01 {
        format!("{:.2}", fa.events_per_sec)
    } else {
        "—".to_string()
    };
    let rate_color = if fa.events_per_sec >= 20.0 {
        p::yellow()
    } else if fa.events_per_sec >= 5.0 {
        p::br_cyan()
    } else if fa.events_per_sec >= 1.0 {
        p::fg()
    } else {
        p::dim()
    };
    let dot = if leader { p::yellow() } else { p::green() };
    let kind_color = match fa.last_kind {
        ActivityKind::Created => p::green(),
        ActivityKind::Modified => p::cyan(),
        ActivityKind::Removed => p::red(),
        ActivityKind::Renamed => p::magenta(),
        _ => p::dim(),
    };
    let path = display_path(&fa.path.display().to_string(), path_w);
    let age = age_label(now.duration_since(fa.last_seen));

    // The cursor's highlight, not the busiest-rate marker: `leader` still
    // drives the dot colour and the bold rate below, independently of
    // which row is currently selected.
    let row_bg = if selected { p::sel_bg() } else { p::bg() };
    f.render_widget(
        Paragraph::new("").style(Style::default().bg(row_bg)),
        Rect {
            x,
            y,
            width: w,
            height: 1,
        },
    );
    let line = Line::from(vec![
        Span::raw(" "),
        Span::styled("\u{25cf}", Style::default().fg(dot)),
        Span::raw(" "),
        Span::styled(pad_right(&path, path_w), Style::default().fg(p::fg())),
        Span::raw("  "),
        Span::styled(owner_label(owner), Style::default().fg(owner_color(owner))),
        Span::raw("  "),
        Span::styled(
            pad_left(&rate_str, 6),
            Style::default().fg(rate_color).add_modifier(if leader {
                Modifier::BOLD
            } else {
                Modifier::empty()
            }),
        ),
        Span::raw("  "),
        Span::styled(
            pad_left(&fa.total_events.to_string(), 6),
            Style::default().fg(p::dim()),
        ),
        Span::raw("  "),
        Span::styled(pad_left(&age, 4), Style::default().fg(p::dim())),
        Span::raw("  "),
        Span::styled(
            fa.last_kind.label().to_string(),
            Style::default().fg(kind_color),
        ),
    ]);
    f.render_widget(
        Paragraph::new(line).style(Style::default().bg(row_bg)),
        Rect {
            x,
            y,
            width: w,
            height: 1,
        },
    );
}

/// The column cell: a clipped `name (pid)`, or an em dash when nothing
/// visible holds the path open — which unprivileged does not mean nothing
/// does.
fn owner_label(owner: Option<&ProcessTick>) -> String {
    match owner {
        Some(o) => pad_right(&o.label(PROC_W), PROC_W),
        None => pad_right("—", PROC_W),
    }
}

/// A holder doing no measurable IO is dimmed rather than hidden. It is
/// still the best available answer, but it has not been corroborated by
/// a byte rate, and the colour is what says so.
fn owner_color(owner: Option<&ProcessTick>) -> ratatui::style::Color {
    match owner {
        Some(o) if o.total_bps() >= 1_000_000.0 => p::yellow(),
        Some(o) if o.total_bps() > 0.0 => p::fg(),
        Some(_) => p::dim(),
        None => p::faint(),
    }
}

fn draw_banner(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p::faint()).bg(p::bg()))
        .style(Style::default().bg(p::bg()));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let cov = app.processes.coverage();
    // Two separate caveats, and conflating them would be the easy
    // mistake: the watcher not carrying bytes is permanent, whereas the
    // process table being half-hidden is a privilege the user can change.
    let mut lines = vec![Line::from(vec![
        Span::styled(
            " note  ",
            Style::default()
                .fg(p::yellow())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "Rates are events/sec, not bytes — the watcher reports paths and kinds only.",
            Style::default().fg(p::fg()),
        ),
    ])];
    // Kept short deliberately: the banner does not wrap, and a line
    // longer than the narrowest terminal that reaches this tab is a line
    // whose last clause nobody ever reads.
    lines.push(Line::from(Span::styled(
        "       PROCESS is the busiest holder of the path, sampled every 2s.",
        Style::default().fg(p::dim()),
    )));
    lines.push(Line::from(Span::styled(
        "       Short-lived writers that close between samples are missed.",
        Style::default().fg(p::dim()),
    )));
    if cov.is_partial() {
        lines.push(Line::from(vec![
            Span::raw("       "),
            Span::styled(
                format!("{} of {} processes hidden", cov.hidden, cov.total()),
                Style::default().fg(p::yellow()),
            ),
            Span::styled(
                " — run as root to attribute them.",
                Style::default().fg(p::dim()),
            ),
        ]));
    }
    f.render_widget(
        Paragraph::new(lines).style(Style::default().bg(p::bg())),
        inner,
    );
}

fn display_path(s: &str, w: usize) -> String {
    let len = s.chars().count();
    if len <= w {
        return s.to_string();
    }
    // Right-truncate with leading ellipsis so the filename is visible.
    let tail: String = s.chars().skip(len - (w - 1)).collect();
    format!("…{}", tail)
}

fn age_label(d: std::time::Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{}s", s)
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else {
        format!("{}h", s / 3600)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collect::ProcessTick;

    fn proc(name: &str, pid: u32, bps: f64) -> ProcessTick {
        ProcessTick {
            pid,
            name: name.to_string(),
            read_bps: 0.0,
            write_bps: bps,
        }
    }

    /// PATH gives way first at the floor: at 80 columns the row still has
    /// to be readable. No ceiling — a wide terminal should get a wider
    /// PATH column rather than empty space beside a fixed-width one (#12).
    #[test]
    fn path_yields_width_to_the_process_column_but_not_all_of_it() {
        assert!(
            path_width(200) > path_width(120),
            "widens with the terminal"
        );
        assert!(path_width(80) >= 18, "a floor, not a collapse");
        // Every column must fit inside the width the row is given, or the
        // last one is sheared off. This is the invariant the header and
        // the row disagreed about.
        for w in 60u16..=200 {
            let row_len = 3 + path_width(w) + 2 + PROC_W + 2 + 6 + 2 + 6 + 2 + 4 + 2 + 6;
            if w as usize >= row_len {
                continue;
            }
            assert!(
                path_width(w) == 18,
                "at {w} cols the row overflows at path_w {}",
                path_width(w)
            );
        }
        // The saturating_sub underflow that would produce a giant width.
        assert_eq!(path_width(0), 18);
        assert_eq!(path_width(10), 18);
    }

    /// The pid survives clipping because it is what you act on; the name
    /// is the half that gets shortened.
    #[test]
    fn a_long_process_name_is_clipped_around_its_pid() {
        let long = proc("systemd-journald-with-a-silly-name", 1234, 0.0);
        let label = owner_label(Some(&long));
        assert_eq!(label.chars().count(), PROC_W);
        assert!(label.contains("(1234)"), "{label}");
        assert!(label.contains('…'), "{label}");
    }

    #[test]
    fn every_owner_label_fills_exactly_one_column_width() {
        for o in [
            Some(proc("vim", 9, 0.0)),
            Some(proc("postgres", 65535, 5_000_000.0)),
            None,
        ] {
            assert_eq!(owner_label(o.as_ref()).chars().count(), PROC_W);
        }
    }

    /// The colour is the honesty signal: a holder we could not corroborate
    /// with a byte rate must not look the same as one we could.
    #[test]
    fn an_uncorroborated_owner_is_dimmer_than_a_busy_one() {
        let busy = proc("dd", 1, 900_000_000.0);
        let trickle = proc("vim", 2, 4096.0);
        let idle = proc("tail", 3, 0.0);
        assert_ne!(owner_color(Some(&busy)), owner_color(Some(&idle)));
        assert_ne!(owner_color(Some(&trickle)), owner_color(Some(&idle)));
        assert_ne!(owner_color(Some(&idle)), owner_color(None));
    }
}
