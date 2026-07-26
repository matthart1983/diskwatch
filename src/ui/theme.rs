//! Color themes for the diskwatch UI.
//!
//! Mirrors the theme conventions used by netwatch and syswatch: a struct of
//! named color slots, a small `by_name` lookup, and a global `RwLock<Theme>`
//! holding the active one. `palette::*` accessors delegate here, so setting a
//! theme recolors the whole UI on the next draw.
//!
//! Slot names are diskwatch's own — literal color names (`red`, `cyan`)
//! rather than the semantic names (`status_error`, `brand`) the other two
//! use — because the diskwatch UI was built against a fixed design handoff
//! that refers to them that way. Renaming them is a separate change.

use ratatui::style::Color;
use std::sync::RwLock;

#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub name: &'static str,

    // ── Surface / text ──────────────────────────────────
    pub bg: Color,
    pub fg: Color,
    pub dim: Color,
    pub faint: Color,

    // ── Base colors ─────────────────────────────────────
    pub red: Color,
    pub green: Color,
    pub yellow: Color,
    pub cyan: Color,
    pub magenta: Color,
    pub white: Color,

    // ── Bright variants ─────────────────────────────────
    pub br_green: Color,
    pub br_cyan: Color,
    pub br_white: Color,

    // ── Backgrounds ─────────────────────────────────────
    pub sel_bg: Color,
    pub warn_bg: Color,
    pub err_bg: Color,
    pub ok_bg: Color,
}

/// Cycle order. Matches syswatch so the two tools feel the same when a
/// user tabs through themes in both.
pub const THEME_NAMES: &[&str] = &[
    "dark",
    "light",
    "ocean",
    "solarized",
    "dracula",
    "nord",
    "terminal",
];

/// The original diskwatch palette. Hex values are byte-identical to the
/// design handoff (`source/tui/grid.jsx` constant `C`) so the JSX mockups
/// and the real terminal render the same — do not "tidy" these.
pub const fn dark() -> Theme {
    Theme {
        name: "dark",
        bg: Color::Rgb(0x0c, 0x14, 0x18),
        fg: Color::Rgb(0xc5, 0xd1, 0xd6),
        dim: Color::Rgb(0x6b, 0x80, 0x88),
        faint: Color::Rgb(0x44, 0x56, 0x60),
        red: Color::Rgb(0xff, 0x78, 0x78),
        green: Color::Rgb(0x5c, 0xd9, 0x89),
        yellow: Color::Rgb(0xf0, 0xc0, 0x60),
        cyan: Color::Rgb(0x5f, 0xdc, 0xff),
        magenta: Color::Rgb(0xd9, 0x7a, 0xff),
        white: Color::Rgb(0xe6, 0xf0, 0xf2),
        br_green: Color::Rgb(0x9a, 0xe6, 0xb4),
        br_cyan: Color::Rgb(0x86, 0xe6, 0xff),
        br_white: Color::Rgb(0xff, 0xff, 0xff),
        sel_bg: Color::Rgb(0x1a, 0x33, 0x40),
        warn_bg: Color::Rgb(0x3a, 0x2c, 0x14),
        err_bg: Color::Rgb(0x3a, 0x1c, 0x1c),
        ok_bg: Color::Rgb(0x16, 0x32, 0x1f),
    }
}

/// Light theme, for terminals with a pale background.
///
/// The mapping needs care in one place: `white` and `br_white` are the
/// *emphasis* slots — the strongest available text — so on a light
/// background they go to near-black rather than to white. Wiring them
/// literally would make selected rows and SMART headers invisible.
///
/// Contrast against the near-white bg (#f5f5f2):
///   fg    #1e1e1e ≈ 14.5:1 AAA · dim #5a5a5a ≈ 5.7:1 AA
///   faint #a0a0a0 ≈  2.5:1 — chrome only (borders), intentionally quiet
pub const fn light() -> Theme {
    Theme {
        name: "light",
        bg: Color::Rgb(0xf5, 0xf5, 0xf2),
        fg: Color::Rgb(30, 30, 30),
        dim: Color::Rgb(90, 90, 90),
        faint: Color::Rgb(160, 160, 160),
        red: Color::Rgb(180, 30, 30),
        green: Color::Rgb(0, 120, 50),
        yellow: Color::Rgb(170, 110, 0),
        cyan: Color::Rgb(0, 100, 160),
        magenta: Color::Rgb(120, 60, 160),
        // Emphasis on a light bg is darker, not lighter.
        white: Color::Rgb(0, 0, 0),
        br_green: Color::Rgb(0, 150, 65),
        br_cyan: Color::Rgb(0, 130, 200),
        br_white: Color::Rgb(0, 0, 0),
        sel_bg: Color::Rgb(220, 230, 240),
        warn_bg: Color::Rgb(0xfa, 0xf0, 0xd6),
        err_bg: Color::Rgb(0xfa, 0xdc, 0xdc),
        ok_bg: Color::Rgb(0xdc, 0xea, 0xf7),
    }
}

