//! DiskWatch 2.0 — the btop-inspired single screen.
//!
//! Six boxes tile the terminal with **zero chrome rows**: no header bar, no
//! menu bar, no status bar. Identity, uptime, sort state, paging and every
//! keybind live inside the box borders, which a box spends anyway.
//!
//! ```text
//! ╭─┤1├─┤ io ├──       mirrored read/write, full width · axis row · vitals row
//! ╭─┤2├─┤ devices ├──  │  ╭─┤3├─┤ latency ├──  histogram
//! ╭─┤4├─┤ volumes ├──  │  ╭─┤5├─┤ smart ├──    device health
//! ╭─┤6├─┤ files ├──    hot files, detail-in-place
//! ```
//!
//! **The mirror is earned here.** In a CPU monitor a mirrored graph is
//! decoration, because compute has no opposing direction. Disk read/write *is*
//! two directions of one flow, so it takes the full width: a restore is a cliff
//! above the axis, a backup a cliff below it, and a database at work is roughly
//! symmetric about it.
//!
//! **The latency histogram is the disk-specific gem.** Bar length comes from
//! the bucket's count, but colour comes from the bucket's *position*, so the
//! right-hand buckets are red even when nearly empty — you learn where the tail
//! lives before it grows. A mean service time of 5ms can hide a p99 of 60, and
//! the tail is what users actually feel.
//!
//! # What is measured, and what is not
//!
//! Every number here is derived from the data it sits beside, and where a
//! platform can't measure something the field renders `--` in dim rather than
//! being filled with a plausible guess:
//!
//! - **Utilisation** is time-with-IO-in-flight, from `/proc/diskstats` field 13.
//!   macOS exposes summed service time instead, which on a deep-queue NVMe
//!   exceeds wall clock, so macOS reports no utilisation at all — and the
//!   devices box then sorts by throughput and *says so* in its sort indicator,
//!   rather than showing a sort that doesn't match its column.
//! - **The latency histogram** buckets per-tick mean service times weighted by
//!   that tick's op count. It is not a per-operation histogram; see
//!   [`crate::collect::io::IoTick::lat_hist`].
//! - **Hot files** are event rates from FSEvents/inotify, not per-file byte
//!   rates, which no OS exposes without tracing. The column is labelled
//!   `EVENTS/S` because that is what it is.
//! - **Stacked devices** (md, dm, LVM) are excluded from every system total:
//!   they sit on top of the physical devices, so counting both double-counts
//!   every block.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::Frame;

use crate::app::App;
use crate::collect::io::{HISTORY_SECS, LAT_BUCKETS, LAT_EDGES_MS, LAT_TAIL_FROM};
use crate::ui::braille::{self as br, Ramp};
use crate::ui::palette as p;

/// A device is "saturated" past this much utilisation.
const BUSY: f64 = 0.8;
/// Sparklines map into this band of the braille cell. A one-row cell resolves
/// only four levels, so a trace has to stay off both the ceiling and the floor
/// to carry any information at all.
const SPARK_BAND: (f64, f64) = (0.18, 0.92);

// ── state ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FileSort {
    #[default]
    Rate,
    Total,
    Name,
}

impl FileSort {
    pub fn next(self) -> Self {
        match self {
            FileSort::Rate => FileSort::Total,
            FileSort::Total => FileSort::Name,
            FileSort::Name => FileSort::Rate,
        }
    }
    fn label(self) -> &'static str {
        match self {
            FileSort::Rate => "events",
            FileSort::Total => "total",
            FileSort::Name => "name",
        }
    }
    /// Which column header lights up. Derived from the same value the sort
    /// uses, so the indicator and the order can't disagree.
    fn column(self) -> &'static str {
        match self {
            FileSort::Rate => "EVENTS/S",
            FileSort::Total => "TOTAL",
            FileSort::Name => "FILE",
        }
    }
}

#[derive(Debug, Default)]
pub struct DenseState {
    pub selected: usize,
    pub offset: usize,
    pub filter_input: bool,
    pub filter_text: String,
    pub sort: FileSort,
}

// ── derivations ────────────────────────────────────────────────────────────

/// Host-wide IO, summed across **physical** devices only.
pub struct Sys {
    pub read_bps: f64,
    pub write_bps: f64,
    pub iops_r: f64,
    pub iops_w: f64,
    /// Busiest member, not a mean: a queue backs up behind its slowest device,
    /// and an average reads below the device that is actually in trouble.
    pub util: Option<f64>,
    pub inflight: Option<u32>,
    pub hist: [u64; LAT_BUCKETS],
    pub busy: Vec<String>,
    pub phys: usize,
    pub stacked: Vec<String>,
}

/// True when this io device is a physical disk rather than a stack on top of
/// one. Device enumeration already excludes md/dm/LVM, so membership in the
/// enumerated set *is* the test — no second list to drift.
fn is_physical(app: &App, device: &str) -> bool {
    app.devices.iter().any(|d| d.name == device)
}

pub fn sys(app: &App) -> Sys {
    let mut s = Sys {
        read_bps: 0.0,
        write_bps: 0.0,
        iops_r: 0.0,
        iops_w: 0.0,
        util: None,
        inflight: None,
        hist: [0; LAT_BUCKETS],
        busy: Vec::new(),
        phys: 0,
        stacked: Vec::new(),
    };
    for t in &app.io.latest {
        if !is_physical(app, &t.device) {
            s.stacked.push(t.device.clone());
            continue;
        }
        s.phys += 1;
        let (r, w) = t.split.unwrap_or((0.0, 0.0));
        s.read_bps += r;
        s.write_bps += w;
        let (ir, iw) = t.iops.unwrap_or((0.0, 0.0));
        s.iops_r += ir;
        s.iops_w += iw;
        for (acc, v) in s.hist.iter_mut().zip(t.lat_hist.iter()) {
            *acc = acc.saturating_add(*v);
        }
        if let Some(u) = t.util {
            s.util = Some(s.util.map_or(u, |m: f64| m.max(u)));
            if u > BUSY {
                s.busy.push(t.device.clone());
            }
        }
        if let Some(q) = t.inflight {
            s.inflight = Some(s.inflight.unwrap_or(0) + q);
        }
    }
    s
}

/// Interpolated percentile over the histogram, in milliseconds.
///
/// `None` when nothing completed. An idle disk is not a zero-latency disk, and
/// every one of these walks divides by the count it just summed — printing
/// `NaN` across six fields at four in the morning is a worse failure than
/// printing `--`.
pub fn pct(hist: &[u64; LAT_BUCKETS], q: f64) -> Option<f64> {
    let total: u64 = hist.iter().sum();
    if total == 0 {
        return None;
    }
    let target = total as f64 * q;
    let mut cum = 0.0;
    for (i, &n) in hist.iter().enumerate() {
        // An empty bucket can satisfy the rank test when the target lands on a
        // boundary, and interpolating inside it divides by zero.
        if n > 0 && cum + n as f64 >= target {
            let (lo, hi) = bucket_edges(i);
            return Some(lo + (hi - lo) * ((target - cum) / n as f64));
        }
        cum += n as f64;
    }
    Some(bucket_edges(LAT_BUCKETS - 1).1)
}

/// Mean completion time. The honest source for "await", rather than a number
/// chosen independently of the distribution drawn right above it.
pub fn await_ms(hist: &[u64; LAT_BUCKETS]) -> Option<f64> {
    let total: u64 = hist.iter().sum();
    if total == 0 {
        return None;
    }
    let sum: f64 = hist
        .iter()
        .enumerate()
        .map(|(i, &n)| {
            let (lo, hi) = bucket_edges(i);
            (lo + hi) / 2.0 * n as f64
        })
        .sum();
    Some(sum / total as f64)
}

/// Ops past 10ms, and their share.
pub fn tail(hist: &[u64; LAT_BUCKETS]) -> (u64, Option<f64>) {
    let total: u64 = hist.iter().sum();
    let over: u64 = hist[LAT_TAIL_FROM..].iter().sum();
    if total == 0 {
        return (0, None);
    }
    (over, Some(over as f64 / total as f64 * 100.0))
}

/// Bucket `i`'s edges in ms. The last bucket is open-ended; the mean needs a
/// finite top, and 100ms is the value that cap takes — stated here rather than
/// buried in the arithmetic.
fn bucket_edges(i: usize) -> (f64, f64) {
    let lo = if i == 0 { 0.0 } else { LAT_EDGES_MS[i - 1] };
    let hi = if i < LAT_EDGES_MS.len() {
        LAT_EDGES_MS[i]
    } else {
        100.0
    };
    (lo, hi)
}

fn bucket_label(i: usize) -> String {
    let (lo, hi) = bucket_edges(i);
    if i == 0 {
        format!("<{}ms", trim(hi))
    } else if i == LAT_BUCKETS - 1 {
        format!(">{}ms", trim(lo))
    } else {
        format!("{}-{}", trim(lo), trim(hi))
    }
}

fn trim(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{v:.0}")
    } else {
        format!("{v}")
    }
}

// ── formatting ─────────────────────────────────────────────────────────────

/// Compact rate for a table cell: `408M`, `1.2G`, `8.2M`.
/// Base-10, like every other size in diskwatch — drives are sold that way.
fn rate_short(bps: f64) -> String {
    if !bps.is_finite() || bps < 1.0 {
        return "0".into();
    }
    if bps >= 1e9 {
        format!("{:.1}G", bps / 1e9)
    } else if bps >= 1e8 {
        format!("{:.0}M", bps / 1e6)
    } else if bps >= 1e6 {
        format!("{:.1}M", bps / 1e6)
    } else if bps >= 1e3 {
        format!("{:.0}K", bps / 1e3)
    } else {
        format!("{bps:.0}B")
    }
}

/// Rate with its unit spelled out, for the headline beside a graph.
fn rate_full(bps: f64) -> String {
    if !bps.is_finite() || bps < 1.0 {
        return "0 B/s".into();
    }
    if bps >= 1e9 {
        format!("{:.1} GB/s", bps / 1e9)
    } else if bps >= 1e6 {
        format!("{:.1} MB/s", bps / 1e6)
    } else if bps >= 1e3 {
        format!("{:.0} KB/s", bps / 1e3)
    } else {
        format!("{bps:.0} B/s")
    }
}

