//! Config file — the general settings diskwatch reads once at startup.
//!
//! ## Where it lives
//!
//! `$DISKWATCH_CONFIG` if set, else `$XDG_CONFIG_HOME/diskwatch/config.toml`,
//! else `~/.config/diskwatch/config.toml`. macOS reads the same path as
//! Linux rather than `~/Library/Application Support`: diskwatch is a
//! terminal tool that sits beside the rest of a dotfiles repo, and every
//! tool it sits beside reads `~/.config`.
//!
//! A missing file is not an error — it is the overwhelmingly common case,
//! and diskwatch's whole pitch is that it runs with zero config.
//!
//! ## Precedence
//!
//! CLI flag > environment variable > config file > built-in default.
//! A flag the user typed always wins over a file they wrote months ago.
//!
//! ## Format
//!
//! A deliberate subset of TOML: flat `key = value` pairs, `#` comments,
//! double-quoted strings, booleans, integers, and single-line arrays of
//! strings.
//!
//! It is a strict subset — every file this parser accepts is also valid
//! TOML — so swapping in the `toml` crate later cannot invalidate a config
//! anyone has already written. We hand-roll it because diskwatch pins
//! `rust-version = "1.75"` and has already taken a bug report for an MSRV
//! it could not hold; the `toml` stack raises its own MSRV on a cadence we
//! don't control, and would put that pin at the mercy of `cargo update`.
//!
//! ## Errors
//!
//! Nothing here is fatal. A key we don't know, a value we can't parse, a
//! file we can't read: each becomes a warning, the rest of the file still
//! applies, and diskwatch still starts. A diagnostics tool that refuses to
//! launch because of a typo in its own config has failed at its one job.
//! Warnings go to stderr before the alternate screen opens, and `--diag`
//! reprints them where they can be read at leisure.

use std::path::{Path, PathBuf};

use crate::app::{TempUnit, ViewMode, VisibleColumns, VIEW_MODE_NAMES};
use crate::tabs::{TabId, TAB_NAMES};
use crate::ui;

/// Every key the parser understands, in the order the generated default
/// file lists them. Used for the "unknown key" warning, so the message can
/// tell the user what they could have written instead.
pub const KNOWN_KEYS: &[&str] = &[
    "theme",
    "graph",
    "graph_fade",
    "view",
    "tab",
    "smart_interval_secs",
    "refresh_interval_ms",
    "temp_unit",
    "columns",
    "watch_paths",
    "extra_watch_paths",
    "ignore_fs_types",
    "ignore_mounts",
];

/// Column spellings accepted by the `columns` key, in table order.
const COLUMN_NAMES: &[(&str, u8)] = &[
    ("size", VisibleColumns::SIZE),
    ("free", VisibleColumns::FREE),
    ("used_pct", VisibleColumns::USED_PCT),
    ("temp", VisibleColumns::TEMP),
    ("smart", VisibleColumns::SMART),
];

/// Settings read from the config file. Every field is `Option` (or, for
/// the additive path list, empty-by-default) so that "the file didn't say"
/// stays distinguishable from "the file said the default" — the CLI layer
/// needs that distinction to apply precedence.
#[derive(Debug, Default, Clone)]
pub struct Config {
    pub theme: Option<String>,
    pub graph: Option<String>,
    pub graph_fade: Option<bool>,
    pub view: Option<ViewMode>,
    pub tab: Option<TabId>,
    pub smart_interval_secs: Option<u64>,
    pub refresh_interval_ms: Option<u64>,
    pub temp_unit: Option<TempUnit>,
    pub columns: Option<VisibleColumns>,
    /// Replaces [`crate::collect::hot_files::default_roots`] outright.
    pub watch_paths: Option<Vec<PathBuf>>,
    /// Appended to whatever the roots end up being — the defaults, or a
    /// `watch_paths` list, or a `--watch` flag.
    pub extra_watch_paths: Vec<PathBuf>,
    /// Filesystem types to leave out of capacity alerts, on top of the
    /// built-in read-only image detection. Matched case-insensitively.
    pub ignore_fs_types: Vec<String>,
    /// Mount points to leave out of capacity alerts. Exact match.
    pub ignore_mounts: Vec<String>,
    /// The file these settings came from, or `None` if no file was found.
    /// The settings overlay shows it so the answer to "where do I change
    /// this?" is on screen rather than in the README.
    pub source: Option<PathBuf>,
    /// Problems found while loading, in file order. Never fatal.
    pub warnings: Vec<String>,
}

