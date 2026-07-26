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

pub const THEME_NAMES: &[&str] = &["dark", "terminal"];

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

#[allow(dead_code)]
pub fn name() -> &'static str {
    active().name
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

    #[test]
    fn terminal_theme_accepts_common_aliases() {
        for alias in ["terminal", "system", "ansi", "TERMINAL", "System"] {
            assert_eq!(by_name(alias).name, "terminal", "alias {alias} failed");
        }
    }
}
