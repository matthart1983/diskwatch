use std::io;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::widgets::Paragraph;
use ratatui::Terminal;
use sysinfo::System;

use crate::collect;
use crate::tabs::{self, TabId, ALL_TABS};
use crate::ui::chrome;
use crate::ui::palette as p;

/// Default SMART poll interval. Upstream shipped 5 minutes because
/// polling more often shortens drive life on some models. Users who want
/// live temperature monitoring can dial it down with `+`/`-` (down to
/// `MIN_SMART_INTERVAL_SECS`).
pub const DEFAULT_SMART_INTERVAL_SECS: u64 = 300;
/// Lower bound for SMART polls. Below 5s we'd be issuing more than one
/// smartctl subprocess per second per disk, which costs CPU and battery
/// for no temperature-resolution benefit (drives typically report
/// temperature at 1-2 Hz internally).
pub const MIN_SMART_INTERVAL_SECS: u64 = 5;
/// Step size for `+`/`-` interval nudges, in seconds.
pub const INTERVAL_STEP_SECS: u64 = 60;

pub struct Options {
    pub start_tab: Option<String>,
}

/// Bitflags for which columns are visible in the Overview tab's
/// DEVICES summary list. Stored as a single `u8` so toggling one
/// column doesn't have to read/write the whole struct.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VisibleColumns(pub u8);

impl VisibleColumns {
    pub const SIZE: u8 = 1 << 0;
    pub const FREE: u8 = 1 << 1;
    pub const USED_PCT: u8 = 1 << 2;
    pub const TEMP: u8 = 1 << 3;
    pub const SMART: u8 = 1 << 4;
    /// All columns visible — the default.
    pub const ALL: u8 = Self::SIZE | Self::FREE | Self::USED_PCT | Self::TEMP | Self::SMART;

    pub fn contains(self, other: u8) -> bool {
        self.0 & other != 0
    }
    pub fn toggle(&mut self, col: u8) {
        self.0 ^= col;
    }
}