fn kcount(n: f64) -> String {
    if !n.is_finite() {
        return "--".into();
    }
    if n >= 10_000.0 {
        format!("{:.0}k", n / 1000.0)
    } else if n >= 1000.0 {
        format!("{:.1}k", n / 1000.0)
    } else {
        format!("{n:.0}")
    }
}

fn gsize(b: u64) -> String {
    const T: f64 = 1e12;
    const G: f64 = 1e9;
    let b = b as f64;
    if b >= T {
        format!("{:.1}T", b / T)
    } else if b >= G {
        format!("{:.0}G", b / G)
    } else {
        format!("{:.0}M", b / 1e6)
    }
}

fn ms_opt(v: Option<f64>) -> String {
    match v {
        None => "--".into(),
        Some(v) if v < 10.0 => format!("{v:.2}ms"),
        Some(v) => format!("{v:.1}ms"),
    }
}

fn pct_opt(v: Option<f64>, dp: usize) -> String {
    match v {
        None => "--".into(),
        Some(v) => format!("{v:.dp$}%"),
    }
}

/// Elapsed time, for "polled N ago".
fn age(secs: u64) -> String {
    if secs >= 3600 {
        format!("{}h", secs / 3600)
    } else if secs >= 120 {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

/// Seconds covered by `columns` of the graph ring.
///
/// The bug this exists to prevent: it is tempting to write a column count
/// where a second count is meant. That mislabelled a five-minute axis as two
/// minutes in v0.2.0.
fn span_secs(columns: usize) -> usize {
    columns * crate::collect::io::GRAPH_SAMPLE_MS / 1000
}

fn secs_label(n: usize) -> String {
    if n >= 120 {
        format!("{}m", n / 60)
    } else {
        format!("{n}s")
    }
}

// ── series ─────────────────────────────────────────────────────────────────

/// Take the newest `want` samples, oldest first, left-padding with zeros while
/// the ring is still warming so a fresh process draws an empty graph rather
/// than a stretched one.
fn window(ring: &std::collections::VecDeque<f64>, want: usize) -> Vec<f64> {
    let have = ring.len().min(want);
    let mut out = vec![0.0; want - have];
    out.extend(ring.iter().skip(ring.len() - have).copied());
    out
}

/// Expand one value per column into the two sub-columns `graph` reads, so both
/// light together and the fill is a solid area.
///
/// A braille cell is two sub-columns wide, and the obvious thing — feed it two
/// consecutive samples — is what the design handoff asked for. On real IO it is
/// wrong twice over. Disk throughput swings between a burst and nothing several
/// times a second, so the two samples inside a cell routinely differ by more
/// than 2x: one sub-column lights, the other doesn't, and the fill renders as a
/// comb of stripes rather than an area. netwatch shipped that exact bug and
/// fixed it in v0.28.0. And pairing at draw time re-pairs on every tick,
/// because the window slides by one sample — so every column changes value and
/// the graph shimmers instead of scrolling. The ring is decimated on the way in
/// instead; see `collect::io::GRAPH_SAMPLE_MS`.
fn subpixels(cols: &[f64]) -> Vec<f64> {
    cols.iter().flat_map(|&v| [v, v]).collect()
}

/// Normalise a series against a derived ceiling, returning (`0..1` values,
/// ceiling). The ceiling comes off the series' own measured peak, so an
/// auto-scaled graph uses the rows it was given instead of clipping or hugging
/// the baseline.
fn scaled(vals: &[f64]) -> (Vec<f64>, f64) {
    let peak = vals.iter().copied().fold(0.0_f64, f64::max);
    // The ladder works in MB/s so its rungs land on numbers an axis label can
    // print; convert once here rather than at every call site.
    let top_mb = br::nice_ceil(peak / 1e6);
    let top = top_mb * 1e6;
    (
        vals.iter()
            .map(|v| {
                if top > 0.0 {
                    (v / top).clamp(0.0, 1.0)
                } else {
                    0.0
                }
            })
            .collect(),
        top,
    )
}

/// Map a value onto [`SPARK_BAND`] against ABSOLUTE limits — never the set's
/// own min and max. A set-relative level makes the busiest row hit the band top
/// whatever it is doing, so a screen under load renders identically to an idle
/// one and the sparkline stops carrying magnitude at all.
fn band(v: f64, lo: f64, hi: f64, log: bool) -> f64 {
    let f = if log {
        let (v, lo, hi) = (v.max(1e-3), lo.max(1e-3), hi.max(1e-3));
        (v.log10() - lo.log10()) / (hi.log10() - lo.log10()).max(f64::EPSILON)
    } else {
        (v - lo) / (hi - lo).max(f64::EPSILON)
    };
    SPARK_BAND.0 + (SPARK_BAND.1 - SPARK_BAND.0) * f.clamp(0.0, 1.0)
}

/// Squeeze the last [`HISTORY_SECS`] of a ring into `cols` columns, grouping
/// samples by their ABSOLUTE index so a group's membership never changes.
///
/// `pushed` is how many samples the ring has ever seen. Grouping by position
/// within the ring instead re-groups every sample the moment one ages out —
/// which is what made the main graph shimmer between two shapes rather than
/// scroll, and what the sparklines were still doing at 5Hz. Anchoring to
/// absolute index means a column keeps its value until it scrolls off the left
/// edge, and a new column appears once every `per` samples.
///
/// `sample_ms` is the ring's own cadence, so every caller covers the same
/// wall-clock window whatever rate it samples at.
fn condense(history: &[f64], pushed: u64, cols: usize, sample_ms: usize) -> Vec<f64> {
    if cols == 0 || sample_ms == 0 {
        return Vec::new();
    }
    let want = (HISTORY_SECS * 1000 / sample_ms).max(cols);
    let per = (want / cols).max(1) as u64;
    let first = pushed.saturating_sub(history.len() as u64);
    let last_group = pushed.saturating_sub(1) / per;
    let mut out = Vec::with_capacity(cols);
    for c in 0..cols as u64 {
        let Some(group) = last_group.checked_sub(cols as u64 - 1 - c) else {
            out.push(0.0);
            continue;
        };
        let (lo, hi) = (group * per, group * per + per);
        let (mut sum, mut n) = (0.0, 0u32);
        for abs in lo..hi {
            if abs < first || abs >= pushed {
                continue;
            }
            sum += history[(abs - first) as usize];
            n += 1;
        }
        out.push(if n == 0 { 0.0 } else { sum / n as f64 });
    }
    out
}

/// Normalise a row sparkline's history against an absolute ceiling, then emit
/// one value per column into both braille sub-columns.
fn spark_vals(
    history: &[f64],
    pushed: u64,
    ceiling: f64,
    log: bool,
    cols: usize,
    sample_ms: usize,
) -> Vec<f64> {
    let cond: Vec<f64> = condense(history, pushed, cols, sample_ms)
        .into_iter()
        .map(|v| {
            if v <= 0.0 {
                0.0
            } else {
                band(v, 0.001, ceiling, log)
            }
        })
        .collect();
    subpixels(&cond)
}

// ── layout ─────────────────────────────────────────────────────────────────

/// The six boxes, resolved for the terminal actually in use.
///
/// The design grid is 130×44. Rather than demand it, each box keeps the height
/// it needs and `files` absorbs the slack — the same way btop gives its process
/// list whatever the graphs don't use. Below the full layout's minimum the
/// screen falls back to the compact one, which is the 80×24 design.
pub struct Layout {
    pub io: Rect,
    pub devices: Rect,
    pub latency: Rect,
    pub volumes: Rect,
    pub smart: Rect,
    pub files: Rect,
    /// Rows the read graph gets; the write graph gets one fewer, exactly as in
    /// the design — reads are the direction with more shape to show.
    pub read_rows: u16,
}

/// Smallest terminal the six-box layout fits in.
pub const MIN_FULL_W: u16 = 104;
pub const MIN_FULL_H: u16 = 32;
/// Smallest terminal the compact layout fits in.
pub const MIN_W: u16 = 60;
pub const MIN_H: u16 = 16;

impl Layout {
    pub fn new(area: Rect) -> Self {
        // io = borders + read rows + axis + write rows + two headlines + vitals.
        let read_rows: u16 = if area.height >= 40 { 4 } else { 3 };
        let io_h = 2 + 1 + read_rows + 1 + (read_rows - 1) + 1 + 1;
        let mid_h: u16 = 12.min(area.height / 4).max(8);
        let low_h: u16 = 8.min(area.height / 5).max(6);
        let files_h = area.height.saturating_sub(io_h + mid_h + low_h).max(5);
        // Devices takes the wider half: its table carries seven columns, the
        // histogram four.
        let left_w = (area.width as u32 * 66 / 130) as u16;
        let right_w = area.width - left_w;
        let y0 = area.y;
        Layout {
            io: Rect::new(area.x, y0, area.width, io_h),
            devices: Rect::new(area.x, y0 + io_h, left_w, mid_h),
            latency: Rect::new(area.x + left_w, y0 + io_h, right_w, mid_h),
            volumes: Rect::new(area.x, y0 + io_h + mid_h, left_w, low_h),
            smart: Rect::new(area.x + left_w, y0 + io_h + mid_h, right_w, low_h),
            files: Rect::new(area.x, y0 + io_h + mid_h + low_h, area.width, files_h),
            read_rows,
        }
    }

    /// File rows on screen: the box interior less its header row.
    pub fn visible_files(&self) -> u16 {
        self.files.height.saturating_sub(3)
    }
}

/// Rows the file list can show at `area`, whichever layout that area selects.
pub fn visible_files(area: Rect) -> u16 {
    if area.width >= MIN_FULL_W && area.height >= MIN_FULL_H {
        Layout::new(area).visible_files()
    } else {
        area.height.saturating_sub(11).max(1)
    }
}

// ── entry point ────────────────────────────────────────────────────────────

pub fn render(f: &mut Frame, area: Rect, app: &App) {
    let buf = f.buffer_mut();
    // Paint the ground first: the boxes tile the whole screen, and any cell
    // they don't reach should still be the view's background rather than
    // whatever the previous view left there.
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_bg(p::bg());
            }
        }
    }
    if area.width < MIN_W || area.height < MIN_H {
        br::text(
            buf,
            area.x,
            area.y,
            &format!("terminal too small — need {MIN_W}x{MIN_H}"),
            p::dim(),
            false,
        );
        return;
    }
    if area.width >= MIN_FULL_W && area.height >= MIN_FULL_H {
        render_full(buf, area, app);
    } else {
        render_compact(buf, area, app);
    }
}

