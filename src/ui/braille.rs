//! btop-grade drawing primitives for the dense view.
//!
//! A port of the design handoff's `nb-braille.jsx` onto a ratatui `Buffer`.
//! Three things separate btop's graphs from a bar chart, and all three are
//! here:
//!
//! 1. **Braille cells** — 2 wide × 4 tall subpixels per character cell, so a
//!    120-column graph carries 240 samples at 4× the vertical resolution of
//!    block glyphs.
//! 2. **Vertical gradient fill** — a cell is coloured by its HEIGHT in the
//!    graph, not by the series it belongs to, so a spike's severity is
//!    readable without measuring it against the axis.
//! 3. **Derived axis ceilings** on a tight ladder, with the scale printed.
//!
//! The existing [`crate::ui::graph`] `dots` style is a different thing: one
//! sample per *cell* column, coloured by horizontal position for the fade.
//! This module is two samples per cell column, coloured by height, with a
//! mirrored mode for the downward-growing write graph — none of which the
//! shared module needs, and adding them there would complicate every caller.

#![allow(dead_code)]

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use crate::ui::palette;
use crate::ui::theme;

// ── braille ────────────────────────────────────────────────────────────────

/// Bit position for each (sub-column, sub-row). Braille dots are numbered 1–8
/// onto bits 0–7, and the fourth row uses dots 7 and 8 — which is why this is
/// not `row + col * 4`.
const BR_BIT: [[u8; 4]; 2] = [
    [0, 1, 2, 6], // left column,  top → bottom
    [3, 4, 5, 7], // right column, top → bottom
];
const BR_BASE: u32 = 0x2800;

// ── ramps ──────────────────────────────────────────────────────────────────

/// Which colour vocabulary a graph speaks.
///
/// **Magnitude** ramps run cool → bright, because high throughput is *busy*,
/// not *bad*: a saturated device during a restore is working. **Bounded-bad**
/// runs green → red, and is used only where the value has a ceiling that means
/// something — utilisation, capacity, wear, latency bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ramp {
    /// Read / inbound. Green, matching the Lite family.
    Read,
    /// Write / outbound. Cyan.
    Write,
    /// Bounded-bad. Green → amber → red.
    Load,
}

const RAMP_READ: [(u8, u8, u8); 5] = [
    (0x13, 0x4f, 0x42),
    (0x1f, 0x7d, 0x58),
    (0x3b, 0xb6, 0x73),
    (0x5c, 0xd9, 0x89),
    (0xa6, 0xf2, 0xc0),
];
const RAMP_WRITE: [(u8, u8, u8); 5] = [
    (0x10, 0x3f, 0x52),
    (0x1c, 0x6f, 0x8c),
    (0x3a, 0xa9, 0xc9),
    (0x5f, 0xdc, 0xff),
    (0xb6, 0xed, 0xff),
];
const RAMP_LOAD: [(u8, u8, u8); 5] = [
    (0x5c, 0xd9, 0x89),
    (0x9f, 0xd0, 0x7a),
    (0xf0, 0xc0, 0x60),
    (0xf3, 0x9a, 0x6a),
    (0xff, 0x78, 0x78),
];

impl Ramp {
    fn stops(self) -> &'static [(u8, u8, u8); 5] {
        match self {
            Ramp::Read => &RAMP_READ,
            Ramp::Write => &RAMP_WRITE,
            Ramp::Load => &RAMP_LOAD,
        }
    }

    /// The colour at `f` ∈ 0..1 along the ramp.
    ///
    /// Under the `terminal` theme this degrades to a themed ANSI slot rather
    /// than interpolating RGB. A gradient has no 16-colour equivalent, and the
    /// whole point of that theme is that diskwatch pins no colours of its own —
    /// so magnitude ramps collapse to their base hue and the bounded-bad ramp
    /// keeps its meaning through green / yellow / red thresholds.
    pub fn at(self, f: f64) -> Color {
        let f = f.clamp(0.0, 1.0);
        if theme::active().name == "terminal" {
            return match self {
                Ramp::Read => palette::green(),
                Ramp::Write => palette::cyan(),
                Ramp::Load => {
                    if f < 0.5 {
                        palette::green()
                    } else if f < 0.8 {
                        palette::yellow()
                    } else {
                        palette::red()
                    }
                }
            };
        }
        let stops = self.stops();
        let t = f * (stops.len() - 1) as f64;
        let i = t.floor() as usize;
        if i >= stops.len() - 1 {
            let (r, g, b) = stops[stops.len() - 1];
            return Color::Rgb(r, g, b);
        }
        let frac = t - i as f64;
        let (ar, ag, ab) = stops[i];
        let (br, bg, bb) = stops[i + 1];
        let lerp = |a: u8, b: u8| (a as f64 + (b as f64 - a as f64) * frac).round() as u8;
        Color::Rgb(lerp(ar, br), lerp(ag, bg), lerp(ab, bb))
    }
}

