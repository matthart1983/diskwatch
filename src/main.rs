use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::Parser;

mod app;
mod collect;
mod config;
mod insights;
mod tabs;
mod ui;

use app::{Options, TempUnit, ViewMode, VisibleColumns};
use config::Config;
use tabs::TabId;

/// Colon-separated replacement list for the Hot Files roots, for the case
/// where editing a config file is the wrong shape — a container image, a
/// systemd unit, a one-off `DISKWATCH_WATCH_PATHS=/srv diskwatch`.
const WATCH_PATHS_ENV: &str = "DISKWATCH_WATCH_PATHS";

#[derive(Parser, Debug)]
#[command(
    name = "diskwatch",
    version,
    about = "Single-host disk diagnostics TUI",
    after_help = "Settings can also be written to a config file — see --write-config.\n\
                  Precedence: CLI flag > environment variable > config file > default."
)]
struct Cli {
    /// Start on a specific tab (overview, devices, volumes, fs, io, smart, hot, insights).
    #[arg(long, value_parser = config::validate_tab)]
    tab: Option<String>,

    /// Color theme. Defaults to "terminal": every color resolves through
    /// the palette your terminal already defines, so a system-wide theme
    /// (a terminal profile, pywal, matugen, a rice) carries straight over
    /// and diskwatch sits beside your other tools instead of fighting them.
    ///
    /// Pass "dark" for diskwatch's own designed palette, or any of the
    /// other built-ins: light, ocean, solarized, dracula, nord.
    #[arg(long, value_parser = config::validate_theme)]
    theme: Option<String>,

    /// Chart style: "bars" (default) or "dots" for btop-style braille,
    /// which resolves four levels per row instead of one. Also accepts
    /// "braille" and "btop".
    #[arg(long, value_parser = config::validate_graph)]
    graph: Option<String>,

    /// btop's gradient: charts fade from bright at `now` to dim at the
    /// left edge, over a faint dot grid. Off by default.
    ///
    /// Needs a theme with real RGB to fade through, so it does nothing
    /// under the default `--theme terminal` — a 16-color palette has no
    /// intermediate shades. Pair it with `--theme dark` (or any other
    /// built-in) to see it.
    #[arg(long)]
    graph_fade: bool,

    /// Turn the gradient off. Only useful against a config file that turns
    /// it on: a bare flag can't express "no", so without this there would
    /// be a setting the file could set and the CLI could not unset.
    #[arg(long, conflicts_with = "graph_fade")]
    no_graph_fade: bool,

    /// Start in Lite: one 80×24 screen, six keys — read and write
    /// throughput, capacity with a time-to-full projection, and the
    /// busiest files. Toggle either way at runtime with `L`.
    #[arg(long)]
    lite: bool,

    /// Start in the dense view: six btop-style boxes tiling the screen with
    /// zero chrome rows — a mirrored read/write graph, per-device table,
    /// latency histogram, capacity, SMART and hot files, all at once.
    /// Cycle views at runtime with `V`.
    ///
    /// `--v2` and `--btop` are kept as hidden aliases: they are what this
    /// view was called in v0.2.x, and a flag that worked yesterday should not
    /// fail today just because it was renamed.
    #[arg(long, alias = "v2", alias = "btop")]
    dense: bool,

    /// Start in a named view: "full" (default), "lite" or "dense". The same
    /// spellings the settings overlay's View row shows, and the same list
    /// `V` cycles through at runtime.
    #[arg(long, value_parser = config::validate_view)]
    view: Option<String>,

    /// Watch this path in the Hot Files tab *instead of* the defaults
    /// ($HOME plus the OS log and tmp directories). Repeatable. A leading
    /// `~` expands to $HOME, for the case where your shell didn't.
    ///
    /// Each path is watched recursively, which on Linux costs one inotify
    /// watch per directory underneath it.
    #[arg(long = "watch", value_name = "PATH")]
    watch: Vec<String>,