/// Apple Terminal.app "Ocean" profile — deep blue background, with the
/// bright ANSI variants preferred for legibility against it.
pub const fn ocean() -> Theme {
    Theme {
        name: "ocean",
        bg: Color::Rgb(0x22, 0x4F, 0xBC),
        fg: Color::Rgb(0xFF, 0xFF, 0xFF),
        // Ocean's bright-black (#818383) fails AA on this bg, so chrome
        // uses a lighter neutral. dim and faint land on the same value:
        // anything fainter is unreadable over the blue.
        dim: Color::Rgb(0xB5, 0xB6, 0xB7),
        faint: Color::Rgb(0xB5, 0xB6, 0xB7),
        red: Color::Rgb(0xFC, 0x39, 0x1F),
        green: Color::Rgb(0x31, 0xE7, 0x22),
        yellow: Color::Rgb(0xEA, 0xEC, 0x23),
        cyan: Color::Rgb(0x14, 0xF0, 0xF0),
        magenta: Color::Rgb(0xFF, 0x40, 0xFF),
        white: Color::Rgb(0xCB, 0xCC, 0xCD),
        br_green: Color::Rgb(0x31, 0xE7, 0x22),
        br_cyan: Color::Rgb(0x14, 0xF0, 0xF0),
        br_white: Color::Rgb(0xFF, 0xFF, 0xFF),
        sel_bg: Color::Rgb(0x21, 0x6D, 0xFF),
        warn_bg: Color::Rgb(0x3A, 0x4A, 0x12),
        err_bg: Color::Rgb(0x4A, 0x1F, 0x1F),
        ok_bg: Color::Rgb(0x1A, 0x3A, 0x6E),
    }
}

/// Ethan Schoonover's Solarized Dark, using the canonical base/accent hexes.
pub const fn solarized() -> Theme {
    Theme {
        name: "solarized",
        bg: Color::Rgb(0, 43, 54),     // base03
        fg: Color::Rgb(131, 148, 150), // base0
        // base00, not base01. Solarized reserves base01 for de-emphasized
        // comments and it lands at 2.8:1 on base03 — under the 3:1 floor.
        // diskwatch leans on `dim` for real content (units, secondary
        // values), so it takes the next step up at 3.4:1.
        dim: Color::Rgb(101, 123, 131), // base00
        faint: Color::Rgb(62, 84, 92),  // between base02 and base01
        red: Color::Rgb(220, 50, 47),
        green: Color::Rgb(133, 153, 0),
        yellow: Color::Rgb(181, 137, 0),
        cyan: Color::Rgb(42, 161, 152),
        magenta: Color::Rgb(211, 54, 130),
        white: Color::Rgb(147, 161, 161), // base1
        br_green: Color::Rgb(152, 175, 0),
        br_cyan: Color::Rgb(58, 180, 170),
        br_white: Color::Rgb(238, 232, 213), // base2
        sel_bg: Color::Rgb(7, 54, 66),       // base02
        warn_bg: Color::Rgb(40, 36, 14),
        err_bg: Color::Rgb(50, 18, 18),
        ok_bg: Color::Rgb(10, 50, 60),
    }
}

/// Dracula, using the canonical palette.
pub const fn dracula() -> Theme {
    Theme {
        name: "dracula",
        bg: Color::Rgb(40, 42, 54),
        fg: Color::Rgb(248, 248, 242),
        dim: Color::Rgb(98, 114, 164), // comment
        faint: Color::Rgb(68, 71, 90),
        red: Color::Rgb(255, 85, 85),
        green: Color::Rgb(80, 250, 123),
        yellow: Color::Rgb(241, 250, 140),
        cyan: Color::Rgb(139, 233, 253),
        magenta: Color::Rgb(189, 147, 249), // purple
        white: Color::Rgb(248, 248, 242),
        br_green: Color::Rgb(120, 255, 150),
        br_cyan: Color::Rgb(170, 240, 255),
        br_white: Color::Rgb(255, 255, 255),
        sel_bg: Color::Rgb(68, 71, 90),
        warn_bg: Color::Rgb(60, 50, 30),
        err_bg: Color::Rgb(70, 30, 30),
        ok_bg: Color::Rgb(35, 55, 70),
    }
}

