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
    /// Which view to open in. Opt-in only — the full 8-tab TUI stays the
    /// default at every terminal size.
    pub view: ViewMode,
}

/// Which view is on screen. Lite and Dense are modes, not tabs: each has
/// its own key surface and its own layout contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    Full,
    Lite,
    /// The dense screen: six btop-style boxes, zero chrome rows. Named to
    /// match netwatch's equivalent view, so the vocabulary carries across the
    /// family along with the `V` that cycles it.
    Dense,
}

/// Config / CLI spellings, in cycle order. `V` walks this list, and the
/// settings overlay's View row shows the current entry. Same convention as
/// netwatch, so the muscle memory carries between the two tools.
pub const VIEW_MODE_NAMES: &[&str] = &["full", "lite", "dense"];

impl ViewMode {
    /// Resolve a config / CLI spelling. `None` for anything not in
    /// [`VIEW_MODE_NAMES`], so a typo is rejected at the CLI rather than
    /// silently starting the default view and looking like a no-op flag.
    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "full" => Some(ViewMode::Full),
            "lite" => Some(ViewMode::Lite),
            // `v2` and `btop` are what this view was called in v0.2.x. Kept
            // so a script or a shell history from then still resolves.
            "dense" | "v2" | "btop" | "2.0" => Some(ViewMode::Dense),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            ViewMode::Full => "full",
            ViewMode::Lite => "lite",
            ViewMode::Dense => "dense",
        }
    }

    pub fn next(self) -> Self {
        match self {
            ViewMode::Full => ViewMode::Lite,
            ViewMode::Lite => ViewMode::Dense,
            ViewMode::Dense => ViewMode::Full,
        }
    }
}