// ── text ───────────────────────────────────────────────────────────────────

/// Draw `s` at (`x`, `y`), one cell per char, clipped to `buf`. Returns the
/// column after the string.
///
/// Clipping matters: every right-aligned label in the 2.0 layout is positioned
/// by subtracting its own length, so a fixture or a hostname one character
/// longer than expected would otherwise write outside the buffer.
pub fn text(buf: &mut Buffer, x: u16, y: u16, s: &str, fg: Color, bold: bool) -> u16 {
    let area = buf.area;
    let mut cx = x;
    for ch in s.chars() {
        if cx >= area.right() || y >= area.bottom() {
            break;
        }
        if cx >= area.x && y >= area.y {
            if let Some(cell) = buf.cell_mut((cx, y)) {
                cell.set_char(ch).set_fg(fg);
                if bold {
                    cell.set_style(Style::default().add_modifier(Modifier::BOLD));
                }
            }
        }
        cx = cx.saturating_add(1);
    }
    cx
}

/// Draw `s` so that it ENDS at column `x_end`.
pub fn text_right(buf: &mut Buffer, x_end: u16, y: u16, s: &str, fg: Color, bold: bool) {
    let w = s.chars().count() as u16;
    let x = x_end.saturating_sub(w.saturating_sub(1));
    text(buf, x, y, s, fg, bold);
}

/// Set the background of a rect without touching its glyphs.
pub fn tint(buf: &mut Buffer, x: u16, y: u16, w: u16, h: u16, bg: Color) {
    for dy in 0..h {
        for dx in 0..w {
            if let Some(cell) = buf.cell_mut((x + dx, y + dy)) {
                cell.set_bg(bg);
            }
        }
    }
}

// ── graph ──────────────────────────────────────────────────────────────────

/// Braille area graph. `vals` are 0..1 and up to `2 × area.width` long.
///
/// `flip` grows the fill DOWNWARD from the top edge, which is what mirrors the
/// write graph about the shared time axis. The gradient always runs low → high
/// along the direction of growth, so both halves brighten as traffic climbs.
///
/// `band` describes the range the caller's values occupy, and only matters for
/// a single-row graph — see the note on colouring below.
pub fn graph(
    buf: &mut Buffer,
    area: Rect,
    vals: &[f64],
    ramp: Ramp,
    flip: bool,
    band: Option<(f64, f64)>,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let (b_lo, b_hi) = band.unwrap_or((0.0, 1.0));
    let h = area.height as usize;
    let sub_h = h * 4;

    for cx in 0..area.width as usize {
        let lv = vals.get(cx * 2).copied().unwrap_or(0.0).max(0.0);
        let rv = vals.get(cx * 2 + 1).copied().unwrap_or(0.0).max(0.0);
        // Floor any non-zero sample to at least one dot. Without this, a value
        // below 1/(2·sub_h) rounds to zero and the column is skipped — so on a
        // single-row sparkline everything under 12.5% renders blank, and a
        // quiet-but-active series reads as no data at all.
        let lh = if lv > 0.0 {
            ((lv * sub_h as f64).round() as usize).max(1)
        } else {
            0
        };
        let rh = if rv > 0.0 {
            ((rv * sub_h as f64).round() as usize).max(1)
        } else {
            0
        };
        if lh == 0 && rh == 0 {
            continue;
        }
        for cy in 0..h {
            let mut bits: u8 = 0;
            for (s, (&left, &right)) in BR_BIT[0].iter().zip(BR_BIT[1].iter()).enumerate() {
                let from_top = cy * 4 + s;
                let d = if flip { from_top + 1 } else { sub_h - from_top };
                if lh >= d {
                    bits |= 1 << left;
                }
                if rh >= d {
                    bits |= 1 << right;
                }
            }
            if bits == 0 {
                continue;
            }
            // Ramp position is normally the cell's HEIGHT in the graph, which
            // is what makes a spike's severity pre-attentive. A single-row
            // graph has one cell row, so height is constant and every cell
            // would take the same mid-ramp colour — throwing away the
            // magnitude channel exactly where it is needed most, since one
            // braille row resolves only four levels and adjacent series
            // collide in glyph space. There, sample by VALUE instead, so
            // colour separates what shape cannot.
            let f = if h == 1 {
                (((lv + rv) / 2.0 - b_lo) / (b_hi - b_lo).max(f64::EPSILON)).clamp(0.0, 1.0)
            } else {
                let mid = (cy * 4 + 2) as f64;
                if flip {
                    mid / sub_h as f64
                } else {
                    (sub_h as f64 - mid) / sub_h as f64
                }
            };
            let ch = char::from_u32(BR_BASE | bits as u32).unwrap_or(' ');
            if let Some(cell) = buf.cell_mut((area.x + cx as u16, area.y + cy as u16)) {
                cell.set_char(ch).set_fg(ramp.at(f));
            }
        }
    }
}