fn render_full(buf: &mut Buffer, area: Rect, app: &App) {
    let l = Layout::new(area);
    let s = sys(app);
    io_box(buf, l.io, app, &s, l.read_rows);
    devices_box(buf, l.devices, app, &s);
    latency_box(buf, l.latency, &s);
    volumes_box(buf, l.volumes, app);
    smart_box(buf, l.smart, app);
    files_box(buf, l.files, app, true);
}

// ── io box ─────────────────────────────────────────────────────────────────

fn io_box(buf: &mut Buffer, area: Rect, app: &App, s: &Sys, read_rows: u16) {
    let hot = s.util.map(|u| u > BUSY).unwrap_or(false);
    let p99 = pct(&s.hist, 0.99);
    let awaited = await_ms(&s.hist);
    let (over, tail_pct) = tail(&s.hist);

    let sub = format!("{} physical · {} volumes", s.phys, app.filesystems.len());
    let right = format!(
        "diskwatch {}   {}   up {}",
        env!("CARGO_PKG_VERSION"),
        app.host.hostname,
        uptime(app.host.uptime_secs)
    );
    let foot_r = match (s.inflight, s.util) {
        (Some(q), Some(u)) => format!("inflight {q} · {}% util", (u * 100.0).round()),
        (Some(q), None) => format!("inflight {q}"),
        (None, Some(u)) => format!("{}% util", (u * 100.0).round()),
        (None, None) => "utilisation unavailable on this platform".into(),
    };
    br::draw_box(
        buf,
        area,
        &br::BoxOpts {
            key: Some("1"),
            title: Some("io"),
            sub: Some(&sub),
            right: Some(&right),
            right_fg: Some(if hot { p::red() } else { p::dim() }),
            foot_l: &[("V", " view"), (",", " settings")],
            foot_r: Some(&foot_r),
            ..Default::default()
        },
    );

    // Geometry: a 6-column value axis on the left, a 1-column gutter on the
    // right, and the graph fills what is left.
    let gx = area.x + 8;
    let gw = area.width.saturating_sub(10);
    if gw < 8 {
        return;
    }
    // One ring entry per column, so a new reading scrolls the graph by exactly
    // one column and every other column keeps the value it had.
    let want = gw as usize;
    let read = window(&app.io.agg.read_bps_graph, want);
    let write = window(&app.io.agg.write_bps_graph, want);
    let (rv, rtop) = scaled(&subpixels(&read));
    let (wv, wtop) = scaled(&subpixels(&write));
    let cur_r = read.last().copied().unwrap_or(0.0);
    let cur_w = write.last().copied().unwrap_or(0.0);
    let peak_r = read.iter().copied().fold(0.0_f64, f64::max);
    let peak_w = write.iter().copied().fold(0.0_f64, f64::max);
    let avg = |v: &[f64]| {
        if v.is_empty() {
            0.0
        } else {
            v.iter().sum::<f64>() / v.len() as f64
        }
    };

    let ry = area.y + 2;
    let ay = ry + read_rows;
    let write_rows = read_rows - 1;

    // read headline
    let y = area.y + 1;
    br::text(buf, area.x + 2, y, "r", p::green(), true);
    let rf = rate_full(cur_r);
    let (num, unit) = rf.split_once(' ').unwrap_or((rf.as_str(), ""));
    let mut cx = br::text(buf, area.x + 4, y, num, p::br_white(), true);
    cx = br::text(buf, cx + 1, y, unit, p::dim(), false);
    cx = br::text(buf, cx + 2, y, "read", p::dim(), false);
    let _ = cx;
    br::text(
        buf,
        area.x + 28,
        y,
        &format!("peak {}", rate_short(peak_r)),
        p::dim(),
        false,
    );
    br::text(
        buf,
        area.x + 40,
        y,
        &format!("avg {}", rate_short(avg(&read))),
        p::dim(),
        false,
    );
    br::text(
        buf,
        area.x + 51,
        y,
        &format!("iops {}", kcount(s.iops_r)),
        p::dim(),
        false,
    );
    // Mean request size, from the two numbers beside it. It is the cheapest
    // read of whether a workload is sequential or random, and it costs a
    // division of figures already on the row.
    br::text_right(
        buf,
        area.right() - 3,
        y,
        &format!(
            "avg req {} · {}",
            req_size(s.read_bps, s.iops_r),
            topology_note(s)
        ),
        p::dim(),
        false,
    );

    // read graph, growing up from the axis
    br::text_right(buf, area.x + 5, ry, &rate_short(rtop), p::faint(), false);
    br::text(buf, area.x + 6, ry, "┤", p::faint(), false);
    br::text_right(buf, area.x + 5, ay - 1, "0", p::faint(), false);
    br::text(buf, area.x + 6, ay - 1, "┤", p::faint(), false);
    br::graph(
        buf,
        Rect::new(gx, ry, gw, read_rows),
        &rv,
        Ramp::Read,
        false,
        None,
    );

    // Shared time axis, alone on its row so every tick sits at its true
    // position. The span is DERIVED from how many samples are actually on
    // screen — a wider terminal shows more history, and the label says so.
    br::rule(buf, gx, ay, gw);
    let span = secs_label(span_secs(want));
    br::text_right(buf, area.x + 5, ay, &span, p::dim(), false);
    br::text(buf, area.x + 6, ay, "┤", p::faint(), false);
    let mid = format!("┤ {} ├", secs_label(span_secs(want) / 2));
    br::text(
        buf,
        gx + gw / 2 - (mid.chars().count() as u16) / 2,
        ay,
        &mid,
        p::dim(),
        false,
    );
    br::text_right(buf, area.right() - 3, ay, "┤ now ├", p::dim(), false);

    // write graph, mirrored: grows downward from the same axis
    br::graph(
        buf,
        Rect::new(gx, ay + 1, gw, write_rows),
        &wv,
        Ramp::Write,
        true,
        None,
    );
    br::text_right(buf, area.x + 5, ay + 1, "0", p::faint(), false);
    br::text(buf, area.x + 6, ay + 1, "┤", p::faint(), false);
    br::text_right(
        buf,
        area.x + 5,
        ay + write_rows,
        &rate_short(wtop),
        p::faint(),
        false,
    );
    br::text(buf, area.x + 6, ay + write_rows, "┤", p::faint(), false);

    let wy = ay + write_rows + 1;
    br::text(buf, area.x + 2, wy, "w", p::cyan(), true);
    let wf = rate_full(cur_w);
    let (num, unit) = wf.split_once(' ').unwrap_or((wf.as_str(), ""));
    let mut cx = br::text(buf, area.x + 4, wy, num, p::br_white(), true);
    cx = br::text(buf, cx + 1, wy, unit, p::dim(), false);
    br::text(buf, cx + 2, wy, "write", p::dim(), false);
    br::text(
        buf,
        area.x + 28,
        wy,
        &format!("peak {}", rate_short(peak_w)),
        p::dim(),
        false,
    );
    br::text(
        buf,
        area.x + 40,
        wy,
        &format!("avg {}", rate_short(avg(&write))),
        p::dim(),
        false,
    );
    br::text(
        buf,
        area.x + 51,
        wy,
        &format!("iops {}", kcount(s.iops_w)),
        p::dim(),
        false,
    );

    // vitals — bounded values only, all on the green→red ramp
    let vy = wy + 1;
    let val_fg = if hot { p::red() } else { p::fg() };
    br::text(buf, area.x + 2, vy, "vitals", p::dim(), false);
    br::text(buf, area.x + 10, vy, "util", p::dim(), false);
    match s.util {
        Some(u) => {
            br::meter(buf, area.x + 15, vy, 16, u, Ramp::Load);
            br::text(
                buf,
                area.x + 33,
                vy,
                &crate::ui::format::pad_left(&format!("{}%", (u * 100.0).round()), 4),
                val_fg,
                true,
            );
        }
        None => {
            br::meter_unavailable(buf, area.x + 15, vy, 16, false);
            br::text(buf, area.x + 33, vy, "  --", p::dim(), false);
        }
    }
    br::text(buf, area.x + 39, vy, "await", p::dim(), false);
    br::text(
        buf,
        area.x + 45,
        vy,
        &crate::ui::format::pad_left(&ms_opt(awaited), 7),
        val_fg,
        true,
    );
    br::text(buf, area.x + 54, vy, "iops", p::dim(), false);
    let left_end = br::text(
        buf,
        area.x + 59,
        vy,
        &kcount(s.iops_r + s.iops_w),
        p::fg(),
        false,
    );
    let headroom = match s.util {
        Some(u) if u > BUSY => "queue saturated".to_string(),
        Some(u) => format!("headroom {}%", ((1.0 - u) * 100.0).round()),
        None => format!("{} ops sampled", kcount(s.hist.iter().sum::<u64>() as f64)),
    };
    // Right-aligned against a left-hand writer on the same row, so it is
    // measured against where that writer actually ended. Progressively shorter
    // versions instead of one truncated one: a clause that doesn't fit is
    // dropped whole, so what remains is always a complete sentence.
    let avail = (area.right() - 3).saturating_sub(left_end + 1);
    let long = format!(
        "p99 {} · {} ops over 10ms · {}",
        ms_opt(p99),
        kcount(over as f64),
        headroom
    );
    let mid = format!("p99 {} · {}", ms_opt(p99), headroom);
    let short = format!("p99 {}", ms_opt(p99));
    let vitals_note = [long, mid, short]
        .into_iter()
        .find(|s| s.chars().count() as u16 <= avail);
    if let Some(note) = vitals_note {
        br::text_right(
            buf,
            area.right() - 3,
            vy,
            &note,
            if hot { p::red() } else { p::dim() },
            false,
        );
    }
    let _ = tail_pct;
}

/// Mean bytes per operation. `--` when nothing completed, rather than a zero
/// that reads as "tiny requests".
fn req_size(bps: f64, iops: f64) -> String {
    if iops <= 0.0 || !bps.is_finite() {
        return "--".into();
    }
    rate_short(bps / iops).trim_end_matches("B").to_string() + "B"
}

