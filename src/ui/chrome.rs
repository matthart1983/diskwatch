use chrono::Local;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use ratatui::Frame;

use crate::app::{HostInfo, LiveState};
use crate::tabs::{TabId, ALL_TABS};
use crate::ui::palette as p;

pub fn draw_header(f: &mut Frame, area: Rect, host: &HostInfo, live: LiveState) {
    let mut left: Vec<Span> = Vec::new();
    left.push(Span::styled(" \u{25cf}", Style::default().fg(p::GREEN)));
    left.push(Span::styled(
        " DiskWatch",
        Style::default().fg(p::CYAN).add_modifier(Modifier::BOLD),
    ));
    left.push(Span::styled(
        format!(" v{}", env!("CARGO_PKG_VERSION")),
        Style::default().fg(p::DIM),
    ));
    left.push(Span::styled("  \u{2502}  ", Style::default().fg(p::FAINT)));
    left.push(Span::styled("host ", Style::default().fg(p::DIM)));
    left.push(Span::styled(
        host.hostname.clone(),
        Style::default().fg(p::FG),
    ));
    left.push(Span::styled("  ", Style::default().fg(p::DIM)));
    left.push(Span::styled(host.os.clone(), Style::default().fg(p::FG)));
    left.push(Span::styled("  up ", Style::default().fg(p::DIM)));
    left.push(Span::styled(
        format_uptime(host.uptime_secs),
        Style::default().fg(p::FG),
    ));
    left.push(Span::styled("  ", Style::default().fg(p::DIM)));
    left.push(Span::styled(
        format!("{} devs", host.device_count),
        Style::default().fg(p::FG),
    ));
    left.push(Span::styled(" attached", Style::default().fg(p::DIM)));

    let (label, color) = match live {
        LiveState::Live => ("LIVE", p::GREEN),
        LiveState::Paused => ("PAUSE", p::YELLOW),
    };
    let ts = Local::now().format("%H:%M:%S").to_string();
    let right_text = format!("\u{25cf} {}  {}", label, ts);
    let right_w = right_text.chars().count() as u16 + 1;
    let right = vec![Span::styled(
        right_text,
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )];

    let left_area = Rect {
        x: area.x,
        y: area.y,
        width: area.width.saturating_sub(right_w),
        height: 1,
    };
    let right_area = Rect {
        x: area.x + area.width.saturating_sub(right_w),
        y: area.y,
        width: right_w,
        height: 1,
    };

    f.render_widget(
        Paragraph::new(Line::from(left)).style(Style::default().bg(p::BG).fg(p::FG)),
        left_area,
    );
    f.render_widget(
        Paragraph::new(Line::from(right)).style(Style::default().bg(p::BG)),
        right_area,
    );
}

/// Two-row tab bar: numbered labels on row 0, underline on row 1 with
/// corner glyphs around the active tab (matches `dwDrawTabBar`).
pub fn draw_tab_bar(f: &mut Frame, area: Rect, active: TabId, insight_count: usize) {
    let mut spans: Vec<Span> = Vec::new();
    let mut underline = String::new();
    let mut active_start: Option<usize> = None;
    let mut active_end: Option<usize> = None;
    let mut col: usize = 0;

    for tab in ALL_TABS {
        let badge = if *tab == TabId::Insights && insight_count > 0 {
            format!(" {}", insight_count)
        } else {
            String::new()
        };
        let label = format!("[{}] {}{}", tab.number(), tab.label(), badge);
        let cell_w = label.chars().count() + 2;

        if *tab == active {
            spans.push(Span::styled(" ", Style::default().bg(p::BG)));
            spans.push(Span::styled(
                label.clone(),
                Style::default()
                    .fg(p::CYAN)
                    .bg(p::BG)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(" ", Style::default().bg(p::BG)));
            active_start = Some(col);
            active_end = Some(col + cell_w);
        } else {
            spans.push(Span::styled(" ", Style::default().fg(p::DIM)));
            spans.push(Span::styled(
                format!("[{}] ", tab.number()),
                Style::default().fg(p::DIM),
            ));
            spans.push(Span::styled(
                tab.label().to_string(),
                Style::default().fg(p::FG),
            ));
            if !badge.is_empty() {
                spans.push(Span::styled(badge, Style::default().fg(p::DIM)));
            }
            spans.push(Span::styled(" ", Style::default().fg(p::DIM)));
        }

        col += cell_w;
    }

    // Underline (row 1): horizontal rule across full width with `┘ … └`
    // corners breaking it under the active tab.
    let total_w = area.width as usize;
    for i in 0..total_w {
        let c = if Some(i) == active_start {
            '\u{2518}'
        } else if Some(i + 1) == active_end {
            '\u{2514}'
        } else if let (Some(s), Some(e)) = (active_start, active_end) {
            if i > s && i + 1 < e {
                ' '
            } else {
                '\u{2500}'
            }
        } else {
            '\u{2500}'
        };
        underline.push(c);
    }

    let labels_area = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: 1,
    };
    let underline_area = Rect {
        x: area.x,
        y: area.y + 1,
        width: area.width,
        height: 1,
    };
    f.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(p::BG)),
        labels_area,
    );
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            underline,
            Style::default().fg(p::FAINT).bg(p::BG),
        ))),
        underline_area,
    );
}