/// Single-row braille sparkline: four levels of subpixel, two samples per cell.
pub fn spark(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    w: u16,
    vals: &[f64],
    ramp: Ramp,
    band: Option<(f64, f64)>,
) {
    graph(
        buf,
        Rect {
            x,
            y,
            width: w,
            height: 1,
        },
        vals,
        ramp,
        false,
        band,
    );
}

/// Horizontal gradient meter — btop's bounded-value idiom.
///
/// The ramp is sampled by POSITION along the bar, so the far end is always the
/// "bad" colour even when the value never reaches it. That is the whole point:
/// you can see where the danger is before you're in it.
pub fn meter(buf: &mut Buffer, x: u16, y: u16, w: u16, frac: f64, ramp: Ramp) {
    if w == 0 {
        return;
    }
    let f = if frac.is_finite() {
        frac.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let filled = (f * w as f64).round() as u16;
    for i in 0..w {
        let on = i < filled;
        // A one-cell meter has no position to sample along: i/(w-1) divides by
        // zero. Narrow layouts are exactly where a panic is least acceptable.
        let pos = if w > 1 { i as f64 / (w - 1) as f64 } else { f };
        let (ch, fg) = if on {
            ('■', ramp.at(pos))
        } else {
            ('·', palette::faint())
        };
        if let Some(cell) = buf.cell_mut((x + i, y)) {
            cell.set_char(ch).set_fg(fg);
        }
    }
}

/// A meter that renders "unavailable" without changing its footprint.
///
/// `labelled` writes `--` into the empty track, because an empty track alone is
/// what a zero reading looks like and "the platform can't measure this" is a
/// different claim. Pass `false` where a `--` already sits beside the meter —
/// two of them read as a value.
pub fn meter_unavailable(buf: &mut Buffer, x: u16, y: u16, w: u16, labelled: bool) {
    for i in 0..w {
        if let Some(cell) = buf.cell_mut((x + i, y)) {
            cell.set_char('·').set_fg(palette::faint());
        }
    }
    if labelled && w >= 2 {
        text(buf, x, y, "--", palette::dim(), false);
    }
}

// ── box ────────────────────────────────────────────────────────────────────

/// A btop box: rounded corners, title embedded in the border, bracketed hotkey,
/// and information hung off both ends of both borders.
///
/// ```text
/// ╭┤1├─┤ io ├─┤ 4 physical ├──────────────┤ diskwatch 2.0 ├─╮
/// ╰─┤ ↹ device ├──────────────────┤ 52% util ├──────────────╯
/// ```
///
/// This is where the 2.0 layout wins its rows back: there is no header bar, no
/// menu bar and no status bar, because identity, hotkeys, sort state and paging
/// all live in border space that a box already spends.
#[derive(Default)]
pub struct BoxOpts<'a> {
    pub key: Option<&'a str>,
    pub title: Option<&'a str>,
    pub title_fg: Option<Color>,
    pub sub: Option<&'a str>,
    pub right: Option<&'a str>,
    pub right_fg: Option<Color>,
    /// `(key, label)` pairs for the bottom-left border.
    pub foot_l: &'a [(&'a str, &'a str)],
    pub foot_r: Option<&'a str>,
    /// Draw the border in an alert colour without moving anything.
    pub border_fg: Option<Color>,
}

