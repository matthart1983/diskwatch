//! DiskWatch Lite — the minimal single-screen view.
//!
//! One screen at 80×24, six advertised keys, four hues. It is the
//! deliberate counterpart to the full TUI (8 tabs, 130×36): the question
//! it answers is "what's filling up my disk right now, and how long have
//! I got?"
//!
//! Entered with `--lite` or toggled with `L`. Opt-in only — the full TUI
//! stays the default at every terminal size.
//!
//! The constants below are **authoritative**. They are transcribed from
//! the design handoff (`design_handoff_diskwatch_lite`) with the
//! corrections recorded in
//! `~/Documents/diskwatch-lite-implementation-plan-2026-07-29.md`, and
//! they match NetWatch Lite's grid character-for-character — the three
//! Lites differ only in subject. The tests at the bottom lock the grid so
//! it cannot drift silently.
//!
//! Colours come from [`crate::ui::palette`], never hardcoded hex, so Lite
//! honours the active theme like every other view. The "four hues"
//! discipline is about how many tokens Lite uses, not which values it
//! burns in.
//!
//! ## What the table can honestly show
//! The handoff specifies `PROCESS · FILE · WRITE · TOTAL · LAT`. Four of
//! those five are not obtainable: FSEvents and inotify report *that* a
//! path changed, carrying neither the writing process nor the byte count
//! (see `collect::hot_files`). Rather than render four columns of `—`, we
//! keep the geometry exactly and label the columns with what is measured.
//! Per-device latency, which is real, lives in the detail block and is
//! labelled as device-level.

// The reference-grid constants (`CONTENT_*`, `FIELDS`, `ROW_PROMPT`, …)
// describe the 80×24 screen the handoff specifies. `Layout` generalises
// them, so at runtime most are read only by the tests asserting the two
// still agree at the reference size. That is the point of keeping them,
// not a sign they are dead.
#![allow(dead_code)]

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::app::{App, LiveState};
use crate::collect::growth::Growth;
use crate::ui::graph;
use crate::ui::palette as p;

// ── The grid ────────────────────────────────────────────────────────────────

/// The grid Lite is designed for. Below this, render the too-small notice
/// rather than a clipped layout.
pub const GRID_W: u16 = 80;
/// See [`GRID_W`].
pub const GRID_H: u16 = 24;

/// Content starts at col 1 — col 0 and col 79 are padding.
pub const CONTENT_X: u16 = 1;
/// Content width in columns.
pub const CONTENT_W: u16 = 78;
/// Last content column, inclusive.
pub const CONTENT_X_END: u16 = CONTENT_X + CONTENT_W - 1;

// ── Rows ────────────────────────────────────────────────────────────────────

pub const ROW_HEADER: u16 = 0;
pub const ROW_READ_LABEL: u16 = 2;
pub const ROW_READ_CHART: u16 = 3;
pub const READ_CHART_H: u16 = 3;
pub const ROW_WRITE_LABEL: u16 = 6;
pub const ROW_WRITE_CHART: u16 = 7;
pub const WRITE_CHART_H: u16 = 2;
pub const ROW_AXIS: u16 = 9;
pub const ROW_CAPACITY: u16 = 10;
pub const ROW_TABLE_HEAD: u16 = 12;
pub const ROW_RULE: u16 = 13;
/// First file row. The list runs to [`ROW_PROMPT`] - 1.
pub const ROW_FILES: u16 = 14;
/// File rows visible in the default state (rows 14..=21).
pub const FILE_ROWS: u16 = 8;
/// Filter prompt row; blank in every other mode.
pub const ROW_PROMPT: u16 = 22;
pub const ROW_FOOTER: u16 = 23;

/// The detail block renders directly beneath the selected row — not below
/// the whole list, which is what the handoff's reference renderer did.
pub const DETAIL_ROWS: u16 = 3;

// ── History ─────────────────────────────────────────────────────────────────

/// Per-row sparkline width. Each column is a *bucket max* over the
/// history — sampling instead would discard most of the series and hide
/// every spike, which for a write-runaway detector defeats the feature.
pub const SPARK_W: u16 = 9;

/// Samples covered by sparkline column `i`, as a half-open range.
pub fn spark_bucket(i: u16, samples: usize) -> std::ops::Range<usize> {
    let lo = (i as usize * samples) / SPARK_W as usize;
    let hi = ((i as usize + 1) * samples) / SPARK_W as usize;
    lo..hi.max(lo + 1)
}

// ── File table ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
}

#[derive(Debug, Clone, Copy)]
pub struct Field {
    pub header: &'static str,
    /// First column, inclusive.
    pub x: u16,
    pub w: u16,
    pub align: Align,
}

impl Field {
    /// Last column, inclusive.
    pub const fn x_end(&self) -> u16 {
        self.x + self.w - 1
    }
}

/// Hot-file table columns. Positions are identical to NetWatch Lite's
/// talker table — the family symmetry is geometric, so it survives the
/// relabelling forced by what FSEvents/inotify actually report. Verified
/// to tile without overlap and end exactly on [`CONTENT_X_END`].
pub const FIELDS: &[Field] = &[
    Field {
        header: "FILE",
        x: 1,
        w: 15,
        align: Align::Left,
    },
    Field {
        header: "PATH",
        x: 17,
        w: 22,
        align: Align::Left,
    },
    Field {
        header: "EV/S",
        x: 40,
        w: 10,
        align: Align::Right,
    },
    Field {
        header: "EVENTS",
        x: 51,
        w: 10,
        align: Align::Right,
    },
    Field {
        header: "KIND",
        x: 62,
        w: 7,
        align: Align::Right,
    },
    Field {
        header: "78s",
        x: 70,
        w: SPARK_W,
        align: Align::Left,
    },
];

/// Widths of the fixed-width fields, and the single blank column between them.
const W_FILE: u16 = 15;
const W_COUNT: u16 = 10;
const W_KIND: u16 = 7;
const FIELD_GAP: u16 = 1;

