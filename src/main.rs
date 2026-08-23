use anyhow::Result;
use clap::Parser;

mod app;
mod collect;
mod insights;
mod tabs;
mod ui;

#[derive(Parser, Debug)]
#[command(
    name = "diskwatch",
    version,
    about = "Single-host disk diagnostics TUI"
)]
struct Cli {
    /// Start on a specific tab (overview, devices, volumes, fs, io, smart, hot, insights).
    #[arg(long)]
    tab: Option<String>,

    /// Color theme. Defaults to "terminal": every color resolves through
    /// the palette your terminal already defines, so a system-wide theme
    /// (a terminal profile, pywal, matugen, a rice) carries straight over
    /// and diskwatch sits beside your other tools instead of fighting them.
    ///
    /// Pass "dark" for diskwatch's own designed palette, or any of the
    /// other built-ins: light, ocean, solarized, dracula, nord.
    #[arg(long, default_value = ui::theme::DEFAULT_THEME, value_parser = parse_theme)]
    theme: String,

    /// Chart style: "bars" (default) or "dots" for btop-style braille,
    /// which resolves four levels per row instead of one. Also accepts
    /// "braille" and "btop".
    #[arg(long, default_value = "bars", value_parser = parse_graph)]
    graph: String,

    /// btop's gradient: charts fade from bright at `now` to dim at the
    /// left edge, over a faint dot grid. Off by default.
    ///
    /// Needs a theme with real RGB to fade through, so it does nothing
    /// under the default `--theme terminal` — a 16-color palette has no
    /// intermediate shades. Pair it with `--theme dark` (or any other
    /// built-in) to see it.
    #[arg(long)]
    graph_fade: bool,

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
    #[arg(long, value_parser = parse_view)]
    view: Option<String>,

    /// Print collected state and exit without launching the TUI.
    /// Useful for diagnosing what each collector is seeing.
    #[arg(long)]
    diag: bool,
}

/// `theme::by_name` falls back to `dark` for anything it doesn't recognise,
/// which is right at runtime but wrong at the CLI: a typo would silently
/// render the wrong theme and look like the flag did nothing. Reject it here
/// instead, and list what's actually available.
fn parse_theme(raw: &str) -> Result<String, String> {
    let resolved = ui::theme::by_name(raw);
    if resolved.name == "dark" && !raw.eq_ignore_ascii_case("dark") {
        return Err(format!(
            "unknown theme {raw:?} (available: {})",
            ui::theme::THEME_NAMES.join(", ")
        ));
    }
    Ok(raw.to_string())
}

/// Same reasoning as [`parse_theme`]: `graph::by_name` falls back to
/// `bars` for anything it doesn't recognise, which is right at runtime
/// but wrong at the CLI — a typo would silently render the default and
/// look like the flag did nothing.
fn parse_graph(raw: &str) -> Result<String, String> {
    if ui::graph::by_name(raw) == ui::graph::GraphStyle::Bars && !raw.eq_ignore_ascii_case("bars") {
        return Err(format!(
            "unknown graph style {raw:?} (available: {})",
            ui::graph::GRAPH_STYLE_NAMES.join(", ")
        ));
    }
    Ok(raw.to_string())
}

/// Same reasoning as [`parse_theme`]: reject an unknown view at the CLI
/// rather than falling back to the default, which would look like the flag
/// did nothing.
fn parse_view(raw: &str) -> Result<String, String> {
    app::ViewMode::from_name(raw)
        .map(|_| raw.to_string())
        .ok_or_else(|| {
            format!(
                "unknown view {raw:?} (available: {})",
                app::VIEW_MODE_NAMES.join(", ")
            )
        })
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.diag {
        return run_diag();
    }
    // Before any drawing: every palette read resolves through the active
    // theme, and every chart through the active graph style.
    ui::theme::set_by_name(&cli.theme);
    ui::graph::set_by_name(&cli.graph);
    ui::graph::set_fade(cli.graph_fade);
    // Precedence: an explicit --view wins, then the two shorthand flags.
    // --dense beats --lite when both are passed — it is the more specific
    // request, and honouring the other silently would look like the flag
    // did nothing.
    let view = cli
        .view
        .as_deref()
        .and_then(app::ViewMode::from_name)
        .unwrap_or(if cli.dense {
            app::ViewMode::Dense
        } else if cli.lite {
            app::ViewMode::Lite
        } else {
            app::ViewMode::Full
        });
    app::run(app::Options {
        start_tab: cli.tab,
        view,
    })
}

fn run_diag() -> Result<()> {
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

    /// diskwatch defers to the terminal's palette unless told otherwise, so a
    /// system-wide theme carries over without configuration. There is no
    /// config file to persist a preference in, which makes this default the
    /// only thing standing between a riced terminal and a tool that ignores
    /// it — so pin it.
    #[test]
    fn default_theme_defers_to_the_terminal() {
        let cli = Cli::parse_from(["diskwatch"]);
        assert_eq!(cli.theme, "terminal");
        assert_eq!(ui::theme::by_name(&cli.theme).name, "terminal");
    }

    /// The designed palette has to stay reachable, and by that exact name.
    #[test]
    fn the_designed_palette_is_still_one_flag_away() {
        let cli = Cli::parse_from(["diskwatch", "--theme", "dark"]);
        assert_eq!(cli.theme, "dark");
        assert_eq!(ui::theme::by_name(&cli.theme).name, "dark");
    }

    /// The flag's default is the shared constant, not a second copy of the
    /// string that could drift from the one the theme module initialises to.
    #[test]
    fn the_flag_default_comes_from_the_shared_constant() {
        let cli = Cli::parse_from(["diskwatch"]);
        assert_eq!(cli.theme, ui::theme::DEFAULT_THEME);
    }
}