impl Config {
    /// Read the config file from the first location that exists. Returns
    /// defaults (with `source: None`) when there is no file to read.
    pub fn load() -> Self {
        match config_path() {
            Some(path) => Self::load_from(&path),
            None => Config::default(),
        }
    }

    /// Read a specific file. Public for tests and for `DISKWATCH_CONFIG`.
    pub fn load_from(path: &Path) -> Self {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Only worth reporting when the user pointed us at it
                // explicitly. A missing default path is the normal case.
                let mut cfg = Config::default();
                if std::env::var_os("DISKWATCH_CONFIG").is_some() {
                    cfg.warnings
                        .push(format!("config file not found: {}", path.display()));
                }
                return cfg;
            }
            Err(e) => {
                let mut cfg = Config::default();
                cfg.warnings
                    .push(format!("cannot read {}: {}", path.display(), e));
                return cfg;
            }
        };
        let mut cfg = Self::parse(&text);
        cfg.source = Some(path.to_path_buf());
        cfg
    }

    /// Parse config text. Split from [`Self::load_from`] so the parser is
    /// testable without touching the filesystem.
    pub fn parse(text: &str) -> Self {
        let mut cfg = Config::default();
        for (lineno, raw) in text.lines().enumerate() {
            let line = strip_comment(raw).trim();
            if line.is_empty() {
                continue;
            }
            let n = lineno + 1;
            if line.starts_with('[') {
                cfg.warn(n, "tables are not supported; write keys at the top level");
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                cfg.warn(n, "expected `key = value`");
                continue;
            };
            let key = key.trim();
            let value = value.trim();
            if value.is_empty() {
                cfg.warn(n, &format!("`{key}` has no value"));
                continue;
            }
            cfg.apply(n, key, value);
        }
        cfg
    }

    fn warn(&mut self, line: usize, msg: &str) {
        self.warnings.push(format!("line {line}: {msg}"));
    }

    fn apply(&mut self, line: usize, key: &str, value: &str) {
        match key {
            "theme" => match parse_string(value).map(|s| validate_theme(&s)) {
                Some(Ok(v)) => self.theme = Some(v),
                Some(Err(e)) => self.warn(line, &e),
                None => self.warn(line, &format!("`{key}` expects a quoted string")),
            },
            "graph" => match parse_string(value).map(|s| validate_graph(&s)) {
                Some(Ok(v)) => self.graph = Some(v),
                Some(Err(e)) => self.warn(line, &e),
                None => self.warn(line, &format!("`{key}` expects a quoted string")),
            },
            "view" => match parse_string(value).map(|s| validate_view(&s)) {
                Some(Ok(v)) => self.view = ViewMode::from_name(&v),
                Some(Err(e)) => self.warn(line, &e),
                None => self.warn(line, &format!("`{key}` expects a quoted string")),
            },
            "tab" => match parse_string(value).map(|s| validate_tab(&s)) {
                Some(Ok(v)) => self.tab = TabId::from_str(&v),
                Some(Err(e)) => self.warn(line, &e),
                None => self.warn(line, &format!("`{key}` expects a quoted string")),
            },
            "graph_fade" => match parse_bool(value) {
                Some(b) => self.graph_fade = Some(b),
                None => self.warn(line, &format!("`{key}` expects true or false")),
            },
            "smart_interval_secs" => match parse_int(value) {
                Some(n) if n >= crate::app::MIN_SMART_INTERVAL_SECS => {
                    self.smart_interval_secs = Some(n)
                }
                Some(n) => self.warn(
                    line,
                    &format!(
                        "`{key}` = {n} is below the {}s floor; polling faster than that \
                         issues more than one smartctl per second per disk",
                        crate::app::MIN_SMART_INTERVAL_SECS
                    ),
                ),
                None => self.warn(line, &format!("`{key}` expects a positive integer")),
            },
            "refresh_interval_ms" => match parse_int(value) {
                Some(n) if n >= crate::app::MIN_REFRESH_INTERVAL_MS => {
                    self.refresh_interval_ms = Some(n)
                }
                Some(n) => self.warn(
                    line,
                    &format!(
                        "`{key}` = {n} is below the {}ms floor; refreshing faster than \
                         that re-polls sysinfo for no benefit",
                        crate::app::MIN_REFRESH_INTERVAL_MS
                    ),
                ),
                None => self.warn(line, &format!("`{key}` expects a positive integer")),
            },
            "temp_unit" => match parse_string(value).as_deref().map(str::to_ascii_lowercase) {
                Some(v) => match v.as_str() {
                    "c" | "celsius" => self.temp_unit = Some(TempUnit::Celsius),
                    "f" | "fahrenheit" => self.temp_unit = Some(TempUnit::Fahrenheit),
                    other => self.warn(
                        line,
                        &format!("unknown temp_unit {other:?} (expected celsius or fahrenheit)"),
                    ),
                },
                None => self.warn(line, &format!("`{key}` expects a quoted string")),
            },
            "columns" => match parse_array(value) {
                Some(items) => {
                    let mut bits = 0u8;
                    for item in items {
                        let want = item.to_ascii_lowercase();
                        match COLUMN_NAMES.iter().find(|(n, _)| *n == want) {
                            Some((_, bit)) => bits |= bit,
                            None => self.warn(
                                line,
                                &format!(
                                    "unknown column {item:?} (available: {})",
                                    COLUMN_NAMES
                                        .iter()
                                        .map(|(n, _)| *n)
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                ),
                            ),
                        }
                    }
                    self.columns = Some(VisibleColumns(bits));
                }
                None => self.warn(line, &format!("`{key}` expects an array of strings")),
            },
            "watch_paths" => match parse_array(value) {
                Some(items) => self.watch_paths = Some(items.iter().map(expand_path).collect()),
                None => self.warn(line, &format!("`{key}` expects an array of strings")),
            },
            "extra_watch_paths" => match parse_array(value) {
                Some(items) => self.extra_watch_paths = items.iter().map(expand_path).collect(),
                None => self.warn(line, &format!("`{key}` expects an array of strings")),
            },
            "ignore_fs_types" => match parse_array(value) {
                Some(items) => self.ignore_fs_types = items,
                None => self.warn(line, &format!("`{key}` expects an array of strings")),
            },
            "ignore_mounts" => match parse_array(value) {
                Some(items) => self.ignore_mounts = items,
                None => self.warn(line, &format!("`{key}` expects an array of strings")),
            },
            other => self.warn(
                line,
                &format!(
                    "unknown key {other:?} (known keys: {})",
                    KNOWN_KEYS.join(", ")
                ),
            ),
        }
    }
}