// ── Footer ──────────────────────────────────────────────────────────────────

/// Keys advertised in the footer.
///
/// Navigation (`↑`/`↓`/`j`/`k`) and `Esc` are deliberately absent: they
/// are conventions from `less`/`vim`/`top` and live in the `?` overlay
/// instead. The handoff claimed "five keybindings" while omitting any way
/// to move the selection, leave the filter, or reach the full TUI — this
/// is the honest set, and it matches NetWatch Lite.
pub const FOOTER_KEYS: &[(&str, &str)] = &[
    ("q", "quit"),
    ("p", "pause"),
    ("/", "filter"),
    ("↵", "detail"),
    ("L", "full"),
    ("?", "help"),
];

/// Blank columns between footer key pairs.
pub const FOOTER_GAP: u16 = 3;

/// Right-aligned footer version string.
pub fn footer_version() -> String {
    format!("diskwatch {}", env!("CARGO_PKG_VERSION"))
}

/// Rendered width of the footer key list, in columns.
pub fn footer_keys_width() -> u16 {
    let pairs: u16 = FOOTER_KEYS
        .iter()
        .map(|(k, label)| (k.chars().count() + 1 + label.chars().count()) as u16)
        .sum();
    pairs + FOOTER_GAP * (FOOTER_KEYS.len() as u16 - 1)
}

// ── Lite view state ─────────────────────────────────────────────────────────

/// Everything Lite remembers between frames.
#[derive(Debug, Default)]
pub struct LiteState {
    pub selected: usize,
    pub offset: usize,
    pub detail_open: bool,
    /// True while `/` is capturing keystrokes. A committed filter stays
    /// applied with this false.
    pub filter_input: bool,
    pub filter_text: String,
}

// ── Adaptive layout ─────────────────────────────────────────────────────────

/// Resolved geometry for the terminal Lite is actually running in.
///
/// The `FIELDS` constants describe the 80×24 reference grid; this
/// generalises them so Lite is a usable mode at any size rather than an
/// 80-column postage stamp in the corner of a wide terminal. PATH absorbs
/// surplus width (it is the field most often truncated, and the handoff's
/// 22 columns are far short of a real path) and the file list absorbs
/// surplus height.
///
/// At exactly 80×24 this reproduces `FIELDS` and the row constants
/// character-for-character — locked by `layout_at_reference_size_matches_spec`.
#[derive(Debug, Clone, Copy)]
pub struct Layout {
    pub content_x: u16,
    pub content_w: u16,
    pub x_file: u16,
    pub w_file: u16,
    pub x_path: u16,
    pub w_path: u16,
    pub x_rate: u16,
    pub x_total: u16,
    pub x_kind: u16,
    pub x_spark: u16,
    pub row_files: u16,
    /// File rows available with no detail block open.
    pub file_rows: u16,
    pub row_prompt: u16,
    pub row_footer: u16,
}

impl Layout {
    pub fn new(area: Rect) -> Self {
        // The key handler resolves a layout from the last drawn area,
        // which is 0×0 until the first frame — and some terminals report
        // 0×0 for a frame after `EnterAlternateScreen`. A keypress in
        // that window must not take the process down, so clamp to the
        // reference grid rather than doing unsigned arithmetic on zero.
        // Nothing is drawn from this fallback; only row counts are read.
        let area = Rect {
            x: area.x,
            y: area.y,
            width: area.width.max(GRID_W),
            height: area.height.max(GRID_H),
        };
        let content_x = area.x + 1;
        let content_w = area.width.saturating_sub(2);
        let x_end = content_x + content_w - 1;

        // Right-anchored fields, walking leftward from the content edge.
        let x_spark = x_end + 1 - SPARK_W;
        let x_kind = x_spark - FIELD_GAP - W_KIND;
        let x_total = x_kind - FIELD_GAP - W_COUNT;
        let x_rate = x_total - FIELD_GAP - W_COUNT;

        // Left-anchored, with PATH taking whatever is left in the middle.
        let x_file = content_x;
        let x_path = x_file + W_FILE + FIELD_GAP;
        let w_path = x_rate.saturating_sub(FIELD_GAP).saturating_sub(x_path);

        let row_footer = area.y + area.height - 1;
        let row_prompt = row_footer - 1;
        let row_files = area.y + ROW_FILES;

        Self {
            content_x,
            content_w,
            x_file,
            w_file: W_FILE,
            x_path,
            w_path,
            x_rate,
            x_total,
            x_kind,
            x_spark,
            row_files,
            file_rows: row_prompt.saturating_sub(row_files),
            row_prompt,
            row_footer,
        }
    }

    pub fn content_x_end(&self) -> u16 {
        self.content_x + self.content_w - 1
    }

    /// File rows available given whether the detail block is open.
    pub fn visible_files(&self, detail_open: bool) -> u16 {
        if detail_open {
            self.file_rows.saturating_sub(DETAIL_ROWS)
        } else {
            self.file_rows
        }
    }

    /// History depth the charts want: one sample per column, so the axis
    /// label is honest at any width. The handoff labelled the axis
    /// "60s ago" while drawing 78 columns, which duplicated ~30% of the
    /// samples.
    pub fn history_samples(&self) -> usize {
        self.content_w as usize
    }
}

// ── Rows ────────────────────────────────────────────────────────────────────

/// One row of the hot-file table.
pub struct HotRow {
    /// Basename of the changed path.
    pub file: String,
    /// Parent directory, rendered left-truncated.
    pub dir: String,
    /// Full path — detail view only.
    pub full_path: String,
    pub events_per_sec: f64,
    pub total_events: u64,
    pub kind: &'static str,
    pub secs_since_seen: u64,
    pub history: Vec<f64>,
    /// Readings ever pushed — lets a sparkline group them by absolute index
    /// rather than by ring position. See `FileActivity::pushed`.
    pub pushed: u64,
}

