//! Pluggable graph rendering for every chart in the app.
//!
//! Mirrors [`crate::ui::theme`]: a `GraphStyle` enum with a `by_name`
//! lookup and a process-wide `ACTIVE` cell, plus a `render` entry point
//! that dispatches to a per-style implementation. Every chart in the UI —
//! the Overview aggregate sparkline, the IO tab's per-device lines, and
//! both Lite charts plus its row sparklines — routes through here, so one
//! setting toggles them all.
//!
//! Ported from netwatch's `graph.rs` so the two tools' `dots` style is
//! pixel-identical; the series type is `f64` here because every diskwatch
//! history is a rate, not a count.
//!
//! ## Styles
//! - `bars` — the stacked eighth-block look diskwatch shipped with.
//! - `dots` — btop-style braille area plot. Each column fills from the
//!   bottom to the sample's height in braille pixels, giving 4× the
//!   vertical resolution of `bars` at the same cell height.
//!
//! `GraphOpts.fade` adds btop's other half: a right-bright / left-dim
//! gradient across each chart plus a faint dot grid behind the data. It is
//! a separate toggle from the style because the gradient is legible under
//! `bars` too, and because `terminal`-theme users need it off.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use std::sync::RwLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphStyle {
    /// Solid-colour stacked blocks — the look diskwatch shipped with.
    Bars,
    /// btop-style braille area plot: 4× the vertical resolution of
    /// `Bars` while keeping the filled-area look.
    Dots,
}

pub const GRAPH_STYLE_NAMES: &[&str] = &["bars", "dots"];

pub fn by_name(name: &str) -> GraphStyle {
    match name.to_lowercase().as_str() {
        // "braille" and "btop" are what users reach for when they mean
        // this; accept them rather than silently falling back to bars and
        // looking like the flag did nothing.
        "dots" | "braille" | "btop" => GraphStyle::Dots,
        _ => GraphStyle::Bars,
    }
}

impl GraphStyle {
    pub fn name(self) -> &'static str {
        match self {
            GraphStyle::Bars => "bars",
            GraphStyle::Dots => "dots",
        }
    }
}

// ── Global setting ──────────────────────────────────────────────────────────

/// Style and fade live in a global for the same reason the theme does:
/// the palette accessors and every chart call site can read them without
/// threading a reference through every render function.
static ACTIVE: RwLock<GraphStyle> = RwLock::new(GraphStyle::Bars);
static FADE: RwLock<bool> = RwLock::new(false);

pub fn active() -> GraphStyle {
    *ACTIVE.read().expect("graph style lock poisoned")
}

pub fn set_by_name(name: &str) {
    *ACTIVE.write().expect("graph style lock poisoned") = by_name(name);
}

pub fn name() -> &'static str {
    active().name()
}

/// Advance to the next style, wrapping. Returns the new name.
pub fn cycle() -> &'static str {
    let next = match active() {
        GraphStyle::Bars => GraphStyle::Dots,
        GraphStyle::Dots => GraphStyle::Bars,
    };
    *ACTIVE.write().expect("graph style lock poisoned") = next;
    next.name()
}

pub fn fade_enabled() -> bool {
    *FADE.read().expect("graph fade lock poisoned")
}

pub fn set_fade(on: bool) {
    *FADE.write().expect("graph fade lock poisoned") = on;
}

pub fn toggle_fade() -> bool {
    let next = !fade_enabled();
    set_fade(next);
    next
}

/// Render preferences resolved from the active theme + settings. Built
/// once per call site rather than stored, so a theme switch takes effect
/// on the next frame with nothing to invalidate.
#[derive(Debug, Clone, Copy)]
pub struct GraphOpts {
    /// Right-bright / left-dim gradient plus the dot grid.
    pub fade: bool,
    /// Theme background, the "fade-to" anchor when interpolating.
    pub bg: Color,
    /// True under the `terminal` theme, where every colour must resolve
    /// through the user's own palette. Fade interpolates in RGB, so it
    /// switches off rather than emitting the 24-bit values that theme
    /// exists to avoid.
    pub terminal_palette: bool,
    /// Draw `▁` (or the bottom braille row) across columns with no data,
    /// so a chart is grounded from frame zero instead of appearing as a
    /// void until the ring fills. This is diskwatch's existing baseline
    /// behaviour and the default.
    pub baseline: bool,
}