/// The file diskwatch will read, whether or not it exists. `None` only when
/// there is no `HOME` and no `XDG_CONFIG_HOME` to hang a path off.
pub fn config_path() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("DISKWATCH_CONFIG") {
        return Some(PathBuf::from(explicit));
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("diskwatch").join("config.toml"))
}

/// A commented file showing every key at its built-in default. Written by
/// `--write-config`, which exists so the answer to "what can I set?" is a
/// file on disk rather than a README section that drifts from the code.
pub fn default_file_contents() -> String {
    format!(
        "# diskwatch config — every key is optional, and every value below is\n\
         # the built-in default. A CLI flag overrides anything written here.\n\
         #\n\
         # Format: flat `key = value`, `#` comments, no tables.\n\
         \n\
         # Color theme: {}.\n\
         # \"terminal\" resolves every color through the palette your terminal\n\
         # already defines, so a system-wide theme carries straight over.\n\
         theme = \"{}\"\n\
         \n\
         # Chart style: {}.\n\
         graph = \"{}\"\n\
         \n\
         # btop's gradient. Needs a theme with real RGB to fade through, so it\n\
         # does nothing under theme = \"terminal\".\n\
         graph_fade = false\n\
         \n\
         # View to open in: {}.\n\
         view = \"full\"\n\
         \n\
         # Tab to open on: {}.\n\
         tab = \"overview\"\n\
         \n\
         # SMART poll cadence, in seconds. Floor is {}s — below that we issue\n\
         # more than one smartctl subprocess per second per disk.\n\
         smart_interval_secs = {}\n\
         \n\
         # How often on-screen usage figures (device %, filesystem/volume\n\
         # tables, capacity trends) refresh, in milliseconds. Floor is {}ms.\n\
         # Also reachable from the settings overlay (,) as \"Refresh speed\".\n\
         # Doesn't affect the io throughput graphs, which sample at their\n\
         # own fixed rate regardless.\n\
         refresh_interval_ms = {}\n\
         \n\
         # Temperature display unit: celsius or fahrenheit. Drive firmware\n\
         # always reports Celsius; this governs the display layer only.\n\
         temp_unit = \"celsius\"\n\
         \n\
         # Columns shown in the Overview tab's DEVICES table, in table order:\n\
         # {}.\n\
         columns = [{}]\n\
         \n\
         # Paths the Hot Files tab watches, replacing the defaults ($HOME plus\n\
         # the OS log and tmp directories). A leading ~ expands to $HOME.\n\
         # Each entry is watched recursively, so on Linux a large tree can\n\
         # exhaust fs.inotify.max_user_watches — diskwatch says so if it does.\n\
         # watch_paths = [\"~/src\", \"/var/log\"]\n\
         \n\
         # Paths added to whatever the roots already are, rather than\n\
         # replacing them. Use this to keep the defaults and add one more.\n\
         # extra_watch_paths = [\"/srv/data\"]\n\
         \n\
         # Read-only images (squashfs, AppImage mounts, ISOs) are always full,\n\
         # so they are shown dimmed and never raise capacity alerts. Add more\n\
         # filesystem types or mount points to leave out of alerts the same way.\n\
         # ignore_fs_types = [\"nfs\"]\n\
         # ignore_mounts = [\"/mnt/backup\"]\n",
        ui::theme::THEME_NAMES.join(", "),
        ui::theme::DEFAULT_THEME,
        ui::graph::GRAPH_STYLE_NAMES.join(", "),
        "bars",
        VIEW_MODE_NAMES.join(", "),
        TAB_NAMES.join(", "),
        crate::app::MIN_SMART_INTERVAL_SECS,
        crate::app::DEFAULT_SMART_INTERVAL_SECS,
        crate::app::MIN_REFRESH_INTERVAL_MS,
        crate::app::DEFAULT_REFRESH_INTERVAL_MS,
        COLUMN_NAMES
            .iter()
            .map(|(n, _)| *n)
            .collect::<Vec<_>>()
            .join(", "),
        COLUMN_NAMES
            .iter()
            .map(|(n, _)| format!("\"{n}\""))
            .collect::<Vec<_>>()
            .join(", "),
    )
}