/// Snapshot the watcher into display rows, sorted by activity.
pub fn collect_rows(app: &App) -> Vec<HotRow> {
    let state = match app.hot_files.state.lock() {
        Ok(s) => s,
        // A poisoned mutex means the watcher thread panicked. Showing an
        // empty list is better than taking the UI down with it; the
        // prompt row explains that attribution is unavailable.
        Err(_) => return Vec::new(),
    };
    let now = std::time::Instant::now();

    let mut rows: Vec<HotRow> = state
        .activity
        .values()
        .map(|a| {
            let full_path = a.path.to_string_lossy().to_string();
            let file = a
                .path
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| full_path.clone());
            let dir = a
                .path
                .parent()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            HotRow {
                file,
                dir,
                full_path,
                events_per_sec: a.events_per_sec,
                total_events: a.total_events,
                kind: a.last_kind.label(),
                secs_since_seen: now.duration_since(a.last_seen).as_secs(),
                history: a.history.iter().copied().collect(),
                pushed: a.pushed,
            }
        })
        .collect();

    // Ties must break on something stable. Rates decay to exactly zero
    // on idle paths, and without a tiebreak the order comes from HashMap
    // iteration — the list reshuffles every tick and the selection lands
    // on a different file each frame.
    rows.sort_by(|a, b| {
        b.events_per_sec
            .partial_cmp(&a.events_per_sec)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.total_events.cmp(&a.total_events))
            .then_with(|| a.full_path.cmp(&b.full_path))
    });
    rows
}

/// Rows matching the active filter. Matches filename **or** path,
/// case-insensitively.
pub fn filter_rows(rows: Vec<HotRow>, query: &str) -> Vec<HotRow> {
    if query.is_empty() {
        return rows;
    }
    let q = query.to_lowercase();
    rows.into_iter()
        .filter(|r| r.full_path.to_lowercase().contains(&q))
        .collect()
}

// ── Capacity ────────────────────────────────────────────────────────────────

/// One mount's capacity state, as the capacity row needs it.
pub struct MountState {
    pub mount: String,
    pub used_pct: u32,
    pub growth: Option<Growth>,
}

/// Percentage at which a mount is "at risk" regardless of trend.
const AT_RISK_PCT: u32 = 90;
/// Days-until-full below which the projection turns red.
const AT_RISK_DAYS: f64 = 30.0;

fn used_pct(fs: &crate::collect::FsTick) -> u32 {
    if fs.size_bytes == 0 {
        return 0;
    }
    ((fs.used_bytes as f64 / fs.size_bytes as f64) * 100.0).round() as u32
}

/// The root mount, and the most-at-risk mount — not every mount.
///
/// "Most at risk" prefers the shortest projected time-to-full, falling
/// back to the highest usage when nothing has a trend yet. Returns
/// `(root, at_risk)`; `at_risk` is `None` when the root *is* the most at
/// risk, so the row never shows the same mount twice.
pub fn capacity_focus(app: &App) -> (Option<MountState>, Option<MountState>) {
    let state_of = |fs: &crate::collect::FsTick| MountState {
        mount: fs.mount.clone(),
        used_pct: used_pct(fs),
        growth: app.growth.growth(&fs.mount, fs.used_bytes, fs.size_bytes),
    };

    let real: Vec<&crate::collect::FsTick> = app
        .filesystems
        .iter()
        .filter(|f| f.size_bytes > 0)
        .collect();

    let root = real.iter().find(|f| f.mount == "/").map(|f| state_of(f));

    let at_risk = real
        .iter()
        .filter(|f| f.mount != "/")
        .map(|f| state_of(f))
        .min_by(|a, b| {
            let key = |m: &MountState| {
                m.growth
                    .and_then(|g| g.days_until_full)
                    .unwrap_or(f64::INFINITY)
            };
            key(a)
                .partial_cmp(&key(b))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.used_pct.cmp(&a.used_pct))
        })
        .filter(|m| {
            // Only worth a slot if it is actually at risk — otherwise
            // the row is padded with a healthy mount that says nothing.
            m.used_pct >= 70
                || m.growth
                    .and_then(|g| g.days_until_full)
                    .is_some_and(|d| d < AT_RISK_DAYS * 2.0)
        });

    (root, at_risk)
}

/// SMART roll-up: (healthy, reporting). Devices that report no verdict
/// are excluded from both, so `3/4` never silently means "one drive has
/// no SMART support".
pub fn health_rollup(app: &App) -> (usize, usize) {
    let reporting: Vec<bool> = app.devices.iter().filter_map(|d| d.smart_ok).collect();
    (reporting.iter().filter(|ok| **ok).count(), reporting.len())
}

/// True when Lite should show red: a mount is nearly full, is projected
/// to fill soon, or a device has failed SMART.
pub fn alerting(app: &App) -> Option<String> {
    let (healthy, reporting) = health_rollup(app);
    if reporting > 0 && healthy < reporting {
        return Some(format!(
            "{} of {reporting} devices degraded",
            reporting - healthy
        ));
    }
    let (root, at_risk) = capacity_focus(app);
    for m in [root, at_risk].into_iter().flatten() {
        if let Some(days) = m.growth.and_then(|g| g.days_until_full) {
            if days < AT_RISK_DAYS {
                return Some(format!("{} full in {}", m.mount, fmt_days(days)));
            }
        }
        if m.used_pct >= AT_RISK_PCT {
            return Some(format!("{} {}% full", m.mount, m.used_pct));
        }
    }
    None
}

// ── Formatting ──────────────────────────────────────────────────────────────

