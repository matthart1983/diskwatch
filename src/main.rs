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

    /// Color theme: "dark" (default) or "terminal" to use the colors your
    /// terminal already defines, so system-wide themes carry over.
    #[arg(long, default_value = "dark", value_parser = parse_theme)]
    theme: String,

    /// Chart style: "bars" (default) or "dots" for btop-style braille,
    /// which resolves four levels per row instead of one. Also accepts
    /// "braille" and "btop".
    #[arg(long, default_value = "bars", value_parser = parse_graph)]
    graph: String,

    /// btop's gradient: charts fade from bright at `now` to dim at the
    /// left edge, over a faint dot grid. Off by default; ignored under
    /// `--theme terminal`, which pins no RGB.
    #[arg(long)]
    graph_fade: bool,

    /// Start in Lite: one 80×24 screen, six keys — read and write
    /// throughput, capacity with a time-to-full projection, and the
    /// busiest files. Toggle either way at runtime with `L`.
    #[arg(long)]
    lite: bool,

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
    app::run(app::Options {
        start_tab: cli.tab,
        lite: cli.lite,
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