// ---------------------------------------------------------------------
// Saving
// ---------------------------------------------------------------------

/// The settings the `,` overlay can change, as they stand.
#[derive(Debug, Clone)]
pub struct OverlaySettings<'a> {
    pub theme: &'a str,
    pub graph: &'a str,
    pub graph_fade: bool,
    pub view: ViewMode,
    pub smart_interval_secs: u64,
    pub refresh_interval_ms: u64,
    pub temp_unit: TempUnit,
    pub columns: VisibleColumns,
}

impl OverlaySettings<'_> {
    /// As config-file values ready to write: `("theme", "\"nord\"")`,
    /// `("graph_fade", "true")`.
    pub fn values(&self) -> Vec<(&'static str, String)> {
        let temp = match self.temp_unit {
            TempUnit::Celsius => "celsius",
            TempUnit::Fahrenheit => "fahrenheit",
        };
        let cols = COLUMN_NAMES
            .iter()
            .filter(|(_, bit)| self.columns.contains(*bit))
            .map(|(n, _)| format!("\"{n}\""))
            .collect::<Vec<_>>()
            .join(", ");
        vec![
            ("theme", format!("\"{}\"", self.theme)),
            ("graph", format!("\"{}\"", self.graph)),
            ("graph_fade", self.graph_fade.to_string()),
            ("view", format!("\"{}\"", self.view.name())),
            ("smart_interval_secs", self.smart_interval_secs.to_string()),
            ("refresh_interval_ms", self.refresh_interval_ms.to_string()),
            ("temp_unit", format!("\"{temp}\"")),
            ("columns", format!("[{cols}]")),
        ]
    }
}