/// Split a rate into (value, unit) so callers can style them separately
/// and print the unit on `peak`/`avg` too. One decimal below 10, else
/// integer — `4.2 MB/s`, `880 KB/s`.
pub fn split_rate(bytes_per_sec: f64) -> (String, &'static str) {
    let (val, unit) = if bytes_per_sec >= 1e9 {
        (bytes_per_sec / 1e9, "GB/s")
    } else if bytes_per_sec >= 1e6 {
        (bytes_per_sec / 1e6, "MB/s")
    } else if bytes_per_sec >= 1e3 {
        (bytes_per_sec / 1e3, "KB/s")
    } else {
        (bytes_per_sec, "B/s")
    };
    let s = if val < 10.0 && val > 0.0 {
        format!("{val:.1}")
    } else {
        format!("{}", val.round() as u64)
    };
    (s, unit)
}

/// Signed bytes/day, for the growth field. Always carries its sign so a
/// shrinking filesystem is unmistakable.
pub fn fmt_growth(bytes_per_day: f64) -> String {
    let sign = if bytes_per_day < 0.0 { '-' } else { '+' };
    let (v, u) = split_rate(bytes_per_day.abs());
    // `split_rate` yields a per-second unit; the value is per-day.
    let unit = match u {
        "GB/s" => "GB",
        "MB/s" => "MB",
        "KB/s" => "KB",
        _ => "B",
    };
    format!("{sign}{v} {unit}/day")
}

/// Days-until-full, at a resolution the number deserves. Below a day,
/// hours; below an hour, "<1h" — projecting minutes from a ten-minute
/// window is false precision.
pub fn fmt_days(days: f64) -> String {
    if days >= 2.0 {
        format!("{:.0} days", days)
    } else if days >= 1.0 {
        "1 day".to_string()
    } else if days * 24.0 >= 1.0 {
        format!("{:.0}h", days * 24.0)
    } else {
        "<1h".to_string()
    }
}

/// Truncate to `w` display columns, appending `…` when it doesn't fit.
///
/// Width-aware rather than byte- or char-aware so CJK filenames don't
/// shear the columns to the right of them.
pub fn truncate_end(s: &str, w: u16) -> String {
    let w = w as usize;
    if s.width() <= w {
        return s.to_string();
    }
    if w <= 1 {
        return "…".into();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for ch in s.chars() {
        let cw = ch.to_string().width();
        if used + cw > w - 1 {
            break;
        }
        out.push(ch);
        used += cw;
    }
    out.push('…');
    out
}

/// Truncate a path from the **left**, keeping the tail.
///
/// Paths are longer than any column we can give them, and the tail is
/// the identifying part: `…/containers/overlay2/abc` tells you what you
/// are looking at, `/var/lib/docker/cont…` does not.
pub fn truncate_path(s: &str, w: u16) -> String {
    let wu = w as usize;
    if s.width() <= wu {
        return s.to_string();
    }
    if wu <= 1 {
        return "…".into();
    }
    // Walk from the right, keeping as much tail as fits after the ellipsis.
    let budget = wu - 1;
    let mut tail: Vec<char> = Vec::new();
    let mut used = 0usize;
    for ch in s.chars().rev() {
        let cw = ch.to_string().width();
        if used + cw > budget {
            break;
        }
        tail.push(ch);
        used += cw;
    }
    let tail: String = tail.into_iter().rev().collect();
    format!("…{tail}")
}

// ── Drawing primitives ──────────────────────────────────────────────────────

fn put(f: &mut Frame, x: u16, y: u16, s: &str, style: Style, clip_x_end: u16) {
    if x > clip_x_end {
        return;
    }
    let max = (clip_x_end - x + 1) as usize;
    f.buffer_mut().set_stringn(x, y, s, max, style);
}

/// Draw right-aligned so the string *ends* on `x_end`.
fn put_right(f: &mut Frame, x_end: u16, y: u16, s: &str, style: Style) {
    let w = s.width() as u16;
    let x = x_end.saturating_sub(w.saturating_sub(1));
    put(f, x, y, s, style, x_end);
}

/// Take the rightmost `width` samples, left-padding with zeros so `now`
/// always lands on the right edge even while the ring is warming.
fn tail_padded(data: &[f64], width: usize) -> Vec<f64> {
    if width == 0 {
        return Vec::new();
    }
    if data.len() >= width {
        return data[data.len() - width..].to_vec();
    }
    let mut out = vec![0.0; width - data.len()];
    out.extend_from_slice(data);
    out
}

/// Collapse a history of any length to exactly [`SPARK_W`] values, taking
/// the **max** of each bucket so a spike anywhere inside it survives.
fn bucket_history(data: &[f64]) -> Vec<f64> {
    if data.is_empty() {
        return Vec::new();
    }
    (0..SPARK_W)
        .map(|i| {
            let b = spark_bucket(i, data.len());
            data[b.start..b.end.min(data.len())]
                .iter()
                .copied()
                .fold(0.0_f64, f64::max)
        })
        .collect()
}

/// Both throughput charts, routed through the graph module so Lite
/// honours the app-wide bars/dots setting like every other chart.
fn chart(f: &mut Frame, area: Rect, samples: &[f64], color: Color) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let data = tail_padded(samples, area.width as usize);
    graph::render(f.buffer_mut(), area, &data, color, graph::opts());
}

// ── Render ──────────────────────────────────────────────────────────────────

pub fn render(f: &mut Frame, app: &App, area: Rect) {
    if area.width < GRID_W || area.height < GRID_H {
        render_too_small(f, area);
        return;
    }

    let l = Layout::new(area);
    let paused = matches!(app.live, LiveState::Paused);
    let alert = alerting(app);

    render_header(f, app, &l, alert.as_deref(), paused);
    render_throughput(f, app, &l, paused, alert.is_some());
    render_axis(f, &l);
    render_capacity(f, app, &l, alert.is_some());
    render_table(f, app, &l, paused);
    render_footer(f, &l);
}

