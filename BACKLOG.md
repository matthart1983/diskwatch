# Backlog

One line per PR, in order, each with the check that closes it.

## 0.7.2, target 13 Dec 2026

- [x] P01 fix/terminal-restore-size-floor: signals and panics restore the terminal, Ctrl-C quits, Full view floor. No panic across the size grid; terminal restored after each signal
- [x] P02 mimalloc on the static builds, and stop `top()` and Lite/Dense cloning every tracked path's history on each draw. Static build under 150 MB through the flood test
- [x] P03 Cap Hot Files watches and say what was skipped. A no-flag run stays under 25% of `max_user_watches` and 150 MB RSS
- [x] P04 Hot Files stops counting its own install walk and itself, and names idle holders in PROCESS. A quiet 110-directory tree shows 0 events
- [x] P05 No "nominal" without SMART evidence. The disk-I/O insight skips zram, and "dominating" needs more than one disk
- [x] P06 Width-aware rows in Dense and Hot. CJK and emoji names keep the columns aligned
- [x] P07 `smartctl -n standby`: show "standby" and keep the last values
- [ ] An HDD owner confirms P07 doesn't wake the disk
- [x] P08 Remove the Homebrew tap job. README: running under `sudo` (`secure_path`, `$HOME`), and the dead-mount limit on macOS and Windows (Linux no longer stalls)
- [ ] Owner: delete diskwatch's `HOMEBREW_TAP_TOKEN` repo secret (and netwatch's, unused since its tap job stopped). Don't revoke the token itself while syswatch's release still uses one by that name
- [ ] Owner check: a real disk-backed writer shows in Hot Files, and the docs match what it shows
- [ ] Listing: awesome-tuis PRs for diskwatch and netwatch, the awesome-rust link, an NFS issue and a help-wanted Windows NVMe issue
- [ ] If time: the `ERROR:` header for rootless podman directories, and the bare no-TTY error

## 0.7.3 install parity, target 10 Jan 2027

- [ ] P09 Static plain Linux asset, with `-static` kept as an alias, and a smoke test on every built binary
- [ ] P10 `.deb` and `.rpm` with an install test, `Recommends: smartmontools`
- [ ] P11 `cargo binstall` metadata, and a Windows `.zip` next to the `.tar.gz` Scoop uses
- [ ] P12 Shell completions and a man page from `clap_complete` and `clap_mangen`
- [ ] P13 Provenance, SBOM and an auditable build. `PACKAGING.md`, verifying a download, `release.sh`
- [ ] Post: ZFS and LVM, 12 Jan