/// Set `values` in config text, keeping everything else: comments, key
/// order, the keys the overlay doesn't touch (watch paths, ignores, tab).
///
/// A key already in the file has its value replaced in place, keeping any
/// trailing comment; every occurrence is replaced, since the parser lets
/// the last one win. A key that isn't there is appended at the end.
/// Commented-out examples (`# watch_paths = …`) are left alone.
pub fn set_values(text: &str, values: &[(&str, String)]) -> String {
    let mut seen = vec![false; values.len()];
    let mut out = String::with_capacity(text.len() + 64);
    for line in text.lines() {
        let code = strip_comment(line);
        let key = code.split_once('=').map(|(k, _)| k.trim());
        match key.and_then(|k| values.iter().position(|(name, _)| *name == k)) {
            Some(i) => {
                seen[i] = true;
                let (name, value) = &values[i];
                let indent = &line[..line.len() - line.trim_start().len()];
                let comment = line[code.len()..].trim();
                out.push_str(&format!("{indent}{name} = {value}"));
                if !comment.is_empty() {
                    out.push_str(&format!(" {comment}"));
                }
            }
            None => out.push_str(line),
        }
        out.push('\n');
    }
    let missing: Vec<usize> = (0..values.len()).filter(|i| !seen[*i]).collect();
    if !missing.is_empty() {
        if !out.is_empty() && !out.ends_with("\n\n") {
            out.push('\n');
        }
        out.push_str("# Saved from the settings overlay (,).\n");
        for i in missing {
            out.push_str(&format!("{} = {}\n", values[i].0, values[i].1));
        }
    }
    out
}

/// Write `values` into the config file at `path`: the existing file with
/// those keys updated, or, when there's none yet, the commented default
/// file with them filled in. Written to a temporary file and renamed into
/// place, so a failed write never leaves a half-written config behind.
pub fn save_values(path: &Path, values: &[(&str, String)]) -> std::io::Result<()> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => default_file_contents(),
        Err(e) => return Err(e),
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, set_values(&text, values))?;
    std::fs::rename(&tmp, path)
}

// ---------------------------------------------------------------------
// Value parsing
// ---------------------------------------------------------------------

/// Cut a line at its first `#` that isn't inside a double-quoted string.
/// Naive splitting on `#` would mangle `watch_paths = ["/tmp/#scratch"]`.
fn strip_comment(line: &str) -> &str {
    let mut in_string = false;
    let mut escaped = false;
    for (i, c) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            '#' if !in_string => return &line[..i],
            _ => {}
        }
    }
    line
}

/// Unquote a double-quoted string. `None` for anything unquoted, so a bare
/// `theme = dark` is reported rather than silently accepted — the file has
/// to stay valid TOML for the parser swap this subset is designed to allow.
fn parse_string(raw: &str) -> Option<String> {
    let inner = raw.strip_prefix('"')?.strip_suffix('"')?;
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(other) => out.push(other),
                None => return None,
            }
        } else if c == '"' {
            // An unescaped quote mid-value means the quoting is wrong.
            return None;
        } else {
            out.push(c);
        }
    }
    Some(out)
}