/// Draws the box and returns its interior rect.
pub fn draw_box(buf: &mut Buffer, area: Rect, o: &BoxOpts) -> Rect {
    if area.width < 2 || area.height < 2 {
        return area;
    }
    let border = o.border_fg.unwrap_or_else(palette::faint);
    let (x, y, w, h) = (area.x, area.y, area.width, area.height);
    for i in 1..w - 1 {
        set(buf, x + i, y, '─', border);
        set(buf, x + i, y + h - 1, '─', border);
    }
    for j in 1..h - 1 {
        set(buf, x, y + j, '│', border);
        set(buf, x + w - 1, y + j, '│', border);
    }
    set(buf, x, y, '╭', border);
    set(buf, x + w - 1, y, '╮', border);
    set(buf, x, y + h - 1, '╰', border);
    set(buf, x + w - 1, y + h - 1, '╯', border);

    // Both border rows carry two writers, one from each end, and a narrow box
    // is exactly where they meet. Everything below is measured BEFORE anything
    // is drawn: the left side keeps its space, the right side is dropped if it
    // would reach into it, and the optional `sub` is given up first — losing a
    // device count is cheaper than losing a hotkey or overwriting a title.
    let key_w = o.key.map_or(0, |k| chars(k) + 3);
    let title_w = o.title.map_or(0, |t| chars(t) + 4);
    let sub_w = o.sub.map_or(0, |s| chars(s) + 5);
    let right_w = o.right.map_or(0, |r| chars(r) + 4);
    let border_w = w.saturating_sub(2);
    // One column of border must survive between the two, or they are touching.
    let show_sub = o.sub.is_some() && key_w + title_w + sub_w + right_w < border_w;
    let left_w = key_w + title_w + if show_sub { sub_w } else { 0 };
    let show_right = o.right.is_some() && left_w + right_w < border_w;

    let mut cx = x + 1;
    if let Some(k) = o.key {
        cx = text(buf, cx, y, "┤", border, false);
        cx = text(buf, cx, y, k, palette::yellow(), true);
        cx = text(buf, cx, y, "├─", border, false);
    }
    if let Some(t) = o.title {
        cx = text(buf, cx, y, "┤ ", border, false);
        cx = text(
            buf,
            cx,
            y,
            t,
            o.title_fg.unwrap_or_else(palette::cyan),
            true,
        );
        cx = text(buf, cx, y, " ├", border, false);
    }
    if show_sub {
        if let Some(s) = o.sub {
            cx = text(buf, cx, y, "─┤ ", border, false);
            cx = text(buf, cx, y, s, palette::dim(), false);
            text(buf, cx, y, " ├", border, false);
        }
    }
    if show_right {
        if let Some(r) = o.right {
            bracket_right(
                buf,
                area,
                y,
                r,
                o.right_fg.unwrap_or_else(palette::dim),
                border,
            );
        }
    }

    let foot_l_w: u16 = if o.foot_l.is_empty() {
        0
    } else {
        3 + o
            .foot_l
            .iter()
            .map(|(k, l)| chars(k) + chars(l))
            .sum::<u16>()
            + 2 * (o.foot_l.len() as u16 - 1)
            + 2
    };
    let foot_r_w = o.foot_r.map_or(0, |r| chars(r) + 4);
    let show_foot_r = o.foot_r.is_some() && foot_l_w + foot_r_w < border_w;

    if !o.foot_l.is_empty() {
        let fy = y + h - 1;
        let mut fx = x + 2;
        fx = text(buf, fx, fy, "┤ ", border, false);
        for (i, (k, label)) in o.foot_l.iter().enumerate() {
            if i > 0 {
                fx = text(buf, fx, fy, "  ", border, false);
            }
            fx = text(buf, fx, fy, k, palette::yellow(), true);
            fx = text(buf, fx, fy, label, palette::dim(), false);
        }
        text(buf, fx, fy, " ├", border, false);
    }
    if show_foot_r {
        if let Some(r) = o.foot_r {
            bracket_right(buf, area, y + h - 1, r, palette::dim(), border);
        }
    }

    Rect {
        x: x + 1,
        y: y + 1,
        width: w - 2,
        height: h - 2,
    }
}

/// `┤ text ├` hung off the right end of a border row, clamped so it can never
/// reach past the box's own corner.
fn bracket_right(buf: &mut Buffer, area: Rect, y: u16, s: &str, fg: Color, border: Color) {
    let inner = format!(" {s} ");
    let len = inner.chars().count() as u16;
    let avail = area.width.saturating_sub(4);
    if len + 2 > avail {
        return;
    }
    let rx = area.x + area.width - 2 - (len + 2);
    text(buf, rx, y, "┤", border, false);
    text(buf, rx + 1, y, &inner, fg, false);
    text(buf, rx + 1 + len, y, "├", border, false);
}