fn render_too_small(f: &mut Frame, area: Rect) {
    // Lite has a hard floor: below it, a clipped grid is worse than a
    // sentence explaining the situation.
    let msg = format!(
        "diskwatch lite needs {GRID_W}×{GRID_H} — this terminal is {}×{}",
        area.width, area.height
    );
    let widget = Paragraph::new(vec![
        Line::from(Span::styled(msg, Style::default().fg(p::dim()))),
        Line::from(Span::styled(
            "resize, or press L for the full view",
            Style::default().fg(p::faint()),
        )),
    ]);
    f.render_widget(widget, area);
}

fn render_header(f: &mut Frame, app: &App, l: &Layout, alert: Option<&str>, paused: bool) {
    let y = ROW_HEADER;
    let end = l.content_x_end();

    let mut x = l.content_x;
    put(
        f,
        x,
        y,
        "diskwatch",
        Style::default().fg(p::fg()).add_modifier(Modifier::BOLD),
        end,
    );
    x += 11;
    put(
        f,
        x,
        y,
        &app.host.hostname,
        Style::default().fg(p::cyan()),
        end,
    );
    x += app.host.hostname.width() as u16;
    let devices = format!(
        " · {} device{}",
        app.host.device_count,
        if app.host.device_count == 1 { "" } else { "s" }
    );
    put(f, x, y, &devices, Style::default().fg(p::dim()), end);

    if paused {
        put_right(
            f,
            end,
            y,
            "◆ PAUSED",
            Style::default()
                .fg(p::yellow())
                .add_modifier(Modifier::BOLD),
        );
        return;
    }

    // The glyph changes shape as well as colour — red-vs-green is only
    // 1.43:1 against each other, so hue alone is not a signal.
    let (dot, text, color) = match alert {
        Some(reason) => ("▲", reason.to_string(), p::red()),
        None => {
            let used: u64 = app.devices.iter().map(|d| d.used_bytes).sum();
            let total: u64 = app.devices.iter().map(|d| d.size_bytes).sum();
            (
                "●",
                format!(
                    "{} / {}",
                    crate::ui::format::fmt_size(used),
                    crate::ui::format::fmt_size(total)
                ),
                p::green(),
            )
        }
    };
    let width = 2 + text.width() as u16;
    let sx = end + 1 - width.min(end - l.content_x);
    put(f, sx, y, dot, Style::default().fg(color), end);
    put(
        f,
        sx + 2,
        y,
        &text,
        Style::default().fg(if alert.is_some() { p::red() } else { p::dim() }),
        end,
    );
}

fn render_throughput(f: &mut Frame, app: &App, l: &Layout, paused: bool, alert: bool) {
    let end = l.content_x_end();
    let (read_now, write_now) = app.io.totals_bps();
    let want = l.history_samples();

    let read: Vec<f64> = app.io.agg.read_bps.iter().copied().collect();
    let write: Vec<f64> = app.io.agg.write_bps.iter().copied().collect();

    // A write runaway is what fills a disk, so the write chart is the one
    // that turns red. Paused greys the charts but leaves the rate numbers
    // legible — greying everything makes pause the least readable state,
    // which is backwards.
    let read_color = if paused { p::faint() } else { p::green() };
    let write_color = if paused {
        p::faint()
    } else if alert {
        p::red()
    } else {
        p::cyan()
    };

    for (glyph, label, y_label, y_chart, h, now, hist, color) in [
        (
            "r",
            "read",
            ROW_READ_LABEL,
            ROW_READ_CHART,
            READ_CHART_H,
            read_now,
            &read,
            read_color,
        ),
        (
            "w",
            "write",
            ROW_WRITE_LABEL,
            ROW_WRITE_CHART,
            WRITE_CHART_H,
            write_now,
            &write,
            write_color,
        ),
    ] {
        put(
            f,
            l.content_x,
            y_label,
            glyph,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
            end,
        );
        let (val, unit) = split_rate(now);
        put(
            f,
            l.content_x + 2,
            y_label,
            &val,
            Style::default()
                .fg(p::br_white())
                .add_modifier(Modifier::BOLD),
            end,
        );
        // The unit flows after the number. The handoff pinned it to a
        // fixed column that only worked for its two-digit fixture rate.
        put(
            f,
            l.content_x + 3 + val.width() as u16,
            y_label,
            &format!("{unit} {label}"),
            Style::default().fg(p::dim()),
            end,
        );

        // peak / avg carry their own units — the headline's unit can
        // differ (a write peaking in MB/s while current sits in KB/s).
        let window: Vec<f64> = hist.iter().rev().take(want).copied().collect();
        let ctx = if window.is_empty() {
            "peak —  avg —".to_string()
        } else {
            let peak = window.iter().copied().fold(0.0_f64, f64::max);
            let avg = window.iter().sum::<f64>() / window.len() as f64;
            let (pv, pu) = split_rate(peak);
            let (av, au) = split_rate(avg);
            format!("peak {pv} {pu}  avg {av} {au}")
        };
        put_right(f, end, y_label, &ctx, Style::default().fg(p::dim()));

        chart(
            f,
            Rect::new(l.content_x, y_chart, l.content_w, h),
            hist,
            color,
        );
    }
}

fn render_axis(f: &mut Frame, l: &Layout) {
    let rule: String = "─".repeat(l.content_w as usize);
    put(
        f,
        l.content_x,
        ROW_AXIS,
        &rule,
        Style::default().fg(p::faint()),
        l.content_x_end(),
    );
    // One sample per column, so the window is exactly as wide as the
    // chart — the label is derived, never a hardcoded "60s".
    let left = format!(" {}s ago ", l.history_samples());
    put(
        f,
        l.content_x,
        ROW_AXIS,
        &left,
        Style::default().fg(p::dim()),
        l.content_x_end(),
    );
    put_right(
        f,
        l.content_x_end(),
        ROW_AXIS,
        " now ",
        Style::default().fg(p::dim()),
    );
}