fn parse_bool(raw: &str) -> Option<bool> {
    match raw {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

fn parse_int(raw: &str) -> Option<u64> {
    raw.parse::<u64>().ok()
}

/// Single-line `["a", "b"]`. Returns `None` if the brackets are missing or
/// any element isn't a quoted string; an empty array is `Some(vec![])`,
/// which is how `columns = []` hides every column.
fn parse_array(raw: &str) -> Option<Vec<String>> {
    let inner = raw.strip_prefix('[')?.strip_suffix(']')?.trim();
    if inner.is_empty() {
        return Some(Vec::new());
    }
    let mut out = Vec::new();
    for part in split_top_level(inner) {
        out.push(parse_string(part.trim())?);
    }
    Some(out)
}

/// Split on commas that aren't inside a string, so `["a,b", "c"]` stays two
/// elements. A trailing comma yields an empty final part, which
/// `parse_string` then rejects — TOML allows it, we don't, and saying so is
/// better than silently dropping it.
fn split_top_level(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            ',' if !in_string => {
                parts.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&s[start..]);
    parts
}

/// Expand a leading `~` or `$HOME` against the environment. Nothing else is
/// expanded: diskwatch reads this file, not a shell, and a config that
/// quietly ran command substitution would be a surprise nobody asked for.
pub fn expand_path(raw: impl AsRef<str>) -> PathBuf {
    let raw = raw.as_ref();
    let Some(home) = std::env::var_os("HOME") else {
        return PathBuf::from(raw);
    };
    let home = PathBuf::from(home);
    if raw == "~" || raw == "$HOME" {
        return home;
    }
    for prefix in ["~/", "$HOME/"] {
        if let Some(rest) = raw.strip_prefix(prefix) {
            return home.join(rest);
        }
    }
    PathBuf::from(raw)
}

// ---------------------------------------------------------------------
// Shared validators
// ---------------------------------------------------------------------
//
// `by_name` / `from_name` fall back to the default for anything they don't
// recognise, which is right at runtime but wrong at every configuration
// surface: a typo would silently select the default and look like the flag
// (or the key) did nothing. These reject it and list what's available.
//
// They live here rather than in main.rs so the CLI and the config file
// agree on what a valid value is by construction — clap calls them as
// `value_parser`s, and `Config::apply` calls them on the way in.

pub fn validate_theme(raw: &str) -> Result<String, String> {
    if ui::theme::by_name(raw).name == "dark" && !raw.eq_ignore_ascii_case("dark") {
        return Err(format!(
            "unknown theme {raw:?} (available: {})",
            ui::theme::THEME_NAMES.join(", ")
        ));
    }
    Ok(raw.to_string())
}

pub fn validate_graph(raw: &str) -> Result<String, String> {
    if ui::graph::by_name(raw) == ui::graph::GraphStyle::Bars && !raw.eq_ignore_ascii_case("bars") {
        return Err(format!(
            "unknown graph style {raw:?} (available: {})",
            ui::graph::GRAPH_STYLE_NAMES.join(", ")
        ));
    }
    Ok(raw.to_string())
}

pub fn validate_view(raw: &str) -> Result<String, String> {
    ViewMode::from_name(raw)
        .map(|_| raw.to_string())
        .ok_or_else(|| {
            format!(
                "unknown view {raw:?} (available: {})",
                VIEW_MODE_NAMES.join(", ")
            )
        })
}

/// `--tab` shipped without a validator, so `--tab hto` silently opened
/// Overview. Same reasoning as the others: reject it and say what's valid.
pub fn validate_tab(raw: &str) -> Result<String, String> {
    TabId::from_str(raw)
        .map(|_| raw.to_string())
        .ok_or_else(|| format!("unknown tab {raw:?} (available: {})", TAB_NAMES.join(", ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_values() -> Vec<(&'static str, String)> {
        let mut cols = VisibleColumns(VisibleColumns::ALL);
        cols.toggle(VisibleColumns::TEMP);
        OverlaySettings {
            theme: "nord",
            graph: "dots",
            graph_fade: true,
            view: ViewMode::Dense,
            smart_interval_secs: 60,
            refresh_interval_ms: 2000,
            temp_unit: TempUnit::Fahrenheit,
            columns: cols,
        }
        .values()
    }

    #[test]
    fn saved_values_parse_back_to_what_was_saved() {
        let text = set_values(&default_file_contents(), &sample_values());
        let cfg = Config::parse(&text);
        assert!(cfg.warnings.is_empty(), "{:?}", cfg.warnings);
        assert_eq!(cfg.theme.as_deref(), Some("nord"));
        assert_eq!(cfg.graph.as_deref(), Some("dots"));
        assert_eq!(cfg.graph_fade, Some(true));
        assert_eq!(cfg.view, Some(ViewMode::Dense));
        assert_eq!(cfg.smart_interval_secs, Some(60));
        assert_eq!(cfg.refresh_interval_ms, Some(2000));
        assert_eq!(cfg.temp_unit, Some(TempUnit::Fahrenheit));
        let cols = cfg.columns.expect("columns");
        assert!(!cols.contains(VisibleColumns::TEMP));
        assert!(cols.contains(VisibleColumns::SMART) && cols.contains(VisibleColumns::SIZE));
        // Every key once: replaced in place, nothing appended.
        assert_eq!(text.matches("\ntheme = ").count(), 1, "{text}");
        assert!(!text.contains("Saved from the settings overlay"), "{text}");
    }

    #[test]
    fn saving_keeps_comments_and_untouched_keys() {
        let before = "\
# my diskwatch
theme = \"dark\"   # was light
watch_paths = [\"~/src\"]
# graph = \"bars\"
ignore_mounts = [\"/mnt/backup\"]
";
        let after = set_values(before, &sample_values());
        assert!(
            after.starts_with("# my diskwatch\ntheme = \"nord\" # was light\n"),
            "{after}"
        );
        assert!(after.contains("watch_paths = [\"~/src\"]\n"), "{after}");
        assert!(
            after.contains("ignore_mounts = [\"/mnt/backup\"]\n"),
            "{after}"
        );
        assert!(
            after.contains("# graph = \"bars\"\n"),
            "a commented example stays: {after}"
        );
        assert!(
            after.contains("# Saved from the settings overlay (,).\ngraph = \"dots\"\n"),
            "{after}"
        );
        let cfg = Config::parse(&after);
        assert!(cfg.warnings.is_empty(), "{:?}", cfg.warnings);
        assert_eq!(cfg.watch_paths.map(|p| p.len()), Some(1));
        assert_eq!(cfg.graph.as_deref(), Some("dots"));
        // Saving again changes nothing.
        assert_eq!(set_values(&after, &sample_values()), after);
    }

    #[test]
    fn a_key_set_twice_is_updated_both_times() {
        let after = set_values("view = \"full\"\nview = \"lite\"\n", &sample_values());
        assert_eq!(after.matches("view = \"dense\"").count(), 2, "{after}");
        assert_eq!(Config::parse(&after).view, Some(ViewMode::Dense));
    }

    #[test]
    fn hiding_every_column_saves_an_empty_list() {
        let v = OverlaySettings {
            theme: "terminal",
            graph: "bars",
            graph_fade: false,
            view: ViewMode::Full,
            smart_interval_secs: 300,
            refresh_interval_ms: 1000,
            temp_unit: TempUnit::Celsius,
            columns: VisibleColumns(0),
        }
        .values();
        let text = set_values("", &v);
        assert!(text.contains("columns = []"), "{text}");
        assert_eq!(Config::parse(&text).columns, Some(VisibleColumns(0)));
    }

    #[test]
    fn saving_creates_the_file_from_the_commented_default() {
        let dir = std::env::temp_dir().join(format!("diskwatch-save-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("diskwatch").join("config.toml");
        save_values(&path, &sample_values()).expect("save");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# diskwatch config"), "{text}");
        assert!(text.contains("theme = \"nord\""), "{text}");
        assert!(!path.with_extension("toml.tmp").exists());
        let cfg = Config::load_from(&path);
        assert!(cfg.warnings.is_empty(), "{:?}", cfg.warnings);
        assert_eq!(cfg.temp_unit, Some(TempUnit::Fahrenheit));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_full_file_parses_into_every_setting() {
        let cfg = Config::parse(
            r#"
            # a comment
            theme = "nord"
            graph = "dots"
            graph_fade = true
            view = "dense"
            tab = "hot"
            smart_interval_secs = 30
            refresh_interval_ms = 2000
            temp_unit = "fahrenheit"
            columns = ["size", "temp"]
            watch_paths = ["/var/log", "/srv"]
            extra_watch_paths = ["/opt"]
            ignore_fs_types = ["nfs"]
            ignore_mounts = ["/mnt/backup"]
            "#,
        );
        assert!(cfg.warnings.is_empty(), "{:?}", cfg.warnings);
        assert_eq!(cfg.theme.as_deref(), Some("nord"));
        assert_eq!(cfg.graph.as_deref(), Some("dots"));
        assert_eq!(cfg.graph_fade, Some(true));
        assert_eq!(cfg.view, Some(ViewMode::Dense));
        assert_eq!(cfg.tab, Some(TabId::Hot));
        assert_eq!(cfg.smart_interval_secs, Some(30));
        assert_eq!(cfg.refresh_interval_ms, Some(2000));
        assert_eq!(cfg.temp_unit, Some(TempUnit::Fahrenheit));
        assert_eq!(
            cfg.columns,
            Some(VisibleColumns(VisibleColumns::SIZE | VisibleColumns::TEMP))
        );
        assert_eq!(
            cfg.watch_paths,
            Some(vec![PathBuf::from("/var/log"), PathBuf::from("/srv")])
        );
        assert_eq!(cfg.extra_watch_paths, vec![PathBuf::from("/opt")]);
        assert_eq!(cfg.ignore_fs_types, vec!["nfs"]);
        assert_eq!(cfg.ignore_mounts, vec!["/mnt/backup"]);
    }

    /// A bad line must not take the file down with it. The alternative —
    /// refusing to start — turns a typo in a preference into an outage of
    /// the tool you reached for to diagnose an outage.
    #[test]
    fn a_bad_line_is_reported_and_the_rest_of_the_file_still_applies() {
        let cfg = Config::parse(
            r#"
            theme = "not-a-theme"
            graph = "dots"
            nonsense = 1
            smart_interval_secs = 1
            "#,
        );
        assert_eq!(cfg.graph.as_deref(), Some("dots"), "good lines still apply");
        assert_eq!(cfg.theme, None, "the bad value is not smuggled through");
        assert_eq!(cfg.warnings.len(), 3, "{:?}", cfg.warnings);
        // Every warning names its line, or it is useless on a long file.
        for w in &cfg.warnings {
            assert!(w.starts_with("line "), "unlocated warning: {w}");
        }
        // And the sub-floor interval says what the floor is, rather than
        // being silently clamped to it — a clamp looks like the setting
        // was ignored.
        assert!(
            cfg.warnings.iter().any(|w| w.contains("floor")),
            "{:?}",
            cfg.warnings
        );
        assert_eq!(cfg.smart_interval_secs, None);
    }

    /// The unknown-key warning is the one a user is most likely to hit
    /// (a guessed name, a stale key from a blog post), so it has to say
    /// what they could have written instead.
    #[test]
    fn an_unknown_key_lists_the_known_ones() {
        let cfg = Config::parse("watchpaths = [\"/a\"]\n");
        assert_eq!(cfg.warnings.len(), 1);
        assert!(
            cfg.warnings[0].contains("watch_paths"),
            "the near-miss should be in the list: {}",
            cfg.warnings[0]
        );
    }

    #[test]
    fn comments_and_strings_do_not_eat_each_other() {
        assert_eq!(strip_comment("a = 1 # trailing"), "a = 1 ");
        assert_eq!(
            strip_comment(r#"watch_paths = ["/tmp/#scratch"] # real"#),
            r#"watch_paths = ["/tmp/#scratch"] "#
        );
        let cfg = Config::parse(r#"watch_paths = ["/tmp/#scratch"] # real"#);
        assert!(cfg.warnings.is_empty(), "{:?}", cfg.warnings);
        assert_eq!(cfg.watch_paths, Some(vec![PathBuf::from("/tmp/#scratch")]));
    }

    #[test]
    fn arrays_split_on_commas_outside_strings_only() {
        assert_eq!(
            parse_array(r#"["a,b", "c"]"#),
            Some(vec!["a,b".to_string(), "c".to_string()])
        );
        assert_eq!(parse_array("[]"), Some(Vec::new()));
        assert_eq!(parse_array("[a]"), None, "unquoted elements are rejected");
        assert_eq!(parse_array(r#"["a",]"#), None, "trailing comma is rejected");
    }

    /// An empty `columns` is a real request — hide every column — and has
    /// to stay distinguishable from an absent key, which means the
    /// defaults.
    #[test]
    fn an_empty_column_list_hides_every_column() {
        let cfg = Config::parse("columns = []\n");
        assert!(cfg.warnings.is_empty(), "{:?}", cfg.warnings);
        assert_eq!(cfg.columns, Some(VisibleColumns(0)));
        assert_eq!(Config::parse("").columns, None);
    }

    #[test]
    fn tables_are_rejected_with_an_explanation_rather_than_ignored() {
        let cfg = Config::parse("[general]\ntheme = \"nord\"\n");
        assert_eq!(cfg.warnings.len(), 1);
        assert!(cfg.warnings[0].contains("top level"), "{:?}", cfg.warnings);
        // The keys under it still apply — dropping them silently would be
        // the worse half of the two behaviours.
        assert_eq!(cfg.theme.as_deref(), Some("nord"));
    }

    #[test]
    fn a_leading_tilde_expands_but_nothing_else_does() {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        if let Some(home) = home {
            assert_eq!(expand_path("~/src"), home.join("src"));
            assert_eq!(expand_path("$HOME/src"), home.join("src"));
            assert_eq!(expand_path("~"), home);
        }
        assert_eq!(expand_path("/a/~/b"), PathBuf::from("/a/~/b"));
        assert_eq!(expand_path("$(whoami)"), PathBuf::from("$(whoami)"));
    }

    /// The subset is only useful as a migration path if it stays a subset.
    /// Bare (unquoted) values are the easy way to drift out of it, so they
    /// are rejected rather than accepted-for-convenience.
    #[test]
    fn unquoted_strings_are_rejected_so_the_file_stays_valid_toml() {
        let cfg = Config::parse("theme = dark\n");
        assert_eq!(cfg.theme, None);
        assert_eq!(cfg.warnings.len(), 1);
        assert!(cfg.warnings[0].contains("quoted"), "{:?}", cfg.warnings);
    }
}