/// Display unit for SMART-reported temperatures. Drive firmware always
/// reports °C, so `temp_unit` only governs the display layer; the
/// conversion happens in the render functions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TempUnit {
    Celsius,
    Fahrenheit,
}
impl TempUnit {
    pub fn next(self) -> Self {
        match self {
            TempUnit::Celsius => TempUnit::Fahrenheit,
            TempUnit::Fahrenheit => TempUnit::Celsius,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            TempUnit::Celsius => "Celsius (°C)",
            TempUnit::Fahrenheit => "Fahrenheit (°F)",
        }
    }
    /// Convert a Celsius temperature (the form drive firmware reports)
    /// to the configured display unit.
    pub fn format_temp(self, c: i16) -> String {
        match self {
            TempUnit::Celsius => format!("{}°C", c),
            TempUnit::Fahrenheit => {
                let f = c as f64 * 9.0 / 5.0 + 32.0;
                format!("{:.0}°F", f)
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveState {
    Live,
    Paused,
}

#[derive(Debug, Clone)]
pub struct HostInfo {
    pub hostname: String,
    pub os: String,
    pub uptime_secs: u64,
    pub device_count: usize,
}

pub struct App {
    pub active_tab: TabId,
    pub live: LiveState,
    pub host: HostInfo,
    pub devices: Vec<collect::DeviceTick>,
    pub filesystems: Vec<collect::FsTick>,
    pub volumes: collect::VolumeTick,
    pub io: collect::IoCollector,
    pub smart: collect::SmartCollector,
    pub hot_files: collect::hot_files::HotFileWatcher,
    pub insights: Vec<crate::insights::Insight>,
    pub selected_device: usize,
    pub selected_fs: usize,
    /// Last full enumeration (slow path — system_profiler + diskutil).
    last_metadata_refresh: Instant,
    /// Last usage refresh (fast path — sysinfo only).
    last_usage_refresh: Instant,
    /// Configured SMART poll cadence. Mutable at runtime via `+` / `-`
    /// and `Shift+1..4` presets.
    pub smart_interval_secs: u64,
    /// Cached label for the footer ("+ 60s") so we don't allocate per
    /// draw frame.
    pub smart_interval_label: String,
    /// True while the `?` help overlay is being shown.
    pub show_help: bool,
    /// True while the `,` settings overlay is being shown.
    pub show_settings: bool,
    /// Currently highlighted row inside the settings overlay (0-indexed).
    pub settings_cursor: usize,
    /// Which columns to show in the Overview tab's DEVICES table.
    /// Each bit toggles a single column. Default: all visible.
    pub visible_columns: VisibleColumns,
    /// Temperature unit (`C` for Celsius, `F` for Fahrenheit). Affects
    /// every place that renders a temperature: Overview TEMP column,
    /// SMART tab summary header, and the smartctl collector's display
    /// of raw values from the SMART tab (which stay in °C since they
    /// come straight from device firmware).
    pub temp_unit: TempUnit,
    /// Set by `r` to force a SMART refresh on the next tick, regardless
    /// of the elapsed-since-last interval.
    pub smart_refresh_requested: bool,
    pub should_quit: bool,
}

impl App {
    fn new(start: TabId) -> Self {
        let devices = collect::devices::collect();
        let filesystems = collect::filesystems::collect();
        let volumes = collect::volumes::collect();
        let io = collect::IoCollector::new();
        let mut smart = collect::SmartCollector::new();
        smart.set_interval(Duration::from_secs(DEFAULT_SMART_INTERVAL_SECS));
        smart.refresh_if_due(&devices);
        let roots = collect::hot_files::default_roots();
        let root_refs: Vec<&std::path::Path> = roots.iter().map(|p| p.as_path()).collect();
        let hot_files = collect::hot_files::HotFileWatcher::start(&root_refs);
        Self {
            active_tab: start,
            live: LiveState::Live,
            host: read_host(devices.len()),
            selected_device: 0,
            selected_fs: 0,
            devices,
            filesystems,
            volumes,
            io,
            smart,
            hot_files,
            insights: Vec::new(),
            last_metadata_refresh: Instant::now(),
            last_usage_refresh: Instant::now(),
            smart_interval_secs: DEFAULT_SMART_INTERVAL_SECS,
            smart_interval_label: format_smart_label(DEFAULT_SMART_INTERVAL_SECS),
            show_help: false,
            show_settings: false,
            settings_cursor: 0,
            visible_columns: VisibleColumns(VisibleColumns::ALL),
            temp_unit: TempUnit::Celsius,
            smart_refresh_requested: false,
            should_quit: false,
        }
    }

    fn tick(&mut self) {
        if matches!(self.live, LiveState::Paused) {
            return;
        }
        // IO is hot enough to warrant 5Hz sampling for its own latency
        // percentile window. The collector rate-limits internally, so
        // calling every frame is fine.
        self.io.sample();

        // Slower path: sysinfo-only — used bytes + mounts list at 1Hz.
        let usage_elapsed = self.last_usage_refresh.elapsed();
        if usage_elapsed >= Duration::from_millis(1000) {
            collect::devices::refresh_usage(&mut self.devices);
            self.filesystems = collect::filesystems::collect();
            if self.selected_fs >= self.filesystems.len() && !self.filesystems.is_empty() {
                self.selected_fs = self.filesystems.len() - 1;
            }
            // Decay per-file EWMA rates and prune idle / overflowed
            // entries. The watcher thread keeps writing into the same
            // map between calls; we just shape it back down.
            self.hot_files.decay(usage_elapsed);
            self.host.uptime_secs = System::uptime();
            self.last_usage_refresh = Instant::now();
        }
        // Slow path: system_profiler + diskutil. Picks up new drives.
        if self.last_metadata_refresh.elapsed() >= Duration::from_secs(30) {
            self.devices = collect::devices::collect();
            self.volumes = collect::volumes::collect();
            self.host.device_count = self.devices.len();
            if self.selected_device >= self.devices.len() && !self.devices.is_empty() {
                self.selected_device = self.devices.len() - 1;
            }
            self.last_metadata_refresh = Instant::now();
        }
        // SMART cadence is configurable at runtime via `+` / `-` / Shift+1..4.
        // Push the current setting into the collector before each tick so
        // user changes take effect immediately.
        self.smart
            .set_interval(Duration::from_secs(self.smart_interval_secs));
        if self.smart_refresh_requested {
            self.smart.force_refresh(&self.devices);
            self.smart_refresh_requested = false;
        } else {
            self.smart.refresh_if_due(&self.devices);
        }

        // Recompute insights each tick — pure functions over current
        // state, so this is cheap.
        self.insights = crate::insights::evaluate(
            &self.devices,
            &self.filesystems,
            &self.io.latest,
            &self.smart,
            &self.hot_files,
        );
    }
}

pub fn run(opts: Options) -> Result<()> {
    let start = opts
        .start_tab
        .as_deref()
        .and_then(TabId::from_str)
        .unwrap_or(TabId::Overview);
    let mut app = App::new(start);

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let res = main_loop(&mut terminal, &mut app);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    res
}

fn main_loop(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &mut App) -> Result<()> {
    let frame_budget = Duration::from_millis(50);
    loop {
        terminal.draw(|f| draw(f, app))?;
        if event::poll(frame_budget)? {
            if let Event::Key(k) = event::read()? {
                if k.kind != KeyEventKind::Release {
                    handle_key(app, k.code);
                }
            }
        }
        app.tick();
        if app.should_quit {
            return Ok(());
        }
    }
}

fn handle_key(app: &mut App, key: KeyCode) {
    // While the help overlay is up, every key except `?` and `q` / `Esc`
    // just dismisses it. Keeps the overlay from accidentally triggering
    // tab switches underneath.
    if app.show_help {
        match key {
            KeyCode::Char('?') | KeyCode::Char('q') | KeyCode::Esc => app.show_help = false,
            KeyCode::Char('Q') => app.should_quit = true,
            _ => app.show_help = false,
        }
        return;
    }

    // Settings modal gets its own keymap. Keys are scoped to the
    // settings list and don't leak into tab cycling. Space toggles
    // column visibility / cycles the temp unit / cycles the SMART
    // interval preset.
    if app.show_settings {
        handle_settings_key(app, key);
        return;
    }

    match key {
        KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,

        KeyCode::Char('p') => {
            app.live = match app.live {
                LiveState::Live => LiveState::Paused,
                LiveState::Paused => LiveState::Live,
            };
        }

        KeyCode::Char('?') => {
            app.show_help = true;
        }

        KeyCode::Char(',') => {
            app.show_settings = !app.show_settings;
            app.settings_cursor = 0;
        }

        // Force a SMART refresh on the next tick.
        KeyCode::Char('r') | KeyCode::Char('R') => {
            app.smart_refresh_requested = true;
        }

        // Nudge the SMART poll interval. `+` / `=` up, `-` / `_` down.
        KeyCode::Char('+') | KeyCode::Char('=') => {
            let next = app.smart_interval_secs.saturating_add(INTERVAL_STEP_SECS);
            app.smart_interval_secs = next;
            app.smart_interval_label = format_smart_label(next);
            let collector_secs = app.smart.current_interval().as_secs();
            if collector_secs != next {
                app.smart.set_interval(Duration::from_secs(next));
            }
        }
        KeyCode::Char('-') | KeyCode::Char('_') => {
            let next = app
                .smart_interval_secs
                .saturating_sub(INTERVAL_STEP_SECS)
                .max(MIN_SMART_INTERVAL_SECS);
            app.smart_interval_secs = next;
            app.smart_interval_label = format_smart_label(next);
            app.smart.set_interval(Duration::from_secs(next));
        }

        // `0` resets to the upstream-conservative default.
        KeyCode::Char('0') => {
            app.smart_interval_secs = DEFAULT_SMART_INTERVAL_SECS;
            app.smart_interval_label =
                format_smart_label(DEFAULT_SMART_INTERVAL_SECS);
            app.smart
                .set_interval(Duration::from_secs(DEFAULT_SMART_INTERVAL_SECS));
            app.smart_refresh_requested = true;
        }

        // Tab cycling: Tab/BackTab always cycle. Left/Right cycle tabs
        // ONLY on tabs that don't have a picker (IO, Insights, Hot Files)
        // — on tabs that have a picker they move the selection (handled
        // below). This matches user expectation: arrows in a list move
        // the cursor; arrows in a single-pane tab cycle neighbors.
        KeyCode::BackTab => cycle_tab(app, -1),
        KeyCode::Tab => cycle_tab(app, 1),

        // Device / fs / volume selectors. All four arrow keys AND
        // h/j/k/l work on every tab that exposes a picker (Devices,
        // SMART, FS, Volumes, Overview's device summary). Left/Right
        // also cycle tabs on tabs that have no picker (IO, Insights,
        // Hot Files), so users never get a "dead" arrow key.
        KeyCode::Up | KeyCode::Char('k') => move_selection(app, -1, 0),
        KeyCode::Down | KeyCode::Char('j') => move_selection(app, 1, 0),
        KeyCode::Left | KeyCode::Char('h') => {
            if picker_active(app) {
                move_selection(app, -1, 0);
            } else {
                cycle_tab(app, -1);
            }
        }
        KeyCode::Right | KeyCode::Char('l') => {
            if picker_active(app) {
                move_selection(app, 1, 0);
            } else {
                cycle_tab(app, 1);
            }
        }
        KeyCode::Home => move_selection(app, 0, -1),
        KeyCode::End => move_selection(app, 0, 1),
        KeyCode::PageUp => move_selection(app, -5, 0),
        KeyCode::PageDown => move_selection(app, 5, 0),

        // Digit keys 1-9 jump directly to tabs (this is the existing
        // upstream behavior — kept identical so muscle memory works).
        KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
            let idx = (c as u8 - b'1') as usize;
            if let Some(t) = ALL_TABS.get(idx) {
                app.active_tab = *t;
            }
        }

        _ => {}
    }
}

/// True when the active tab exposes a list of items (devices,
/// filesystems) the user navigates with arrow keys. On these tabs,
/// ←/→ moves the cursor; on tabs without a picker, ←/→ cycles tabs.
///
/// Volumes has its own picker state (selected_container /
/// selected_array) that's managed inside tabs/volumes.rs, so it owns
/// its own Up/Down handling — we treat it as "no picker" here so Left/
/// Right still cycle tabs and the user never hits a dead arrow key.
fn picker_active(app: &App) -> bool {
    matches!(
        app.active_tab,
        TabId::Overview | TabId::Devices | TabId::Smart | TabId::Fs
    )
}

// Settings modal: how many rows in the dialog. Kept as a constant so
// `handle_settings_key` and `draw_settings_overlay` agree on the bounds.
const SETTINGS_ROWS: usize = 7;
// Below this index, rows are toggles (column visibility bitflags);
// at or above, rows are cycle setters (temp unit, SMART interval).
const SETTINGS_FIRST_CYCLE: usize = 5;

fn handle_settings_key(app: &mut App, key: KeyCode) {
    match key {
        KeyCode::Esc | KeyCode::Char(',') | KeyCode::Char('q') => {
            app.show_settings = false;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if app.settings_cursor == 0 {
                app.settings_cursor = SETTINGS_ROWS - 1;
            } else {
                app.settings_cursor -= 1;
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.settings_cursor = (app.settings_cursor + 1) % SETTINGS_ROWS;
        }
        KeyCode::Char(' ') | KeyCode::Enter => {
            // Toggle if the row is a column flag, cycle if it's the
            // temp-unit or SMART-interval row.
            match app.settings_cursor {
                // Column visibility toggles — rows 0..5.
                0 => app.visible_columns.toggle(VisibleColumns::SIZE),
                1 => app.visible_columns.toggle(VisibleColumns::FREE),
                2 => app.visible_columns.toggle(VisibleColumns::USED_PCT),
                3 => app.visible_columns.toggle(VisibleColumns::TEMP),
                4 => app.visible_columns.toggle(VisibleColumns::SMART),
                // Cycling setters — rows 5 and 6.
                5 => {
                    app.temp_unit = app.temp_unit.next();
                }
                6 => {
                    // Cycle SMART interval through the four preset
                    // buckets. Used `r` key for "force refresh" and
                    // `+`/`-` to nudge — this is the third dial.
                    let current = app.smart_interval_secs;
                    let presets = [10u64, 30, 60, 300];
                    let next = presets
                        .iter()
                        .find(|&&p| p > current)
                        .copied()
                        .unwrap_or(presets[0]);
                    app.smart_interval_secs = next;
                    app.smart_interval_label = format_smart_label(next);
                    app.smart.set_interval(Duration::from_secs(next));
                    app.smart_refresh_requested = true;
                }
                _ => {}
            }
        }
        _ => {}
    }
}

/// Move the per-tab selection. `dy` is the relative step (-1 for up,
/// +1 for down, ±5 for page jumps). `mode` selects jump-to-end: -1
/// for Home (first), +1 for End (last), 0 for relative step.
/// Wraps around at the boundaries so the user can't get stuck.
///
/// Volumes uses its own selection state (selected_container /
/// selected_array) so this helper only handles the device picker and
/// the filesystem picker, which share the same `selected_fs` /
/// `selected_device` index pair.
fn move_selection(app: &mut App, dy: i32, mode: i32) {
    let (cur, len) = match app.active_tab {
        TabId::Fs => (app.selected_fs, app.filesystems.len()),
        _ => (app.selected_device, app.devices.len()),
    };
    if len == 0 {
        return;
    }
    let next: usize = if mode == -1 {
        0
    } else if mode == 1 {
        len - 1
    } else {
        let n = len as i32;
        let r = ((cur as i32 + dy) % n + n) % n;
        r as usize
    };
    match app.active_tab {
        TabId::Fs => app.selected_fs = next,
        _ => app.selected_device = next,
    }
}

fn cycle_tab(app: &mut App, delta: i32) {
    let cur = ALL_TABS
        .iter()
        .position(|t| *t == app.active_tab)
        .unwrap_or(0);
    let n = ALL_TABS.len() as i32;
    let next = ((cur as i32 + delta) % n + n) % n;
    app.active_tab = ALL_TABS[next as usize];
}

fn format_smart_label(secs: u64) -> String {
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h", secs / 3600)
    }
}

/// Footer hint label for the +/- interval keys. Shows the step size in
/// seconds so users know the magnitude of the nudge without opening the
/// help overlay.
fn step_label() -> String {
    format!("±{}s", INTERVAL_STEP_SECS)
}

fn draw(f: &mut ratatui::Frame, app: &App) {
    // Defensive: ratatui 0.29 panics on any draw into a Rect with
    // width=0 or height=0 (Buffer::cell_mut bounds check at
    // buffer.rs:253). Some terminals report a 0×0 size for the first
    // ~1 frame after EnterAlternateScreen — most often when stdin is a
    // pty but stdout sizing hasn't propagated yet. Skip the frame and
    // wait for the next tick; the event loop polls at 50ms so the user
    // sees no perceptible delay.
    let full = f.area();
    if full.width == 0 || full.height == 0 {
        return;
    }
    // Paint the whole canvas with the terminal-bg before chrome draws, so
    // unfilled regions don't show through with the host terminal's default.
    f.render_widget(Paragraph::new("").style(Style::default().bg(p::BG)), full);

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // header
            Constraint::Length(2), // tab bar (labels + underline)
            Constraint::Min(1),    // content
            Constraint::Length(2), // footer (divider + text)
        ])
        .split(full);

    chrome::draw_header(f, layout[0], &app.host, app.live);
    chrome::draw_tab_bar(f, layout[1], app.active_tab, app.insights.len());
    let content = Rect {
        x: layout[2].x,
        y: layout[2].y,
        width: layout[2].width,
        height: layout[2].height,
    };
    tabs::draw(f, content, app);

    let step = step_label();
    let extra: Vec<(char, &str)> = vec![
        ('r', "Refresh"),
        ('+', app.smart_interval_label.as_str()),
        ('-', step.as_str()),
    ];
    chrome::draw_footer(f, layout[3], &extra);

    if app.show_help {
        draw_help_overlay(f, full);
    }

    if app.show_settings {
        draw_settings_overlay(f, full, app);
    }
}

fn draw_help_overlay(f: &mut ratatui::Frame, area: Rect) {
    use ratatui::style::Modifier;
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Block, Borders, Clear, Paragraph};

