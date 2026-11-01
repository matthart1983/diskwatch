//! Terminal safety for text that other people control.
//!
//! File names, mount paths, device names and drive models can all be set by
//! another user on the machine, and the SMART tab tells people to run
//! diskwatch under sudo. ratatui doesn't protect against that: only
//! `Buffer::set_stringn` filters control characters, `Span` rendering keeps
//! them, and `braille::text` writes cells directly. The backend then emits
//! every cell verbatim, so a file named `x\x1b]0;owned\x07` retitles the
//! viewer's terminal, and the same route reaches screen clears, cursor moves
//! and OSC 52 clipboard writes.
//!
//! Collectors keep paths raw, because they're also map keys and get matched
//! against what `/proc` reports. So the scrubbing happens where text leaves
//! the process: [`scrub_buffer`] runs on every finished frame, which covers
//! any render path, and `--diag` prints through [`scrub`].

use std::borrow::Cow;

use ratatui::buffer::Buffer;

/// Stand-in for a character that must never reach the terminal. Matches
/// netwatch and syswatch.
const REPLACEMENT: char = '·';

/// True for characters that can move the cursor, start an escape sequence or
/// reorder the visible line.
fn unsafe_for_display(c: char) -> bool {
    // C0 (including ESC, BEL, TAB, CR and LF), DEL, and C1, which includes the
    // 8-bit CSI U+009B that xterm-family terminals honour as readily as ESC [.
    c.is_control()
        // Bidirectional controls, which can make a name read as another one,
        // and the two line separators.
        || matches!(
            c,
            '\u{061C}'
                | '\u{200E}'
                | '\u{200F}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2066}'..='\u{2069}'
        )
}

/// `s` with every unsafe character replaced. Allocates only when there's
/// something to replace, which on a healthy machine is never.
pub fn scrub(s: &str) -> Cow<'_, str> {
    if !s.chars().any(unsafe_for_display) {
        return Cow::Borrowed(s);
    }
    Cow::Owned(
        s.chars()
            .map(|c| {
                if unsafe_for_display(c) {
                    REPLACEMENT
                } else {
                    c
                }
            })
            .collect(),
    )
}

/// Clean a finished frame before the backend sees it.
pub fn scrub_buffer(buf: &mut Buffer) {
    for cell in &mut buf.content {
        let symbol = cell.symbol();
        if !symbol.chars().any(unsafe_for_display) {
            continue;
        }
        // A control character alone in a cell holds a column, so the
        // replacement keeps the row aligned. One appended to a visible
        // grapheme holds no column, so it just goes.
        let kept: String = symbol.chars().filter(|c| !unsafe_for_display(*c)).collect();
        if kept.is_empty() {
            cell.set_char(REPLACEMENT);
        } else {
            cell.set_symbol(&kept);
        }
    }
}

/// True when no cell holds anything the terminal could act on.
#[cfg(test)]
pub fn is_clean(buf: &Buffer) -> bool {
    buf.content
        .iter()
        .all(|cell| !cell.symbol().chars().any(unsafe_for_display))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;
    use ratatui::style::Color;
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Paragraph, Widget};

    const HOSTILE: &str = "evil\u{1b}[31mRED\u{1b}]0;pwned\u{7}\u{202E}.log";

    fn text_of(buf: &Buffer) -> String {
        buf.content.iter().map(|c| c.symbol()).collect()
    }

    /// OSC 0 retitles the terminal and BEL ends it. Neither byte may survive.
    #[test]
    fn strips_the_title_sequence() {
        assert_eq!(scrub("sh\u{1b}]0;owned\u{7}x"), "sh·]0;owned·x");
    }

    #[test]
    fn strips_c0_c1_and_del() {
        for c in ['\u{1b}', '\u{7}', '\t', '\n', '\r', '\u{7f}', '\u{9b}'] {
            assert_eq!(scrub(&format!("a{c}b")), "a·b", "U+{:04X}", c as u32);
        }
    }

    /// `gnp\u{202E}.exe` renders as `exe.png` under the override.
    #[test]
    fn strips_bidi_controls() {
        for c in ['\u{202E}', '\u{2066}', '\u{2069}', '\u{200F}', '\u{061C}'] {
            assert_eq!(scrub(&format!("a{c}b")), "a·b", "U+{:04X}", c as u32);
        }
    }

    /// Real file names are full of these, and a ZWJ emoji or a combining mark
    /// must come through whole.
    #[test]
    fn leaves_ordinary_text_alone() {
        for s in [
            "/var/log/syslog",
            "日本語のファイル名.log",
            "café",
            "e\u{301}",
            "\u{1F468}\u{200D}\u{1F4BB}",
        ] {
            assert!(matches!(scrub(s), Cow::Borrowed(_)), "{s}");
        }
    }

    /// Table rows and panels render through `Line` and `Span`.
    #[test]
    fn cleans_the_span_path() {
        let area = Rect::new(0, 0, 40, 1);
        let mut buf = Buffer::empty(area);
        Paragraph::new(Line::from(Span::raw(HOSTILE))).render(area, &mut buf);
        assert!(!is_clean(&buf), "this path should need the scrub");

        scrub_buffer(&mut buf);
        assert!(is_clean(&buf), "{:?}", text_of(&buf));
        assert!(text_of(&buf).contains("evil"));
        assert!(text_of(&buf).contains(".log"));
    }

    /// Dense writes cells directly, so nothing in ratatui filters it.
    #[test]
    fn cleans_the_direct_cell_path() {
        let area = Rect::new(0, 0, 40, 1);
        let mut buf = Buffer::empty(area);
        crate::ui::braille::text(&mut buf, 0, 0, HOSTILE, Color::White, false);
        assert!(!is_clean(&buf), "this path should need the scrub");

        scrub_buffer(&mut buf);
        assert!(is_clean(&buf), "{:?}", text_of(&buf));
        assert!(text_of(&buf).starts_with("evil·[31mRED·]0;pwned··.log"));
    }

    /// A zero-width character can arrive appended to a visible one. Replacing
    /// it would put two columns of text in one cell.
    #[test]
    fn drops_a_character_appended_to_a_visible_one() {
        let area = Rect::new(0, 0, 3, 1);
        let mut buf = Buffer::empty(area);
        buf[(0, 0)].set_symbol("a\u{202E}");
        scrub_buffer(&mut buf);
        assert_eq!(buf[(0, 0)].symbol(), "a");
    }

    #[test]
    fn leaves_a_clean_frame_alone() {
        let area = Rect::new(0, 0, 12, 1);
        let mut buf = Buffer::empty(area);
        Paragraph::new("日本語 ok").render(area, &mut buf);
        let before = buf.clone();
        scrub_buffer(&mut buf);
        assert_eq!(buf, before);
    }
}