/// Advance to the next view, from whichever view is asking.
///
/// Each view owns its own transient state — Lite's detail pane, the dense
/// view's filter capture — and leaving a view has to close it, or it
/// reopens against a different selection when the cycle comes back around.
fn cycle_view(app: &mut App) {
    app.view = app.view.next();
    if app.view != ViewMode::Lite {
        app.lite.detail_open = false;
    }
    if app.view != ViewMode::Dense {
        app.dense.filter_input = false;
    }
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
    pub view: ViewMode,
    /// Lite's selection / filter / detail state. Persists across a
    /// round trip to the full TUI and back.
    pub lite: crate::ui::lite::LiteState,
    /// The dense view's selection / filter / sort state. Persists across a
    /// round trip to another view and back, same as Lite's.
    pub dense: crate::ui::dense::DenseState,
    /// Last drawn area, so the key handler can ask the Lite layout how
    /// many rows are visible without a frame in hand.
    pub last_area: Rect,
    pub live: LiveState,
    pub host: HostInfo,
    pub devices: Vec<collect::DeviceTick>,
    pub filesystems: Vec<collect::FsTick>,
    pub volumes: collect::VolumeTick,
    pub io: collect::IoCollector,
    pub smart: collect::SmartCollector,
    pub hot_files: collect::hot_files::HotFileWatcher,
    /// Per-mount capacity trend. Feeds Lite's growth + time-to-full.
    pub growth: collect::GrowthTracker,
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

#[cfg(test)]
pub fn draw_for_test(f: &mut ratatui::Frame, app: &mut App) {
    draw(f, app);
}

impl App {
    #[cfg(test)]
    pub fn new_for_test(start: TabId, view: ViewMode) -> Self {
        Self::new(start, view)
    }
    fn new(start: TabId, view: ViewMode) -> Self {
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
            view,
            lite: crate::ui::lite::LiteState::default(),
            dense: crate::ui::dense::DenseState::default(),
            last_area: Rect::new(0, 0, 0, 0),
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
            growth: collect::GrowthTracker::new(),
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
            self.growth.observe(&self.filesystems);
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
    let mut app = App::new(start, opts.view);

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
    // The settings overlay is shared by every view that offers it, so it
    // claims the keyboard before any view-specific handler — otherwise the
    // dense view's `s` would sort the file list behind an open dialog.
    if app.show_settings && !app.show_help {
        handle_settings_key(app, key);
        return;
    }

    // Lite owns its whole key surface — the full TUI's tab cycling,
    // digit jumps and SMART-interval nudges have no meaning there, and
    // letting them through would silently mutate state the user can't
    // see. The help overlay is shared, so it is handled first.
    if app.view == ViewMode::Lite && !app.show_help {
        handle_lite_key(app, key);
        return;
    }
    if app.view == ViewMode::Dense && !app.show_help {
        handle_dense_key(app, key);
        return;
    }

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

        // Shift+L drops to the minimal single-screen view. Lowercase `l`
        // is already the vim-style right/selection key.
        KeyCode::Char('L') => app.view = ViewMode::Lite,

        // `V` cycles full → lite → dense → full, netwatch's convention.
        // `L` above stays as the direct jump to Lite.
        KeyCode::Char('v') | KeyCode::Char('V') => cycle_view(app),

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
            app.smart_interval_label = format_smart_label(DEFAULT_SMART_INTERVAL_SECS);
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

/// Lite's complete key surface.
///
/// Six keys are advertised in the footer (`q p / ↵ L ?`); navigation and
/// `Esc` are deliberately unadvertised because they are conventions from
/// `less`/`vim`/`top`, and the `?` overlay lists them. The design handoff
/// specified five keys and no way to move the selection or leave the
/// filter — see the implementation plan for why that count was wrong.
fn handle_lite_key(app: &mut App, key: KeyCode) {
    use crate::ui::lite;

    // Filter input swallows printable keys, so it has to be checked
    // before anything that binds a letter.
    if app.lite.filter_input {
        match key {
            KeyCode::Esc => {
                app.lite.filter_input = false;
                app.lite.filter_text.clear();
                app.lite.selected = 0;
                app.lite.offset = 0;
            }
            // Commit the filter but keep it applied — the list stays
            // narrowed so ↵ can then open detail on a match.
            KeyCode::Enter => app.lite.filter_input = false,
            KeyCode::Backspace => {
                app.lite.filter_text.pop();
                app.lite.selected = 0;
                app.lite.offset = 0;
            }
            KeyCode::Char(c) => {
                app.lite.filter_text.push(c);
                app.lite.selected = 0;
                app.lite.offset = 0;
            }
            _ => {}
        }
        return;
    }

    let count = lite::filter_rows(lite::collect_rows(app), &app.lite.filter_text).len();
    let visible = lite::Layout::new(app.last_area).visible_files(app.lite.detail_open);

    match key {
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Char('L') => {
            app.view = ViewMode::Full;
            app.lite.detail_open = false;
            app.lite.filter_input = false;
        }
        KeyCode::Char('v') | KeyCode::Char('V') => {
            app.lite.filter_input = false;
            cycle_view(app);
        }
        KeyCode::Char('?') => app.show_help = true,
        KeyCode::Char('p') => {
            app.live = match app.live {
                LiveState::Live => LiveState::Paused,
                LiveState::Paused => LiveState::Live,
            };
        }
        KeyCode::Char('/') => {
            app.lite.filter_input = true;
            app.lite.filter_text.clear();
        }
        KeyCode::Enter => app.lite.detail_open = !app.lite.detail_open,
        KeyCode::Esc => {
            // Esc unwinds one layer at a time: detail, then the filter.
            // Only when neither is open does it mean "quit", matching
            // the full TUI.
            if app.lite.detail_open {
                app.lite.detail_open = false;
            } else if !app.lite.filter_text.is_empty() {
                app.lite.filter_text.clear();
                app.lite.selected = 0;
                app.lite.offset = 0;
            } else {
                app.should_quit = true;
            }
        }
        KeyCode::Down | KeyCode::Char('j') if count > 0 => {
            app.lite.selected = (app.lite.selected + 1).min(count - 1);
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.lite.selected = app.lite.selected.saturating_sub(1);
        }
        KeyCode::Home => app.lite.selected = 0,
        KeyCode::End => app.lite.selected = count.saturating_sub(1),
        _ => {}
    }

    clamp_lite_scroll(app, count, visible);
}

/// Keys for the dense view.
///
/// It owns its whole key surface for the same reason Lite does: the
/// 8-tab view's tab cycling and digit jumps have no meaning against six
/// boxes, and letting them through would mutate state the user can't
/// see. Everything it does bind is advertised in a box border.
fn handle_dense_key(app: &mut App, key: KeyCode) {
    use crate::ui::dense;

    // Filter input swallows printable keys, so it is checked before
    // anything that binds a letter.
    if app.dense.filter_input {
        match key {
            KeyCode::Esc => {
                app.dense.filter_input = false;
                app.dense.filter_text.clear();
                app.dense.selected = 0;
                app.dense.offset = 0;
            }
            KeyCode::Enter => app.dense.filter_input = false,
            KeyCode::Backspace => {
                app.dense.filter_text.pop();
                app.dense.selected = 0;
                app.dense.offset = 0;
            }
            KeyCode::Char(c) => {
                app.dense.filter_text.push(c);
                app.dense.selected = 0;
                app.dense.offset = 0;
            }
            _ => {}
        }
        return;
    }

    let count = dense::sorted_rows(app).len();
    let visible = dense::visible_files(app.last_area);

    match key {
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Char('v') | KeyCode::Char('V') => {
            app.dense.filter_input = false;
            cycle_view(app);
        }
        KeyCode::Char('L') => {
            app.view = ViewMode::Lite;
            app.dense.filter_input = false;
        }
        // The dense view carries the settings overlay, unlike Lite: it is a
        // whole-screen replacement for the 8-tab view rather than a
        // deliberately six-key one, so the dials have to be reachable
        // without leaving it.
        KeyCode::Char(',') => {
            app.show_settings = true;
            app.settings_cursor = 0;
        }
        KeyCode::Char('?') => app.show_help = true,
        KeyCode::Char('p') => {
            app.live = match app.live {
                LiveState::Live => LiveState::Paused,
                LiveState::Paused => LiveState::Live,
            };
        }
        KeyCode::Char('/') => {
            app.dense.filter_input = true;
            app.dense.filter_text.clear();
        }
        KeyCode::Char('s') => app.dense.sort = app.dense.sort.next(),
        KeyCode::Esc => {
            if !app.dense.filter_text.is_empty() {
                app.dense.filter_text.clear();
                app.dense.selected = 0;
                app.dense.offset = 0;
            } else {
                app.should_quit = true;
            }
        }
        KeyCode::Down | KeyCode::Char('j') if count > 0 => {
            app.dense.selected = (app.dense.selected + 1).min(count - 1);
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.dense.selected = app.dense.selected.saturating_sub(1);
        }
        KeyCode::Home => app.dense.selected = 0,
        KeyCode::End => app.dense.selected = count.saturating_sub(1),
        _ => {}
    }

    clamp_dense_scroll(app, count, visible);
}

/// Keep the selection inside the file box's visible window.
fn clamp_dense_scroll(app: &mut App, count: usize, visible: u16) {
    let st = &mut app.dense;
    if count == 0 || visible == 0 {
        st.selected = 0;
        st.offset = 0;
        return;
    }
    st.selected = st.selected.min(count - 1);
    let visible = visible as usize;
    if st.selected < st.offset {
        st.offset = st.selected;
    } else if st.selected >= st.offset + visible {
        st.offset = st.selected + 1 - visible;
    }
    st.offset = st.offset.min(count.saturating_sub(visible.min(count)));
}

/// Keep the selection inside the visible window, and — when detail is
/// open — far enough from the bottom that its three rows fit before the
/// prompt row.
fn clamp_lite_scroll(app: &mut App, count: usize, visible: u16) {
    let lite = &mut app.lite;
    if count == 0 {
        lite.selected = 0;
        lite.offset = 0;
        return;
    }
    lite.selected = lite.selected.min(count - 1);
    let visible = visible.max(1) as usize;

    if lite.selected < lite.offset {
        lite.offset = lite.selected;
    }
    // The detail block consumes rows *below* the selected row, so when
    // it's open the selection must sit at least DETAIL_ROWS above the
    // window bottom. `visible` already excludes those rows, so the same
    // arithmetic covers both cases.
    if lite.selected >= lite.offset + visible {
        lite.offset = lite.selected + 1 - visible;
    }
    let max_offset = count.saturating_sub(visible);
    lite.offset = lite.offset.min(max_offset);
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
const SETTINGS_ROWS: usize = 11;
// Below this index, rows are toggles (column visibility bitflags);
// at or above, rows are cycle setters (temp unit, SMART interval, theme).
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
                7 => {
                    // Theme lives in a global rather than on App, so the
                    // palette accessors can read it without threading a
                    // reference through every render fn. Nothing to store
                    // here — the next draw picks it up.
                    crate::ui::theme::cycle();
                }
                // The same cycle `V` walks, reachable for anyone who found
                // the views through the menu rather than the keybinding.
                // The overlay stays open across the switch, so the next
                // press keeps cycling and you can see each view behind it.
                8 => cycle_view(app),
                // Graph style and fade live in the same kind of global as
                // the theme, for the same reason.
                9 => {
                    crate::ui::graph::cycle();
                }
                10 => {
                    crate::ui::graph::toggle_fade();
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

fn draw(f: &mut ratatui::Frame, app: &mut App) {
    // Stash the area so the Lite key handler can resolve its layout
    // between frames. Done before the 0×0 guard so a resize to nothing
    // and back doesn't leave a stale size behind.
    app.last_area = f.area();
    draw_inner(f, app);
}

fn draw_inner(f: &mut ratatui::Frame, app: &App) {
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
    f.render_widget(Paragraph::new("").style(Style::default().bg(p::bg())), full);

    // Lite is a whole-screen view: no header, no tab bar, no footer
    // chrome. It draws its own 24 rows and nothing else.
    if app.view == ViewMode::Lite {
        crate::ui::lite::render(f, app, full);
        if app.show_help {
            draw_lite_help_overlay(f, full);
        }
        return;
    }

    // The dense view goes further: its boxes tile every row, and the
    // identity, hotkeys and paging that would need chrome live inside
    // the borders instead.
    if app.view == ViewMode::Dense {
        crate::ui::dense::render(f, full, app);
        if app.show_settings {
            draw_settings_overlay(f, full, app);
        }
        if app.show_help {
            draw_dense_help_overlay(f, full);
        }
        return;
    }

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
    // 21 content lines plus the two border rows. The list already filled
    // the box exactly before `L` was added, so this has to grow with it
    // or the last line silently drops off.
    let popup_h = 23u16.min(area.height.saturating_sub(4));
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
        .border_style(Style::default().fg(p::cyan()))
        .title(Span::styled(
            " DiskWatch — key bindings ",
            Style::default().fg(p::cyan()).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(p::bg()));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let key = |k: &'static str| {
        Span::styled(
            format!(" {:<10}", k),
            Style::default().fg(p::cyan()).add_modifier(Modifier::BOLD),
        )
    };
    let desc = |d: &'static str| Span::styled(d, Style::default().fg(p::fg()));

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
        Line::from(vec![key("L"), desc("switch to Lite — one 80×24 screen")]),
        Line::from(vec![key("V"), desc("cycle view: full → lite → dense")]),
        Line::from(vec![key("?"), desc("toggle this help")]),
        Line::from(vec![
            key(","),
            desc("settings — toggle columns / units / interval"),
        ]),
        Line::from(vec![key("q / Esc"), desc("quit")]),
        Line::from(""),
        Line::from(Span::styled(
            "  press any key to dismiss",
            Style::default().fg(p::dim()),
        )),
    ];
    f.render_widget(Paragraph::new(lines), inner);
}

/// Lite's `?` overlay. Separate from the full TUI's because Lite binds a
/// different set — and because this is where the unadvertised keys are
/// documented, which is the whole justification for keeping the footer to
/// six. Returns to the same screen; never a seventh view.
fn draw_lite_help_overlay(f: &mut ratatui::Frame, area: Rect) {
    use ratatui::style::Modifier;
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Block, Borders, Clear, Paragraph};

    if area.width < 4 || area.height < 4 {
        return;
    }

    let popup_w = 54u16.min(area.width.saturating_sub(4));
    // 13 content lines plus two border rows, capped so it still fits
    // inside Lite's own 24-row floor.
    let popup_h = 15u16.min(area.height.saturating_sub(4));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(popup_w)) / 2,
        y: area.y + (area.height.saturating_sub(popup_h)) / 2,
        width: popup_w,
        height: popup_h,
    };
    if popup.width == 0 || popup.height == 0 {
        return;
    }

    f.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p::cyan()))
        .title(Span::styled(
            " DiskWatch Lite — key bindings ",
            Style::default().fg(p::cyan()).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(p::bg()));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let key = |k: &'static str| {
        Span::styled(
            format!(" {:<10}", k),
            Style::default().fg(p::cyan()).add_modifier(Modifier::BOLD),
        )
    };
    let desc = |d: &'static str| Span::styled(d, Style::default().fg(p::fg()));

    let lines = vec![
        Line::from(vec![key("↑ ↓ / j k"), desc("move selection")]),
        Line::from(vec![key("↵"), desc("expand / collapse detail")]),
        Line::from(vec![key("/"), desc("filter by file or path")]),
        Line::from(vec![key("Esc"), desc("close detail, then filter")]),
        Line::from(vec![key("Home End"), desc("first / last row")]),
        Line::from(""),
        Line::from(vec![key("p"), desc("pause / resume")]),
        Line::from(vec![key("V"), desc("cycle view: full → lite → dense")]),
        Line::from(vec![key("L"), desc("back to the full 8-tab view")]),
        Line::from(vec![key("q"), desc("quit")]),
        Line::from(""),
        Line::from(Span::styled(
            // Lite has no settings overlay — six keys is the point. Say
            // where the dials are rather than leaving them undiscoverable.
            "  theme + graph style: V or L, then ,  (or --graph dots)",
            Style::default().fg(p::dim()),
        )),
        Line::from(Span::styled(
            "  read-only: diskwatch never deletes or trims",
            Style::default().fg(p::dim()),
        )),
        Line::from(Span::styled(
            "  press any key to dismiss",
            Style::default().fg(p::dim()),
        )),
    ];
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_dense_help_overlay(f: &mut ratatui::Frame, area: Rect) {
    use ratatui::style::Modifier;
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Block, Borders, Clear, Paragraph};

    if area.width < 4 || area.height < 4 {
        return;
    }
    let popup_w = 62u16.min(area.width.saturating_sub(4));
    let popup_h = 16u16.min(area.height.saturating_sub(4));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(popup_w)) / 2,
        y: area.y + (area.height.saturating_sub(popup_h)) / 2,
        width: popup_w,
        height: popup_h,
    };
    if popup.width == 0 || popup.height == 0 {
        return;
    }
    f.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p::cyan()))
        .title(Span::styled(
            " DiskWatch Dense — key bindings ",
            Style::default().fg(p::cyan()).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(p::bg()));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let key = |k: &'static str| {
        Span::styled(
            format!(" {:<10}", k),
            Style::default().fg(p::cyan()).add_modifier(Modifier::BOLD),
        )
    };
    let desc = |d: &'static str| Span::styled(d, Style::default().fg(p::fg()));
    let note = |d: &'static str| Line::from(Span::styled(d, Style::default().fg(p::dim())));

    let lines = vec![
        Line::from(vec![key("↑ ↓ / j k"), desc("move selection in files")]),
        Line::from(vec![key("/"), desc("filter by file or path")]),
        Line::from(vec![key("s"), desc("cycle sort: events / total / name")]),
        Line::from(vec![key("Home End"), desc("first / last row")]),
        Line::from(""),
        Line::from(vec![key("p"), desc("pause / resume")]),
        Line::from(vec![key("V"), desc("cycle view: full → lite → dense")]),
        Line::from(vec![key("L"), desc("jump straight to Lite")]),
        Line::from(vec![key(","), desc("settings (theme, view, graphs)")]),
        Line::from(vec![key("q"), desc("quit")]),
        Line::from(""),
        note("  util needs /proc/diskstats — macOS shows --"),
        note("  latency buckets sample per-tick means, not per-op"),
        note("  press any key to dismiss"),
    ];
    f.render_widget(Paragraph::new(lines), inner);
}