impl Default for GraphOpts {
    fn default() -> Self {
        Self {
            fade: false,
            bg: Color::Reset,
            terminal_palette: false,
            baseline: true,
        }
    }
}

/// The options every call site should use: active settings, resolved
/// against the active theme.
pub fn opts() -> GraphOpts {
    let theme = crate::ui::theme::active();
    GraphOpts {
        fade: fade_enabled(),
        bg: theme.bg,
        terminal_palette: theme.name == "terminal",
        baseline: true,
    }
}

/// Lowest fraction of the base colour the leftmost (oldest) column gets
/// when fade is on. 0.30 keeps the old data visible without letting it
/// compete with the right edge.
const MIN_FADE_ALPHA: f32 = 0.30;

/// Smallest chart (in cells) where the grid overlay renders. Row
/// sparklines are too narrow to benefit — the overlay is just noise.
const GRID_MIN_W: u16 = 16;
const GRID_MIN_H: u16 = 4;

// ── Entry points ────────────────────────────────────────────────────────────

/// Render `data` into `area`, auto-scaling the y-axis to the visible window.
pub fn render(buf: &mut Buffer, area: Rect, data: &[f64], color: Color, opts: GraphOpts) {
    let start = data.len().saturating_sub(area.width as usize);
    let max = data[start..].iter().copied().fold(0.0_f64, f64::max);
    render_with_max(buf, area, data, max, color, opts);
}

/// Like [`render`], but with an explicit y-axis max — required when two
/// charts must share a scale.
pub fn render_with_max(
    buf: &mut Buffer,
    area: Rect,
    data: &[f64],
    max: f64,
    color: Color,
    opts: GraphOpts,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    if opts.fade && area.width >= GRID_MIN_W && area.height >= GRID_MIN_H {
        render_grid(buf, area, opts.bg, opts.terminal_palette);
    }
    match active() {
        GraphStyle::Bars => render_bars(buf, area, data, max, color, opts),
        GraphStyle::Dots => render_dots(buf, area, data, max, color, opts),
    }
}

/// Colour for column `i` of `n`, honouring the fade setting.
fn column_color(base: Color, i: usize, n: usize, opts: GraphOpts) -> Color {
    if !opts.fade {
        return base;
    }
    let denom = n.saturating_sub(1).max(1) as f32;
    let alpha = MIN_FADE_ALPHA + (1.0 - MIN_FADE_ALPHA) * (i as f32 / denom);
    fade_color(base, opts.bg, alpha, opts.terminal_palette)
}

/// Take the rightmost `width` samples and report how many leading columns
/// have no data behind them.
fn window(data: &[f64], width: usize) -> (&[f64], usize) {
    let start = data.len().saturating_sub(width);
    let visible = &data[start..];
    (visible, width.saturating_sub(visible.len()))
}

// ── bars ────────────────────────────────────────────────────────────────────

const BAR_GLYPHS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

fn render_bars(buf: &mut Buffer, area: Rect, data: &[f64], max: f64, base: Color, opts: GraphOpts) {
    let cell_w = area.width as usize;
    let cell_h = area.height as usize;
    let (visible, leading) = window(data, cell_w);
    // A flat-zero series still gets its baseline; without the guard the
    // division below would produce NaN.
    let max = if max > 0.0 { max } else { f64::INFINITY };

    for x in 0..cell_w {
        let v = if x < leading {
            0.0
        } else {
            visible[x - leading]
        };
        let color = column_color(base, x, cell_w, opts);
        let normalized = (v / max).clamp(0.0, 1.0);
        let total_eighths = (normalized * cell_h as f64 * 8.0).round() as usize;

        for cy in 0..cell_h {
            let from_bottom = cell_h - 1 - cy;
            let eighths = total_eighths.saturating_sub(from_bottom * 8).min(8);
            let glyph = if eighths > 0 {
                BAR_GLYPHS[eighths]
            } else if from_bottom == 0 && opts.baseline {
                // Ground the chart so it reads as an empty axis rather
                // than a rendering failure.
                BAR_GLYPHS[1]
            } else {
                continue;
            };
            if let Some(cell) = buf.cell_mut((area.x + x as u16, area.y + cy as u16)) {
                cell.set_char(glyph).set_fg(color).set_bg(opts.bg);
            }
        }
    }
}