    /// Watch this path *in addition to* whatever the roots already are.
    /// Repeatable. Use this to keep the defaults and add one more.
    #[arg(long = "watch-add", value_name = "PATH")]
    watch_add: Vec<String>,

    /// Read this config file instead of the default location. Equivalent
    /// to setting DISKWATCH_CONFIG.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Ignore any config file and run on built-in defaults. For proving
    /// that a config file is what's causing something, without moving it.
    #[arg(long, conflicts_with = "config")]
    no_config: bool,

    /// Write a commented config file listing every setting at its default,
    /// then exit. Won't overwrite an existing file without --force.
    #[arg(long)]
    write_config: bool,

    /// Allow --write-config to overwrite an existing config file.
    #[arg(long, requires = "write_config")]
    force: bool,

    /// Print collected state and exit without launching the TUI.
    /// Useful for diagnosing what each collector is seeing.
    #[arg(long)]
    diag: bool,
}

/// Every setting after CLI, environment and config file have been folded
/// together. Built by [`resolve`], which is pure so the precedence rules
/// can be tested without a filesystem or a terminal.
#[derive(Debug, PartialEq)]
struct Resolved {
    theme: String,
    graph: String,
    graph_fade: bool,
    view: ViewMode,
    tab: TabId,
    smart_interval_secs: u64,
    temp_unit: TempUnit,
    visible_columns: VisibleColumns,
    /// Roots replacing the defaults, or `None` to keep them.
    watch_replace: Option<Vec<PathBuf>>,
    watch_extra: Vec<PathBuf>,
}

/// Fold the three sources into one set of settings.
///
/// The rule everywhere is the same: a flag the user just typed beats an
/// environment variable, which beats a file they wrote months ago, which
/// beats the built-in default. `env_watch` is passed in rather than read
/// here so the precedence is testable without mutating the process
/// environment — which no test can do safely once another test is running
/// in the same process.
fn resolve(cli: &Cli, cfg: &Config, env_watch: Option<Vec<PathBuf>>) -> Resolved {
    // Precedence among the view flags themselves is unchanged: an explicit
    // --view wins, then --dense (the more specific request) over --lite.
    // Only if none was passed does the config file get a say.
    let view = cli
        .view
        .as_deref()
        .and_then(ViewMode::from_name)
        .or(if cli.dense {
            Some(ViewMode::Dense)
        } else if cli.lite {
            Some(ViewMode::Lite)
        } else {
            None
        })
        .or(cfg.view)
        .unwrap_or(ViewMode::Full);

    let watch_replace = if !cli.watch.is_empty() {
        Some(cli.watch.iter().map(config::expand_path).collect())
    } else if let Some(env) = env_watch {
        Some(env)
    } else {
        cfg.watch_paths.clone()
    };

    let watch_extra = if cli.watch_add.is_empty() {
        cfg.extra_watch_paths.clone()
    } else {
        cli.watch_add.iter().map(config::expand_path).collect()
    };

    Resolved {
        theme: cli
            .theme
            .clone()
            .or_else(|| cfg.theme.clone())
            .unwrap_or_else(|| ui::theme::DEFAULT_THEME.to_string()),
        graph: cli
            .graph
            .clone()
            .or_else(|| cfg.graph.clone())
            .unwrap_or_else(|| "bars".to_string()),
        graph_fade: if cli.graph_fade {
            true
        } else if cli.no_graph_fade {
            false
        } else {
            cfg.graph_fade.unwrap_or(false)
        },
        view,
        tab: cli
            .tab
            .as_deref()
            .and_then(TabId::from_str)
            .or(cfg.tab)
            .unwrap_or(TabId::Overview),
        smart_interval_secs: cfg
            .smart_interval_secs
            .unwrap_or(app::DEFAULT_SMART_INTERVAL_SECS),
        temp_unit: cfg.temp_unit.unwrap_or(TempUnit::Celsius),
        visible_columns: cfg.columns.unwrap_or(VisibleColumns(VisibleColumns::ALL)),
        watch_replace,
        watch_extra,
    }
}