pub fn draw_footer(f: &mut Frame, area: Rect, extra: &[(char, &str)]) {
    let mut spans: Vec<Span> = Vec::new();
    spans.push(Span::raw(" "));
    let groups: &[&[(char, &str)]] = &[
        &[('p', "Pause"), (',', "Settings")],
        &[
            ('S', "Snapshot"),
            ('D', "Diff"),
            ('P', "Profile"),
            ('R', "Rec"),
        ],
        &[('/', "Filter"), ('q', "Quit"), ('1', "Tab")],
        &[('?', "Help")],
    ];
    for (gi, g) in groups.iter().enumerate() {
        if gi > 0 {
            spans.push(Span::styled(" \u{2502} ", Style::default().fg(p::FAINT)));
        }
        for (k, label) in g.iter() {
            let key_str = if *k == '1' {
                "1-9".to_string()
            } else {
                k.to_string()
            };
            spans.push(Span::styled(
                key_str,
                Style::default().fg(p::CYAN).add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(
                format!(":{} ", label),
                Style::default().fg(p::DIM),
            ));
        }
    }
    if !extra.is_empty() {
        spans.push(Span::styled(" \u{2502} ", Style::default().fg(p::FAINT)));
        for (k, label) in extra.iter() {
            spans.push(Span::styled(
                k.to_string(),
                Style::default().fg(p::YELLOW).add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(
                format!(":{} ", label),
                Style::default().fg(p::DIM),
            ));
        }
    }
    // Divider row above the footer text.
    let divider_area = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: 1,
    };
    let text_area = Rect {
        x: area.x,
        y: area.y + 1,
        width: area.width,
        height: 1,
    };
    let divider: String = "\u{2500}".repeat(area.width as usize);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            divider,
            Style::default().fg(p::FAINT).bg(p::BG),
        ))),
        divider_area,
    );
    f.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(p::BG)),
        text_area,
    );
}

fn format_uptime(secs: u64) -> String {
    let d = secs / 86_400;
    let h = (secs % 86_400) / 3600;
    let m = (secs % 3600) / 60;
    if d > 0 {
        format!("{}d {:02}:{:02}", d, h, m)
    } else {
        format!("{:02}:{:02}", h, m)
    }
}

/// Tints a Rect's background — used for selected rows / warning rows.
#[allow(dead_code)]
pub fn tint_rect(buf: &mut Buffer, area: Rect, bg: ratatui::style::Color) {
    for y in area.y..area.y.saturating_add(area.height) {
        for x in area.x..area.x.saturating_add(area.width) {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_bg(bg);
            }
        }
    }
}

/// `Widget` adapter so callers can `render_widget` a tint over an area —
/// kept around for future tabs even though chrome doesn't use it directly.
#[allow(dead_code)]
pub struct Tint(pub ratatui::style::Color);
impl Widget for Tint {
    fn render(self, area: Rect, buf: &mut Buffer) {
        tint_rect(buf, area, self.0);
    }
}
