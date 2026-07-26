//! Color accessors for the diskwatch UI.
//!
//! Every read routes through the active theme (`crate::ui::theme`), so
//! switching themes recolors the whole UI on the next draw. These were plain
//! `const`s until the `terminal` theme landed; they are functions now purely
//! so the values can vary at runtime. The `dark` theme still holds the
//! byte-identical design-handoff hexes that used to live here.

#![allow(dead_code)]

use ratatui::style::Color;

use crate::ui::theme;

// ── Surface / text ─────────────────────────────────────────
pub fn bg() -> Color {
    theme::active().bg
}
pub fn fg() -> Color {
    theme::active().fg
}
pub fn dim() -> Color {
    theme::active().dim
}
pub fn faint() -> Color {
    theme::active().faint
}

// ── Base colors ────────────────────────────────────────────
pub fn red() -> Color {
    theme::active().red
}
pub fn green() -> Color {
    theme::active().green
}
pub fn yellow() -> Color {
    theme::active().yellow
}
pub fn cyan() -> Color {
    theme::active().cyan
}
pub fn magenta() -> Color {
    theme::active().magenta
}
pub fn white() -> Color {
    theme::active().white
}

// ── Bright variants ────────────────────────────────────────
pub fn br_green() -> Color {
    theme::active().br_green
}
pub fn br_cyan() -> Color {
    theme::active().br_cyan
}
pub fn br_white() -> Color {
    theme::active().br_white
}

// ── Backgrounds ────────────────────────────────────────────
pub fn sel_bg() -> Color {
    theme::active().sel_bg
}
pub fn warn_bg() -> Color {
    theme::active().warn_bg
}
pub fn err_bg() -> Color {
    theme::active().err_bg
}
pub fn ok_bg() -> Color {
    theme::active().ok_bg
}