/// Nord, using the canonical Polar Night / Snow Storm / Frost / Aurora sets.
pub const fn nord() -> Theme {
    Theme {
        name: "nord",
        bg: Color::Rgb(46, 52, 64),    // polar night 0
        fg: Color::Rgb(216, 222, 233), // snow storm 0
        // Polar Night 3 (#4C566A) is what Nord editor themes use for
        // comments, but it's a background shade — 1.7:1 on Polar Night 0,
        // effectively invisible for the secondary values diskwatch puts in
        // `dim`. Frost 2 is the nearest canonical color that carries text.
        dim: Color::Rgb(129, 161, 193), // frost 2
        faint: Color::Rgb(67, 76, 94),  // polar night 2
        red: Color::Rgb(191, 97, 106),  // aurora red
        green: Color::Rgb(163, 190, 140),
        yellow: Color::Rgb(235, 203, 139),
        cyan: Color::Rgb(136, 192, 208), // frost 1
        magenta: Color::Rgb(180, 142, 173),
        white: Color::Rgb(229, 233, 240),
        br_green: Color::Rgb(180, 205, 160),
        br_cyan: Color::Rgb(150, 205, 220),
        br_white: Color::Rgb(236, 239, 244),
        sel_bg: Color::Rgb(59, 66, 82),
        warn_bg: Color::Rgb(60, 56, 40),
        err_bg: Color::Rgb(60, 36, 38),
        ok_bg: Color::Rgb(48, 62, 78),
    }
}

/// Defers entirely to the terminal's own palette: ANSI slots 0–15 for color
/// and `Color::Reset` for foreground and background, so whatever the user's
/// terminal theme defines is what diskwatch renders. Nothing here is a fixed
/// RGB value — that is the whole point. Users running a system-wide theming
/// setup (pywal, matugen, a terminal profile) get a diskwatch that matches
/// the rest of their desktop without maintaining a separate palette.
///
/// Two deliberate compromises, both forced by the 16-color palette:
///
/// - `warn_bg` / `err_bg` / `ok_bg` are `Reset` rather than a tint. ANSI has
///   no "slightly red background" — the nearest option is a full-intensity
///   fill, far louder than the subtle row tint these slots exist for.
///   Severity still reads from the foreground color, which is how
///   terminal-native tools convey it anyway.
/// - `sel_bg` uses `Indexed(8)` (bright black), the only slot conventionally
///   rendered as a neutral mid-grey in both light and dark themes. A terminal
///   theme that maps slot 8 very close to its background will show a faint
///   selection bar; that is a property of the user's theme, and the
///   alternative — a saturated color slot — is worse everywhere else.
pub const fn terminal() -> Theme {
    Theme {
        name: "terminal",
        // Reset = the terminal's own background and configured foreground.
        bg: Color::Reset,
        fg: Color::Reset,
        dim: Color::Gray,
        faint: Color::DarkGray,
        red: Color::Red,
        green: Color::Green,
        yellow: Color::Yellow,
        cyan: Color::Cyan,
        magenta: Color::Magenta,
        white: Color::White,
        br_green: Color::LightGreen,
        br_cyan: Color::LightCyan,
        br_white: Color::White,
        sel_bg: Color::Indexed(8),
        warn_bg: Color::Reset,
        err_bg: Color::Reset,
        ok_bg: Color::Reset,
    }
}

pub fn by_name(name: &str) -> Theme {
    match name.to_lowercase().as_str() {
        // "system" and "ansi" are what users coming from other TUIs tend to
        // reach for; accept both rather than silently falling back to dark
        // and looking like the feature is missing.
        "terminal" | "system" | "ansi" => terminal(),
        "light" => light(),
        "ocean" => ocean(),
        "solarized" => solarized(),
        "dracula" => dracula(),
        "nord" => nord(),
        _ => dark(),
    }
}

static ACTIVE: RwLock<Theme> = RwLock::new(dark());

pub fn active() -> Theme {
    *ACTIVE.read().expect("theme lock poisoned")
}

pub fn set_by_name(name: &str) {
    *ACTIVE.write().expect("theme lock poisoned") = by_name(name);
}

pub fn name() -> &'static str {
    active().name
}