fn chars(s: &str) -> u16 {
    s.chars().count() as u16
}

fn set(buf: &mut Buffer, x: u16, y: u16, ch: char, fg: Color) {
    if let Some(cell) = buf.cell_mut((x, y)) {
        cell.set_char(ch).set_fg(fg);
    }
}

/// Draw a horizontal rule across `w` cells.
pub fn rule(buf: &mut Buffer, x: u16, y: u16, w: u16) {
    for i in 0..w {
        set(buf, x + i, y, '─', palette::faint());
    }
}

// ── axis ───────────────────────────────────────────────────────────────────

/// Mantissas of the axis ladder, at most 25% apart.
///
/// A sparse ladder (128 → 192) leaves the top braille row entirely unused,
/// costing a quarter of a four-row graph. At ≤25% spacing a peak always lands
/// in the top fifth of the axis — worst case exactly 1/1.25 = 80%.
const MANTISSA: [f64; 13] = [
    1.0, 1.25, 1.5, 1.8, 2.0, 2.5, 3.0, 3.5, 4.0, 5.0, 6.0, 7.0, 8.0,
];

/// Smallest ladder value at or above `v`, in whatever unit `v` is in.
///
/// There is deliberately no headroom fudge factor: multiplying the peak by a
/// few percent before the lookup pushes it back down the axis, which is the
/// one thing a tight ladder exists to prevent.
pub fn nice_ceil(v: f64) -> f64 {
    if !v.is_finite() || v <= 0.0 {
        return MANTISSA[0];
    }
    // Decades from 1e-3 to 1e6 of the caller's unit. Called with MiB/s that is
    // 1 KiB/s to 1 PiB/s — the ceiling has to be out of reach of real hardware,
    // not of the fixtures. A ladder stopping at 2 GiB/s renders every drive
    // faster than a single Gen4 NVMe as the same clipped slab.
    let mut decade = 1e-3_f64;
    while decade <= 1e6 {
        for m in MANTISSA {
            let c = m * decade;
            if c >= v {
                return c;
            }
        }
        decade *= 10.0;
    }
    8e6
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ladder_rungs_are_at_most_25_percent_apart() {
        // The property the whole auto-scale rests on. A 28% rung — which the
        // 1/1.25/1.6/2 ladder has at every third step — leaves part of the top
        // braille row permanently unused.
        let mut prev = MANTISSA[0];
        for m in MANTISSA.iter().skip(1) {
            assert!(
                m / prev <= 1.2501,
                "rung {prev} → {m} is {:.1}% apart",
                (m / prev - 1.0) * 100.0
            );
            prev = *m;
        }
        // And across the decade boundary: 8 → 10.
        assert!(10.0 / MANTISSA[MANTISSA.len() - 1] <= 1.2501);
    }

    #[test]
    fn peak_always_lands_in_the_top_fifth_of_the_axis() {
        let mut v = 0.01_f64;
        while v < 1e5 {
            let top = nice_ceil(v);
            assert!(top >= v, "ceiling {top} below peak {v}");
            assert!(
                v / top >= 0.7999,
                "peak {v} only reaches {:.1}% of a {top} axis",
                v / top * 100.0
            );
            v *= 1.017;
        }
    }

    #[test]
    fn ladder_reaches_past_real_hardware() {
        // A Gen5 NVMe pair sustains ~14 GiB/s. In MiB/s that is ~14336.
        assert!(nice_ceil(14336.0) >= 14336.0);
        assert!(nice_ceil(14336.0) < 20000.0, "and without wasting the axis");
    }

    #[test]
    fn nice_ceil_survives_garbage() {
        assert_eq!(nice_ceil(0.0), 1.0);
        assert_eq!(nice_ceil(-5.0), 1.0);
        assert_eq!(nice_ceil(f64::NAN), 1.0);
    }

    fn buffer(w: u16, h: u16) -> Buffer {
        Buffer::empty(Rect::new(0, 0, w, h))
    }

    #[test]
    fn a_non_zero_sample_always_lights_at_least_one_dot() {
        // Anything under 1/(2·sub_h) rounds to zero. On a one-row sparkline
        // that is everything below 12.5%, and a quiet-but-active series would
        // read as no data at all.
        let mut buf = buffer(4, 1);
        spark(&mut buf, 0, 0, 4, &[0.001; 8], Ramp::Write, None);
        for x in 0..4 {
            assert_ne!(buf.cell((x, 0)).unwrap().symbol(), " ", "column {x} blank");
        }
    }

    #[test]
    fn zero_stays_blank() {
        let mut buf = buffer(4, 1);
        spark(&mut buf, 0, 0, 4, &[0.0; 8], Ramp::Write, None);
        for x in 0..4 {
            assert_eq!(buf.cell((x, 0)).unwrap().symbol(), " ");
        }
    }

    #[test]
    fn flip_mirrors_the_fill_about_the_axis() {
        // The mirrored write graph is the identity of the tool: a restore is a
        // cliff above the axis, a backup a cliff below it.
        let mut up = buffer(1, 2);
        graph(
            &mut up,
            Rect::new(0, 0, 1, 2),
            &[0.5, 0.5],
            Ramp::Read,
            false,
            None,
        );
        let mut down = buffer(1, 2);
        graph(
            &mut down,
            Rect::new(0, 0, 1, 2),
            &[0.5, 0.5],
            Ramp::Write,
            true,
            None,
        );
        // Growing up fills the BOTTOM row; growing down fills the TOP row.
        assert_eq!(up.cell((0, 1)).unwrap().symbol(), "⣿");
        assert_eq!(up.cell((0, 0)).unwrap().symbol(), " ");
        assert_eq!(down.cell((0, 0)).unwrap().symbol(), "⣿");
        assert_eq!(down.cell((0, 1)).unwrap().symbol(), " ");
    }

    #[test]
    fn both_sub_columns_are_filled() {
        // Lighting only the left sub-column reads as a sparse dot matrix
        // rather than an area fill — the bug netwatch shipped and fixed.
        let mut buf = buffer(1, 1);
        spark(&mut buf, 0, 0, 1, &[1.0, 1.0], Ramp::Read, None);
        assert_eq!(buf.cell((0, 0)).unwrap().symbol(), "⣿");
    }

    #[test]
    fn a_one_cell_meter_does_not_divide_by_zero() {
        let mut buf = buffer(1, 1);
        meter(&mut buf, 0, 0, 1, 0.5, Ramp::Load);
        assert_eq!(buf.cell((0, 0)).unwrap().symbol(), "■");
    }

    #[test]
    fn a_meter_handles_a_nan_fraction() {
        let mut buf = buffer(4, 1);
        meter(&mut buf, 0, 0, 4, f64::NAN, Ramp::Load);
        assert_eq!(buf.cell((0, 0)).unwrap().symbol(), "·");
    }

    #[test]
    fn text_clips_instead_of_writing_off_the_buffer() {
        let mut buf = buffer(4, 1);
        text(&mut buf, 2, 0, "abcdef", palette::fg(), false);
        assert_eq!(buf.cell((3, 0)).unwrap().symbol(), "b");
    }

    #[test]
    fn single_row_graphs_colour_by_value() {
        // Height is constant on a one-row graph, so without the band every
        // cell takes the same mid-ramp colour and two very different series
        // render identically.
        let mut lo = buffer(1, 1);
        spark(
            &mut lo,
            0,
            0,
            1,
            &[0.31, 0.31],
            Ramp::Write,
            Some((0.30, 0.68)),
        );
        let mut hi = buffer(1, 1);
        spark(
            &mut hi,
            0,
            0,
            1,
            &[0.67, 0.67],
            Ramp::Write,
            Some((0.30, 0.68)),
        );
        assert_ne!(lo.cell((0, 0)).unwrap().fg, hi.cell((0, 0)).unwrap().fg);
    }

    #[test]
    fn terminal_theme_pins_no_rgb() {
        // The `terminal` theme's whole promise is that diskwatch defines no
        // colours of its own. A gradient has no 16-colour equivalent, so it
        // has to degrade rather than leak RGB.
        theme::set_by_name("terminal");
        for f in [0.0, 0.4, 0.9, 1.0] {
            for r in [Ramp::Read, Ramp::Write, Ramp::Load] {
                assert!(
                    !matches!(r.at(f), Color::Rgb(..)),
                    "{r:?} at {f} pinned RGB under the terminal theme"
                );
            }
        }
        theme::set_by_name("dark");
        assert!(matches!(Ramp::Read.at(0.5), Color::Rgb(..)));
    }
}
