//! Pluggable graph rendering for every chart in the app.
//!
//! Mirrors [`crate::ui::theme`]: a `GraphStyle` enum with a `by_name`
//! lookup and a process-wide `ACTIVE` cell, plus a `render` entry point
//! that dispatches to a per-style implementation. Every chart in the UI —
//! the Overview aggregate chart, the IO tab's per-device chart, and both
//! Lite charts plus its row sparklines — routes through here, so one
//! setting toggles them all.
//!
//! Ported from netwatch's `graph.rs`, so the two tools draw the same
//! graph; the series type is `f64` here because every diskwatch history is
//! a rate, not a count.
//!
//! ## Styles
//! - `dots` — the default. A braille area plot: two samples per cell
//!   column and four pixel rows per cell, so a graph carries twice the
//!   history of `bars` at four times the vertical resolution.
//! - `bars` — stacked eighth blocks, for fonts with no braille.
//!
//! ## Fade
//! `GraphOpts.fade` colours every cell by how high it sits in the plot:
//! dim at the baseline, the series colour through the body, bright at the
//! peak. That is btop's gradient, and what makes a filled area read as
//! depth rather than as a solid block. Under the `terminal` theme it is
//! built from the RGB the terminal reports for its own palette (see
//! [`crate::ui::term_palette`]), or steps between two palette tokens when
//! the terminal doesn't say.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use std::sync::RwLock;

use crate::ui::term_palette::{self, Palette};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphStyle {
    /// Solid-colour stacked blocks, for terminals whose font has no
    /// braille coverage.
    Bars,
    /// btop-style braille area plot: twice the samples and 4× the
    /// vertical resolution of `Bars`. The default.
    Dots,
}

pub const GRAPH_STYLE_NAMES: &[&str] = &["bars", "dots"];

/// The style and fade diskwatch uses when nothing says otherwise. `main.rs`
/// and the `--write-config` file read these, and the globals below start
/// at them, so the three can't drift.
pub const DEFAULT_STYLE: GraphStyle = GraphStyle::Dots;
pub const DEFAULT_FADE: bool = true;

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

    /// Samples one cell column shows: braille packs two, blocks one.
    pub fn samples_per_column(self) -> usize {
        match self {
            GraphStyle::Dots => 2,
            GraphStyle::Bars => 1,
        }
    }
}

// ── Global setting ──────────────────────────────────────────────────────────

/// Style and fade live in a global for the same reason the theme does:
/// the palette accessors and every chart call site can read them without
/// threading a reference through every render function.
static ACTIVE: RwLock<GraphStyle> = RwLock::new(DEFAULT_STYLE);
static FADE: RwLock<bool> = RwLock::new(DEFAULT_FADE);

pub fn active() -> GraphStyle {
    *ACTIVE.read().expect("graph style lock poisoned")
}

pub fn set_by_name(name: &str) {
    *ACTIVE.write().expect("graph style lock poisoned") = by_name(name);
}

pub fn name() -> &'static str {
    active().name()
}