// ── braille dots ────────────────────────────────────────────────────────────

/// Bit position in a braille cell mask for each (sub_col, sub_row).
/// Braille dots are numbered 1–8 mapping to bits 0–7; the 4th row uses
/// dots 7 and 8 (bits 6 and 7), which is why this is not `row + col * 4`.
const BRAILLE_BIT: [[u8; 4]; 2] = [
    [0, 1, 2, 6], // sub_col 0: rows 0..=3 → dots 1, 2, 3, 7
    [3, 4, 5, 7], // sub_col 1: rows 0..=3 → dots 4, 5, 6, 8
];

const BRAILLE_BASE: u32 = 0x2800;

/// Mask lighting every dot in one sub-row, across both sub-columns.
///
/// Filling only `BRAILLE_BIT[0]` leaves every cell half-empty, which
/// reads as a sparse dot matrix rather than the filled area this style is
/// meant to produce — the bug netwatch shipped and fixed in v0.28.0.
fn row_mask(row_in_cell: usize) -> u8 {
    (1 << BRAILLE_BIT[0][row_in_cell]) | (1 << BRAILLE_BIT[1][row_in_cell])
}

fn render_dots(buf: &mut Buffer, area: Rect, data: &[f64], max: f64, base: Color, opts: GraphOpts) {
    let cell_w = area.width as usize;
    let cell_h = area.height as usize;
    let pix_h = cell_h * 4;
    let (visible, leading) = window(data, cell_w);
    let max = if max > 0.0 { max } else { f64::INFINITY };

    let mut masks = vec![vec![0u8; cell_w]; cell_h];

    for x in 0..cell_w {
        let v = if x < leading {
            0.0
        } else {
            visible[x - leading]
        };
        let normalized = (v / max).clamp(0.0, 1.0);
        // Height in braille pixels, counted from the bottom.
        let top = (normalized * (pix_h - 1) as f64).round() as usize;

        if v <= 0.0 && !opts.baseline {
            continue;
        }
        // `top == 0` still lights the bottom pixel row, which is what
        // gives a zero-valued column the same grounded baseline `bars`
        // draws with `▁`.
        for fill in 0..=top {
            let pix_y_from_top = (pix_h - 1) - fill;
            masks[pix_y_from_top / 4][x] |= row_mask(pix_y_from_top % 4);
        }
    }

    for (y, row) in masks.iter().enumerate() {
        for (x, &mask) in row.iter().enumerate() {
            if mask == 0 {
                continue;
            }
            let ch = char::from_u32(BRAILLE_BASE | mask as u32).unwrap_or(' ');
            let color = column_color(base, x, cell_w, opts);
            if let Some(cell) = buf.cell_mut((area.x + x as u16, area.y + y as u16)) {
                cell.set_char(ch).set_fg(color).set_bg(opts.bg);
            }
        }
    }
}

// ── fade + grid ─────────────────────────────────────────────────────────────

/// Linear-interpolate from `bg` toward `base` at fraction `alpha`.
///
/// Only meaningful in RGB. Under the `terminal` theme the base passes
/// through untouched: there is no 16-colour equivalent of a gradient, so
/// fade degrades to off rather than to wrong — and `bg` is `Reset` there,
/// so interpolating would fade toward an assumed black regardless of the
/// terminal's real background.
pub fn fade_color(base: Color, bg: Color, alpha: f32, defer_to_terminal: bool) -> Color {
    if defer_to_terminal {
        return base;
    }
    let alpha = alpha.clamp(0.0, 1.0);
    let (br, bgn, bb) = to_rgb_or_default(base, (255, 255, 255));
    let (gr, gg, gb) = to_rgb_or_default(bg, (0, 0, 0));
    Color::Rgb(
        lerp_u8(gr, br, alpha),
        lerp_u8(gg, bgn, alpha),
        lerp_u8(gb, bb, alpha),
    )
}

