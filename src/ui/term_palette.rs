//! The terminal's own colours, asked for once at startup.
//!
//! The `terminal` theme draws in the sixteen palette slots and names no RGB
//! value of its own, so the user's terminal theme carries straight over. A
//! btop-style fade needs the shades between those slots, and only RGB has
//! them. So diskwatch asks the terminal for the slot values instead of
//! guessing: OSC 4 reports one palette slot's RGB, and OSC 11 the background.
//! The fade is built from the colours the user already sees, and it still
//! changes when they change their terminal theme.
//!
//! The queries end with a primary device attributes request (DA1). Every
//! terminal answers DA1, and answers in order, so once its reply arrives
//! every OSC reply the terminal was going to send has arrived too. Without
//! that, a reply that came in after the read gave up would land in the key
//! reader, as `]`, `4`, `;` and so on — and `4` switches tabs.
//!
//! A terminal that answers no OSC query leaves [`get`] empty, and the fade
//! falls back to two palette tokens.

// The query only exists on unix; elsewhere its parsing is reached from the
// tests alone.
#![cfg_attr(not(unix), allow(dead_code))]

use ratatui::style::Color;
use std::sync::OnceLock;

/// What the terminal reported. Any slot it didn't answer for is `None`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Palette {
    pub slots: [Option<(u8, u8, u8)>; 16],
    pub bg: Option<(u8, u8, u8)>,
}

impl Palette {
    /// The RGB the terminal draws `c` in, for the colours the `terminal`
    /// theme uses. `None` for anything the terminal didn't report and for
    /// `Reset`, which has no single colour.
    pub fn rgb(&self, c: Color) -> Option<(u8, u8, u8)> {
        let slot = match c {
            Color::Rgb(r, g, b) => return Some((r, g, b)),
            Color::Black => 0,
            Color::Red => 1,
            Color::Green => 2,
            Color::Yellow => 3,
            Color::Blue => 4,
            Color::Magenta => 5,
            Color::Cyan => 6,
            Color::Gray => 7,
            Color::DarkGray => 8,
            Color::LightRed => 9,
            Color::LightGreen => 10,
            Color::LightYellow => 11,
            Color::LightBlue => 12,
            Color::LightMagenta => 13,
            Color::LightCyan => 14,
            Color::White => 15,
            Color::Indexed(n) if n < 16 => n as usize,
            _ => return None,
        };
        self.slots[slot]
    }

    /// Enough to build a fade from: a background to fade against.
    fn usable(&self) -> bool {
        self.bg.is_some() && self.slots.iter().any(Option::is_some)
    }
}

static PALETTE: OnceLock<Palette> = OnceLock::new();

/// The palette the terminal reported at startup, if it reported one.
pub fn get() -> Option<&'static Palette> {
    PALETTE.get()
}

/// Ask the terminal, and keep the answer for [`get`]. Call once, in raw
/// mode, before anything else reads keys.
pub fn init() {
    if let Some(p) = query() {
        let _ = PALETTE.set(p);
    }
}

#[cfg(unix)]
fn query() -> Option<Palette> {
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::time::{Duration, Instant};

    if !worth_asking(
        std::env::var("TERM").ok().as_deref(),
        std::env::var("TERM_PROGRAM").ok().as_deref(),
        std::env::var("COLORTERM").ok().as_deref(),
    ) {
        return None;
    }
    // The controlling terminal, not stdin: the key reader drains the same
    // device whichever descriptor it holds, and stdin can be a pipe.
    let mut tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .ok()?;
    let mut ask = String::new();
    for n in 0..16 {
        ask.push_str(&format!("\x1b]4;{n};?\x1b\\"));
    }
    ask.push_str("\x1b]11;?\x1b\\\x1b[c");
    tty.write_all(ask.as_bytes()).ok()?;
    tty.flush().ok()?;

    // Local terminals answer in a few milliseconds. The limit is for a slow
    // ssh link, and for a terminal that ignores DA1, which would otherwise
    // hold startup forever.
    let deadline = Instant::now() + Duration::from_millis(600);
    let mut got = Vec::with_capacity(1024);
    let mut chunk = [0u8; 512];
    while !answered(&got) {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let mut fds = libc::pollfd {
            fd: tty.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd, and the count says so.
        let ready = unsafe { libc::poll(&mut fds, 1, left.as_millis() as libc::c_int) };
        if ready <= 0 {
            break;
        }
        match tty.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => got.extend_from_slice(&chunk[..n]),
        }
        if got.len() > 16 * 1024 {
            break;
        }
    }
    let p = parse(&got);
    p.usable().then_some(p)
}

#[cfg(not(unix))]
fn query() -> Option<Palette> {
    None
}

/// Whether to send the queries at all.
///
/// The Linux console doesn't do RGB, and older kernels print an unknown OSC
/// as text. Terminal.app before macOS 26 answers colour queries but can't
/// draw 24-bit colour, and says so by leaving `COLORTERM` unset.
fn worth_asking(term: Option<&str>, term_program: Option<&str>, colorterm: Option<&str>) -> bool {
    let term = term.unwrap_or("");
    if term.is_empty() || term == "dumb" || term == "linux" {
        return false;
    }
    let truecolor = matches!(colorterm, Some("truecolor" | "24bit"));
    !(term_program == Some("Apple_Terminal") && !truecolor)
}