/// Samples per cell column under the active style. A caller that trims a
/// history to the chart's width needs this many per column, or a braille
/// chart draws its data in the right half only.
pub fn samples_per_column() -> usize {
    active().samples_per_column()
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

/// Serializes tests (crate-wide) that mutate the process-wide graph style
/// and restores whatever was active before. Crate-visible for the same
/// reason `theme::exclusive_theme` is: `ui::braille`'s dispatcher also reads
/// this global, and a second module racing this one would reintroduce the
/// exact flake this exists to prevent.
#[cfg(test)]
static ACTIVE_STYLE_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Run `f` with the process-wide style pinned to `style`, restoring
/// whatever was active before once `f` returns.
#[cfg(test)]
pub(crate) fn with_style<T>(style: GraphStyle, f: impl FnOnce() -> T) -> T {
    let _guard = ACTIVE_STYLE_TESTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let prev = active();
    set_by_name(style.name());
    let out = f();
    set_by_name(prev.name());
    out
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
    /// Colour each cell by its height in the plot — dim at the baseline,
    /// the series colour through the body, bright at the peak. Off draws
    /// the series in one flat colour.
    pub fade: bool,
    /// Theme background, the colour a fade's dim end leans toward.
    pub bg: Color,
    /// True under the `terminal` theme, where `bg` is `Reset` and the
    /// series colours are palette slots with no RGB of their own. The fade
    /// then comes from the terminal's reported palette, or from two palette
    /// tokens — see [`terminal_gradient`].
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

// ── Entry points ────────────────────────────────────────────────────────────

/// Render `data` into `area`, auto-scaling the y-axis to the visible window.
pub fn render(buf: &mut Buffer, area: Rect, data: &[f64], color: Color, opts: GraphOpts) {
    let shown = area.width as usize * samples_per_column();
    let start = data.len().saturating_sub(shown);
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
    match active() {
        GraphStyle::Bars => render_bars(buf, area, data, max, color, opts),
        GraphStyle::Dots => render_dots(buf, area, data, max, color, opts),
    }
}

/// Take the rightmost `want` samples and report how many leading slots
/// have no data behind them.
fn window(data: &[f64], want: usize) -> (&[f64], usize) {
    let start = data.len().saturating_sub(want);
    let visible = &data[start..];
    (visible, want.saturating_sub(visible.len()))
}

/// Where the middle of cell row `cy` sits in a plot `h` rows tall, from 0.0
/// at the baseline to 1.0 at the top. The midpoint, so a one-row sparkline
/// samples the middle of the gradient — the series colour — rather than
/// its dim end.
fn cell_height(cy: usize, h: usize) -> f32 {
    ((h - cy) as f32 - 0.5) / h as f32
}

// ── bars ────────────────────────────────────────────────────────────────────

// `pub(crate)`: the dense view's braille module reuses this exact glyph
// ladder so its own bars-style rendering matches this one pixel-for-pixel
// rather than drifting into a second, slightly different "bars" look.
pub(crate) const BAR_GLYPHS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

fn render_bars(buf: &mut Buffer, area: Rect, data: &[f64], max: f64, base: Color, opts: GraphOpts) {
    let cell_w = area.width as usize;
    let cell_h = area.height as usize;
    let (visible, leading) = window(data, cell_w);
    // A flat-zero series still gets its baseline; without the guard the
    // division below would produce NaN.
    let max = if max > 0.0 { max } else { f64::INFINITY };
    let gradient = gradient(base, opts);

    for x in 0..cell_w {
        let v = if x < leading {
            0.0
        } else {
            visible[x - leading]
        };
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
            // Each cell takes its colour from where it sits in the plot,
            // so a bar darkens toward its base and brightens at its tip.
            let color = gradient
                .as_ref()
                .map_or(base, |g| g.at(cell_height(cy, cell_h)));
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

/// Two samples per cell column, one per braille sub-column, each filled
/// from the bottom to its height in braille pixels.
fn render_dots(buf: &mut Buffer, area: Rect, data: &[f64], max: f64, base: Color, opts: GraphOpts) {
    let cell_w = area.width as usize;
    let cell_h = area.height as usize;
    let pix_h = cell_h * 4;
    let (visible, leading) = window(data, cell_w * 2);
    let max = if max > 0.0 { max } else { f64::INFINITY };
    let gradient = gradient(base, opts);

    // Pixel rows lit from the bottom, 0 for none. Zero lights the bottom
    // row when there's a baseline, which grounds a quiet series the same
    // way `bars` does with `▁`; the top of the axis lights all of them.
    let lit = |i: usize| -> usize {
        let v = if i < leading {
            0.0
        } else {
            visible[i - leading]
        };
        if v <= 0.0 && !opts.baseline {
            return 0;
        }
        let normalized = (v / max).clamp(0.0, 1.0);
        (normalized * (pix_h - 1) as f64).round() as usize + 1
    };

    for cx in 0..cell_w {
        let (left, right) = (lit(cx * 2), lit(cx * 2 + 1));
        if left == 0 && right == 0 {
            continue;
        }
        for cy in 0..cell_h {
            let mut bits: u8 = 0;
            for (s, (l_dot, r_dot)) in BRAILLE_BIT[0].iter().zip(BRAILLE_BIT[1]).enumerate() {
                // This pixel row's height, counted from 1 at the bottom.
                let depth = pix_h - (cy * 4 + s);
                if left >= depth {
                    bits |= 1 << l_dot;
                }
                if right >= depth {
                    bits |= 1 << r_dot;
                }
            }
            if bits == 0 {
                continue;
            }
            let ch = char::from_u32(BRAILLE_BASE | bits as u32).unwrap_or(' ');
            let color = gradient
                .as_ref()
                .map_or(base, |g| g.at(cell_height(cy, cell_h)));
            if let Some(cell) = buf.cell_mut((area.x + cx as u16, area.y + cy as u16)) {
                cell.set_char(ch).set_fg(color).set_bg(opts.bg);
            }
        }
    }
}

// ── gradient ────────────────────────────────────────────────────────────────

/// Bottom of the gradient: the fraction of the series colour a cell on the
/// baseline starts from, before the contrast floor lifts it. 0.30 keeps a
/// quiet series visible without letting it compete with a busy one.
const MIN_FADE_ALPHA: f32 = 0.30;

/// The least a gradient's dimmest stop may contrast with the background.
/// An area graph puts most of its ink near the baseline, so that end has to
/// be readable, not only the peaks.
const MIN_RAMP_CONTRAST: f64 = 4.0;

/// The least the series colour may contrast with the background. It has to
/// clear [`MIN_RAMP_CONTRAST`] by enough for the stops below it to climb.
const MIN_SERIES_CONTRAST: f64 = 6.0;

/// What a gradient is measured against when the background is the
/// terminal's and the terminal didn't report it.
const ASSUMED_DARK_BG: (u8, u8, u8) = (0x1a, 0x1b, 0x26);

/// Colour stops, sampled 0.0..=1.0 from the baseline to the top of a plot.
///
/// A gradient of repeated palette tokens is the 16-colour step: [`lerp`] returns its lower input for
/// anything that isn't RGB, so sampling lands on a token rather than
/// inventing a colour between two.
#[derive(Debug, Clone, PartialEq)]
pub struct Gradient {
    stops: Vec<Color>,
}

impl Gradient {
    pub fn new(stops: Vec<Color>) -> Self {
        Self { stops }
    }

    /// Sample at `f`, clamped to 0.0..=1.0.
    pub fn at(&self, f: f32) -> Color {
        let f = if f.is_finite() {
            f.clamp(0.0, 1.0)
        } else {
            0.0
        };
        match self.stops.len() {
            0 => Color::Reset,
            1 => self.stops[0],
            n => {
                let t = f * (n - 1) as f32;
                let i = t.floor() as usize;
                if i >= n - 1 {
                    return self.stops[n - 1];
                }
                lerp(self.stops[i], self.stops[i + 1], t - i as f32)
            }
        }
    }
}

/// The gradient a chart in `base` paints with, or `None` when fade is off.
pub fn gradient(base: Color, opts: GraphOpts) -> Option<Gradient> {
    if !opts.fade {
        return None;
    }
    Some(if opts.terminal_palette {
        terminal_gradient(base, term_palette::get())
    } else {
        magnitude(base, opts.bg)
    })
}

/// The gradient under the `terminal` theme.
///
/// With the palette the terminal reported, it is the full blend, built from
/// the terminal's own RGB for `base` and for its background. That keeps
/// the theme's promise: the colours are still the ones the user chose, and
/// diskwatch only adds the shades between them.
///
/// Without one, two palette tokens: the series colour along the body of the
/// plot and its bright variant over the top quarter, which is what btop
/// draws on a 16-colour terminal. A one-row sparkline samples the middle and
/// stays the series colour.
pub fn terminal_gradient(base: Color, palette: Option<&Palette>) -> Gradient {
    let reported = palette.and_then(|p| Some((p.rgb(base)?, p.bg?)));
    match reported {
        Some(((r, g, b), (br, bgc, bb))) => magnitude(Color::Rgb(r, g, b), Color::Rgb(br, bgc, bb)),
        None => {
            let bright = bright_token(base);
            Gradient::new(vec![base, base, base, bright, bright])
        }
    }
}

/// The palette's bright variant of a named or indexed colour.
///
/// Already-bright tokens and `Reset` come back unchanged: a series drawn
/// in the terminal's foreground has no brighter self, and inventing one is
/// what the palette-deferring theme promises not to do.
pub fn bright_token(c: Color) -> Color {
    match c {
        Color::Black => Color::DarkGray,
        Color::Red => Color::LightRed,
        Color::Green => Color::LightGreen,
        Color::Yellow => Color::LightYellow,
        Color::Blue => Color::LightBlue,
        Color::Magenta => Color::LightMagenta,
        Color::Cyan => Color::LightCyan,
        Color::Gray => Color::White,
        Color::DarkGray => Color::Gray,
        Color::Indexed(n) if n < 8 => Color::Indexed(n + 8),
        other => other,
    }
}

/// btop's gradient: dim at the baseline, the series colour in the middle,
/// and at the peak moved further from the background — lighter on a dark
/// one, darker on a light one.
///
/// A cell's colour comes from how high it sits, so the shape of the data is
/// legible even where the fill is continuous. diskwatch used to fade along
/// the time axis instead, old columns dim and new ones bright, which dims
/// the history rather than describing it and leaves a steady series a flat
/// wall.
pub fn magnitude(base: Color, bg: Color) -> Gradient {
    // Resolve the series colour to RGB first: a named colour would pass
    // through `lerp` unchanged, and the top stops would all collapse to it.
    let (r, g, b) = to_rgb_or_default(base, (255, 255, 255));
    let behind = to_rgb_or_default(bg, ASSUMED_DARK_BG);
    let base = with_contrast(Color::Rgb(r, g, b), behind, MIN_SERIES_CONTRAST);
    // The floor moves up the fade rather than toward white, so it keeps the hue.
    let floor = (0..=70)
        .map(|step| MIN_FADE_ALPHA + step as f32 / 100.0)
        .find(|&alpha| contrast(fade_color(base, bg, alpha), behind) >= MIN_RAMP_CONTRAST)
        .unwrap_or(1.0);
    // The peak moves away from the background as the floor moves toward it.
    // On a light background it darkens, and less far, so it keeps the hue
    // rather than going to black.
    let (away, near, far) = if luminance(behind) < 0.18 {
        (Color::Rgb(255, 255, 255), 0.42, 0.76)
    } else {
        (Color::Rgb(0, 0, 0), 0.25, 0.5)
    };
    Gradient::new(vec![
        fade_color(base, bg, floor),
        fade_color(base, bg, floor + (1.0 - floor) * 0.46),
        base,
        lerp(base, away, near),
        lerp(base, away, far),
    ])
}

/// `c`, moved away from the background only as far as `min` needs.
fn with_contrast(c: Color, behind: (u8, u8, u8), min: f64) -> Color {
    let away = if luminance(behind) < 0.18 {
        Color::Rgb(255, 255, 255)
    } else {
        Color::Rgb(0, 0, 0)
    };
    (0..=100)
        .map(|step| lerp(c, away, step as f32 / 100.0))
        .find(|&moved| contrast(moved, behind) >= min)
        .unwrap_or(away)
}

/// WCAG contrast ratio.
fn contrast(c: Color, behind: (u8, u8, u8)) -> f64 {
    let a = luminance(to_rgb_or_default(c, behind));
    let b = luminance(behind);
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

/// WCAG relative luminance.
fn luminance((r, g, b): (u8, u8, u8)) -> f64 {
    let linear = |v: u8| {
        let v = v as f64 / 255.0;
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
}

/// Linear blend, RGB only. Anything else returns `a` unchanged rather than
/// guessing at what a palette slot looks like, which is what makes a
/// gradient of palette tokens step instead of blend.
pub fn lerp(a: Color, b: Color, f: f32) -> Color {
    match (a, b) {
        (Color::Rgb(ar, ag, ab), Color::Rgb(br, bg, bb)) => {
            let f = f.clamp(0.0, 1.0);
            Color::Rgb(lerp_u8(ar, br, f), lerp_u8(ag, bg, f), lerp_u8(ab, bb, f))
        }
        _ => a,
    }
}

/// Linear-interpolate from `bg` toward `base` at fraction `alpha`, in RGB.
/// Named colours resolve through the standard palette first, and a `Reset`
/// background counts as black.
pub fn fade_color(base: Color, bg: Color, alpha: f32) -> Color {
    let alpha = alpha.clamp(0.0, 1.0);
    let (br, bgn, bb) = to_rgb_or_default(base, (255, 255, 255));
    let (gr, gg, gb) = to_rgb_or_default(bg, (0, 0, 0));
    Color::Rgb(
        lerp_u8(gr, br, alpha),
        lerp_u8(gg, bgn, alpha),
        lerp_u8(gb, bb, alpha),
    )
}

/// Standard VGA palette mapping. Several themes use ANSI named colours;
/// without it every faded chart would collapse to the same grey gradient
/// regardless of its series colour.
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

#[cfg(test)]
mod tests {
    use super::*;

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

    fn fg(buf: &Buffer, x: u16, y: u16) -> Color {
        buf.cell((x, y)).unwrap().fg
    }

    fn rgb(c: Color) -> (u8, u8, u8) {
        match c {
            Color::Rgb(r, g, b) => (r, g, b),
            other => panic!("expected Rgb, got {other:?}"),
        }
    }

    const DARK: Color = Color::Rgb(0x0c, 0x14, 0x18);

    fn faded(bg: Color) -> GraphOpts {
        GraphOpts {
            fade: true,
            bg,
            ..GraphOpts::default()
        }
    }

    #[test]
    fn dots_fill_both_braille_sub_columns() {
        // Filling only BRAILLE_BIT[0] yields `⡇` and the chart reads as a
        // sparse dot matrix instead of a filled area — the bug netwatch
        // shipped in its first dots release.
        let area = Rect::new(0, 0, 1, 1);
        let buf = draw(GraphStyle::Dots, area, &[10.0, 10.0], GraphOpts::default());
        assert_eq!(symbol(&buf, 0, 0), "⣿");
    }

    #[test]
    fn dots_draw_two_samples_per_cell() {
        // The left sub-column is the older sample. A quiet one beside a
        // full one is a half-lit cell, not an average of the two.
        let area = Rect::new(0, 0, 1, 1);
        let buf = draw_scaled(
            GraphStyle::Dots,
            area,
            &[0.0, 10.0],
            10.0,
            GraphOpts::default(),
        );
        // Dot 7 (the left baseline) plus dots 4, 5, 6 and 8.
        assert_eq!(symbol(&buf, 0, 0), "⣸");
    }

    #[test]
    fn dots_partial_height_still_spans_the_cell() {
        // A low sample lights the bottom sub-row across *both* sub-columns
        // — dots 7 and 8 → 0x28C0 → `⣀` — not just the left one.
        let area = Rect::new(0, 0, 1, 1);
        let buf = draw_scaled(
            GraphStyle::Dots,
            area,
            &[1.0, 1.0],
            10.0,
            GraphOpts::default(),
        );
        assert_eq!(symbol(&buf, 0, 0), "⣀");
    }

    #[test]
    fn both_styles_ground_a_zero_series_on_the_baseline() {
        // diskwatch's charts have always shown a floor rather than a
        // void; switching styles must not change that.
        let area = Rect::new(0, 0, 4, 2);
        let zeros = [0.0; 8];

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
    fn dots_show_twice_the_history_of_bars() {
        // Eight samples across four columns: bars keeps the last four, and
        // dots all eight. The spike in sample 1 is only on screen under dots.
        let area = Rect::new(0, 0, 4, 1);
        let data = [0.0, 9.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0];
        let dots = draw_scaled(GraphStyle::Dots, area, &data, 9.0, GraphOpts::default());
        assert_eq!(symbol(&dots, 0, 0), "⣸", "dots dropped the oldest samples");
        let bars = draw_scaled(GraphStyle::Bars, area, &data, 9.0, GraphOpts::default());
        assert!(
            (0..4).all(|x| symbol(&bars, x, 0) != "█"),
            "bars drew a sample it has no column for"
        );
    }

    #[test]
    fn dots_resolve_four_levels_where_bars_resolve_one() {
        // The whole reason to offer dots: one cell high, a single row
        // resolves four braille pixel rows. Values on the quarter points
        // of the axis must therefore give four distinct glyphs.
        let area = Rect::new(0, 0, 4, 1);
        let buf = draw_scaled(
            GraphStyle::Dots,
            area,
            &[0.0, 0.0, 1.0, 1.0, 2.0, 2.0, 3.0, 3.0],
            3.0,
            GraphOpts::default(),
        );
        let glyphs: Vec<String> = (0..4).map(|x| symbol(&buf, x, 0)).collect();
        assert_eq!(glyphs, vec!["⣀", "⣤", "⣶", "⣿"], "levels out of order");
    }

    #[test]
    fn fade_off_paints_every_cell_the_same_colour() {
        let area = Rect::new(0, 0, 8, 4);
        let data = [10.0; 16];
        for style in [GraphStyle::Bars, GraphStyle::Dots] {
            let buf = draw(style, area, &data, GraphOpts::default());
            for (x, y) in [(0, 0), (0, 3), (7, 0), (7, 3)] {
                assert_eq!(
                    fg(&buf, x, y),
                    Color::Green,
                    "{style:?} faded with fade off"
                );
            }
        }
    }

    #[test]
    fn fade_brightens_with_height_not_with_time() {
        // btop's gradient runs up the plot. The old fade ran along it,
        // dimming the history; a steady series is now the same colour from
        // one end of the window to the other.
        let area = Rect::new(0, 0, 8, 4);
        let data = [10.0; 16];
        for style in [GraphStyle::Bars, GraphStyle::Dots] {
            let buf = draw(style, area, &data, faded(DARK));
            for y in 0..4 {
                assert_eq!(fg(&buf, 0, y), fg(&buf, 7, y), "{style:?} faded along time");
            }
            let lum = |y| luminance(rgb(fg(&buf, 3, y)));
            assert!(
                lum(3) < lum(2) && lum(2) < lum(1) && lum(1) < lum(0),
                "{style:?} doesn't brighten toward the top"
            );
        }
    }

    #[test]
    fn a_one_row_sparkline_is_the_series_colour() {
        // One row sits at the middle of the gradient, which is the series
        // colour, not the dim end.
        let area = Rect::new(0, 0, 4, 1);
        let buf = draw(GraphStyle::Dots, area, &[5.0; 8], faded(DARK));
        let g = magnitude(Color::Green, DARK);
        assert_eq!(fg(&buf, 0, 0), g.at(0.5));
        assert_eq!(g.at(0.5), g.stops[2]);
    }

    #[test]
    fn the_dim_end_stays_legible() {
        // Most of an area graph's ink is near the baseline, so the bottom
        // stop has to clear the floor on every theme's background.
        for theme in crate::ui::theme::THEME_NAMES {
            let t = crate::ui::theme::by_name(theme);
            if t.name == "terminal" {
                continue;
            }
            for base in [t.green, t.cyan] {
                let g = magnitude(base, t.bg);
                let behind = to_rgb_or_default(t.bg, ASSUMED_DARK_BG);
                let c = contrast(g.at(0.0), behind);
                assert!(c >= MIN_RAMP_CONTRAST - 0.05, "{theme}: floor is {c:.2}:1");
            }
        }
    }

    #[test]
    fn the_peak_moves_away_from_a_light_background() {
        // Lightening toward white on a pale background fades the peak into
        // it. There, the peak darkens instead.
        let paper = Color::Rgb(0xf5, 0xf5, 0xf2);
        let g = magnitude(Color::Rgb(0x1f, 0x88, 0x3d), paper);
        assert!(luminance(rgb(g.at(1.0))) < luminance(rgb(g.at(0.5))));
        let g = magnitude(Color::Rgb(0x1f, 0x88, 0x3d), DARK);
        assert!(luminance(rgb(g.at(1.0))) > luminance(rgb(g.at(0.5))));
    }

    #[test]
    fn terminal_fade_without_a_palette_steps_between_two_tokens() {
        // Nothing reported, so nothing to blend: the series colour, then
        // its bright variant over the top quarter. No RGB.
        let g = terminal_gradient(Color::Green, None);
        for i in 0..=20 {
            let c = g.at(i as f32 / 20.0);
            assert!(
                c == Color::Green || c == Color::LightGreen,
                "invented {c:?}"
            );
        }
        assert_eq!(g.at(0.5), Color::Green);
        assert_eq!(g.at(1.0), Color::LightGreen);
    }

    #[test]
    fn terminal_fade_blends_the_colours_the_terminal_reported() {
        // The terminal says its green is #5cd989 on #0c1418. The gradient
        // is built from those, not from a standard palette's green.
        let mut p = Palette::default();
        p.slots[2] = Some((0x5c, 0xd9, 0x89));
        p.bg = Some((0x0c, 0x14, 0x18));
        let g = terminal_gradient(Color::Green, Some(&p));
        assert_eq!(g, magnitude(Color::Rgb(0x5c, 0xd9, 0x89), DARK));
        let (r, gr, b) = rgb(g.at(0.0));
        assert!(gr > r && gr > b, "lost the reported hue: {:?}", (r, gr, b));

        // A slot the terminal didn't report falls back to tokens.
        let g = terminal_gradient(Color::Cyan, Some(&p));
        assert_eq!(g.at(1.0), Color::LightCyan);
    }

    #[test]
    fn named_colours_get_a_real_gradient() {
        // `lerp` passes named colours through, so without resolving the
        // series colour to RGB first the top three stops were one token.
        let g = magnitude(Color::Green, Color::Reset);
        assert_ne!(g.at(0.5), g.at(0.75));
        assert_ne!(g.at(0.75), g.at(1.0));
    }

    #[test]
    fn fade_color_endpoints_and_midpoint() {
        let base = Color::Rgb(200, 100, 50);
        let bg = Color::Rgb(0, 0, 0);
        assert_eq!(fade_color(base, bg, 1.0), base);
        assert_eq!(fade_color(base, bg, 0.0), bg);
        assert_eq!(fade_color(base, bg, 0.5), Color::Rgb(100, 50, 25));
        // Out-of-range alpha clamps rather than wrapping.
        assert_eq!(fade_color(base, bg, 2.0), base);
        assert_eq!(fade_color(base, bg, -1.0), bg);
    }

    #[test]
    fn gradient_survives_nan() {
        let g = magnitude(Color::Green, DARK);
        assert_eq!(g.at(f32::NAN), g.at(0.0));
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
        let _g = ACTIVE_STYLE_TESTS.lock().unwrap_or_else(|e| e.into_inner());
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
        let _g = ACTIVE_STYLE_TESTS.lock().unwrap_or_else(|e| e.into_inner());
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