/// `DISKWATCH_WATCH_PATHS=/var/log:/srv`. Colon-separated to match PATH,
/// which is the separator anyone reaching for an env var already has in
/// their fingers. Empty entries are dropped so a trailing colon is
/// harmless rather than a request to watch "".
fn env_watch_paths() -> Option<Vec<PathBuf>> {
    let raw = std::env::var(WATCH_PATHS_ENV).ok()?;
    let paths: Vec<PathBuf> = raw
        .split(':')
        .filter(|s| !s.trim().is_empty())
        .map(config::expand_path)
        .collect();
    if paths.is_empty() {
        None
    } else {
        Some(paths)
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // --config is sugar for the env var the loader already reads, so there
    // is one code path deciding which file gets opened.
    if let Some(path) = &cli.config {
        std::env::set_var("DISKWATCH_CONFIG", path);
    }
    if cli.write_config {
        return write_config(cli.config.as_deref(), cli.force);
    }

    let cfg = if cli.no_config {
        Config::default()
    } else {
        Config::load()
    };
    // Printed before the alternate screen opens, so it is on the scrollback
    // when the user quits. The settings overlay repeats the count, and
    // --diag reprints them in full, for whoever misses this.
    for w in &cfg.warnings {
        eprintln!("diskwatch: config: {w}");
    }

    let r = resolve(&cli, &cfg, env_watch_paths());
    let watch_roots = collect::hot_files::resolve_roots(r.watch_replace.clone(), &r.watch_extra);

    if cli.diag {
        return run_diag(&cfg, &watch_roots);
    }

    // Before any drawing: every palette read resolves through the active
    // theme, and every chart through the active graph style.
    ui::theme::set_by_name(&r.theme);
    ui::graph::set_by_name(&r.graph);
    ui::graph::set_fade(r.graph_fade);

    app::run(Options {
        start_tab: r.tab,
        view: r.view,
        smart_interval_secs: r.smart_interval_secs,
        temp_unit: r.temp_unit,
        visible_columns: r.visible_columns,
        watch_roots,
        config_source: cfg.source.clone(),
        config_warnings: cfg.warnings.clone(),
    })
}

/// Write the commented default config, so "what can I set?" is answerable
/// with a file rather than a README section that drifts from the code.
fn write_config(explicit: Option<&std::path::Path>, force: bool) -> Result<()> {
    let path = match explicit {
        Some(p) => p.to_path_buf(),
        None => config::config_path()
            .context("no HOME or XDG_CONFIG_HOME to place a config file under")?,
    };
    if path.exists() && !force {
        bail!(
            "{} already exists — pass --force to overwrite it",
            path.display()
        );
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&path, config::default_file_contents())
        .with_context(|| format!("writing {}", path.display()))?;
    println!("wrote {}", path.display());
    Ok(())
}
fn run_diag(cfg: &Config, watch_roots: &[PathBuf]) -> Result<()> {
    println!("=== Config ===");
    match &cfg.source {
        Some(p) => println!("  file: {}", p.display()),
        None => match config::config_path() {
            Some(p) => println!("  file: (none) — would read {}", p.display()),
            None => println!("  file: (none) — no HOME or XDG_CONFIG_HOME"),
        },
    }
    if cfg.warnings.is_empty() {
        println!("  warnings: none");
    } else {
        println!("  warnings ({}):", cfg.warnings.len());
        for w in &cfg.warnings {
            println!("    {}", w);
        }
    }

    println!("\n=== Hot Files roots ({}) ===", watch_roots.len());
    for r in watch_roots {
        // Say which ones won't work before the watcher says it in a
        // banner the user has to open a tab to see.
        let note = if r.exists() { "" } else { "  (missing)" };
        println!("  {}{}", r.display(), note);
    }

    let devices = collect::devices::collect();
    println!("=== Devices ({}) ===", devices.len());
    for d in &devices {
        println!(
            "  {}  kind={:?}  size={}  used={}  model={:?}  smart={:?}",
            d.name, d.kind, d.size_bytes, d.used_bytes, d.model, d.smart_ok
        );
    }
    let total: u64 = devices.iter().map(|d| d.size_bytes).sum();
    let used: u64 = devices.iter().map(|d| d.used_bytes).sum();
    let pct = if total > 0 {
        (used as f64 / total as f64 * 100.0).round() as u32
    } else {
        0
    };
    println!("  TOTAL: size={}  used={}  pct={}%", total, used, pct);

    #[cfg(target_os = "macos")]
    {
        println!("\n=== container_to_physical map ===");
        let cmap = collect::macos::container_to_physical_map();
        if cmap.is_empty() {
            println!("  (empty)");
        }
        for (synth, phys) in &cmap {
            println!("  {} -> {}", synth, phys);
        }
    }

    println!(
        "\n=== Filesystems ({}) ===",
        collect::filesystems::collect().len()
    );
    for m in collect::filesystems::collect() {
        println!(
            "  {} -> {}  ({})  size={}  used={}",
            m.device, m.mount, m.fs_type, m.size_bytes, m.used_bytes
        );
    }

    Ok(())
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    fn cli(args: &[&str]) -> Cli {
        let mut all = vec!["diskwatch"];
        all.extend_from_slice(args);
        Cli::parse_from(all)
    }

    /// Nothing on the command line, nothing in a config file: the built-in
    /// defaults, which is how the overwhelming majority of runs start.
    fn defaults(args: &[&str]) -> Resolved {
        resolve(&cli(args), &Config::default(), None)
    }

    /// diskwatch defers to the terminal's palette unless told otherwise, so a
    /// system-wide theme carries over without configuration. Now that a
    /// config file exists, this is the default at the *bottom* of the
    /// precedence chain rather than a clap default — pin it there.
    #[test]
    fn default_theme_defers_to_the_terminal() {
        assert_eq!(defaults(&[]).theme, "terminal");
        assert_eq!(ui::theme::by_name(&defaults(&[]).theme).name, "terminal");
    }

    /// The designed palette has to stay reachable, and by that exact name.
    #[test]
    fn the_designed_palette_is_still_one_flag_away() {
        assert_eq!(defaults(&["--theme", "dark"]).theme, "dark");
        assert_eq!(
            ui::theme::by_name(&defaults(&["--theme", "dark"]).theme).name,
            "dark"
        );
    }

    /// The fallback is the shared constant, not a second copy of the string
    /// that could drift from the one the theme module initialises to.
    #[test]
    fn the_fallback_comes_from_the_shared_constant() {
        assert_eq!(defaults(&[]).theme, ui::theme::DEFAULT_THEME);
    }

    /// The whole point of the precedence chain: a flag typed just now beats
    /// a file written months ago. If this ever inverts, a user's config
    /// silently ignores their command line, which is the worst possible
    /// failure for a diagnostics tool being used to test a hypothesis.
    #[test]
    fn a_flag_beats_the_config_file() {
        let cfg = Config {
            theme: Some("nord".into()),
            view: Some(ViewMode::Lite),
            tab: Some(TabId::Io),
            ..Config::default()
        };
        let r = resolve(
            &cli(&["--theme", "dark", "--dense", "--tab", "smart"]),
            &cfg,
            None,
        );
        assert_eq!(r.theme, "dark");
        assert_eq!(r.view, ViewMode::Dense);
        assert_eq!(r.tab, TabId::Smart);
    }

    /// ...and with no flag, the file is what's left standing.
    #[test]
    fn the_config_file_applies_when_no_flag_contradicts_it() {
        let cfg = Config {
            theme: Some("nord".into()),
            graph: Some("dots".into()),
            graph_fade: Some(true),
            view: Some(ViewMode::Dense),
            tab: Some(TabId::Hot),
            smart_interval_secs: Some(30),
            temp_unit: Some(TempUnit::Fahrenheit),
            columns: Some(VisibleColumns(VisibleColumns::SIZE)),
            ..Config::default()
        };
        let r = resolve(&cli(&[]), &cfg, None);
        assert_eq!(r.theme, "nord");
        assert_eq!(r.graph, "dots");
        assert!(r.graph_fade);
        assert_eq!(r.view, ViewMode::Dense);
        assert_eq!(r.tab, TabId::Hot);
        assert_eq!(r.smart_interval_secs, 30);
        assert_eq!(r.temp_unit, TempUnit::Fahrenheit);
        assert_eq!(r.visible_columns, VisibleColumns(VisibleColumns::SIZE));
    }

    /// A bare `--graph-fade` can only say yes. Without its negation there
    /// would be a setting a config file could turn on and the command line
    /// could not turn off — a one-way door into a preference.
    #[test]
    fn the_gradient_can_be_turned_off_against_a_config_that_turns_it_on() {
        let cfg = Config {
            graph_fade: Some(true),
            ..Config::default()
        };
        assert!(resolve(&cli(&[]), &cfg, None).graph_fade);
        assert!(!resolve(&cli(&["--no-graph-fade"]), &cfg, None).graph_fade);
    }

    /// The three ways to name watch roots, in the order they beat each
    /// other. This is what issue #10 asked for.
    #[test]
    fn watch_roots_come_from_flag_then_env_then_config() {
        let cfg = Config {
            watch_paths: Some(vec![PathBuf::from("/from-config")]),
            ..Config::default()
        };
        let env = || Some(vec![PathBuf::from("/from-env")]);

        assert_eq!(
            resolve(&cli(&[]), &cfg, None).watch_replace,
            Some(vec![PathBuf::from("/from-config")])
        );
        assert_eq!(
            resolve(&cli(&[]), &cfg, env()).watch_replace,
            Some(vec![PathBuf::from("/from-env")])
        );
        assert_eq!(
            resolve(&cli(&["--watch", "/from-flag"]), &cfg, env()).watch_replace,
            Some(vec![PathBuf::from("/from-flag")])
        );
    }

    /// `--watch` is repeatable, and `--watch-add` is the additive form —
    /// the "keep the defaults, add one more" case the issue also asked for.
    #[test]
    fn watch_flags_are_repeatable_and_the_additive_form_keeps_the_defaults() {
        let r = defaults(&["--watch", "/a", "--watch", "/b"]);
        assert_eq!(
            r.watch_replace,
            Some(vec![PathBuf::from("/a"), PathBuf::from("/b")])
        );

        let r = defaults(&["--watch-add", "/c"]);
        assert_eq!(r.watch_replace, None, "the defaults are untouched");
        assert_eq!(r.watch_extra, vec![PathBuf::from("/c")]);
    }

    /// `--tab` shipped with no validator, so `--tab hto` silently opened
    /// Overview and looked like the flag did nothing — the exact failure
    /// `--theme` has a validator to prevent.
    #[test]
    fn an_unknown_tab_is_rejected_rather_than_silently_ignored() {
        assert!(Cli::try_parse_from(["diskwatch", "--tab", "hto"]).is_err());
        assert_eq!(defaults(&["--tab", "hot"]).tab, TabId::Hot);
    }

    /// The generated file has to survive a round trip through the parser
    /// that reads it — otherwise `--write-config` hands the user a file
    /// diskwatch then complains about.
    #[test]
    fn the_generated_config_parses_without_warnings() {
        let cfg = Config::parse(&config::default_file_contents());
        assert!(
            cfg.warnings.is_empty(),
            "the file we write should be a file we can read: {:?}",
            cfg.warnings
        );
        // And it must round-trip to the same settings it documents as
        // defaults, or the comments are lying.
        let r = resolve(&cli(&[]), &cfg, None);
        assert_eq!(r, defaults(&[]));
    }
}