/// Standard xterm palette mapping. Necessary because several themes use
/// ANSI named colours; without it every faded chart would collapse to the
/// same grey gradient regardless of its series colour, which looks
/// identical to "fade off" at a glance.
fn to_rgb_or_default(c: Color, fallback: (u8, u8, u8)) -> (u8, u8, u8) {
    match c {
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Reset => fallback,
        Color::Black => (0, 0, 0),
        Color::Red => (170, 0, 0),
        Color::Green => (0, 170, 0),
        Color::Yellow => (170, 85, 0),
        Color::Blue => (0, 0, 170),
        Color::Magenta => (170, 0, 170),
        Color::Cyan => (0, 170, 170),
        Color::Gray => (170, 170, 170),
        Color::DarkGray => (85, 85, 85),
        Color::LightRed => (255, 85, 85),
        Color::LightGreen => (85, 255, 85),
        Color::LightYellow => (255, 255, 85),
        Color::LightBlue => (85, 85, 255),
        Color::LightMagenta => (255, 85, 255),
        Color::LightCyan => (85, 255, 255),
        Color::White => (255, 255, 255),
        _ => fallback,
    }
}

fn lerp_u8(a: u8, b: u8, t: f32) -> u8 {
    (a as f32 + (b as f32 - a as f32) * t)
        .round()
        .clamp(0.0, 255.0) as u8
}

