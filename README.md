<p align="center">
  <h1 align="center">DiskWatch</h1>
  <p align="center">
    <strong>Single-host disk diagnostics in your terminal. The terminal you open when the disk light won't stop blinking — before you reach for iostat, iotop, smartctl, lsblk, df, du, and a panic.</strong>
  </p>
  <p align="center">
    <a href="https://crates.io/crates/diskwatch"><img src="https://img.shields.io/crates/v/diskwatch.svg" alt="crates.io"></a>
    <a href="https://github.com/matthart1983/diskwatch/releases"><img src="https://img.shields.io/github/v/release/matthart1983/diskwatch" alt="Release"></a>
    <a href="https://repology.org/project/diskwatch/versions"><img src="https://repology.org/badge/tiny-repos/diskwatch.svg" alt="Packaging status"></a>
    <img src="https://img.shields.io/badge/platform-macOS%20%7C%20Linux-blue" alt="Platform">
    <img src="https://img.shields.io/badge/license-MIT-green" alt="License">
  </p>
</p>

<p align="center">
  <em>Sibling to <a href="https://github.com/matthart1983/netwatch">NetWatch</a> and <a href="https://github.com/matthart1983/syswatch">SysWatch</a>. Same chrome. Same palette. Eight tabs covering every disk on one box.</em>
</p>

<p align="center">
  <img src="demo.gif" alt="DiskWatch — Overview, Devices, Volumes, FS, IO, SMART, Hot Files, Insights" width="800">
</p>

---

## What it shows

| # | Tab | Replaces |
|---|---|---|
| 1 | Overview | one screen across capacity, IO, health, hot files |
| 2 | Devices | `lsblk`, `nvme list`, `diskutil list`, `hdparm -I` |
| 3 | Volumes | `lvs` + `vgs`, `mdadm --detail`, `diskutil apfs list` |
| 4 | FS | `df -h`, `df -i`, `mount`, `findmnt` |
| 5 | IO | `iostat -x 1`, biolatency-style averages |
| 6 | SMART | `smartctl -A`, `nvme smart-log` |
| 7 | Hot Files | `fanotify`/`fseventsd` watcher (paths, not bytes) |
| 8 | Insights | plain-English anomaly summaries |

Where `lsblk` shows you *which disks exist*, DiskWatch shows you *what's happening on them* — capacity trending, IO throughput, p99 latency, SMART health, and the files being written *right now* — and tells you why in plain English when something's anomalous.

## Lite

```bash
diskwatch --lite
```

One screen at 80×24, six keys, no tabs. Read and write throughput, a capacity line that
answers *how long have I got* rather than *how full am I*, and the busiest files — sized to
run in a tmux split or an SSH session to a NAS.

```
diskwatch  nas-01 · 4 devices                              ● 2.6 / 3.8 TB

r 128 MB/s read                              peak 412 MB/s  avg 96 MB/s
▁▂▃▅█▇▅▃▂▁▁▂▄▆█▇▄▂▁▁▁▂▃▅▇█▆▄▂▁▁▁▂▃▄▆█▇▅▃▂▁▁▂▃▅▇█▆▄▂▁▁▁▂▃▄▅▇█▆▄▂▁▁▂▃▄▆▇█▅▃▂▁▁▂
w 14 MB/s write                                    peak 38 MB/s  avg 11 MB/s
▁▁▂▁▁▂▃▂▁▁▁▂▁▁▂▂▁▁▁▂▃▂▁▁▁▁▂▁▁▂▂▁▁▁▂▁▁▁▂▂▁▁▁▂▃▂▁▁▁▂▁▁▂▂▁▁▁▂▁▁▂▃▂▁▁▁▂▁▁▂▂▁▁▂▁▁▂
 78s ago ─────────────────────────────────────────────────────────── now
/ 68%   /var 94%   health 4/4   growth +8.2 GB/day    /var 94% · 11 days left
```

It is not the full tool with tabs hidden — it is a different view for one machine and one
question. Toggle either direction at runtime with `L`; the full 8-tab TUI stays the default
at every terminal size. Below 80×24 Lite shows a notice rather than a clipped grid.