/// Popup width, and the columns the row renderer gives to the label.
/// The value gets whatever is left, which is why several values below are
/// written tersely — see `settings_values_fit_their_column`.
const SETTINGS_POPUP_W: u16 = 60;
const SETTINGS_LABEL_W: u16 = 38;
/// Columns the cursor marker occupies (`" > "` / `"   "`).
const SETTINGS_MARKER_W: u16 = 3;

/// Columns a settings row's *value* may occupy before the renderer
/// truncates it mid-word. Two borders eat 2 of the popup width.
const fn settings_value_w() -> u16 {
    SETTINGS_POPUP_W - 2 - SETTINGS_MARKER_W - SETTINGS_LABEL_W
}

/// The settings rows, as (label, value) pairs. Extracted from the
/// renderer so the value-width constraint can be asserted in a test
/// instead of restated as a comment on every row that has to respect it.
fn settings_rows(app: &App) -> Vec<(&'static str, String)> {
    let on = "[x]";
    let off = "[ ]";
    vec![
        (
            "Overview  DEVICES — SIZE column",
            (if app.visible_columns.contains(VisibleColumns::SIZE) {
                on
            } else {
                off
            })
            .to_string(),
        ),
        (
            "Overview  DEVICES — FREE column",
            (if app.visible_columns.contains(VisibleColumns::FREE) {
                on
            } else {
                off
            })
            .to_string(),
        ),
        (
            "Overview  DEVICES — USED % column",
            (if app.visible_columns.contains(VisibleColumns::USED_PCT) {
                on
            } else {
                off
            })
            .to_string(),
        ),
        (
            "Overview  DEVICES — TEMP column",
            (if app.visible_columns.contains(VisibleColumns::TEMP) {
                on
            } else {
                off
            })
            .to_string(),
        ),
        (
            "Overview  DEVICES — SMART column",
            (if app.visible_columns.contains(VisibleColumns::SMART) {
                on
            } else {
                off
            })
            .to_string(),
        ),
        ("Temperature unit", app.temp_unit.label().to_string()),
        (
            "SMART poll interval",
            // "(r refreshes now)" overran the 17-column value slot and
            // rendered as "(r refreshes n". Same constraint as the Theme
            // and Graph fade rows below.
            format!("{} (r refreshes)", app.smart_interval_label),
        ),
        (
            // Value stays a bare name: the popup is 60 cols and the row
            // renderer truncates, so an explanatory parenthetical here
            // gets cut off mid-word. What `terminal` means is documented
            // in the README and `--help`.
            "Theme",
            crate::ui::theme::name().to_string(),
        ),
        // Value is the bare name, matching `VIEW_MODE_NAMES` and the
        // `--lite` / `--dense` flags, so what the menu says is what the CLI
        // takes. `V` walks the same list.
        ("View", app.view.name().to_string()),
        ("Graph style", crate::ui::graph::name().to_string()),
        (
            "Graph fade (btop gradient)",
            if crate::ui::theme::name() == "terminal" {
                // Say why rather than showing a toggle that does nothing:
                // fade interpolates in RGB, which is exactly what this
                // theme exists to avoid. Kept short — the value column is
                // 17 cols and the row renderer truncates mid-word.
                "n/a (terminal)".to_string()
            } else if crate::ui::graph::fade_enabled() {
                "on".to_string()
            } else {
                "off".to_string()
            },
        ),
    ]
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

    let popup_w = SETTINGS_POPUP_W.min(area.width.saturating_sub(4));
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
        .border_style(Style::default().fg(p::cyan()))
        .title(Span::styled(
            " DiskWatch — settings ",
            Style::default().fg(p::cyan()).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(p::bg()));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let rows = settings_rows(app);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        "  Move: ↑ ↓   Toggle: Space / Enter   Close: Esc / , / q",
        Style::default().fg(p::dim()),
    )));
    lines.push(Line::from(""));
    for (i, (label, value)) in rows.iter().enumerate() {
        // Both forms are padded to SETTINGS_MARKER_W so the value-column
        // arithmetic in `settings_value_w` stays true of what's drawn.
        let marker = format!(
            "{:^width$}",
            if i == app.settings_cursor { ">" } else { "" },
            width = SETTINGS_MARKER_W as usize
        );
        let is_cycle = i >= SETTINGS_FIRST_CYCLE;
        let label_color = if is_cycle { p::yellow() } else { p::fg() };
        let value_color = if is_cycle {
            p::cyan()
        } else if value.contains("[x]") {
            p::green()
        } else {
            p::dim()
        };
        let marker_color = if i == app.settings_cursor {
            p::br_white()
        } else {
            p::dim()
        };
        lines.push(Line::from(vec![
            Span::styled(marker, Style::default().fg(marker_color)),
            Span::styled(
                format!("{:<width$}", label, width = SETTINGS_LABEL_W as usize),
                Style::default().fg(label_color),
            ),
            // Belt and braces: `settings_values_fit_their_column` keeps
            // values inside the budget, but if one ever escapes it,
            // degrade to a visible ellipsis rather than a silent shear
            // that reads as a typo.
            Span::styled(
                crate::ui::lite::truncate_end(value, settings_value_w()),
                Style::default().fg(value_color),
            ),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  (settings persist for this session only)",
        Style::default().fg(p::dim()),
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
    fn settings_rows_match_their_action_dispatch() {
        // The overlay builds its row list in one place and dispatches on
        // the cursor index in another. If they drift, the last rows
        // become unreachable — silently, because the cursor still moves
        // over them. This pins the two together.
        let app = App::new(TabId::Overview, ViewMode::Full);
        let backend = TestBackend::new(130, 36);
        let mut term = Terminal::new(backend).expect("terminal");
        term.draw(|f| super::draw_settings_overlay(f, Rect::new(0, 0, 130, 36), &app))
            .expect("draw");

        // Every index the cursor can reach must do something. `handle_
        // settings_key` is the only place that knows, so exercise it:
        // Space on the last row has to change observable state.
        let mut app = App::new(TabId::Overview, ViewMode::Full);
        app.settings_cursor = SETTINGS_ROWS - 1;
        let before = crate::ui::graph::fade_enabled();
        handle_settings_key(&mut app, KeyCode::Char(' '));
        assert_ne!(
            crate::ui::graph::fade_enabled(),
            before,
            "the last settings row is unreachable — SETTINGS_ROWS and the \
             dispatch arms have drifted apart"
        );
        // Leave the global as we found it.
        crate::ui::graph::set_fade(before);
    }

    #[test]
    fn settings_values_fit_their_column() {
        // The row renderer pads the label to a fixed width and lets the
        // value take the rest, with no wrapping — an over-long value is
        // silently sheared mid-word. This shipped that way on the SMART
        // row ("5m (r refreshes n") and nothing caught it, so pin it.
        let mut app = App::new(TabId::Overview, ViewMode::Full);
        let budget = settings_value_w() as usize;

        // Exercise the states that produce the longest values: the
        // widest temperature unit, every theme (the `terminal` theme
        // swaps the fade value for an explanation), and both fade states.
        for theme in crate::ui::theme::THEME_NAMES {
            crate::ui::theme::set_by_name(theme);
            for unit in [TempUnit::Celsius, TempUnit::Fahrenheit] {
                app.temp_unit = unit;
                for fade in [true, false] {
                    crate::ui::graph::set_fade(fade);
                    for (label, value) in settings_rows(&app) {
                        assert!(
                            value.chars().count() <= budget,
                            "settings value {value:?} for {label:?} is {} cols, \
                             but the column is {budget} — it will render truncated",
                            value.chars().count()
                        );
                    }
                }
            }
        }
        crate::ui::theme::set_by_name("dark");
        crate::ui::graph::set_fade(false);
    }

    #[test]
    fn settings_row_count_matches_the_rendered_rows() {
        // SETTINGS_ROWS drives cursor bounds and popup height; the row
        // list drives what's drawn. If they disagree the cursor either
        // walks off the visible list or can't reach the last row.
        let app = App::new(TabId::Overview, ViewMode::Full);
        assert_eq!(settings_rows(&app).len(), SETTINGS_ROWS);
    }

    #[test]
    fn graph_style_survives_a_round_trip_through_the_settings_overlay() {
        let mut app = App::new(TabId::Overview, ViewMode::Full);
        let start = crate::ui::graph::name();
        app.settings_cursor = SETTINGS_ROWS - 2; // Graph style
        handle_settings_key(&mut app, KeyCode::Char(' '));
        assert_ne!(crate::ui::graph::name(), start);
        handle_settings_key(&mut app, KeyCode::Char(' '));
        assert_eq!(crate::ui::graph::name(), start);
    }

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
        let mut app = App::new(TabId::Overview, ViewMode::Full);
        for tab in ALL_TABS {
            app.active_tab = *tab;
            term.draw(|f| super::draw(f, &mut app)).expect("draw");
        }
    }

    /// Draw Lite in every state it can be in, at one size.
    fn render_lite(width: u16, height: u16) {
        let backend = TestBackend::new(width, height);
        let mut term = Terminal::new(backend).expect("terminal");
        let mut app = App::new(TabId::Overview, ViewMode::Lite);
        // Give the growth tracker and the file list something to chew
        // on so the capacity row and table exercise their real paths.
        app.growth.observe(&app.filesystems);

        for (paused, detail, filter, help) in [
            (false, false, "", false),
            (true, false, "", false),
            (false, true, "", false),
            (false, false, "log", false),
            (false, true, "log", false),
            (false, false, "", true),
        ] {
            app.live = if paused {
                LiveState::Paused
            } else {
                LiveState::Live
            };
            app.lite.detail_open = detail;
            app.lite.filter_text = filter.to_string();
            app.show_help = help;
            term.draw(|f| super::draw(f, &mut app)).expect("draw");
        }
    }

    /// Draw the dense view in every state it can be in, at one size.
    fn render_dense(width: u16, height: u16) {
        let backend = TestBackend::new(width, height);
        let mut term = Terminal::new(backend).expect("terminal");
        let mut app = App::new(TabId::Overview, ViewMode::Dense);
        app.growth.observe(&app.filesystems);
        app.io.sample();

        for (filter, filtering, sort, help) in [
            ("", false, crate::ui::dense::FileSort::Rate, false),
            ("", false, crate::ui::dense::FileSort::Total, false),
            ("", false, crate::ui::dense::FileSort::Name, false),
            ("log", false, crate::ui::dense::FileSort::Rate, false),
            ("lo", true, crate::ui::dense::FileSort::Rate, false),
            ("", false, crate::ui::dense::FileSort::Rate, true),
        ] {
            app.dense.filter_text = filter.to_string();
            app.dense.filter_input = filtering;
            app.dense.sort = sort;
            app.show_help = help;
            term.draw(|f| super::draw(f, &mut app)).expect("draw");
        }
    }

    #[test]
    fn dense_renders_at_the_reference_grid() {
        // The design grid: 130×44.
        render_dense(130, 44);
    }

    #[test]
    fn dense_renders_across_the_sizes_a_terminal_actually_takes() {
        // Both sides of every layout threshold, including one column and
        // one row below each — where a saturating_sub would underflow into
        // a huge width and a box would try to draw off-screen.
        for (w, h) in [
            (crate::ui::dense::MIN_FULL_W, crate::ui::dense::MIN_FULL_H),
            (
                crate::ui::dense::MIN_FULL_W - 1,
                crate::ui::dense::MIN_FULL_H,
            ),
            (
                crate::ui::dense::MIN_FULL_W,
                crate::ui::dense::MIN_FULL_H - 1,
            ),
            (crate::ui::dense::MIN_W, crate::ui::dense::MIN_H),
            (crate::ui::dense::MIN_W - 1, crate::ui::dense::MIN_H - 1),
            (200, 60),
            (400, 100),
            (80, 24),
            (100, 30),
            (60, 16),
            (20, 6),
        ] {
            render_dense(w, h);
        }
    }

    #[test]
    fn dense_key_surface_does_not_leak_into_the_full_tui() {
        // Same contract as Lite: a stray `3` must not switch a tab the
        // view doesn't show, and `s` must sort rather than do nothing.
        let mut app = App::new(TabId::Overview, ViewMode::Dense);
        super::handle_key(&mut app, KeyCode::Char('3'));
        assert_eq!(app.active_tab, TabId::Overview);
        let before = app.dense.sort;
        super::handle_key(&mut app, KeyCode::Char('s'));
        assert_ne!(app.dense.sort, before);
    }

    #[test]
    fn v_cycles_every_view_and_returns_to_where_it_started() {
        // netwatch's convention: one key walks full → lite → dense → full,
        // from whichever view is on screen. `L` stays as the direct jump to
        // Lite, and the cycle has to come home or a view becomes a trap.
        for start in [ViewMode::Full, ViewMode::Lite, ViewMode::Dense] {
            let mut app = App::new(TabId::Overview, start);
            let mut seen = vec![app.view];
            for _ in 0..VIEW_MODE_NAMES.len() {
                super::handle_key(&mut app, KeyCode::Char('V'));
                seen.push(app.view);
            }
            assert_eq!(app.view, start, "V did not return to {start:?}");
            assert_eq!(
                seen.len() - 1,
                VIEW_MODE_NAMES.len(),
                "the cycle skipped a view"
            );
            for m in [ViewMode::Full, ViewMode::Lite, ViewMode::Dense] {
                assert!(seen.contains(&m), "{m:?} is unreachable from {start:?}");
            }
        }
        // Lowercase does the same thing, as it does in netwatch.
        let mut app = App::new(TabId::Overview, ViewMode::Full);
        super::handle_key(&mut app, KeyCode::Char('v'));
        assert_eq!(app.view, ViewMode::Lite);
    }

    #[test]
    fn the_settings_menu_reaches_the_views_and_names_them_as_the_cli_does() {
        // Found through the menu rather than the keybinding: the View row
        // has to cycle the same list, and print the spelling `--lite` and
        // `--dense` accept.
        let mut app = App::new(TabId::Overview, ViewMode::Dense);
        super::handle_key(&mut app, KeyCode::Char(','));
        assert!(app.show_settings, "`,` does not open settings from dense");
        let view_row = settings_rows(&app)
            .into_iter()
            .position(|(label, _)| label == "View")
            .expect("a View row");
        app.settings_cursor = view_row;
        let before = app.view;
        super::handle_key(&mut app, KeyCode::Enter);
        assert_ne!(app.view, before, "the View row did not cycle");
        assert!(app.show_settings, "the overlay closed on a cycle");
        assert!(
            VIEW_MODE_NAMES.contains(&app.view.name()),
            "{} is not a spelling the CLI accepts",
            app.view.name()
        );
        // And the dialog still owns the keyboard while it is open: `s`
        // must not sort the file list behind it.
        app.view = ViewMode::Dense;
        let sort = app.dense.sort;
        super::handle_key(&mut app, KeyCode::Char('s'));
        assert_eq!(app.dense.sort, sort, "a key leaked past the open dialog");
    }

    #[test]
    fn every_key_the_2_0_view_advertises_actually_does_something() {
        // The view has no footer chrome: its bindings are printed in the box
        // borders, and a border that advertises a key the handler ignores is
        // the worst kind of documentation. This pins each one to an
        // observable effect. It caught `↹ device`, carried over from the
        // design handoff, which nothing had ever implemented.
        let fresh = || App::new(TabId::Overview, ViewMode::Dense);

        let mut app = fresh();
        super::handle_key(&mut app, KeyCode::Char('q'));
        assert!(app.should_quit, "q");

        let mut app = fresh();
        super::handle_key(&mut app, KeyCode::Char('/'));
        assert!(app.dense.filter_input, "/");

        let mut app = fresh();
        let sort = app.dense.sort;
        super::handle_key(&mut app, KeyCode::Char('s'));
        assert_ne!(app.dense.sort, sort, "s");

        let mut app = fresh();
        super::handle_key(&mut app, KeyCode::Char('V'));
        assert_ne!(app.view, ViewMode::Dense, "V");

        let mut app = fresh();
        super::handle_key(&mut app, KeyCode::Char(','));
        assert!(app.show_settings, ",");

        let mut app = fresh();
        super::handle_key(&mut app, KeyCode::Char('?'));
        assert!(app.show_help, "?");

        // ↑↓ only move when there is something to move through, which is
        // the one case a fresh App can't guarantee — so assert the clamp
        // instead: they must never leave the selection out of bounds.
        let mut app = fresh();
        super::handle_key(&mut app, KeyCode::Down);
        super::handle_key(&mut app, KeyCode::End);
        let count = crate::ui::dense::sorted_rows(&app).len();
        assert!(app.dense.selected < count.max(1), "↑↓ left the list");
    }

    #[test]
    fn dense_filter_captures_printable_keys() {
        // `s` sorts — unless the filter is capturing, in which case it is
        // just a letter. Getting this order wrong makes the filter
        // unusable for anything containing an s.
        let mut app = App::new(TabId::Overview, ViewMode::Dense);
        super::handle_key(&mut app, KeyCode::Char('/'));
        let sort = app.dense.sort;
        for c in "syslog".chars() {
            super::handle_key(&mut app, KeyCode::Char(c));
        }
        assert_eq!(app.dense.filter_text, "syslog");
        assert_eq!(app.dense.sort, sort, "sort changed while typing a filter");
        super::handle_key(&mut app, KeyCode::Esc);
        assert!(app.dense.filter_text.is_empty());
    }

    #[test]
    fn dense_esc_unwinds_the_filter_before_it_quits() {
        let mut app = App::new(TabId::Overview, ViewMode::Dense);
        app.dense.filter_text = "log".into();
        super::handle_key(&mut app, KeyCode::Esc);
        assert!(app.dense.filter_text.is_empty());
        assert!(!app.should_quit);
        super::handle_key(&mut app, KeyCode::Esc);
        assert!(app.should_quit);
    }

    #[test]
    fn lite_renders_at_the_reference_grid() {
        render_lite(crate::ui::lite::GRID_W, crate::ui::lite::GRID_H);
    }

    #[test]
    fn lite_renders_on_a_wide_terminal() {
        // The adaptive layout has to hold at sizes the handoff never
        // considered — it specified 80×24 and nothing else.
        render_lite(200, 60);
    }

    #[test]
    fn lite_renders_below_its_floor_without_panicking() {
        // Under 80×24 Lite shows a notice instead of a clipped grid.
        render_lite(40, 12);
        render_lite(79, 23);
    }

    #[test]
    fn lite_key_surface_does_not_leak_into_the_full_tui() {
        // Lite binds `L` to leave, `Esc` to unwind, and swallows the
        // full TUI's tab keys — a stray `3` must not switch tabs
        // underneath a view that has none.
        let mut app = App::new(TabId::Overview, ViewMode::Lite);
        super::handle_key(&mut app, KeyCode::Char('3'));
        assert_eq!(app.active_tab, TabId::Overview);
        assert_eq!(app.view, ViewMode::Lite);

        super::handle_key(&mut app, KeyCode::Char('L'));
        assert_eq!(app.view, ViewMode::Full);
        super::handle_key(&mut app, KeyCode::Char('L'));
        assert_eq!(app.view, ViewMode::Lite);
    }

    #[test]
    fn lite_filter_captures_printable_keys() {
        // `/` then `p` must type a `p`, not toggle pause — the filter
        // has to be checked before any letter binding.
        let mut app = App::new(TabId::Overview, ViewMode::Lite);
        super::handle_key(&mut app, KeyCode::Char('/'));
        super::handle_key(&mut app, KeyCode::Char('p'));
        super::handle_key(&mut app, KeyCode::Char('q'));
        assert_eq!(app.lite.filter_text, "pq");
        assert_eq!(app.live, LiveState::Live, "p must not have paused");
        assert!(!app.should_quit, "q must not have quit");
    }

    #[test]
    fn lite_esc_unwinds_one_layer_at_a_time() {
        let mut app = App::new(TabId::Overview, ViewMode::Lite);
        app.lite.filter_text = "log".into();
        app.lite.detail_open = true;

        super::handle_key(&mut app, KeyCode::Esc);
        assert!(!app.lite.detail_open, "first Esc closes detail");
        assert_eq!(app.lite.filter_text, "log", "and leaves the filter");

        super::handle_key(&mut app, KeyCode::Esc);
        assert!(app.lite.filter_text.is_empty(), "second Esc clears filter");
        assert!(!app.should_quit, "and still does not quit");

        super::handle_key(&mut app, KeyCode::Esc);
        assert!(app.should_quit, "third Esc, with nothing open, quits");
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