    // See comment in `draw()` — ratatui 0.29 panics on 0×0 rects.
    if area.width < 4 || area.height < 4 {
        return;
    }

    let popup_w = 60u16.min(area.width.saturating_sub(4));
    let popup_h = 22u16.min(area.height.saturating_sub(4));
    let x = area.x + (area.width.saturating_sub(popup_w)) / 2;
    let y = area.y + (area.height.saturating_sub(popup_h)) / 2;
    let popup = Rect {
        x,
        y,
        width: popup_w,
        height: popup_h,
    };
    if popup.width == 0 || popup.height == 0 {
        return;
    }

    f.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p::CYAN))
        .title(Span::styled(
            " DiskWatch — key bindings ",
            Style::default().fg(p::CYAN).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(p::BG));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let key = |k: &'static str| {
        Span::styled(
            format!(" {:<10}", k),
            Style::default().fg(p::CYAN).add_modifier(Modifier::BOLD),
        )
    };
    let desc = |d: &'static str| Span::styled(d, Style::default().fg(p::FG));

    let lines = vec![
        Line::from(""),
        Line::from(vec![key("Tab / ←→"), desc("previous / next tab")]),
        Line::from(vec![key("Shift+Tab"), desc("previous tab")]),
        Line::from(vec![key("1 — 9"), desc("jump directly to tab N")]),
        Line::from(""),
        Line::from(vec![key("↑ ↓ / j k"), desc("move device / fs selection")]),
        Line::from(vec![key("h l"), desc("move selection (SMART tab)")]),
        Line::from(vec![key("Home End"), desc("jump to first / last")]),
        Line::from(vec![key("PgUp PgDn"), desc("jump by 5")]),
        Line::from(""),
        Line::from(vec![key("r"), desc("force SMART refresh now")]),
        Line::from(vec![key("+ -"), desc("nudge SMART poll interval")]),
        Line::from(vec![key("0"), desc("reset interval to 5 min (default)")]),
        Line::from(""),
        Line::from(vec![key("p"), desc("pause / resume live updates")]),
        Line::from(vec![key("?"), desc("toggle this help")]),
        Line::from(vec![key(","), desc("settings — toggle columns / units / interval")]),
        Line::from(vec![key("q / Esc"), desc("quit")]),
        Line::from(""),
        Line::from(Span::styled(
            "  press any key to dismiss",
            Style::default().fg(p::DIM),
        )),
    ];
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_settings_overlay(f: &mut ratatui::Frame, area: Rect, app: &App) {
    use ratatui::style::Modifier;
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Block, Borders, Clear, Paragraph};

    // Same defensive guard as the help overlay — ratatui 0.29 panics on
    // 0×0 areas. See comment in `draw()`.
    if area.width < 4 || area.height < 4 {
        return;
    }

    let popup_w = 60u16.min(area.width.saturating_sub(4));
    let popup_h = (SETTINGS_ROWS as u16 + 6).min(area.height.saturating_sub(4));
    let x = area.x + (area.width.saturating_sub(popup_w)) / 2;
    let y = area.y + (area.height.saturating_sub(popup_h)) / 2;
    let popup = Rect {
        x,
        y,
        width: popup_w,
        height: popup_h,
    };
    if popup.width == 0 || popup.height == 0 {
        return;
    }

    f.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p::CYAN))
        .title(Span::styled(
            " DiskWatch — settings ",
            Style::default().fg(p::CYAN).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(p::BG));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    // Build rows. Each row is a tuple of (label, value-text). The
    // cursor paints a `>` marker on the highlighted row.
    let on = "[x]";
    let off = "[ ]";
    let rows: Vec<(&str, String)> = vec![
        (
            "Overview  DEVICES — SIZE column",
            (if app.visible_columns.contains(VisibleColumns::SIZE) { on } else { off }).to_string(),
        ),
        (
            "Overview  DEVICES — FREE column",
            (if app.visible_columns.contains(VisibleColumns::FREE) { on } else { off }).to_string(),
        ),
        (
            "Overview  DEVICES — USED % column",
            (if app.visible_columns.contains(VisibleColumns::USED_PCT) { on } else { off }).to_string(),
        ),
        (
            "Overview  DEVICES — TEMP column",
            (if app.visible_columns.contains(VisibleColumns::TEMP) { on } else { off }).to_string(),
        ),
        (
            "Overview  DEVICES — SMART column",
            (if app.visible_columns.contains(VisibleColumns::SMART) { on } else { off }).to_string(),
        ),
        ("Temperature unit", app.temp_unit.label().to_string()),
        (
            "SMART poll interval",
            format!("{} (r refreshes now)", app.smart_interval_label),
        ),
    ];

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        "  Move: ↑ ↓   Toggle: Space / Enter   Close: Esc / , / q",
        Style::default().fg(p::DIM),
    )));
    lines.push(Line::from(""));
    for (i, (label, value)) in rows.iter().enumerate() {
        let marker = if i == app.settings_cursor { " > " } else { "   " };
        let is_cycle = i >= SETTINGS_FIRST_CYCLE;
        let label_color = if is_cycle { p::YELLOW } else { p::FG };
        let value_color = if is_cycle {
            p::CYAN
        } else if value.contains("[x]") {
            p::GREEN
        } else {
            p::DIM
        };
        let marker_color = if i == app.settings_cursor {
            p::BR_WHITE
        } else {
            p::DIM
        };
        lines.push(Line::from(vec![
            Span::styled(marker, Style::default().fg(marker_color)),
            Span::styled(
                format!("{:<38}", label),
                Style::default().fg(label_color),
            ),
            Span::styled(value.clone(), Style::default().fg(value_color)),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  (settings persist for this session only)",
        Style::default().fg(p::DIM),
    )));

    f.render_widget(Paragraph::new(lines), inner);
}

