//! Formatting + small UI helpers shared across tabs.

use ratatui::style::Color;

use crate::ui::palette as p;

/// Decimal-byte formatter (TB / GB / MB / KB / B).
/// Matches `fmtBytes` in `grid.jsx` — drives use base-10, not base-2.
pub fn fmt_size(b: u64) -> String {
    const TB: u64 = 1_000_000_000_000;
    const GB: u64 = 1_000_000_000;
    const MB: u64 = 1_000_000;
    const KB: u64 = 1_000;
    if b >= TB {
        format!("{:.1} TB", b as f64 / TB as f64)
    } else if b >= GB {
        format!("{:.0} GB", b as f64 / GB as f64)
    } else if b >= MB {
        format!("{:.0} MB", b as f64 / MB as f64)
    } else if b >= KB {
        format!("{:.0} KB", b as f64 / KB as f64)
    } else {
        format!("{} B", b)
    }
}

/// Bytes-per-second formatter for IO rates.
pub fn fmt_rate(bps: f64) -> String {
    if bps < 1.0 {
        return "   -- ".to_string();
    }
    if bps >= 1_000_000.0 {
        format!("{:>5.1} MB/s", bps / 1_000_000.0)
    } else if bps >= 1_000.0 {
        format!("{:>5.1} KB/s", bps / 1_000.0)
    } else {
        format!("{:>5.0}  B/s", bps)
    }
}

/// Bytes-per-second in prose, for running text rather than a column:
/// no padding, and "idle" instead of a dash. [`fmt_rate`] is the
/// fixed-width form the tables use.
pub fn fmt_rate_compact(bps: f64) -> String {
    if bps >= 1_000_000_000.0 {
        format!("{:.1} GB/s", bps / 1_000_000_000.0)
    } else if bps >= 1_000_000.0 {
        format!("{:.1} MB/s", bps / 1_000_000.0)
    } else if bps >= 1000.0 {
        format!("{:.0} kB/s", bps / 1000.0)
    } else if bps > 0.0 {
        format!("{:.0} B/s", bps)
    } else {
        "idle".to_string()
    }
}

/// Terminal columns `ch` takes: 2 for CJK and most emoji, 0 for combining
/// marks. A control or bidi character counts as 1, because the screen
/// scrubber draws it as a `·` of its own, which keeps a right-to-left
/// override visible instead of folding it into its neighbour.
pub fn char_cells(ch: char) -> usize {
    use unicode_width::UnicodeWidthChar;
    if crate::sanitize::unsafe_for_display(ch) {
        return 1;
    }
    ch.width().unwrap_or(1)
}

/// Terminal columns `s` takes. Counting chars instead put a CJK filename
/// at half its width and pushed every column after it off the row.
pub fn cells(s: &str) -> usize {
    s.chars().map(char_cells).sum()
}

/// The longest prefix of `s` that fits in `n` columns, and its width. A wide
/// character that would straddle the edge is left out, not split.
pub fn take_cells(s: &str, n: usize) -> (String, usize) {
    let mut out = String::new();
    let mut used = 0;
    for ch in s.chars() {
        let w = char_cells(ch);
        if used + w > n {
            break;
        }
        out.push(ch);
        used += w;
    }
    (out, used)
}

/// The longest suffix of `s` that fits in `n` columns, and its width.
pub fn take_cells_end(s: &str, n: usize) -> (String, usize) {
    let mut kept = Vec::new();
    let mut used = 0;
    for ch in s.chars().rev() {
        let w = char_cells(ch);
        if used + w > n {
            break;
        }
        kept.push(ch);
        used += w;
    }
    (kept.into_iter().rev().collect(), used)
}

/// `s` padded or cut to exactly `n` columns, left-aligned.
pub fn pad_right(s: &str, n: usize) -> String {
    let (mut out, used) = take_cells(s, n);
    out.push_str(&" ".repeat(n - used));
    out
}

/// `s` padded or cut to exactly `n` columns, right-aligned.
pub fn pad_left(s: &str, n: usize) -> String {
    let (cut, used) = take_cells(s, n);
    let mut out = " ".repeat(n - used);
    out.push_str(&cut);
    out
}

/// `green < 80, yellow 80-89, red >= 90` — matches JSX usage-bar thresholds.
pub fn usage_color(used_pct: u32) -> Color {
    if used_pct >= 90 {
        p::red()
    } else if used_pct >= 80 {
        p::yellow()
    } else {
        p::fg()
    }
}

/// Same thresholds, but returns the bar fill color (green when ok).
pub fn usage_bar_color(used_pct: u32) -> Color {
    if used_pct >= 90 {
        p::red()
    } else if used_pct >= 80 {
        p::yellow()
    } else {
        p::green()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The compact form is for prose, so it carries no padding and says
    /// "idle" where the column form would show a dash.
    #[test]
    fn the_compact_rate_reads_as_prose() {
        assert_eq!(fmt_rate_compact(0.0), "idle");
        assert_eq!(fmt_rate_compact(512.0), "512 B/s");
        assert_eq!(fmt_rate_compact(2_000.0), "2 kB/s");
        assert_eq!(fmt_rate_compact(5_400_000.0), "5.4 MB/s");
        assert_eq!(fmt_rate_compact(2_100_000_000.0), "2.1 GB/s");
        for bps in [0.0, 1.0, 999.0, 1e6, 1e12] {
            assert!(!fmt_rate_compact(bps).starts_with(' '), "{bps}");
        }
    }
}

#[cfg(test)]
mod width_tests {
    use super::*;

    #[test]
    fn padding_counts_columns_not_chars() {
        assert_eq!(cells("日本語.log"), 10);
        assert_eq!(cells(&pad_right("日本語.log", 12)), 12);
        assert_eq!(cells(&pad_left("😀x", 6)), 6);
        // A wide character that would straddle the edge is left out and
        // the gap padded, so the width is still exact.
        assert_eq!(pad_right("日本語", 5), "日本 ");
        assert_eq!(take_cells_end("ab日本", 3), ("本".to_string(), 2));
        assert_eq!(take_cells_end("ab日本", 5), ("b日本".to_string(), 5));
        assert_eq!(cells("x\u{202E}y"), 3, "a bidi override shows as ·");
        assert_eq!(pad_right("plain", 3), "pla");
    }
}