fn render_capacity(f: &mut Frame, app: &App, l: &Layout, alert: bool) {
    let end = l.content_x_end();
    let mut x = l.content_x;

    let (root, at_risk) = capacity_focus(app);
    let (healthy, reporting) = health_rollup(app);

    let mut pairs: Vec<(String, String, bool)> = Vec::new();
    for m in [&root, &at_risk].into_iter().flatten() {
        let bad = m.used_pct >= AT_RISK_PCT
            || m.growth
                .and_then(|g| g.days_until_full)
                .is_some_and(|d| d < AT_RISK_DAYS);
        pairs.push((m.mount.clone(), format!("{}%", m.used_pct), bad));
    }
    pairs.push((
        "health".into(),
        if reporting == 0 {
            "—".into()
        } else {
            format!("{healthy}/{reporting}")
        },
        reporting > 0 && healthy < reporting,
    ));
    // Growth reports on whichever mount the projection is about, so the
    // number and the verdict can't disagree.
    let focus = at_risk.as_ref().or(root.as_ref());
    let growth_val = focus
        .and_then(|m| m.growth)
        .map(|g| fmt_growth(g.bytes_per_day))
        // Nothing is projected before a minute of observation — a slope
        // measured over three seconds extrapolates to nonsense.
        .unwrap_or_else(|| "—".into());
    pairs.push(("growth".into(), growth_val, false));

    for (label, value, bad) in &pairs {
        put(
            f,
            x,
            ROW_CAPACITY,
            &format!("{label} "),
            Style::default().fg(p::dim()),
            end,
        );
        x += label.width() as u16 + 1;
        put(
            f,
            x,
            ROW_CAPACITY,
            value,
            Style::default().fg(if *bad { p::red() } else { p::fg() }),
            end,
        );
        x += value.width() as u16 + 3;
    }

    // The verdict: a projection beats a gauge. "11 days left" is
    // actionable in a way "94% full" is not.
    let (verdict, style) = match focus.and_then(|m| m.growth.and_then(|g| g.days_until_full)) {
        Some(days) if days < AT_RISK_DAYS => (
            format!(
                "{} {}% · {} left",
                focus.map(|m| m.mount.as_str()).unwrap_or("/"),
                focus.map(|m| m.used_pct).unwrap_or(0),
                fmt_days(days)
            ),
            Style::default().fg(p::red()),
        ),
        _ if alert => ("check capacity".to_string(), Style::default().fg(p::red())),
        // A status readout, not chrome — never render it on `faint`,
        // which is the lowest-contrast token in every theme.
        _ => ("all nominal".to_string(), Style::default().fg(p::dim())),
    };
    // Guard the collision the handoff's fixed layout ignores: a long
    // growth value plus a long projection overruns 78 columns.
    let verdict_x = end + 1 - (verdict.width() as u16).min(l.content_w);
    if verdict_x > x {
        put_right(f, end, ROW_CAPACITY, &verdict, style);
    }
}

fn render_table(f: &mut Frame, app: &App, l: &Layout, paused: bool) {
    let end = l.content_x_end();
    let head = Style::default().fg(p::dim());

    put(f, l.x_file, ROW_TABLE_HEAD, "FILE", head, end);
    put(f, l.x_path, ROW_TABLE_HEAD, "PATH", head, end);
    put_right(f, l.x_rate + W_COUNT - 1, ROW_TABLE_HEAD, "EV/S", head);
    put_right(f, l.x_total + W_COUNT - 1, ROW_TABLE_HEAD, "EVENTS", head);
    put_right(f, l.x_kind + W_KIND - 1, ROW_TABLE_HEAD, "KIND", head);
    put(
        f,
        l.x_spark,
        ROW_TABLE_HEAD,
        &format!("{}s", l.history_samples()),
        head,
        end,
    );

    let rule: String = "─".repeat(l.content_w as usize);
    put(
        f,
        l.content_x,
        ROW_RULE,
        &rule,
        Style::default().fg(p::faint()),
        end,
    );

    let lite = &app.lite;
    let rows = filter_rows(collect_rows(app), &lite.filter_text);

    if rows.is_empty() {
        let (_, _, err) = app.hot_files.snapshot_meta();
        let msg = if let Some(e) = err {
            // The homelab-without-privileges case is the target persona,
            // not an edge case: say what's missing rather than looking idle.
            format!("file activity unavailable — {e}")
        } else if !lite.filter_text.is_empty() {
            "no files match".to_string()
        } else {
            "no file activity in the watched roots".to_string()
        };
        put(
            f,
            l.content_x,
            l.row_files,
            &truncate_end(&msg, l.content_w),
            Style::default().fg(p::dim()),
            end,
        );
        render_prompt(f, app, l, 0);
        return;
    }

    let visible = l.visible_files(lite.detail_open) as usize;
    let offset = lite.offset.min(rows.len().saturating_sub(1));
    let sel = lite.selected.min(rows.len() - 1);

    let rate_style = |hot: bool| {
        Style::default().fg(if paused {
            p::faint()
        } else if hot {
            p::yellow()
        } else {
            p::cyan()
        })
    };

    let mut y = l.row_files;
    for (i, row) in rows.iter().enumerate().skip(offset).take(visible) {
        let selected = i == sel;
        if selected {
            for x in l.content_x..=end {
                if let Some(cell) = f.buffer_mut().cell_mut((x, y)) {
                    cell.set_bg(p::sel_bg());
                }
            }
        }
        // On the selected row, secondary text is promoted to primary:
        // `dim` on `sel_bg` is where the user is actually looking.
        let secondary = Style::default().fg(if selected { p::fg() } else { p::dim() });

        put(
            f,
            l.x_file,
            y,
            &truncate_end(&row.file, l.w_file),
            Style::default().fg(p::fg()),
            end,
        );
        put(
            f,
            l.x_path,
            y,
            &truncate_path(&row.dir, l.w_path),
            secondary,
            end,
        );
        put_right(
            f,
            l.x_rate + W_COUNT - 1,
            y,
            &format!("{:.1}", row.events_per_sec),
            rate_style(row.events_per_sec > 10.0),
        );
        put_right(
            f,
            l.x_total + W_COUNT - 1,
            y,
            &row.total_events.to_string(),
            Style::default().fg(if paused { p::faint() } else { p::green() }),
        );
        put_right(f, l.x_kind + W_KIND - 1, y, row.kind, secondary);

        let bucketed = bucket_history(&row.history);
        if !bucketed.is_empty() {
            // The row's own background has to carry through, or the
            // selected row's tint gets punched out by the sparkline.
            // Fade is off here regardless of the setting: nine columns
            // is too narrow for a gradient to read as anything but a
            // colour glitch.
            let opts = graph::GraphOpts {
                fade: false,
                bg: if selected { p::sel_bg() } else { p::bg() },
                ..graph::opts()
            };
            graph::render(
                f.buffer_mut(),
                Rect::new(l.x_spark, y, SPARK_W, 1),
                &bucketed,
                if selected { p::cyan() } else { p::faint() },
                opts,
            );
        }
        y += 1;

        // Detail renders directly beneath its own row — not below the
        // whole list, which is what the handoff's reference renderer did.
        if selected && lite.detail_open {
            render_detail(f, app, l, row, &mut y);
        }
    }

    render_prompt(f, app, l, rows.len());
}