fn read_host(device_count: usize) -> HostInfo {
    let hostname = System::host_name().unwrap_or_else(|| "localhost".to_string());
    let name = System::name().unwrap_or_else(|| "unknown".to_string());
    let version = System::os_version().unwrap_or_default();
    let arch = std::env::consts::ARCH;
    let os = if version.is_empty() {
        format!("{} {}", name, arch)
    } else {
        format!("{} {} {}", name, version, arch)
    };
    HostInfo {
        hostname,
        os,
        uptime_secs: System::uptime(),
        device_count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    #[test]
    fn devices_collector_returns_something() {
        // Smoke test on whichever platform `cargo test` runs on. We don't
        // assert specific counts — CI VMs vary — only that it doesn't
        // panic and (on a real workstation) it sees at least one disk.
        let devs = crate::collect::devices::collect();
        if !devs.is_empty() {
            let d = &devs[0];
            assert!(!d.name.is_empty(), "device name should not be empty");
            // On macOS we expect model + protocol to be filled in.
            #[cfg(target_os = "macos")]
            {
                assert!(d.size_bytes > 0, "macOS device should report size");
                assert!(
                    !d.model.is_empty() && d.model != "Unknown",
                    "macOS device should have a model"
                );
            }
        }
    }

    fn render_all_tabs(width: u16, height: u16) {
        let backend = TestBackend::new(width, height);
        let mut term = Terminal::new(backend).expect("terminal");
        let mut app = App::new(TabId::Overview);
        for tab in ALL_TABS {
            app.active_tab = *tab;
            term.draw(|f| super::draw(f, &app)).expect("draw");
        }
    }

    #[test]
    fn renders_at_design_size() {
        render_all_tabs(130, 36);
    }

    #[test]
    fn renders_at_minimum_supported_size() {
        // README declares responsive ≥ 110×30.
        render_all_tabs(110, 30);
    }

    #[test]
    fn renders_at_undersized_terminal_without_panic() {
        // We don't promise pretty output below the supported floor, only
        // that we don't panic.
        render_all_tabs(60, 20);
    }
}