/// Faint dot grid behind the chart. Drawn before the data so any data
/// cell overwrites it, leaving the grid visible only through the empty
/// regions.
fn render_grid(buf: &mut Buffer, area: Rect, bg: Color, defer_to_terminal: bool) {
    // The grid's base is a fixed grey, so under the terminal theme it
    // would survive as raw RGB. DarkGray is that palette's own answer to
    // "faint chrome" and tracks whatever the user's terminal defines.
    let grid_color = if defer_to_terminal {
        Color::DarkGray
    } else {
        fade_color(Color::Rgb(150, 150, 150), bg, 0.20, false)
    };
    let cell_w = area.width as usize;
    let cell_h = area.height as usize;
    if cell_w < GRID_MIN_W as usize || cell_h < GRID_MIN_H as usize {
        return;
    }
    let v_step = (cell_w / 4).max(2);
    let h_step = (cell_h / 4).max(1);

    for x in (v_step..cell_w).step_by(v_step) {
        for cy in 0..cell_h {
            if let Some(cell) = buf.cell_mut((area.x + x as u16, area.y + cy as u16)) {
                cell.set_char('·').set_fg(grid_color).set_bg(bg);
            }
        }
    }
    for y in (h_step..cell_h).step_by(h_step) {
        for cx in 0..cell_w {
            if let Some(cell) = buf.cell_mut((area.x + cx as u16, area.y + y as u16)) {
                // Leave intersections alone so they stay visually balanced.
                if cell.symbol() != "·" {
                    cell.set_char('·').set_fg(grid_color).set_bg(bg);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialises tests that mutate the process-wide style. Without it,
    /// `cargo test`'s thread pool lets one test's `set_by_name` leak into
    /// another's assertions.
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_style<T>(style: GraphStyle, f: impl FnOnce() -> T) -> T {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = active();
        set_by_name(style.name());
        let out = f();
        set_by_name(prev.name());
        out
    }

    fn draw(style: GraphStyle, area: Rect, data: &[f64], opts: GraphOpts) -> Buffer {
        with_style(style, || {
            let mut buf = Buffer::empty(area);
            render(&mut buf, area, data, Color::Green, opts);
            buf
        })
    }

    /// Draw against a fixed y-axis. Anything asserting on *height* has to
    /// use this — `render` auto-scales to the visible window, so a
    /// one-column area rescales its single sample to full height and the
    /// assertion would be about the scaling, not the glyph mapping.
    fn draw_scaled(
        style: GraphStyle,
        area: Rect,
        data: &[f64],
        max: f64,
        opts: GraphOpts,
    ) -> Buffer {
        with_style(style, || {
            let mut buf = Buffer::empty(area);
            render_with_max(&mut buf, area, data, max, Color::Green, opts);
            buf
        })
    }

    fn symbol(buf: &Buffer, x: u16, y: u16) -> String {
        buf.cell((x, y)).unwrap().symbol().to_string()
    }

    #[test]
    fn dots_fill_both_braille_sub_columns() {
        // Filling only BRAILLE_BIT[0] yields `⡇` and the chart reads as a
        // sparse dot matrix instead of a filled area — the bug netwatch
        // shipped in its first dots release.
        let area = Rect::new(0, 0, 1, 1);
        let buf = draw(GraphStyle::Dots, area, &[10.0], GraphOpts::default());
        assert_eq!(symbol(&buf, 0, 0), "⣿");
    }

    #[test]
    fn dots_partial_height_still_spans_the_cell() {
        // A low sample lights the bottom sub-row across *both* sub-columns
        // — dots 7 and 8 → 0x28C0 → `⣀` — not just the left one.
        let area = Rect::new(0, 0, 1, 1);
        let buf = draw_scaled(GraphStyle::Dots, area, &[1.0], 10.0, GraphOpts::default());
        assert_eq!(symbol(&buf, 0, 0), "⣀");
    }

    #[test]
    fn both_styles_ground_a_zero_series_on_the_baseline() {
        // diskwatch's charts have always shown a floor rather than a
        // void; switching styles must not change that.
        let area = Rect::new(0, 0, 4, 2);
        let zeros = [0.0; 4];

        let bars = draw(GraphStyle::Bars, area, &zeros, GraphOpts::default());
        assert_eq!(symbol(&bars, 0, 1), "▁", "bars lost its baseline");
        assert_eq!(symbol(&bars, 0, 0), " ", "bars filled above the baseline");

        let dots = draw(GraphStyle::Dots, area, &zeros, GraphOpts::default());
        assert_eq!(symbol(&dots, 0, 1), "⣀", "dots lost its baseline");
        assert_eq!(symbol(&dots, 0, 0), " ", "dots filled above the baseline");
    }

    #[test]
    fn a_short_series_anchors_to_the_right_edge() {
        // `now` is the right edge under both styles. Left-aligning a
        // warming ring would put the newest sample in the wrong half.
        let area = Rect::new(0, 0, 4, 1);
        for style in [GraphStyle::Bars, GraphStyle::Dots] {
            let buf = draw(style, area, &[0.0, 10.0], GraphOpts::default());
            let last = symbol(&buf, 3, 0);
            let first = symbol(&buf, 0, 0);
            assert_ne!(last, first, "{style:?} did not anchor to the right edge");
        }
    }

    #[test]
    fn dots_resolve_four_levels_where_bars_resolve_one() {
        // The whole reason to offer dots: one cell high, a single row
        // resolves four braille pixel rows. Values on the quarter points
        // of the axis must therefore give four distinct glyphs — the same
        // series under `bars` collapses to far fewer legible steps.
        let area = Rect::new(0, 0, 4, 1);
        let buf = draw_scaled(
            GraphStyle::Dots,
            area,
            &[0.0, 1.0, 2.0, 3.0],
            3.0,
            GraphOpts::default(),
        );
        let glyphs: Vec<String> = (0..4).map(|x| symbol(&buf, x, 0)).collect();
        let unique: std::collections::HashSet<&String> = glyphs.iter().collect();
        assert_eq!(
            unique.len(),
            4,
            "expected 4 distinct levels, got {glyphs:?}"
        );
        assert_eq!(glyphs, vec!["⣀", "⣤", "⣶", "⣿"], "levels out of order");
    }

    #[test]
    fn fade_off_paints_every_column_the_same_colour() {
        let area = Rect::new(0, 0, 8, 2);
        let data: Vec<f64> = (1..=8).map(|v| v as f64).collect();
        for style in [GraphStyle::Bars, GraphStyle::Dots] {
            let buf = draw(style, area, &data, GraphOpts::default());
            let left = buf.cell((0, 1)).unwrap().fg;
            let right = buf.cell((7, 1)).unwrap().fg;
            assert_eq!(left, right, "{style:?} faded with fade off");
        }
    }

    #[test]
    fn fade_on_dims_the_oldest_column() {
        let area = Rect::new(0, 0, 8, 2);
        let data = [5.0; 8];
        let opts = GraphOpts {
            fade: true,
            bg: Color::Rgb(0, 0, 0),
            ..GraphOpts::default()
        };
        for style in [GraphStyle::Bars, GraphStyle::Dots] {
            let buf = draw(style, area, &data, opts);
            let left = buf.cell((0, 1)).unwrap().fg;
            let right = buf.cell((7, 1)).unwrap().fg;
            assert_ne!(left, right, "{style:?} ignored fade");
            match (left, right) {
                (Color::Rgb(_, lg, _), Color::Rgb(_, rg, _)) => {
                    assert!(lg < rg, "{style:?} faded the wrong direction");
                }
                other => panic!("expected Rgb pair, got {other:?}"),
            }
        }
    }

    #[test]
    fn fade_passes_base_through_untouched_under_the_terminal_theme() {
        // Fade interpolates in RGB. Under `terminal` that would emit
        // 24-bit colour for every faded cell and silently undo the one
        // guarantee that theme makes.
        let base = Color::Cyan;
        for alpha in [0.0, 0.3, 0.55, 1.0] {
            assert_eq!(fade_color(base, Color::Reset, alpha, true), base);
        }
    }

    #[test]
    fn fade_color_endpoints_and_midpoint() {
        let base = Color::Rgb(200, 100, 50);
        let bg = Color::Rgb(0, 0, 0);
        assert_eq!(fade_color(base, bg, 1.0, false), base);
        assert_eq!(fade_color(base, bg, 0.0, false), bg);
        assert_eq!(fade_color(base, bg, 0.5, false), Color::Rgb(100, 50, 25));
        // Out-of-range alpha clamps rather than wrapping.
        assert_eq!(fade_color(base, bg, 2.0, false), base);
        assert_eq!(fade_color(base, bg, -1.0, false), bg);
    }

    #[test]
    fn named_colours_keep_their_hue_when_faded() {
        // Regression: without the palette mapping, Color::Green against a
        // Reset bg faded through greyscale — indistinguishable from fade
        // being off.
        let dim = fade_color(Color::Green, Color::Reset, 0.3, false);
        match dim {
            Color::Rgb(r, g, b) => {
                assert_eq!((r, b), (0, 0), "hue drifted off green");
                assert!(g > 0 && g < 170, "green should dim, not vanish");
            }
            other => panic!("expected Rgb, got {other:?}"),
        }
    }

    #[test]
    fn by_name_falls_back_to_bars_and_accepts_aliases() {
        assert_eq!(by_name("nonsense"), GraphStyle::Bars);
        assert_eq!(by_name(""), GraphStyle::Bars);
        assert_eq!(by_name("bars"), GraphStyle::Bars);
        assert_eq!(by_name("DOTS"), GraphStyle::Dots);
        assert_eq!(by_name("braille"), GraphStyle::Dots);
        assert_eq!(by_name("btop"), GraphStyle::Dots);
    }

    #[test]
    fn name_roundtrips_and_cycle_returns_home() {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for n in GRAPH_STYLE_NAMES {
            assert_eq!(by_name(n).name(), *n);
        }
        let start = name();
        cycle();
        assert_ne!(name(), start);
        cycle();
        assert_eq!(name(), start, "cycling twice must return to the start");
    }

    #[test]
    fn zero_sized_areas_are_a_no_op() {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut buf = Buffer::empty(Rect::new(0, 0, 4, 2));
        for area in [Rect::new(0, 0, 0, 2), Rect::new(0, 0, 4, 0)] {
            render(
                &mut buf,
                area,
                &[1.0, 2.0],
                Color::Green,
                GraphOpts::default(),
            );
        }
    }
}