fn render_detail(f: &mut Frame, app: &App, l: &Layout, row: &HotRow, y: &mut u16) {
    let end = l.content_x_end();
    let dim = Style::default().fg(p::dim());

    put(
        f,
        l.content_x + 2,
        *y,
        "└─",
        Style::default().fg(p::faint()),
        end,
    );
    // Even with the whole row to itself a real path can overrun. Clip
    // from the left like the table does, so the filename survives —
    // silently shearing the tail would hide the identifying part.
    put(
        f,
        l.content_x + 5,
        *y,
        &truncate_path(&row.full_path, l.content_w.saturating_sub(5)),
        dim,
        end,
    );
    *y += 1;

    let peak = row.history.iter().copied().fold(0.0_f64, f64::max);
    put(
        f,
        l.content_x + 5,
        *y,
        &format!(
            "events {}   last {}   {}s ago   peak {:.1}/s",
            row.total_events, row.kind, row.secs_since_seen, peak
        ),
        dim,
        end,
    );
    *y += 1;

    // Latency is per *device*, not per file — FSEvents/inotify carry no
    // per-file timing. Label it as such rather than implying otherwise.
    let lat = app
        .io
        .latest
        .iter()
        .find_map(|t| t.latency_pct.map(|pct| (t.device.clone(), pct)));
    let line = match lat {
        Some((dev, pct)) => format!(
            "device {dev}   p50 {:.1}ms  p99 {:.1}ms   (device-wide, not per file)",
            pct.p50_w / 1000.0,
            pct.p99_w / 1000.0
        ),
        None => "device latency unavailable".to_string(),
    };
    put(
        f,
        l.content_x + 5,
        *y,
        &truncate_end(&line, l.content_w),
        dim,
        end,
    );
    *y += 1;
}

/// The row above the footer: the filter prompt while editing, the active
/// filter once committed, otherwise the attribution caveat. Blank once
/// the user has read the caveat and started filtering.
fn render_prompt(f: &mut Frame, app: &App, l: &Layout, matched: usize) {
    let end = l.content_x_end();
    let lite = &app.lite;

    if lite.filter_input || !lite.filter_text.is_empty() {
        put(
            f,
            l.content_x,
            l.row_prompt,
            "/",
            Style::default()
                .fg(p::yellow())
                .add_modifier(Modifier::BOLD),
            end,
        );
        let q = &lite.filter_text;
        put(
            f,
            l.content_x + 2,
            l.row_prompt,
            q,
            Style::default().fg(p::fg()),
            end,
        );
        if lite.filter_input {
            put(
                f,
                l.content_x + 2 + q.width() as u16,
                l.row_prompt,
                "█",
                Style::default().fg(p::fg()),
                end,
            );
        }
        let note = if lite.filter_input {
            format!("{matched} match")
        } else {
            format!("{matched} match · esc clears")
        };
        // Right-aligned so a long query can't overwrite it. The handoff
        // pinned this to col 9, which its five-character fixture query
        // just happened to clear.
        put_right(f, end, l.row_prompt, &note, Style::default().fg(p::dim()));
        return;
    }

    // Say what the numbers are, because they are not what a reader of the
    // full tool's Hot Files tab might assume. FSEvents and inotify report
    // that a path changed, not who wrote it or how many bytes.
    put(
        f,
        l.content_x,
        l.row_prompt,
        &truncate_end(
            "file events, not bytes — process attribution needs elevated privileges",
            l.content_w,
        ),
        Style::default().fg(p::faint()),
        end,
    );
}

