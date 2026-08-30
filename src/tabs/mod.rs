use ratatui::layout::Rect;
use ratatui::Frame;

use crate::app::App;

pub mod devices;
pub mod fs;
pub mod hot;
pub mod insights;
pub mod io;
pub mod overview;
pub mod smart;
pub mod volumes;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabId {
    Overview,
    Devices,
    Volumes,
    Fs,
    Io,
    Smart,
    Hot,
    Insights,
}

pub const ALL_TABS: &[TabId] = &[
    TabId::Overview,
    TabId::Devices,
    TabId::Volumes,
    TabId::Fs,
    TabId::Io,
    TabId::Smart,
    TabId::Hot,
    TabId::Insights,
];

/// Canonical CLI / config spellings, in tab order — the names `--tab` and
/// the config file's `tab` key take. Aliases (`filesystems`, `hotfiles`)
/// still resolve through [`TabId::from_str`]; this is the list we *offer*,
/// so an error message names one spelling per tab rather than all of them.
pub const TAB_NAMES: &[&str] = &[
    "overview", "devices", "volumes", "fs", "io", "smart", "hot", "insights",
];

impl TabId {
    pub fn label(&self) -> &'static str {
        match self {
            TabId::Overview => "Overview",
            TabId::Devices => "Devices",
            TabId::Volumes => "Volumes",
            TabId::Fs => "FS",
            TabId::Io => "IO",
            TabId::Smart => "SMART",
            TabId::Hot => "Hot Files",
            TabId::Insights => "Insights",
        }
    }

    pub fn number(&self) -> usize {
        ALL_TABS.iter().position(|t| t == self).unwrap() + 1
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "overview" => Some(TabId::Overview),
            "devices" => Some(TabId::Devices),
            "volumes" => Some(TabId::Volumes),
            "fs" | "filesystems" => Some(TabId::Fs),
            "io" => Some(TabId::Io),
            "smart" => Some(TabId::Smart),
            "hot" | "hotfiles" => Some(TabId::Hot),
            "insights" => Some(TabId::Insights),
            _ => None,
        }
    }
}

pub fn draw(f: &mut Frame, area: Rect, app: &App) {
    match app.active_tab {
        TabId::Overview => overview::draw(f, area, app),
        TabId::Devices => devices::draw(f, area, app),
        TabId::Volumes => volumes::draw(f, area, app),
        TabId::Fs => fs::draw(f, area, app),
        TabId::Io => io::draw(f, area, app),
        TabId::Smart => smart::draw(f, area, app),
        TabId::Hot => hot::draw(f, area, app),
        TabId::Insights => insights::draw(f, area, app),
    }
}
