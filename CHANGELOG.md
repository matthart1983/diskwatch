# Changelog

Changes from 0.7.1 on. Earlier releases are on the
[releases page](https://github.com/matthart1983/diskwatch/releases).

## [Unreleased]

### Fixed

- Stopping diskwatch with SIGTERM, SIGINT, SIGHUP or SIGQUIT, or a crash, no
  longer leaves your shell in raw mode on the alternate screen, which needed
  `reset` to fix. If diskwatch is stuck waiting on something, such as a slow
  `smartctl`, it puts the terminal back and exits a second after the signal.
- Ctrl-C quits. It did nothing before, because raw mode delivers it as a key.
- The full view no longer crashes in a small terminal. It crashed below 5
  rows, or narrower than its header's clock. Below 40×12 it now says the
  terminal is too small. Lite and Dense draw at any size.
- The static Linux builds no longer grow without limit while files change
  fast. Under 80,000 writes a second across 20,000 files, 0.7.1 reached
  912 MB and was still growing a minute after the writes stopped. The same
  run now peaks between 37 and 61 MB and falls back to about 20 MB. They use
  mimalloc now. The glibc, macOS and Windows builds keep their system
  allocator.
- On Linux, Hot Files no longer takes nearly every inotify watch on the
  machine. Watching `$HOME` recursively took 523,541 of this host's 547,398
  and about 0.5 GB, which starves editors and sync tools of watches. It now
  takes at most a quarter of `fs.inotify.max_user_watches`, and no more than
  65,536 directories, shallowest first across all roots, keeping an eighth
  for directories made after startup. The Hot Files tab says when a root
  isn't watched all the way down, and `--diag` shows the limit. The same
  default run now holds 57,344 watches and 38 MB.
- Hot Files no longer counts opens and reads as activity, including its own.
  On Linux a quiet tree of 110 directories showed 110 events from
  diskwatch's own walk, and `ls -R` added 222 more. Both now show none. A
  write's last event is labelled "modify" instead of "other".
- The PROCESS column in Hot Files names a process that holds the file open
  even when it's doing no IO, dimmed, as the docs describe. It was blank on
  every row, because processes doing no IO were left out of the lookup.
- diskwatch no longer lists itself as the busiest process.
- Insights says "All systems nominal" only when SMART was read for every
  disk. Without root it wasn't, and the card said nominal next to a HEALTH
  tile reading "unknown". It now says no problems were found but SMART
  wasn't read, and why. Lite's capacity row says "capacity ok" instead of
  "all nominal", which is all that row checks.
- Wide characters no longer break rows. Dense drew
  `日本語のファイル名テスト.log` as `日           log`, and in the Hot tab a CJK
  or emoji filename pushed the columns after it right, clipping AGE and
  KIND. Padding and truncation now count terminal columns, and a wide
  character takes its two cells.
- "Dominating disk IO" counts physical disks only, and needs at least two.
  It named zram0, which is swap, and on a one-disk host it named the only
  disk.
- Hot Files copies the history of the files on screen only, not of every
  file it tracks, on each frame. With 4,096 busy files, the most it tracks,
  that copying took up to 7.5% of a core.

## [0.7.1] - 2026-11-01

### Security

- File, mount and device names can no longer send terminal escape sequences. A
  name holding control or bidirectional characters reached the terminal raw from
  the Hot Files tab, Dense and `--diag`, so another user on the machine could
  plant a name that changed the terminal's title or colours while you ran
  diskwatch. Upgrade if you run it as root or on a shared machine. These
  characters now show as `·`.

### Fixed

- A mount that doesn't answer no longer stops diskwatch starting. On Linux,
  diskwatch read every mount's size on its main thread before showing
  anything, so one stuck mount, such as a hung FUSE daemon or an automount
  waiting on a disk, left the TUI and `--diag` waiting forever (#34). Each
  mount now gets a second to answer. One that doesn't is left out, named in
  Insights and in `--diag`, and asked again on the next refresh.
- `S` in settings no longer overwrites your config file when you started with
  `--no-config`. It now says the settings weren't saved.
- `diskwatch --diag | head -1` exits cleanly instead of panicking on the closed
  pipe.

### Changed

- The minimum supported Rust is now 1.95. The declared 1.75 no longer built.
- Updated `anyhow` and `crossbeam-epoch`, which had published advisories.

### Release process

- A release now stops unless its tag matches `Cargo.toml`, it has a section
  here, the commit is on `main` and CI passed on it.
- Releases carry `SHA256SUMS`. Verify a download with
  `sha256sum --ignore-missing -c SHA256SUMS`.
- Added `SECURITY.md`, and turned on private vulnerability reporting.
- Workflows are read-only by default, with every action pinned to a commit.
- Added Dependabot, `cargo deny`, a minimum-Rust build and `clippy -D warnings`
  to CI.