fn render_footer(f: &mut Frame, l: &Layout) {
    let end = l.content_x_end();
    let mut x = l.content_x;
    for (k, label) in FOOTER_KEYS {
        put(
            f,
            x,
            l.row_footer,
            k,
            Style::default().fg(p::cyan()).add_modifier(Modifier::BOLD),
            end,
        );
        x += k.width() as u16;
        put(
            f,
            x,
            l.row_footer,
            &format!(" {label}"),
            Style::default().fg(p::dim()),
            end,
        );
        x += 1 + label.width() as u16 + FOOTER_GAP;
    }
    put_right(
        f,
        end,
        l.row_footer,
        &footer_version(),
        Style::default().fg(p::faint()),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_tile_to_the_content_edge() {
        let mut prev_end = CONTENT_X - 1;
        for f in FIELDS {
            assert!(
                f.x > prev_end,
                "field {} starts at col {} but the previous field ends at {}",
                f.header,
                f.x,
                prev_end
            );
            prev_end = f.x_end();
        }
        assert_eq!(
            prev_end, CONTENT_X_END,
            "the last field must end exactly on the content edge"
        );
    }

    #[test]
    fn headers_fit_their_columns() {
        for f in FIELDS {
            assert!(
                f.header.width() as u16 <= f.w,
                "header {} needs {} cols but has {}",
                f.header,
                f.header.width(),
                f.w
            );
        }
    }

    #[test]
    fn layout_at_reference_size_matches_spec() {
        // The FIELDS constants describe the 80×24 reference grid; Layout
        // generalises them. At the reference size the two must agree
        // exactly, or the design handoff and the code have silently
        // diverged.
        let l = Layout::new(Rect::new(0, 0, GRID_W, GRID_H));
        assert_eq!(l.content_x, CONTENT_X);
        assert_eq!(l.content_w, CONTENT_W);
        assert_eq!(l.content_x_end(), CONTENT_X_END);

        assert_eq!(l.x_file, FIELDS[0].x);
        assert_eq!(l.w_file, FIELDS[0].w);
        assert_eq!(l.x_path, FIELDS[1].x);
        assert_eq!(l.w_path, FIELDS[1].w);
        assert_eq!(l.x_rate, FIELDS[2].x);
        assert_eq!(l.x_total, FIELDS[3].x);
        assert_eq!(l.x_kind, FIELDS[4].x);
        assert_eq!(l.x_spark, FIELDS[5].x);

        assert_eq!(l.row_files, ROW_FILES);
        assert_eq!(l.file_rows, FILE_ROWS);
        assert_eq!(l.row_prompt, ROW_PROMPT);
        assert_eq!(l.row_footer, ROW_FOOTER);
    }

    #[test]
    fn history_is_one_sample_per_chart_column() {
        // The handoff labelled the axis "60s ago" while drawing 78
        // columns, which duplicated ~30% of the samples. The window is
        // derived from the width at every size, so it cannot drift.
        for w in [GRID_W, 100, 160, 240] {
            let l = Layout::new(Rect::new(0, 0, w, GRID_H));
            assert_eq!(l.history_samples(), l.content_w as usize);
        }
    }

    #[test]
    fn layout_widens_the_path_column_only() {
        // Surplus width goes to PATH; every fixed field keeps its width
        // and stays right-anchored to the content edge.
        let l = Layout::new(Rect::new(0, 0, 160, 50));
        assert_eq!(l.w_file, W_FILE);
        assert_eq!(l.x_spark + SPARK_W - 1, l.content_x_end());
        assert_eq!(l.x_spark - (l.x_kind + W_KIND), FIELD_GAP);
        assert_eq!(l.x_kind - (l.x_total + W_COUNT), FIELD_GAP);
        assert_eq!(l.x_total - (l.x_rate + W_COUNT), FIELD_GAP);
        assert!(
            l.w_path > FIELDS[1].w,
            "PATH should absorb the surplus, got {}",
            l.w_path
        );
        // And the list absorbs surplus height.
        assert!(l.file_rows > FILE_ROWS);
    }

    #[test]
    fn spark_buckets_tile_the_history() {
        for samples in [1usize, 8, 9, 10, 60, 78, 300] {
            let mut prev_end = 0;
            for i in 0..SPARK_W {
                let b = spark_bucket(i, samples);
                assert!(b.start <= b.end, "bucket {i} inverted for {samples}");
                if i > 0 {
                    assert!(
                        b.start <= prev_end,
                        "gap before bucket {i} at {samples} samples"
                    );
                }
                prev_end = b.end;
            }
        }
    }

    #[test]
    fn bucket_history_keeps_spikes() {
        // A single spike in the middle of an otherwise flat series must
        // survive into the sparkline. Sampling would drop it.
        let mut data = vec![0.0; 78];
        data[40] = 99.0;
        let bucketed = bucket_history(&data);
        assert_eq!(bucketed.len() as u16, SPARK_W);
        assert!(bucketed.contains(&99.0), "the spike was lost: {bucketed:?}");
    }

    #[test]
    fn footer_fits_the_reference_grid() {
        let version = footer_version();
        let need = footer_keys_width() + 1 + version.width() as u16;
        assert!(
            need <= CONTENT_W,
            "footer needs {need} cols but the grid gives {CONTENT_W}"
        );
    }

    #[test]
    fn paths_truncate_from_the_left() {
        // The tail identifies the file; the head is boilerplate.
        let out = truncate_path("/var/lib/docker/overlay2/abc", 12);
        assert_eq!(out.width(), 12);
        assert!(out.starts_with('…'));
        assert!(out.ends_with("abc"), "kept the wrong end: {out}");
    }

    #[test]
    fn truncation_is_display_width_aware() {
        // Two double-width chars fill 4 columns; a char-based clamp would
        // shear every column to the right of this one.
        let out = truncate_end("日本語テキスト", 6);
        assert!(out.width() <= 6, "{out} is {} cols", out.width());
    }

    #[test]
    fn growth_formats_carry_a_sign_and_a_daily_unit() {
        assert_eq!(fmt_growth(2_400_000_000.0), "+2.4 GB/day");
        assert!(fmt_growth(-500_000_000.0).starts_with('-'));
    }

    #[test]
    fn days_are_reported_at_honest_resolution() {
        assert_eq!(fmt_days(11.4), "11 days");
        assert_eq!(fmt_days(1.2), "1 day");
        assert_eq!(fmt_days(0.5), "12h");
        assert_eq!(fmt_days(0.01), "<1h");
    }

    #[test]
    fn detail_block_fits_the_list() {
        // Three detail rows must leave at least one file row visible, or
        // opening detail hides the row it describes.
        let l = Layout::new(Rect::new(0, 0, GRID_W, GRID_H));
        assert!(l.visible_files(true) >= 1);
        assert_eq!(l.visible_files(true), FILE_ROWS - DETAIL_ROWS);
    }
}
