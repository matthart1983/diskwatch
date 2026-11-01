# Changelog

Changes from 0.7.1 on. Earlier releases are on the
[releases page](https://github.com/matthart1983/diskwatch/releases).

## [0.7.1] - 2026-11-01

### Security

- File, mount and device names can no longer send terminal escape sequences. A
  name holding control or bidirectional characters reached the terminal raw from
  the Hot Files tab, Dense and `--diag`, so another user on the machine could
  plant a name that changed the terminal's title or colours while you ran
  diskwatch. Upgrade if you run it as root or on a shared machine. These
  characters now show as `·`.

### Fixed

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
