//! Insights tab — port of `dwRenderInsights`.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::app::App;
use crate::insights::{Insight, Severity};
use crate::ui::palette as p;

pub fn draw(f: &mut Frame, area: Rect, app: &App) {
    let crit = app
        .insights
        .iter()
        .filter(|i| i.sev == Severity::Crit)
        .count();
    let warn = app
        .insights
        .iter()
        .filter(|i| i.sev == Severity::Warn)
        .count();
    let info = app
        .insights
        .iter()
        .filter(|i| i.sev == Severity::Info)
        .count();

    let header_line = Line::from(vec![
        Span::raw(" "),
        Span::styled(
            "\u{25cf} ",
            Style::default().fg(if crit > 0 {
                p::red()
            } else if warn > 0 {
                p::yellow()
            } else {
                p::cyan()
            }),
        ),
        Span::styled(
            format!("{} active", app.insights.len()),
            Style::default().fg(p::fg()).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!("{} crit  {} warn  {} info", crit, warn, info),
            Style::default().fg(p::dim()),
        ),
    ]);
    f.render_widget(
        Paragraph::new(header_line).style(Style::default().bg(p::bg())),
        Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: 1,
        },
    );

    // Each card is 6 rows tall.
    let mut y = area.y + 2;
    let max_y = area.y + area.height;
    for ins in &app.insights {
        if y + 6 > max_y {
            break;
        }
        draw_card(
            f,
            Rect {
                x: area.x,
                y,
                width: area.width,
                height: 6,
            },
            ins,
        );
        y += 6;
    }

    if y < max_y {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  Insights are read-only suggestions   they never modify devices, volumes, or filesystems.",
                Style::default().fg(p::dim()),
            )))
            .style(Style::default().bg(p::bg())),
            Rect {
                x: area.x,
                y,
                width: area.width,
                height: 1,
            },
        );
    }
}

fn draw_card(f: &mut Frame, area: Rect, ins: &Insight) {
    let (sev_color, sev_bg) = match ins.sev {
        Severity::Crit => (p::red(), p::err_bg()),
        Severity::Warn => (p::yellow(), p::warn_bg()),
        Severity::Info => (p::cyan(), p::ok_bg()),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(sev_color).bg(p::bg()))
        .style(Style::default().bg(p::bg()));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line<'static>> = Vec::new();
    let header = Line::from(vec![
        Span::raw(" "),
        Span::styled(
            format!(" {} ", ins.sev.label()),
            Style::default()
                .fg(sev_color)
                .bg(sev_bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            ins.title.clone(),
            Style::default()
                .fg(p::br_white())
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    lines.push(header);

    for (i, b) in ins.body.iter().enumerate().take(3) {
        let color = if i == 0 { p::fg() } else { p::dim() };
        lines.push(Line::from(Span::styled(
            format!(" {}", b),
            Style::default().fg(color),
        )));
    }
    if !ins.suggested_tab.is_empty() {
        lines.push(Line::from(Span::styled(
            format!(" \u{2192} open [{}] tab", ins.suggested_tab),
            Style::default().fg(p::cyan()),
        )));
    }
    f.render_widget(
        Paragraph::new(lines).style(Style::default().bg(p::bg())),
        inner,
    );
}