Same grid, keys and palette as [`netwatch --lite`](https://github.com/matthart1983/netwatch)
— only the subject changes.

The capacity projection needs about a minute of observation before it will commit to a
number, and stays silent when usage is flat or shrinking. The file table reports **events,
not bytes**: FSEvents and inotify say that a path changed, not who wrote it or how much
(see [What's real, what's deferred](#whats-real-what-deferred)).

## Install

```bash
brew install diskwatch                # macOS / Linux
nix-shell -p diskwatch                # NixOS / Nix
paru -S diskwatch                     # Arch
cargo install diskwatch               # anywhere with Rust
```

Or grab a pre-built binary from [Releases](https://github.com/matthart1983/diskwatch/releases/latest).

The Nix and Arch packages are maintained by community packagers — thank you. File packaging
issues with them; file diskwatch bugs here. The [Repology page](https://repology.org/project/diskwatch/versions)
shows which packages are current.

<details>
<summary><strong>All platforms & options</strong></summary>

| Platform | Download |
|----------|----------|
| Linux (x86_64) | [`diskwatch-linux-x86_64.tar.gz`](https://github.com/matthart1983/diskwatch/releases/latest) |
| Linux (aarch64) | [`diskwatch-linux-aarch64.tar.gz`](https://github.com/matthart1983/diskwatch/releases/latest) |
| Linux (x86_64, static) | [`diskwatch-linux-x86_64-static.tar.gz`](https://github.com/matthart1983/diskwatch/releases/latest) |
| Linux (aarch64, static) | [`diskwatch-linux-aarch64-static.tar.gz`](https://github.com/matthart1983/diskwatch/releases/latest) |
| macOS (Intel) | [`diskwatch-macos-x86_64.tar.gz`](https://github.com/matthart1983/diskwatch/releases/latest) |
| macOS (Apple Silicon) | [`diskwatch-macos-aarch64.tar.gz`](https://github.com/matthart1983/diskwatch/releases/latest) |

**From source:**

```bash
git clone https://github.com/matthart1983/diskwatch.git && cd diskwatch
cargo build --release
./target/release/diskwatch
```

</details>

**Prerequisites:** Rust 1.75+ (only if building from source). No system dependencies on Linux. macOS calls the standard `ioreg`, `diskutil`, and `system_profiler` binaries — all preinstalled. Optional: `smartmontools` (`brew install smartmontools` / `apt install smartmontools`) for full SMART attribute tables — without it, the SMART tab falls back to the basic verified/failing flag from `diskutil`.

## Keys

| Key | Action |
|---|---|
| `1`–`8` | Switch tabs |
| `↑` / `↓` / `j` / `k` | Move selection (Devices, FS) |
| `p` | Pause / resume sampling |
| `,` | Settings (columns, temperature unit, SMART interval, theme) |
| `L` | Switch between the full TUI and Lite |
| `?` | Help |
| `q` / `Esc` | Quit |
| `--lite` | Start in the minimal single-screen view |
| `--graph` | `bars` (default) or `dots` for btop-style braille |
| `--graph-fade` | btop's brightness gradient + dot grid |
| `--diag` | Print collected state and exit (no TUI) |
| `--theme` | `dark` (default), `light`, `ocean`, `solarized`, `dracula`, `nord`, `terminal` |

### Themes

Seven built-in themes, matching syswatch's set and cycle order:

**dark** (default) · **light** · **ocean** · **solarized** · **dracula** · **nord** · **terminal**

`terminal` pins no colors of its own — every slot resolves to an ANSI palette entry, and
foreground and background use your terminal's own defaults. If you theme your whole desktop
with pywal, matugen, or a terminal profile, this is the one that follows along.

Switch it live from the settings overlay (`,` → **Theme** → `Space`), or start with it:

```bash
diskwatch --theme terminal      # also accepts: system, ansi
```

The choice isn't persisted between runs — like the other settings, it resets to `dark` on
restart. Use the flag to make it stick.

### Graph style

Every chart in the app — the Overview aggregate, the IO tab's per-device panels, and both
Lite charts — draws through one renderer, so a single setting changes them all.

```bash
diskwatch --graph dots                 # btop-style braille (also: braille, btop)
diskwatch --graph dots --graph-fade    # ...plus btop's gradient and dot grid
```

**bars** is the stacked eighth-block look diskwatch shipped with — eight levels per row.
**dots** packs four braille pixel rows into each cell, so a one-row sparkline resolves four
distinct heights instead of reading as on/off. On the tall charts the difference is
subtler; on Lite's 9-column row sparklines it's the difference between a shape and a blob.

**`--graph-fade`** is btop's other half: each chart runs bright at `now` and dims toward the
left edge, over a faint dot grid. It's a separate switch because the gradient reads fine
under `bars` too — and because it interpolates in RGB, so it's ignored under
`--theme terminal`, which exists precisely to pin no RGB. The grid only draws on charts at
least 16×4, which means the IO and Overview panels get it and Lite's 3- and 2-row charts
don't.

Both are live in the settings overlay (`,` → **Graph style** / **Graph fade** → `Space`).
Lite has no settings overlay of its own — press `L`, change it, press `L` back.

## Tabs in detail

**[1] Overview** — 5 KPI tiles (capacity, IO, p99 latency, health, insights), per-device summary, aggregate IO sparkline, top insights, segmented capacity bar.

**[2] Devices** — block-device table with model, firmware, serial, used %, SMART status. Detail panel for the selected device.

**[3] Volumes** — APFS containers (macOS) with nested volumes, role, mount, FileVault. mdraid arrays (Linux) with members, slot state `[UUUU]`, resync/recovery progress.

**[4] FS** — mounted filesystems with inline usage bars, threshold colors, system/user/removable classification.

**[5] IO** — per-device read / write throughput, 48s sparkline, p50 + p99 latency (read and write) over a 60s rolling window.

**[6] SMART** — full NVMe / ATA attribute tables when `smartctl` is on PATH; degraded banner with install instructions when not. Always shows the basic verified/failing flag.

**[7] Hot Files** — paths by event rate via FSEvents (macOS) / inotify (Linux). Honest footer: this tab can't show bytes/sec or process attribution without root (`fs_usage`) / Endpoint Security entitlement / eBPF biosnoop.

**[8] Insights** — anomaly cards over the collected state: capacity warnings, SMART failures, NVMe wear, drive temperature, p99 latency outliers, IO-dominant devices, hot-file runaway, removable drives.

## What's real, what's deferred

| Metric | macOS | Linux |
|---|---|---|
| Device model / serial / firmware | ✅ `system_profiler` + IOKit | ✅ `/sys/block/*/device/{model,serial,firmware_rev}` |
| Per-device used bytes | ✅ via APFS container map | ✅ summed from `sysinfo` mounts |
| Read/write byte rates (split) | ✅ IOKit `Statistics` | ✅ `/proc/diskstats` cols 5/9 |
| Avg per-op latency | ✅ `Total Time / Operations` | ✅ `/proc/diskstats` cols 6/10 |
| p50 / p99 latency | ✅ tick-averaged over 60s | ✅ tick-averaged over 60s |
| True per-op p99 (histogram) | ❌ needs IOReport entitlement | ❌ needs eBPF biolatency (CAP_BPF) |
| SMART attributes | ✅ `smartctl` if installed | ✅ `smartctl` if installed |
| Volumes — APFS | ✅ `diskutil apfs list` | n/a |
| Volumes — mdraid | n/a | ✅ `/proc/mdstat` |
| Volumes — ZFS, LVM | ⏳ deferred | ⏳ deferred |
| Hot files (paths) | ✅ FSEvents | ✅ inotify |
| Hot files — bytes / pid | ❌ needs root `fs_usage` / entitlement | ❌ needs eBPF biosnoop |
| Capacity growth + time-to-full | ✅ 10-min usage window | ✅ 10-min usage window |
| Aggregate queue depth | ❌ not exposed by IOKit | ⏳ `/proc/diskstats` col 12, deferred |

## Design

Inherits the *Watch family chrome — `#0c1418` background, terminal-green accent, JetBrains Mono, 130×36 character grid with responsive reflow ≥ 110×30. The same character-grid mockups that drive NetWatch and SysWatch drive DiskWatch.

## Anti-goals

- **Not multi-host.** Use NetWatch Cloud if you need a fleet view.
- **Not a daemon.** No long-running collector, no persisted DB.
- **Not a deduper / cleaner.** We surface what's eating disk; we don't delete anything. Mutation is a different tool.
- **Not a backup product.** Snapshots are observed, not authored.
- **Not a benchmark.** We measure what's happening, not what's possible.

## License

MIT.