/// How the physical/stacked split is described, in the plural the count needs.
fn topology_note(s: &Sys) -> String {
    let disks = if s.phys == 1 { "disk" } else { "disks" };
    if s.stacked.is_empty() {
        format!("{} {disks} summed", s.phys)
    } else {
        format!(
            "{} {disks} summed · {} stacked, excluded",
            s.phys,
            s.stacked.len()
        )
    }
}

fn uptime(secs: u64) -> String {
    let d = secs / 86_400;
    let h = (secs % 86_400) / 3600;
    let m = (secs % 3600) / 60;
    if d > 0 {
        format!("{d}d {h:02}:{m:02}")
    } else {
        format!("{h:02}:{m:02}")
    }
}

// ── devices box ────────────────────────────────────────────────────────────

struct DevRow {
    name: String,
    kind: String,
    size: u64,
    read: f64,
    write: f64,
    util: Option<f64>,
    stacked: bool,
    history: Vec<f64>,
    pushed: u64,
}

fn dev_rows(app: &App) -> Vec<DevRow> {
    let mut rows: Vec<DevRow> = app
        .io
        .latest
        .iter()
        .map(|t| {
            let (r, w) = t.split.unwrap_or((0.0, 0.0));
            let dev = app.devices.iter().find(|d| d.name == t.device);
            DevRow {
                name: t.device.clone(),
                kind: dev
                    .map(|d| format!("{:?}", d.kind).to_lowercase())
                    .unwrap_or_else(|| "stack".into()),
                size: dev.map(|d| d.size_bytes).unwrap_or(0),
                read: r,
                write: w,
                util: t.util,
                stacked: dev.is_none(),
                history: app
                    .io
                    .history
                    .get(&t.device)
                    .map(|h| h.combined.iter().copied().collect())
                    .unwrap_or_default(),
                pushed: app.io.history.get(&t.device).map(|h| h.pushed).unwrap_or(0),
            }
        })
        .collect();
    // Sort by whatever is actually measurable. The box's indicator is written
    // from the same choice, so it can never advertise a sort it didn't do.
    if rows.iter().any(|r| r.util.is_some()) {
        rows.sort_by(|a, b| {
            b.util
                .unwrap_or(0.0)
                .partial_cmp(&a.util.unwrap_or(0.0))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    } else {
        rows.sort_by(|a, b| {
            (b.read + b.write)
                .partial_cmp(&(a.read + a.write))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }
    rows
}

/// One table column: label, start offset inside the box, width, alignment.
///
/// Headers and data are drawn from the same table, so a label can't drift from
/// its column and the sort highlight can't land on the wrong one. The label
/// borrows rather than being `'static` so a column whose header states a
/// measured span — `48s`, `5m` — can compute it from the ring it draws instead
/// of restating it as a literal that goes stale.
struct Col<'a> {
    lab: &'a str,
    x: u16,
    w: u16,
    right: bool,
}

fn header(buf: &mut Buffer, x0: u16, y: u16, cols: &[Col], sort_col: &str) {
    for c in cols {
        let s = if c.right {
            crate::ui::format::pad_left(c.lab, c.w as usize)
        } else {
            crate::ui::format::pad_right(c.lab, c.w as usize)
        };
        let is_sort = c.lab == sort_col;
        br::text(
            buf,
            x0 + c.x,
            y,
            &s,
            if is_sort { p::cyan() } else { p::dim() },
            is_sort,
        );
    }
}

fn devices_box(buf: &mut Buffer, area: Rect, app: &App, s: &Sys) {
    let rows = dev_rows(app);
    let by_util = rows.iter().any(|r| r.util.is_some());
    let sub = format!("{} · {} physical", rows.len(), s.phys);
    let foot = if !by_util {
        "utilisation needs /proc/diskstats".to_string()
    } else if s.busy.is_empty() {
        format!("no device over {}%", (BUSY * 100.0) as u32)
    } else {
        format!("{} saturated", s.busy.join(", "))
    };
    let inner = br::draw_box(
        buf,
        area,
        &br::BoxOpts {
            key: Some("2"),
            title: Some("devices"),
            sub: Some(&sub),
            right: Some(if by_util {
                "sort ↓ util"
            } else {
                "sort ↓ throughput"
            }),
            foot_r: Some(&foot),
            ..Default::default()
        },
    );

    // The sparkline takes what the fixed columns leave, less a one-column
    // gutter. Anchoring to the right edge is the same rule the design applies
    // to every pair of writers that share a row: the flexible one is sized
    // from what is beside it, so neither can ever reach into the other.
    let spark_w = inner.width.saturating_sub(51).min(14);
    let dev_span = secs_label(crate::collect::io::HISTORY_SECS);
    let cols = vec![
        Col {
            lab: "DEVICE",
            x: 3,
            w: 9,
            right: false,
        },
        Col {
            lab: "TYPE",
            x: 13,
            w: 5,
            right: false,
        },
        Col {
            lab: "SIZE",
            x: 19,
            w: 5,
            right: true,
        },
        Col {
            lab: "READ",
            x: 25,
            w: 8,
            right: true,
        },
        Col {
            lab: "WRITE",
            x: 34,
            w: 8,
            right: true,
        },
        Col {
            lab: "UTIL",
            x: 43,
            w: 5,
            right: true,
        },
        // The label is the ring's real span, not a round number: a column
        // headed 60s that shows five seconds is worse than no header at all.
        Col {
            lab: &dev_span,
            x: 50,
            w: spark_w,
            right: false,
        },
    ];
    let sort_col = if by_util { "UTIL" } else { "WRITE" };
    header(buf, inner.x, inner.y, &cols, sort_col);

    // Two summary lines live at the bottom of the box, so the table gets what
    // is left after them.
    let table_h = inner.height.saturating_sub(4);
    for (i, r) in rows.iter().take(table_h as usize).enumerate() {
        let y = inner.y + 1 + i as u16;
        let busy = r.util.map(|u| u > BUSY).unwrap_or(false);
        let dot_fg = match r.util {
            Some(u) if u > BUSY => p::red(),
            Some(u) if u > 0.4 => p::yellow(),
            Some(_) => p::green(),
            None => p::dim(),
        };
        br::text(buf, inner.x + 1, y, "●", dot_fg, false);
        br::text(
            buf,
            inner.x + cols[0].x,
            y,
            &crate::ui::format::pad_right(&r.name, cols[0].w as usize),
            if r.stacked { p::dim() } else { p::fg() },
            false,
        );
        br::text(
            buf,
            inner.x + cols[1].x,
            y,
            &crate::ui::format::pad_right(&r.kind, cols[1].w as usize),
            if r.kind == "hdd" {
                p::yellow()
            } else {
                p::dim()
            },
            false,
        );
        let size = if r.size > 0 {
            gsize(r.size)
        } else {
            "--".into()
        };
        br::text(
            buf,
            inner.x + cols[2].x,
            y,
            &crate::ui::format::pad_left(&size, cols[2].w as usize),
            p::dim(),
            false,
        );
        br::text(
            buf,
            inner.x + cols[3].x,
            y,
            &crate::ui::format::pad_left(&rate_short(r.read), cols[3].w as usize),
            p::green(),
            false,
        );
        br::text(
            buf,
            inner.x + cols[4].x,
            y,
            &crate::ui::format::pad_left(&rate_short(r.write), cols[4].w as usize),
            p::cyan(),
            false,
        );
        let util = match r.util {
            Some(u) => format!("{}%", (u * 100.0).round()),
            None => "--".into(),
        };
        br::text(
            buf,
            inner.x + cols[5].x,
            y,
            &crate::ui::format::pad_left(&util, cols[5].w as usize),
            if busy {
                p::red()
            } else if r.util.map(|u| u > 0.4).unwrap_or(false) {
                p::yellow()
            } else {
                p::fg()
            },
            false,
        );
        if spark_w > 0 {
            // Level is ABSOLUTE — log-scaled from 1 MB/s to 2 GB/s — so a quiet
            // device draws a low trace instead of being stretched to fill the
            // band just because it happens to be the busiest row on screen.
            let vals = spark_vals(
                &r.history,
                r.pushed,
                2e9,
                true,
                spark_w as usize,
                crate::collect::io::SAMPLE_MS,
            );
            br::spark(
                buf,
                inner.x + cols[6].x,
                y,
                spark_w,
                &vals,
                Ramp::Read,
                Some(SPARK_BAND),
            );
        }
    }

    let sep_y = inner.y + inner.height - 3;
    br::rule(buf, inner.x, sep_y, inner.width);
    br::text(buf, inner.x + 1, sep_y + 1, "aggregate", p::dim(), false);
    br::text(
        buf,
        inner.x + 12,
        sep_y + 1,
        &format!("r {}", rate_short(s.read_bps)),
        p::green(),
        false,
    );
    br::text(
        buf,
        inner.x + 23,
        sep_y + 1,
        &format!("w {}", rate_short(s.write_bps)),
        p::cyan(),
        false,
    );
    br::text(
        buf,
        inner.x + 34,
        sep_y + 1,
        &format!("iops {}", kcount(s.iops_r + s.iops_w)),
        p::dim(),
        false,
    );
    br::text(buf, inner.x + 1, sep_y + 2, "backing", p::dim(), false);
    let topo = if s.stacked.is_empty() {
        format!("{} physical, none stacked", s.phys)
    } else {
        format!(
            "{} physical summed · {} stacked",
            s.phys,
            s.stacked.join(", ")
        )
    };
    br::text(
        buf,
        inner.x + 12,
        sep_y + 2,
        &clamp(&topo, inner.width.saturating_sub(13) as usize),
        p::dim(),
        false,
    );
}

/// Clamp a string to `n` display cells. Every right-aligned or trailing label
/// goes through this: a long device name or mount point must lose its own tail
/// rather than overwrite the field beside it.
fn clamp(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    s.chars().take(n).collect()
}

// ── latency box ────────────────────────────────────────────────────────────

fn latency_box(buf: &mut Buffer, area: Rect, s: &Sys) {
    let p99 = pct(&s.hist, 0.99);
    let (_, tail_pct) = tail(&s.hist);
    let hot = p99.map(|v| v > 10.0).unwrap_or(false);
    let foot = format!("tail {} over 10ms", pct_opt(tail_pct, 2));
    let inner = br::draw_box(
        buf,
        area,
        &br::BoxOpts {
            key: Some("3"),
            title: Some("latency"),
            sub: Some("io completion · sampled"),
            right: Some(&format!("p99 {}", ms_opt(p99))),
            right_fg: Some(if hot { p::red() } else { p::dim() }),
            foot_r: Some(&foot),
            ..Default::default()
        },
    );

    // OPS and SHARE are anchored to the right edge and the bars take what is
    // left, so a narrow box loses bar resolution rather than losing the numbers.
    let share_x = inner.width.saturating_sub(8);
    let ops_x = share_x.saturating_sub(7);
    let dist_w = ops_x.saturating_sub(10).clamp(4, 32);
    let cols = vec![
        Col {
            lab: "BUCKET",
            x: 1,
            w: 7,
            right: false,
        },
        Col {
            lab: "DISTRIBUTION",
            x: 9,
            w: dist_w,
            right: false,
        },
        Col {
            lab: "OPS",
            x: ops_x,
            w: 6,
            right: true,
        },
        Col {
            lab: "SHARE",
            x: share_x,
            w: 7,
            right: true,
        },
    ];
    header(buf, inner.x, inner.y, &cols, "");

    let total: u64 = s.hist.iter().sum();
    let max = s.hist.iter().copied().max().unwrap_or(1).max(1);
    for i in 0..LAT_BUCKETS {
        let y = inner.y + 1 + i as u16;
        if y >= inner.y + inner.height - 2 {
            break;
        }
        let n = s.hist[i];
        // Colour comes from the bucket's POSITION, not its count: the
        // right-hand buckets are red even when nearly empty, so you can see
        // where the tail would appear before it does.
        let f = i as f64 / (LAT_BUCKETS - 1) as f64;
        let col = Ramp::Load.at(f);
        br::text(
            buf,
            inner.x + cols[0].x,
            y,
            &crate::ui::format::pad_right(&bucket_label(i), cols[0].w as usize),
            if i >= LAT_TAIL_FROM { col } else { p::dim() },
            false,
        );
        let filled = if n > 0 {
            (((n as f64 / max as f64) * dist_w as f64).round() as u16).max(1)
        } else {
            0
        };
        for k in 0..dist_w {
            let on = k < filled;
            if let Some(cell) = buf.cell_mut((inner.x + cols[1].x + k, y)) {
                cell.set_char(if on { '■' } else { '·' })
                    .set_fg(if on { col } else { p::faint() });
            }
        }
        let fg = if n == 0 { p::faint() } else { p::dim() };
        br::text(
            buf,
            inner.x + cols[2].x,
            y,
            &crate::ui::format::pad_left(&kcount(n as f64), cols[2].w as usize),
            fg,
            false,
        );
        let share = if total == 0 {
            "--".to_string()
        } else {
            format!("{:.1}%", n as f64 / total as f64 * 100.0)
        };
        br::text(
            buf,
            inner.x + cols[3].x,
            y,
            &crate::ui::format::pad_left(&share, cols[3].w as usize),
            fg,
            false,
        );
    }

    let sep_y = inner.y + inner.height - 2;
    br::rule(buf, inner.x, sep_y, inner.width);
    let mut left_end = inner.x;
    for (i, (lab, q)) in [("p50", 0.5), ("p95", 0.95), ("p99", 0.99)]
        .iter()
        .enumerate()
    {
        let x = inner.x + 1 + i as u16 * 12;
        br::text(buf, x, sep_y + 1, lab, p::dim(), false);
        let v = pct(&s.hist, *q);
        let fg = match (*lab, v) {
            ("p99", Some(v)) if v > 10.0 => p::red(),
            ("p95", Some(v)) if v > 10.0 => p::yellow(),
            _ => p::fg(),
        };
        left_end = br::text(
            buf,
            x + 4,
            sep_y + 1,
            &crate::ui::format::pad_left(&ms_opt(v), 7),
            fg,
            true,
        );
    }
    // The percentiles own this row; the sample count is only drawn if what is
    // left of the row can hold it whole.
    let note = format!("{} ops sampled", kcount(total as f64));
    if inner.right().saturating_sub(2 + left_end) >= note.chars().count() as u16 {
        br::text_right(buf, inner.right() - 2, sep_y + 1, &note, p::dim(), false);
    }
}

// ── volumes box ────────────────────────────────────────────────────────────

struct VolRow {
    mount: String,
    fs: String,
    size: u64,
    used: u64,
    frac: f64,
    per_day: Option<f64>,
    days: Option<f64>,
}

fn vol_rows(app: &App) -> Vec<VolRow> {
    let mut rows: Vec<VolRow> = app
        .filesystems
        .iter()
        .filter(|f| f.size_bytes > 0)
        .map(|f| {
            let g = app.growth.growth(&f.mount, f.used_bytes, f.size_bytes);
            VolRow {
                mount: f.mount.clone(),
                fs: f.fs_type.clone(),
                size: f.size_bytes,
                used: f.used_bytes,
                frac: f.used_bytes as f64 / f.size_bytes as f64,
                per_day: g.as_ref().map(|g| g.bytes_per_day),
                days: g.as_ref().and_then(|g| g.days_until_full),
            }
        })
        .collect();
    rows.sort_by(|a, b| {
        b.frac
            .partial_cmp(&a.frac)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    rows
}

fn volumes_box(buf: &mut Buffer, area: Rect, app: &App) {
    let rows = vol_rows(app);
    // Soonest to fill, ignoring volumes with no fill date at all. A flat or
    // shrinking volume has no deadline; dividing by its trend anyway is how a
    // log rotation that frees a gigabyte a day gets reported as the most
    // urgent volume on the box, at minus forty-seven days.
    let soonest = rows
        .iter()
        .filter(|r| r.days.is_some())
        .min_by(|a, b| a.days.unwrap().partial_cmp(&b.days.unwrap()).unwrap());
    let tightest = rows.first();
    let crit = tightest.map(|r| r.frac > 0.9).unwrap_or(false);
    let foot = match soonest {
        Some(r) => format!("{} full in {:.0}d", r.mount, r.days.unwrap()),
        None => "no volume trending full".to_string(),
    };
    let right = match tightest {
        Some(r) if crit => format!("⚠ {} {}%", r.mount, (r.frac * 100.0).round()),
        _ => "all healthy".to_string(),
    };
    let sub = format!("{} mounted", rows.len());
    let inner = br::draw_box(
        buf,
        area,
        &br::BoxOpts {
            key: Some("4"),
            title: Some("volumes"),
            sub: Some(&sub),
            right: Some(&right),
            right_fg: Some(if crit { p::red() } else { p::green() }),
            foot_r: Some(&foot),
            ..Default::default()
        },
    );

    let trend_x = inner.width.saturating_sub(9);
    let pct_x = trend_x.saturating_sub(6);
    let meter_w = pct_x.saturating_sub(35).clamp(0, 16);
    let cols = vec![
        Col {
            lab: "MOUNT",
            x: 1,
            w: 12,
            right: false,
        },
        Col {
            lab: "FS",
            x: 14,
            w: 5,
            right: false,
        },
        Col {
            lab: "SIZE",
            x: 20,
            w: 5,
            right: true,
        },
        Col {
            lab: "USED",
            x: 26,
            w: 6,
            right: true,
        },
        Col {
            lab: "CAPACITY",
            x: 34,
            w: meter_w,
            right: false,
        },
        Col {
            lab: "USE%",
            x: pct_x,
            w: 5,
            right: true,
        },
        Col {
            lab: "TREND",
            x: trend_x,
            w: 8,
            right: true,
        },
    ];
    header(buf, inner.x, inner.y, &cols, "USE%");

    let table_h = inner.height.saturating_sub(2);
    for (i, r) in rows.iter().take(table_h as usize).enumerate() {
        let y = inner.y + 1 + i as u16;
        let crit = r.frac > 0.9;
        let fg = if crit { p::red() } else { p::fg() };
        br::text(
            buf,
            inner.x + cols[0].x,
            y,
            &crate::ui::format::pad_right(
                &tail_path(&r.mount, cols[0].w as usize),
                cols[0].w as usize,
            ),
            fg,
            false,
        );
        br::text(
            buf,
            inner.x + cols[1].x,
            y,
            &crate::ui::format::pad_right(&r.fs, cols[1].w as usize),
            p::dim(),
            false,
        );
        br::text(
            buf,
            inner.x + cols[2].x,
            y,
            &crate::ui::format::pad_left(&gsize(r.size), cols[2].w as usize),
            p::dim(),
            false,
        );
        br::text(
            buf,
            inner.x + cols[3].x,
            y,
            &crate::ui::format::pad_left(&gsize(r.used), cols[3].w as usize),
            fg,
            false,
        );
        br::meter(buf, inner.x + cols[4].x, y, meter_w, r.frac, Ramp::Load);
        br::text(
            buf,
            inner.x + cols[5].x,
            y,
            &crate::ui::format::pad_left(
                &format!("{}%", (r.frac * 100.0).round()),
                cols[5].w as usize,
            ),
            if crit { p::red() } else { p::dim() },
            false,
        );
        // A trend needs observation time to exist. Until then this is `--`,
        // not zero: "not measured yet" and "not growing" are different claims.
        let trend = match r.per_day {
            Some(b) if b.abs() >= 1e6 => format!(
                "{}{}",
                if b > 0.0 { "+" } else { "-" },
                gsize(b.abs() as u64)
            ),
            Some(_) => "flat".into(),
            None => "--".into(),
        };
        br::text(
            buf,
            inner.x + cols[6].x,
            y,
            &crate::ui::format::pad_left(&trend, cols[6].w as usize),
            if crit { p::red() } else { p::dim() },
            false,
        );
    }
}

/// Keep the tail of a path when it doesn't fit — `/very/long/…/target` reads
/// better as its last components, since the leading ones are usually shared.
fn tail_path(s: &str, n: usize) -> String {
    let len = s.chars().count();
    if len <= n {
        return s.to_string();
    }
    let keep: String = s.chars().skip(len - (n - 1)).collect();
    format!("…{keep}")
}

// ── smart box ──────────────────────────────────────────────────────────────

fn smart_box(buf: &mut Buffer, area: Rect, app: &App) {
    let rows: Vec<(
        &crate::collect::DeviceTick,
        Option<&crate::collect::smart::SmartTick>,
    )> = app
        .devices
        .iter()
        .map(|d| (d, app.smart.by_device.get(&d.name)))
        .collect();
    let warn: Vec<&str> = rows
        .iter()
        .filter(|(d, t)| {
            d.smart_ok == Some(false)
                || t.map(|t| t.percentage_used.unwrap_or(0) >= 80)
                    .unwrap_or(false)
        })
        .map(|(d, _)| d.name.as_str())
        .collect();
    let pass = rows.len() - warn.len();
    let available = app.smart.smartctl_available();
    let sub = format!("{} physical", rows.len());
    let right = if available {
        format!("{pass}/{} passed", rows.len())
    } else {
        "smartctl not installed".to_string()
    };
    let foot = if !available {
        "brew install smartmontools".to_string()
    } else {
        match app.smart.last_refresh_at {
            // Elapsed since the last poll — the configured interval is a
            // different number, and printing it here claims a freshness the
            // data may not have.
            Some(t) => format!("polled {} ago", age(t.elapsed().as_secs())),
            None => "not polled yet".to_string(),
        }
    };
    let inner = br::draw_box(
        buf,
        area,
        &br::BoxOpts {
            key: Some("5"),
            title: Some("smart"),
            sub: Some(&sub),
            right: Some(&right),
            right_fg: Some(if !available {
                p::dim()
            } else if warn.is_empty() {
                p::green()
            } else {
                p::yellow()
            }),
            foot_r: Some(&foot),
            ..Default::default()
        },
    );

    let spare_x = inner.width.saturating_sub(7);
    let temp_x = spare_x.saturating_sub(7);
    let written_x = temp_x.saturating_sub(10);
    let wear_w = written_x.saturating_sub(22).clamp(0, 16);
    let cols = vec![
        Col {
            lab: "DEVICE",
            x: 3,
            w: 9,
            right: false,
        },
        Col {
            lab: "HEALTH",
            x: 13,
            w: 7,
            right: false,
        },
        Col {
            lab: "WEAR",
            x: 21,
            w: wear_w,
            right: false,
        },
        Col {
            lab: "WRITTEN",
            x: written_x,
            w: 9,
            right: true,
        },
        Col {
            lab: "TEMP",
            x: temp_x,
            w: 6,
            right: true,
        },
        Col {
            lab: "SPARE",
            x: spare_x,
            w: 6,
            right: true,
        },
    ];
    header(buf, inner.x, inner.y, &cols, "");

    let table_h = inner.height.saturating_sub(1);
    for (i, (d, t)) in rows.iter().take(table_h as usize).enumerate() {
        let y = inner.y + 1 + i as u16;
        let bad = d.smart_ok == Some(false);
        let wear = t.and_then(|t| t.percentage_used);
        let warn = bad || wear.map(|w| w >= 80).unwrap_or(false);
        br::text(
            buf,
            inner.x + 1,
            y,
            "●",
            if warn {
                p::yellow()
            } else if d.smart_ok == Some(true) {
                p::green()
            } else {
                p::dim()
            },
            false,
        );
        br::text(
            buf,
            inner.x + cols[0].x,
            y,
            &crate::ui::format::pad_right(&d.name, cols[0].w as usize),
            p::fg(),
            false,
        );
        let (health, fg) = match d.smart_ok {
            Some(true) => ("PASSED", p::green()),
            Some(false) => ("FAILED", p::red()),
            None => ("--", p::dim()),
        };
        br::text(
            buf,
            inner.x + cols[1].x,
            y,
            &crate::ui::format::pad_right(health, cols[1].w as usize),
            fg,
            false,
        );
        // A meter alone can't tell 0% wear from unmeasured wear — both are an
        // empty track — so the number goes beside it.
        let bar_w = wear_w.saturating_sub(5);
        match wear {
            Some(w) => {
                br::meter(
                    buf,
                    inner.x + cols[2].x,
                    y,
                    bar_w,
                    w as f64 / 100.0,
                    Ramp::Load,
                );
                br::text(
                    buf,
                    inner.x + cols[2].x + bar_w,
                    y,
                    &crate::ui::format::pad_left(&format!("{w}%"), 5),
                    if w >= 80 { p::red() } else { p::dim() },
                    false,
                );
            }
            None => {
                br::meter_unavailable(buf, inner.x + cols[2].x, y, bar_w, false);
                br::text(
                    buf,
                    inner.x + cols[2].x + bar_w,
                    y,
                    &crate::ui::format::pad_left("--", 5),
                    p::dim(),
                    false,
                );
            }
        }
        let written = t
            .and_then(|t| t.data_units_written)
            .map(|u| gsize(u.saturating_mul(512_000)))
            .unwrap_or_else(|| "--".into());
        br::text(
            buf,
            inner.x + cols[3].x,
            y,
            &crate::ui::format::pad_left(&written, cols[3].w as usize),
            p::dim(),
            false,
        );
        let temp = t
            .and_then(|t| t.temperature_c)
            .map(|c| app.temp_unit.format_temp(c))
            .unwrap_or_else(|| "--".into());
        br::text(
            buf,
            inner.x + cols[4].x,
            y,
            &crate::ui::format::pad_left(&temp, cols[4].w as usize),
            p::dim(),
            false,
        );
        let spare = t
            .and_then(|t| t.available_spare)
            .map(|s| format!("{s}%"))
            .unwrap_or_else(|| "--".into());
        br::text(
            buf,
            inner.x + cols[5].x,
            y,
            &crate::ui::format::pad_left(&spare, cols[5].w as usize),
            p::dim(),
            false,
        );
    }
}

// ── files box ──────────────────────────────────────────────────────────────

pub fn sorted_rows(app: &App) -> Vec<crate::ui::lite::HotRow> {
    let mut rows =
        crate::ui::lite::filter_rows(crate::ui::lite::collect_rows(app), &app.dense.filter_text);
    // Sorted from the DECLARED key, so the order and the indicator in the box
    // border cannot disagree.
    match app.dense.sort {
        FileSort::Rate => rows.sort_by(|a, b| {
            b.events_per_sec
                .partial_cmp(&a.events_per_sec)
                .unwrap_or(std::cmp::Ordering::Equal)
        }),
        FileSort::Total => rows.sort_by_key(|r| std::cmp::Reverse(r.total_events)),
        FileSort::Name => rows.sort_by(|a, b| a.file.cmp(&b.file)),
    }
    rows
}

fn files_box(buf: &mut Buffer, area: Rect, app: &App, wide: bool) {
    let rows = sorted_rows(app);
    let st = &app.dense;
    let total = rows.len();
    let visible = area.height.saturating_sub(3) as usize;
    let first = st.offset.min(total.saturating_sub(1));
    let last = (first + visible).min(total);

    let sub = if st.filter_text.is_empty() {
        format!("{total} active")
    } else {
        format!("{total} matching \"{}\"", clamp(&st.filter_text, 20))
    };
    let right = format!("sort ↓ {}", st.sort.label());
    let foot_r = if total == 0 {
        "nothing changing".to_string()
    } else {
        format!("{}-{} of {}", first + 1, last, total)
    };
    let foot_l: &[(&str, &str)] = if wide {
        &[
            ("q", "uit"),
            ("↑↓", " select"),
            ("/", " filter"),
            ("s", "ort"),
            ("V", " view"),
            ("?", " help"),
        ]
    } else {
        &[("q", "uit"), ("/", " filter"), ("s", "ort"), ("V", " view")]
    };
    let inner = br::draw_box(
        buf,
        area,
        &br::BoxOpts {
            key: Some("6"),
            title: Some("files"),
            sub: Some(&sub),
            right: Some(&right),
            foot_l,
            foot_r: Some(&foot_r),
            ..Default::default()
        },
    );

    if st.filter_input {
        br::text(
            buf,
            inner.x + 1,
            inner.y,
            &format!("/{}_", st.filter_text),
            p::cyan(),
            true,
        );
        return;
    }
    if total == 0 {
        let (_, roots, err) = app.hot_files.snapshot_meta();
        let msg = match err {
            Some(e) => format!("file watcher unavailable: {e}"),
            None => format!("watching {} root(s) — nothing has changed yet", roots.len()),
        };
        br::text(
            buf,
            inner.x + 1,
            inner.y + 1,
            &clamp(&msg, inner.width as usize - 2),
            p::dim(),
            false,
        );
        return;
    }

    // Columns are DROPPED whole as the box narrows, from the least useful in,
    // rather than left to be clipped by the buffer. A half-drawn column reads
    // as a rendering bug; an absent one reads as a narrow terminal.
    let w = inner.width;
    let file_span = secs_label(crate::collect::io::HISTORY_SECS);
    let spark_w = if wide && w >= 108 {
        (w - 96).min(18)
    } else {
        0
    };
    let want_seen = w >= 76;
    let want_total = w >= 68;
    let want_kind = w >= 60;
    // Everything to the right of DIR is fixed, so DIR gets the slack — the
    // path is the field that benefits most from an extra column and degrades
    // most gracefully without one.
    let fixed_right = 10 + u16::from(want_total) * 8 + u16::from(want_seen) * 7 + spark_w;
    let dir_w = w
        .saturating_sub(29 + u16::from(want_kind) * 8 + fixed_right)
        .min(44);
    let mut cols = vec![Col {
        lab: "FILE",
        x: 3,
        w: 24,
        right: false,
    }];
    let mut x = 28;
    if dir_w > 0 {
        cols.push(Col {
            lab: "DIR",
            x,
            w: dir_w,
            right: false,
        });
        x += dir_w + 1;
    }
    if want_kind {
        cols.push(Col {
            lab: "KIND",
            x,
            w: 7,
            right: false,
        });
        x += 8;
    }
    cols.push(Col {
        lab: "EVENTS/S",
        x,
        w: 9,
        right: true,
    });
    x += 10;
    if want_total {
        cols.push(Col {
            lab: "TOTAL",
            x,
            w: 7,
            right: true,
        });
        x += 8;
    }
    if want_seen {
        cols.push(Col {
            lab: "SEEN",
            x,
            w: 6,
            right: true,
        });
        x += 7;
    }
    if spark_w > 0 {
        cols.push(Col {
            lab: &file_span,
            x,
            w: spark_w,
            right: false,
        });
    }
    // Look-ups by label, so a dropped column is simply absent rather than
    // shifting every index after it.
    let col = |lab: &str| cols.iter().find(|c| c.lab == lab).map(|c| (c.x, c.w));
    header(buf, inner.x, inner.y, &cols, st.sort.column());

    for (i, r) in rows[first..last].iter().enumerate() {
        let y = inner.y + 1 + i as u16;
        let is_sel = first + i == st.selected;
        if is_sel {
            br::tint(buf, inner.x, y, inner.width, 1, p::sel_bg());
        }
        br::text(
            buf,
            inner.x + 1,
            y,
            if is_sel { "▶" } else { "●" },
            if is_sel { p::cyan() } else { p::green() },
            is_sel,
        );
        if let Some((cx, cw)) = col("FILE") {
            br::text(
                buf,
                inner.x + cx,
                y,
                &crate::ui::format::pad_right(&r.file, cw as usize),
                p::fg(),
                is_sel,
            );
        }
        if let Some((cx, cw)) = col("DIR") {
            br::text(
                buf,
                inner.x + cx,
                y,
                &crate::ui::format::pad_right(&tail_path(&r.dir, cw as usize), cw as usize),
                p::dim(),
                false,
            );
        }
        if let Some((cx, cw)) = col("KIND") {
            br::text(
                buf,
                inner.x + cx,
                y,
                &crate::ui::format::pad_right(r.kind, cw as usize),
                p::faint(),
                false,
            );
        }
        if let Some((cx, cw)) = col("EVENTS/S") {
            br::text(
                buf,
                inner.x + cx,
                y,
                &crate::ui::format::pad_left(&format!("{:.1}", r.events_per_sec), cw as usize),
                p::cyan(),
                false,
            );
        }
        if let Some((cx, cw)) = col("TOTAL") {
            br::text(
                buf,
                inner.x + cx,
                y,
                &crate::ui::format::pad_left(&kcount(r.total_events as f64), cw as usize),
                p::green(),
                false,
            );
        }
        if let Some((cx, cw)) = col("SEEN") {
            br::text(
                buf,
                inner.x + cx,
                y,
                &crate::ui::format::pad_left(&format!("{}s", r.secs_since_seen), cw as usize),
                p::dim(),
                false,
            );
        }
        if spark_w > 0 {
            // Absolute again: 0.1 to 200 events/s, log-scaled, so a busy file
            // and a quiet one draw different heights instead of both filling
            // the band.
            let vals = spark_vals(&r.history, r.pushed, 200.0, true, spark_w as usize, 1000);
            let (cx, _) = col(&file_span).unwrap_or((0, 0));
            br::spark(
                buf,
                inner.x + cx,
                y,
                spark_w,
                &vals,
                Ramp::Write,
                Some(SPARK_BAND),
            );
        }
    }
}

// ── compact ────────────────────────────────────────────────────────────────

/// The 80×24 screen. `devices`, `volumes` and `smart` collapse to summary
/// lines, but the mirror survives because it is the identity of the tool, and
/// the latency percentiles keep a seat because the tail is the point.
fn render_compact(buf: &mut Buffer, area: Rect, app: &App) {
    let s = sys(app);
    let io_h = 11.min(area.height / 2).max(8);
    let io = Rect::new(area.x, area.y, area.width, io_h);
    let files = Rect::new(area.x, area.y + io_h, area.width, area.height - io_h);

    let graph_rows = (io_h - 6).max(2);
    let read_rows = graph_rows / 2 + graph_rows % 2;
    let write_rows = graph_rows - read_rows;

    let sub = format!("{} physical", s.phys);
    let inner = br::draw_box(
        buf,
        io,
        &br::BoxOpts {
            key: Some("1"),
            title: Some("io"),
            sub: Some(&sub),
            right: Some(&format!("up {}", uptime(app.host.uptime_secs))),
            foot_l: &[("V", " view"), (",", " settings")],
            foot_r: Some(&format!("p99 {}", ms_opt(pct(&s.hist, 0.99)))),
            ..Default::default()
        },
    );

    let gx = inner.x + 8;
    let gw = inner.width.saturating_sub(9);
    if gw < 8 {
        return;
    }
    let want = gw as usize;
    let read = window(&app.io.agg.read_bps_graph, want);
    let write = window(&app.io.agg.write_bps_graph, want);
    let (rv, rtop) = scaled(&subpixels(&read));
    let (wv, wtop) = scaled(&subpixels(&write));

    let y = inner.y;
    br::text(buf, inner.x + 1, y, "r", p::green(), true);
    br::text(
        buf,
        inner.x + 3,
        y,
        &rate_short(read.last().copied().unwrap_or(0.0)),
        p::br_white(),
        true,
    );
    br::text_right(
        buf,
        inner.right() - 2,
        y,
        &format!(
            "peak {}  iops {}",
            rate_short(read.iter().copied().fold(0.0, f64::max)),
            kcount(s.iops_r + s.iops_w)
        ),
        p::dim(),
        false,
    );
    let ry = y + 1;
    let ay = ry + read_rows;
    br::text_right(buf, inner.x + 5, ry, &rate_short(rtop), p::faint(), false);
    br::text(buf, inner.x + 6, ry, "┤", p::faint(), false);
    br::graph(
        buf,
        Rect::new(gx, ry, gw, read_rows),
        &rv,
        Ramp::Read,
        false,
        None,
    );
    br::rule(buf, gx, ay, gw);
    br::text_right(
        buf,
        inner.x + 5,
        ay,
        &secs_label(span_secs(want)),
        p::dim(),
        false,
    );
    br::text(buf, inner.x + 6, ay, "┤", p::faint(), false);
    br::text_right(buf, inner.right() - 2, ay, "┤ now ├", p::dim(), false);
    br::graph(
        buf,
        Rect::new(gx, ay + 1, gw, write_rows),
        &wv,
        Ramp::Write,
        true,
        None,
    );
    br::text_right(
        buf,
        inner.x + 5,
        ay + write_rows,
        &rate_short(wtop),
        p::faint(),
        false,
    );
    br::text(buf, inner.x + 6, ay + write_rows, "┤", p::faint(), false);

    let wy = ay + write_rows + 1;
    br::text(buf, inner.x + 1, wy, "w", p::cyan(), true);
    br::text(
        buf,
        inner.x + 3,
        wy,
        &rate_short(write.last().copied().unwrap_or(0.0)),
        p::br_white(),
        true,
    );
    br::text_right(
        buf,
        inner.right() - 2,
        wy,
        &format!(
            "peak {}  iops {}",
            rate_short(write.iter().copied().fold(0.0, f64::max)),
            kcount(s.iops_w)
        ),
        p::dim(),
        false,
    );

    // The vitals line only exists if the box has a row spare for it. Compact
    // drops boxes, never rows: an empty row inside a box is wasted screen in
    // the layout that has the least of it.
    let vy = wy + 1;
    if vy < inner.bottom() {
        br::text(buf, inner.x + 1, vy, "util", p::dim(), false);
        match s.util {
            Some(u) => {
                br::meter(buf, inner.x + 6, vy, 14, u, Ramp::Load);
                br::text(
                    buf,
                    inner.x + 21,
                    vy,
                    &format!("{}%", (u * 100.0).round()),
                    p::fg(),
                    false,
                );
            }
            None => {
                br::meter_unavailable(buf, inner.x + 6, vy, 14, true);
            }
        }
        br::text_right(
            buf,
            inner.right() - 2,
            vy,
            &format!(
                "await {}  p50 {}  p99 {}",
                ms_opt(await_ms(&s.hist)),
                ms_opt(pct(&s.hist, 0.5)),
                ms_opt(pct(&s.hist, 0.99))
            ),
            p::dim(),
            false,
        );
    }

    files_box(buf, files, app, false);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hist(v: [u64; LAT_BUCKETS]) -> [u64; LAT_BUCKETS] {
        v
    }

    #[test]
    fn an_idle_disk_reports_nothing_rather_than_nan() {
        // Every one of these divides by the count it just summed. Four in the
        // morning is exactly when nobody is around to distrust a NaN.
        let h = hist([0; LAT_BUCKETS]);
        assert_eq!(pct(&h, 0.99), None);
        assert_eq!(await_ms(&h), None);
        assert_eq!(tail(&h), (0, None));
        assert_eq!(ms_opt(pct(&h, 0.5)), "--");
        assert_eq!(pct_opt(tail(&h).1, 2), "--");
    }

    #[test]
    fn a_rank_landing_on_an_empty_bucket_does_not_divide_by_zero() {
        // p100 of a histogram whose last populated bucket is followed by empty
        // ones: the rank test passes on a zero-count bucket.
        let h = hist([100, 0, 0, 0, 0, 0, 0]);
        let v = pct(&h, 1.0).expect("measurable");
        assert!(v.is_finite(), "got {v}");
        assert!((0.0..=0.1).contains(&v));
    }

    #[test]
    fn percentiles_are_monotonic_and_the_mean_sits_below_p99() {
        let h = hist([41200, 28400, 9800, 2100, 2400, 520, 20]);
        let (p50, p95, p99) = (
            pct(&h, 0.5).unwrap(),
            pct(&h, 0.95).unwrap(),
            pct(&h, 0.99).unwrap(),
        );
        assert!(p50 <= p95 && p95 <= p99, "{p50} {p95} {p99}");
        assert!(await_ms(&h).unwrap() < p99);
    }

    #[test]
    fn the_tail_starts_at_ten_milliseconds() {
        // Derived from the edges rather than typed, so the tail and the
        // histogram's own colouring can't disagree about where it starts.
        assert_eq!(bucket_edges(LAT_TAIL_FROM).0, 10.0);
        let h = hist([0, 0, 0, 0, 0, 3, 1]);
        assert_eq!(tail(&h).0, 4);
        assert_eq!(tail(&h).1, Some(100.0));
    }

    #[test]
    fn bucket_labels_span_the_edges_without_a_gap() {
        assert_eq!(bucket_label(0), "<0.1ms");
        assert_eq!(bucket_label(LAT_BUCKETS - 1), ">50ms");
        for i in 1..LAT_BUCKETS - 1 {
            assert_eq!(bucket_edges(i).0, bucket_edges(i - 1).1);
        }
    }

    #[test]
    fn spark_levels_are_absolute_not_set_relative() {
        // The failure this prevents: normalising against the set's own max
        // makes the busiest row hit the band top whatever it is doing, so a
        // saturated machine renders identically to an idle one.
        let quiet = band(1e6, 0.001, 2e9, true);
        let loud = band(1e9, 0.001, 2e9, true);
        assert!(loud > quiet + 0.1, "quiet {quiet} loud {loud}");
        // And the band stays inside the caps a one-row braille cell can show.
        for v in [0.0_f64, 1.0, 1e3, 1e6, 1e9, 1e12] {
            let b = band(v, 0.001, 2e9, true);
            assert!((SPARK_BAND.0..=SPARK_BAND.1).contains(&b), "{v} → {b}");
        }
    }

    #[test]
    fn a_warming_ring_left_pads_instead_of_stretching() {
        let mut ring = std::collections::VecDeque::new();
        ring.push_back(5.0);
        ring.push_back(6.0);
        let w = window(&ring, 5);
        assert_eq!(w, vec![0.0, 0.0, 0.0, 5.0, 6.0]);
    }

    #[test]
    fn a_sparkline_scrolls_instead_of_re_grouping() {
        // The jank this pins, and the reason `pushed` exists. Grouping samples
        // by their position in the ring re-groups all of them the moment one
        // ages out: every column changes value and the sparkline shimmers.
        //
        // Grouping by absolute index gives the property that matters: a column
        // NEVER changes once it is complete. Only the newest column moves, as
        // samples land in it, and once it fills the picture scrolls by exactly
        // one.
        let cols = 12;
        let ms = crate::collect::io::SAMPLE_MS;
        let per = HISTORY_SECS * 1000 / ms / cols;
        let mut ring: Vec<f64> = (0..600).map(|i| i as f64).collect();
        let mut pushed = 600u64;

        let mut shifts = 0;
        let mut prev = condense(&ring, pushed, cols, ms);
        for i in 0..per * 3 {
            ring.remove(0);
            ring.push(600.0 + i as f64);
            pushed += 1;
            let now = condense(&ring, pushed, cols, ms);
            let settled = cols - 1;
            if now[..settled] == prev[..settled] {
                // Held: every settled column kept its value.
            } else {
                // Scrolled: the settled columns are yesterday's, shifted one
                // left. Anything else is a re-group.
                assert_eq!(
                    now[..settled - 1],
                    prev[1..settled],
                    "columns re-grouped at push {i}"
                );
                shifts += 1;
            }
            prev = now;
        }
        assert_eq!(shifts, 3, "expected one scroll per {per} samples");
    }

    #[test]
    fn every_history_on_the_screen_covers_the_same_window() {
        // Four windows and three animation rates used to share this screen: a
        // 48s graph, a 48s device spark, a 60s latency window and a 5m file
        // spark. Nothing on it could be read against anything else.
        let ms = crate::collect::io::SAMPLE_MS;
        for (name, cadence) in [("device ring", ms), ("file history", 1000)] {
            let samples = HISTORY_SECS * 1000 / cadence;
            let ring: Vec<f64> = (0..samples * 2).map(|i| i as f64).collect();
            let out = condense(&ring, ring.len() as u64, 12, cadence);
            assert_eq!(out.len(), 12, "{name}");
            // The oldest column must come from HISTORY_SECS ago, not from the
            // whole retained ring.
            let oldest_abs = ring.len() as f64 - samples as f64;
            assert!(
                out[0] >= oldest_abs - samples as f64 / 12.0,
                "{name} reaches further back than {HISTORY_SECS}s"
            );
        }
        // And the graph ring is decimated to the same cadence.
        assert_eq!(
            crate::collect::io::GRAPH_SAMPLE_MS,
            crate::collect::io::HISTORY_MS
        );
    }

    #[test]
    fn the_time_axis_states_the_span_it_actually_draws() {
        // The bug this pins: writing a COLUMN count where a second count is
        // meant. It captioned a five-minute axis as `2m` in v0.2.0, and put
        // the mid-tick in the wrong place by the same factor.
        let ms = crate::collect::io::GRAPH_SAMPLE_MS;
        assert_eq!(span_secs(5), 5 * ms / 1000);
        // At one column per HISTORY_MS, the graph reaches HISTORY_SECS at the
        // width the rest of the screen is sized around.
        let cols_for_window = HISTORY_SECS * 1000 / ms;
        assert_eq!(span_secs(cols_for_window), HISTORY_SECS);
        // The mid-tick is half the span, not half the column count.
        assert_eq!(span_secs(120) / 2, 30);
        assert_eq!(secs_label(span_secs(cols_for_window)), "60s");
        assert_eq!(secs_label(span_secs(480)), "4m");
    }

    #[test]
    fn a_new_reading_scrolls_the_graph_by_exactly_one_column() {
        // The jank this pins: with two samples per column averaged at draw
        // time, the window slid by one sample per tick, the pairing parity
        // flipped, and EVERY column changed value — the graph shimmered
        // between two shapes instead of scrolling. One ring entry per column
        // means the tail of one frame is the head of the next.
        let mut ring: std::collections::VecDeque<f64> = (0..200).map(|i| i as f64).collect();
        let before = window(&ring, 110);
        ring.push_back(999.0);
        let after = window(&ring, 110);
        assert_eq!(
            &before[1..],
            &after[..109],
            "columns shifted by more than one"
        );
        assert_eq!(
            after[109], 999.0,
            "the newest reading is not at the right edge"
        );
    }

    #[test]
    fn sparkline_headers_state_the_span_they_draw() {
        // Both headers are computed from the ring behind them rather than
        // typed, so this only has to pin the formatter they share.
        // Every one of them now states the same window, because every one of
        // them draws it.
        assert_eq!(secs_label(HISTORY_SECS), "60s");
        assert_eq!(secs_label(90), "90s");
        assert_eq!(secs_label(300), "5m");
    }

    #[test]
    fn request_size_and_age_read_correctly() {
        assert_eq!(req_size(0.0, 0.0), "--");
        assert_eq!(req_size(128e3 * 10.0, 10.0), "128KB");
        assert_eq!(age(5), "5s");
        assert_eq!(age(300), "5m");
        assert_eq!(age(7200), "2h");
    }

    #[test]
    fn the_axis_ceiling_never_clips_the_peak() {
        let vals = vec![1.2e9, 4.0e8, 9.9e8];
        let (norm, top) = scaled(&vals);
        assert!(top >= 1.2e9);
        assert!(norm.iter().all(|v| (0.0..=1.0).contains(v)));
        assert!(norm.iter().copied().fold(0.0, f64::max) >= 0.79);
    }

    #[test]
    fn rates_and_sizes_survive_garbage() {
        assert_eq!(rate_short(f64::NAN), "0");
        assert_eq!(rate_full(-1.0), "0 B/s");
        assert_eq!(kcount(f64::INFINITY), "--");
    }

    #[test]
    fn long_paths_lose_their_own_head_not_the_column_beside_them() {
        let s = tail_path("/very/long/path/to/somewhere", 10);
        assert_eq!(s.chars().count(), 10);
        assert!(s.starts_with('…'));
        assert_eq!(clamp("abcdef", 3), "abc");
    }

    #[test]
    fn the_full_layout_tiles_the_area_exactly() {
        // Zero chrome rows is the whole premise: every row belongs to a box.
        for h in [32u16, 40, 44, 60] {
            for w in [104u16, 130, 200] {
                let area = Rect::new(0, 0, w, h);
                let l = Layout::new(area);
                assert_eq!(l.io.y, 0);
                assert_eq!(l.devices.y, l.io.bottom());
                assert_eq!(l.volumes.y, l.devices.bottom());
                assert_eq!(l.files.y, l.volumes.bottom());
                assert_eq!(l.files.bottom(), h, "{w}x{h} leaves a gap at the bottom");
                assert_eq!(l.devices.width + l.latency.width, w);
                assert_eq!(l.volumes.width + l.smart.width, w);
                assert!(l.visible_files() >= 1);
            }
        }
    }

    #[test]
    fn nothing_ever_overruns_a_box_border() {
        // The layout's hardest rule: two writers never share a column range.
        // Every box border is a wall between a right-aligned string and the
        // box beside it, so an overrun shows up as a corner or an edge that a
        // label has eaten. Checking the walls catches it at every width,
        // including the ones nobody thought to look at.
        use crate::app::{App, ViewMode};
        use crate::tabs::TabId;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut app = App::new_for_test(TabId::Overview, ViewMode::Dense);
        app.growth.observe(&app.filesystems);
        app.io.sample();
        for (w, h) in [
            (MIN_FULL_W, MIN_FULL_H),
            (110, 34),
            (120, 40),
            (130, 44),
            (160, 50),
            (240, 70),
            (400, 100),
        ] {
            let backend = TestBackend::new(w, h);
            let mut term = Terminal::new(backend).expect("terminal");
            term.draw(|f| crate::app::draw_for_test(f, &mut app))
                .expect("draw");
            let buf = term.backend().buffer();
            let l = Layout::new(Rect::new(0, 0, w, h));
            for (name, r) in [
                ("io", l.io),
                ("devices", l.devices),
                ("latency", l.latency),
                ("volumes", l.volumes),
                ("smart", l.smart),
                ("files", l.files),
            ] {
                let at = |x: u16, y: u16| buf.cell((x, y)).unwrap().symbol().to_string();
                assert_eq!(at(r.x, r.y), "╭", "{name} top-left at {w}x{h}");
                assert_eq!(at(r.right() - 1, r.y), "╮", "{name} top-right at {w}x{h}");
                assert_eq!(
                    at(r.x, r.bottom() - 1),
                    "╰",
                    "{name} bottom-left at {w}x{h}"
                );
                assert_eq!(
                    at(r.right() - 1, r.bottom() - 1),
                    "╯",
                    "{name} bottom-right at {w}x{h}"
                );
                for y in r.y + 1..r.bottom() - 1 {
                    assert_eq!(at(r.x, y), "│", "{name} left wall row {y} at {w}x{h}");
                    assert_eq!(
                        at(r.right() - 1, y),
                        "│",
                        "{name} right wall row {y} at {w}x{h}"
                    );
                }
            }
        }
    }

    #[test]
    fn sort_label_and_sort_column_come_from_the_same_value() {
        // The regression this blocks: a box advertising "sort ↓ write" while
        // ordering by something else.
        for s in [FileSort::Rate, FileSort::Total, FileSort::Name] {
            assert!(!s.label().is_empty());
            assert!(!s.column().is_empty());
        }
        assert_eq!(FileSort::Rate.next().next().next(), FileSort::Rate);
    }
}