/// Advance to the next built-in theme, wrapping. Returns the new name.
pub fn cycle() -> &'static str {
    let current = name();
    let i = THEME_NAMES.iter().position(|n| *n == current).unwrap_or(0);
    let next = THEME_NAMES[(i + 1) % THEME_NAMES.len()];
    set_by_name(next);
    next
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_builtin_themes_load() {
        for n in THEME_NAMES {
            assert_eq!(by_name(n).name, *n);
        }
    }

    #[test]
    fn unknown_falls_back_to_dark() {
        assert_eq!(by_name("nonsense").name, "dark");
        assert_eq!(by_name("").name, "dark");
    }

    #[test]
    fn dark_preserves_the_design_handoff() {
        // Guards the byte-identical contract with the JSX mockups.
        let t = dark();
        assert_eq!(t.bg, Color::Rgb(0x0c, 0x14, 0x18));
        assert_eq!(t.fg, Color::Rgb(0xc5, 0xd1, 0xd6));
        assert_eq!(t.cyan, Color::Rgb(0x5f, 0xdc, 0xff));
        assert_eq!(t.red, Color::Rgb(0xff, 0x78, 0x78));
        assert_eq!(t.sel_bg, Color::Rgb(0x1a, 0x33, 0x40));
    }

    #[test]
    fn terminal_theme_pins_no_rgb() {
        // The entire contract of this theme is that every slot resolves
        // through the terminal's own palette. Debug-formatting the whole
        // struct checks every field at once, so a slot added later can't
        // quietly acquire a fixed color without failing here.
        let rendered = format!("{:?}", terminal());
        assert!(
            !rendered.contains("Rgb"),
            "terminal theme must not pin RGB values: {rendered}"
        );
    }

    #[test]
    fn terminal_theme_never_paints_backgrounds() {
        let t = terminal();
        assert_eq!(t.bg, Color::Reset);
        assert_eq!(t.fg, Color::Reset);
        assert_eq!(t.warn_bg, Color::Reset);
        assert_eq!(t.err_bg, Color::Reset);
        assert_eq!(t.ok_bg, Color::Reset);
    }

    /// WCAG relative luminance. Only defined for concrete RGB.
    fn luminance(c: Color) -> Option<f64> {
        let (r, g, b) = match c {
            Color::Rgb(r, g, b) => (r, g, b),
            _ => return None,
        };
        let ch = |v: u8| {
            let s = v as f64 / 255.0;
            if s <= 0.03928 {
                s / 12.92
            } else {
                ((s + 0.055) / 1.055).powf(2.4)
            }
        };
        Some(0.2126 * ch(r) + 0.7152 * ch(g) + 0.0722 * ch(b))
    }

    fn contrast(a: Color, b: Color) -> Option<f64> {
        let (la, lb) = (luminance(a)?, luminance(b)?);
        let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
        Some((hi + 0.05) / (lo + 0.05))
    }

    #[test]
    fn every_theme_has_readable_body_text() {
        // The failure mode when adding a theme is text that disappears
        // into its own background — a light palette wired up with light
        // text, say. `terminal` is skipped: both slots are Reset, so the
        // contrast is whatever the user's terminal defines.
        for n in THEME_NAMES {
            let t = by_name(n);
            let Some(ratio) = contrast(t.fg, t.bg) else {
                continue;
            };
            assert!(
                ratio >= 4.5,
                "{n}: body text contrast {ratio:.1}:1 is below WCAG AA (4.5:1)"
            );
        }
    }

    #[test]
    fn every_theme_keeps_muted_text_legible() {
        // `dim` carries real content (units, secondary values), not just
        // chrome, so it gets the 3:1 large-text floor rather than being
        // allowed to fade out entirely. `faint` is borders only and is
        // deliberately exempt.
        for n in THEME_NAMES {
            let t = by_name(n);
            let Some(ratio) = contrast(t.dim, t.bg) else {
                continue;
            };
            assert!(
                ratio >= 3.0,
                "{n}: dim text contrast {ratio:.1}:1 below 3:1"
            );
        }
    }

    #[test]
    fn light_theme_emphasis_goes_darker_not_brighter() {
        // `white` / `br_white` are the "strongest text" slots. Mapping them
        // literally to white on a pale background hides selected rows and
        // SMART headers completely.
        let t = light();
        let bg = luminance(t.bg).unwrap();
        assert!(luminance(t.br_white).unwrap() < bg);
        assert!(luminance(t.white).unwrap() < bg);
    }

    #[test]
    fn cycle_visits_every_theme_and_returns_home() {
        let start = name();
        let mut seen = Vec::new();
        for _ in 0..THEME_NAMES.len() {
            seen.push(cycle());
        }
        seen.sort_unstable();
        let mut expected = THEME_NAMES.to_vec();
        expected.sort_unstable();
        assert_eq!(seen, expected);
        assert_eq!(name(), start, "a full cycle must land back where it began");
    }

    #[test]
    fn terminal_theme_accepts_common_aliases() {
        for alias in ["terminal", "system", "ansi", "TERMINAL", "System"] {
            assert_eq!(by_name(alias).name, "terminal", "alias {alias} failed");
        }
    }
}