/// Whether the DA1 reply, `ESC [ ? … c`, is in.
fn answered(buf: &[u8]) -> bool {
    let mut i = 0;
    while let Some(at) = find(&buf[i..], b"\x1b[?") {
        let start = i + at + 3;
        let tail = &buf[start..];
        let end = tail
            .iter()
            .position(|b| !(b.is_ascii_digit() || *b == b';'))
            .map(|p| (p, tail[p]));
        if let Some((_, b'c')) = end {
            return true;
        }
        i = start;
    }
    false
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Every OSC 4 and OSC 11 reply in `buf`. Replies end in ST (`ESC \`) or
/// BEL; both are in use.
fn parse(buf: &[u8]) -> Palette {
    let mut p = Palette::default();
    let text = String::from_utf8_lossy(buf);
    for reply in text.split("\x1b]").skip(1) {
        let body = reply.split(['\x07', '\x1b']).next().unwrap_or_default();
        let mut parts = body.splitn(3, ';');
        match (parts.next(), parts.next(), parts.next()) {
            (Some("4"), Some(n), Some(spec)) => {
                if let (Ok(n), Some(rgb)) = (n.parse::<usize>(), parse_rgb(spec)) {
                    if n < 16 {
                        p.slots[n] = Some(rgb);
                    }
                }
            }
            (Some("11"), Some(spec), None) => p.bg = parse_rgb(spec),
            _ => {}
        }
    }
    p
}

/// `rgb:RRRR/GGGG/BBBB`, one to four hex digits per channel. `rgba:` adds an
/// alpha channel, which is ignored.
fn parse_rgb(spec: &str) -> Option<(u8, u8, u8)> {
    let channels = spec
        .strip_prefix("rgb:")
        .or_else(|| spec.strip_prefix("rgba:"))?;
    let mut it = channels.split('/').map(|h| {
        if h.is_empty() || h.len() > 4 {
            return None;
        }
        let v = u32::from_str_radix(h, 16).ok()?;
        let max = (1u32 << (4 * h.len())) - 1;
        Some(((v * 255 + max / 2) / max) as u8)
    });
    Some((it.next()??, it.next()??, it.next()??))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_xterm_and_kitty_style_replies() {
        // xterm: four digits per channel, ST. kitty: two digits, BEL.
        let buf = b"\x1b]4;2;rgb:5c5c/d9d9/8989\x1b\\\
                    \x1b]4;6;rgb:5f/dc/ff\x07\
                    \x1b]11;rgb:0c0c/1414/1818\x1b\\\
                    \x1b[?62;22c";
        let p = parse(buf);
        assert_eq!(p.slots[2], Some((0x5c, 0xd9, 0x89)));
        assert_eq!(p.slots[6], Some((0x5f, 0xdc, 0xff)));
        assert_eq!(p.bg, Some((0x0c, 0x14, 0x18)));
        assert_eq!(p.rgb(Color::Green), Some((0x5c, 0xd9, 0x89)));
        assert_eq!(p.rgb(Color::Cyan), Some((0x5f, 0xdc, 0xff)));
        assert_eq!(p.rgb(Color::Red), None);
        assert!(p.usable());
    }

    #[test]
    fn short_channels_scale_to_full_range() {
        assert_eq!(parse_rgb("rgb:f/0/8"), Some((255, 0, 136)));
        assert_eq!(parse_rgb("rgb:fff/000/800"), Some((255, 0, 128)));
        assert_eq!(parse_rgb("rgba:ffff/0000/0000/ffff"), Some((255, 0, 0)));
        assert_eq!(parse_rgb("rgb:ff/00"), None);
        assert_eq!(parse_rgb("rgb:fffff/0/0"), None);
        assert_eq!(parse_rgb("#ff0000"), None);
    }

    #[test]
    fn a_terminal_that_only_answers_da1_reports_nothing() {
        let p = parse(b"\x1b[?1;2c");
        assert_eq!(p, Palette::default());
        assert!(!p.usable());
    }

    #[test]
    fn slots_without_a_background_are_not_enough() {
        // The fade is measured against the background. Without it there is
        // nothing to fade toward, and the two-token fallback is the honest one.
        let p = parse(b"\x1b]4;2;rgb:00/ff/00\x1b\\\x1b[?6c");
        assert!(!p.usable());
    }

    #[test]
    fn waits_for_the_da1_reply_and_nothing_else() {
        assert!(!answered(b""));
        assert!(!answered(b"\x1b]4;2;rgb:00/ff/00\x1b\\"));
        // A reply split across two reads.
        assert!(!answered(b"\x1b[?62;22"));
        assert!(answered(b"\x1b[?62;22c"));
        assert!(answered(b"\x1b]11;rgb:0/0/0\x07\x1b[?6c"));
        // A cursor report is not DA1.
        assert!(!answered(b"\x1b[?1;2R"));
    }

    #[test]
    fn skips_terminals_that_cannot_use_the_answer() {
        assert!(worth_asking(Some("xterm-256color"), None, None));
        assert!(worth_asking(Some("xterm-kitty"), None, Some("truecolor")));
        assert!(!worth_asking(Some("linux"), None, None));
        assert!(!worth_asking(Some("dumb"), None, None));
        assert!(!worth_asking(None, None, None));
        assert!(!worth_asking(
            Some("xterm-256color"),
            Some("Apple_Terminal"),
            None
        ));
        assert!(worth_asking(
            Some("xterm-256color"),
            Some("Apple_Terminal"),
            Some("truecolor")
        ));
    }

    #[test]
    fn named_and_low_indexed_colours_map_to_their_slots() {
        let mut p = Palette::default();
        for n in 0..16u8 {
            p.slots[n as usize] = Some((n, n, n));
        }
        assert_eq!(p.rgb(Color::Black), Some((0, 0, 0)));
        assert_eq!(p.rgb(Color::LightCyan), Some((14, 14, 14)));
        assert_eq!(p.rgb(Color::Indexed(9)), Some((9, 9, 9)));
        assert_eq!(p.rgb(Color::Indexed(200)), None);
        assert_eq!(p.rgb(Color::Reset), None);
    }
}
