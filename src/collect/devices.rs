//! Cross-platform device enumeration.
//!
//! - **macOS**: `system_profiler -json` for model / firmware / serial /
//!   SMART / removability, plus `diskutil list` to map APFS-container
//!   volumes back to the physical disk they live on. sysinfo provides
//!   used-byte attribution per mount.
//! - **Other**: `sysinfo::Disks` as the sole source; metadata fields are
//!   left as placeholders until a platform-specific collector lands.

use std::collections::HashMap;

use sysinfo::Disks;

#[cfg(target_os = "macos")]
use crate::collect::macos;

#[cfg(target_os = "linux")]
use crate::collect::linux;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    Nvme,
    /// Reserved for Linux sysfs classification (SATA SSDs) — macOS reports
    /// these as SATA which we currently bucket as HDD until rotation_rpm
    /// distinguishes them.
    #[allow(dead_code)]
    Ssd,
    Hdd,
    UsbMassStorage,
    Unknown,
}

impl DeviceKind {
    pub fn label(&self) -> &'static str {
        match self {
            DeviceKind::Nvme => "NVMe",
            DeviceKind::Ssd => "SSD",
            DeviceKind::Hdd => "HDD",
            DeviceKind::UsbMassStorage => "USB",
            DeviceKind::Unknown => "?",
        }
    }
}

#[derive(Debug, Clone)]
pub struct DeviceTick {
    pub name: String,
    pub kind: DeviceKind,
    pub model: String,
    pub bus: String,
    pub size_bytes: u64,
    pub used_bytes: u64,
    pub is_removable: bool,
    pub firmware: Option<String>,
    pub serial: Option<String>,
    pub smart_ok: Option<bool>,
    pub idle: bool,
}

pub fn collect() -> Vec<DeviceTick> {
    #[cfg(not(target_os = "linux"))]
    let mounts_used = sysinfo_mount_used();

    #[cfg(target_os = "macos")]
    {
        let mut macs = macos::collect();
        // Apple's internal SSD reports identical model entries per
        // controller occasionally; dedupe by bsd_name.
        macs.sort_by(|a, b| a.bsd_name.cmp(&b.bsd_name));
        macs.dedup_by(|a, b| a.bsd_name == b.bsd_name);

        let cmap = macos::container_to_physical_map();
        let mut used_by_phys: HashMap<String, u64> = HashMap::new();
        for (mount_disk, used) in &mounts_used {
            let phys = cmap.get(mount_disk).cloned().unwrap_or(mount_disk.clone());
            let entry = used_by_phys.entry(phys).or_insert(0);
            if *used > *entry {
                *entry = *used;
            }
        }

        let mut out: Vec<DeviceTick> = macs
            .into_iter()
            .map(|m| {
                let kind = match m.controller_kind {
                    macos::ControllerKind::Nvme => DeviceKind::Nvme,
                    macos::ControllerKind::Sata => DeviceKind::Hdd,
                    macos::ControllerKind::Usb => DeviceKind::UsbMassStorage,
                    macos::ControllerKind::Unknown => DeviceKind::Unknown,
                };
                let used = used_by_phys.get(&m.bsd_name).copied().unwrap_or(0);
                DeviceTick {
                    name: m.bsd_name,
                    kind,
                    model: m.model,
                    bus: m.protocol,
                    size_bytes: m.size_bytes,
                    used_bytes: used,
                    is_removable: m.removable,
                    firmware: m.firmware,
                    serial: m.serial,
                    smart_ok: m.smart_ok,
                    idle: m.size_bytes == 0,
                }
            })
            .collect();
        out.sort_by_key(|d| std::cmp::Reverse(d.size_bytes));
        out
    }

    #[cfg(target_os = "linux")]
    {
        let used_by_device = linux_used_by_device();
        let mut out: Vec<DeviceTick> = linux::collect()
            .into_iter()
            .map(|l| {
                let kind = match l.kind {
                    linux::LinuxKind::Nvme => DeviceKind::Nvme,
                    linux::LinuxKind::Ssd => DeviceKind::Ssd,
                    linux::LinuxKind::Hdd => DeviceKind::Hdd,
                    linux::LinuxKind::UsbMassStorage => DeviceKind::UsbMassStorage,
                    linux::LinuxKind::Unknown => DeviceKind::Unknown,
                };
                let bus = match kind {
                    DeviceKind::Nvme => "PCIe / NVMe".to_string(),
                    DeviceKind::UsbMassStorage => "USB".to_string(),
                    DeviceKind::Hdd | DeviceKind::Ssd => "SATA / internal".to_string(),
                    DeviceKind::Unknown => "—".to_string(),
                };
                let used = used_by_device.get(&l.name).copied().unwrap_or(0);
                DeviceTick {
                    name: l.name,
                    kind,
                    model: l.model,
                    bus,
                    size_bytes: l.size_bytes,
                    used_bytes: used,
                    is_removable: l.removable,
                    firmware: l.firmware,
                    serial: l.serial,
                    // SMART status comes from smartctl in the separate
                    // SmartCollector; the linux collector doesn't set it.
                    smart_ok: None,
                    idle: l.size_bytes == 0,
                }
            })
            .collect();
        out.sort_by_key(|d| std::cmp::Reverse(d.size_bytes));
        out
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let mut out: Vec<DeviceTick> = mounts_used
            .into_iter()
            .map(|(name, used)| {
                let kind = classify_by_name(&name);
                DeviceTick {
                    name: name.clone(),
                    kind,
                    model: "—".to_string(),
                    bus: bus_hint(&kind),
                    size_bytes: 0,
                    used_bytes: used,
                    is_removable: false,
                    firmware: None,
                    serial: None,
                    smart_ok: None,
                    idle: false,
                }
            })
            .collect();
        let disks = Disks::new_with_refreshed_list();
        for d in disks.list() {
            let n = short_name(&d.name().to_string_lossy());
            if let Some(e) = out.iter_mut().find(|e| e.name == n) {
                e.size_bytes = e.size_bytes.max(d.total_space());
                if d.is_removable() {
                    e.is_removable = true;
                    e.kind = DeviceKind::UsbMassStorage;
                }
            }
        }
        out.sort_by_key(|d| std::cmp::Reverse(d.size_bytes));
        out
    }
}

/// Fast path: re-reads sysinfo and updates `used_bytes` on an existing
/// device list without redoing the slow `system_profiler` enrichment.
///
/// On macOS we re-use the cached container→physical map. If the topology
/// changed (drive plugged in / out) the next full `collect()` picks it up.
pub fn refresh_usage(devices: &mut [DeviceTick]) {
    // Linux: shared attribution with collect() — a mount's usage lands
    // only on the device(s) it actually lives on. (v0.1.1 summed every
    // mount into every device here, so an 8-disk bcachefs box showed
    // each disk at 266% — issue #4.)
    #[cfg(target_os = "linux")]
    {
        let used = linux_used_by_device();
        for d in devices.iter_mut() {
            d.used_bytes = used.get(&d.name).copied().unwrap_or(0);
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        let mounts = sysinfo_mount_used();

        #[cfg(target_os = "macos")]
        let cmap = macos::container_to_physical_map();

        for d in devices.iter_mut() {
            // macOS APFS shares free across volumes → max-used per physical.
            let mut used_max = 0u64;
            for (mount_disk, mu) in &mounts {
                #[cfg(target_os = "macos")]
                let phys = cmap
                    .get(mount_disk)
                    .cloned()
                    .unwrap_or_else(|| mount_disk.clone());
                #[cfg(not(target_os = "macos"))]
                let phys = mount_disk.clone();
                if phys == d.name && *mu > used_max {
                    used_max = *mu;
                }
            }
            d.used_bytes = used_max;
        }
    }
}

/// Linux: map whole-disk device names to used bytes, attributing each
/// mounted filesystem to the device(s) backing it.
#[cfg(target_os = "linux")]
fn linux_used_by_device() -> HashMap<String, u64> {
    let disks = Disks::new_with_refreshed_list();
    // Dedupe by raw mount source first: the same source mounted at
    // several points (bind mounts, btrfs subvolumes, `/` + `/nix/store`)
    // is one filesystem and must count once.
    let mut by_source: HashMap<String, u64> = HashMap::new();
    for d in disks.list() {
        let used = d.total_space().saturating_sub(d.available_space());
        if used == 0 {
            continue;
        }
        let source = d.name().to_string_lossy().to_string();
        let entry = by_source.entry(source).or_insert(0);
        if used > *entry {
            *entry = used;
        }
    }
    let bmap = bcachefs_member_map();
    attribute_sources(&by_source, &sysfs_slaves, &|m: &str| bmap.get(m).cloned())
}

/// Attribute per-mount-source used bytes to whole-disk device names.
///
/// Handles the source shapes that broke v0.1.1 (issue #4):
/// - `/dev/sda1` — plain partition → parent disk.
/// - `/dev/sda:/dev/sdb:...` — bcachefs multi-device → split across members.
/// - `/dev/md0`, `/dev/mapper/vg-lv` — stacked devices → resolved to their
///   member disks via `slaves`.
/// - `overlay`, `tmpfs`, ZFS datasets — no `/dev/` source → attributed to
///   nothing rather than to everything.
///
/// statfs totals are filesystem-wide, so a filesystem spanning N disks is
/// split evenly — per-member truth isn't knowable from statfs alone.
///
/// `slaves` resolves a stacked block device path to its member disk names
/// (`/dev/md0` → `["sda", "sdb"]`), returning `None` for plain devices.
///
/// `bcachefs_members` maps any bcachefs member (short name) to the full,
/// sorted member set of the filesystem it belongs to (from `/sys/fs/bcachefs`),
/// or `None` for non-bcachefs devices. This is what makes multi-device
/// attribution survive a reboot (issue #5): after a remount the mount source
/// often shows just a *single* member device instead of the colon-joined
/// list, so trusting the source string alone dumps the whole filesystem onto
/// one disk (>100% used). Expanding any member back to the full set restores
/// the even split regardless of how the filesystem was mounted.
///
/// Sources that resolve *through the bcachefs member map* are grouped by that
/// member set before splitting and counted once (max): one bcachefs filesystem
/// mounted several times (subvolumes, snapshots) reports the same
/// filesystem-wide statfs usage under different source strings, so summing
/// those would multiply it. Every other source sums, because a shared member
/// set does **not** imply a shared filesystem: two LVs on one multi-PV VG both
/// resolve to the same PVs via `slaves` yet are independent filesystems whose
/// usage must add up. (Collapsing those was a v0.1.3 regression — it silently
/// dropped all but the largest LV.)
// Only called from Linux collection, but compiled everywhere so the
// pure-logic tests run on any development machine.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn attribute_sources(
    by_source: &HashMap<String, u64>,
    slaves: &dyn Fn(&str) -> Option<Vec<String>>,
    bcachefs_members: &dyn Fn(&str) -> Option<Vec<String>>,
) -> HashMap<String, u64> {
    use std::collections::BTreeSet;

    fn spread(out: &mut HashMap<String, u64>, members: &[String], used: u64) {
        let share = used / members.len() as u64;
        for m in members {
            let entry = out.entry(m.clone()).or_insert(0);
            *entry = entry.saturating_add(share);
        }
    }

    let mut out: HashMap<String, u64> = HashMap::new();
    // One entry per bcachefs filesystem, keyed by its sysfs member set —
    // the only grouping where different source strings are provably the
    // same filesystem.
    let mut bcachefs_fs: HashMap<Vec<String>, u64> = HashMap::new();

    for (source, used) in by_source {
        let mut set: BTreeSet<String> = BTreeSet::new();
        let mut via_bcachefs = false;
        for piece in source.split(':').filter(|p| p.starts_with("/dev/")) {
            let base = match slaves(piece) {
                Some(m) if !m.is_empty() => m,
                _ => vec![short_name(piece)],
            };
            for m in base {
                // A bcachefs member stands in for its whole filesystem.
                match bcachefs_members(&m) {
                    Some(full) if !full.is_empty() => {
                        via_bcachefs = true;
                        set.extend(full);
                    }
                    _ => {
                        set.insert(m);
                    }
                }
            }
        }
        if set.is_empty() {
            continue;
        }
        let members: Vec<String> = set.into_iter().collect();
        if via_bcachefs {
            // Same filesystem under any of its members: count it once.
            let entry = bcachefs_fs.entry(members).or_insert(0);
            if *used > *entry {
                *entry = *used;
            }
        } else {
            // Independent filesystem — sums, even if it shares members
            // with another (partitions on a disk, LVs on a VG).
            spread(&mut out, &members, *used);
        }
    }

    for (members, used) in bcachefs_fs {
        spread(&mut out, &members, used);
    }
    out
}

/// Resolve a stacked block device (`/dev/md0`, `/dev/mapper/vg-lv`) to its
/// member disk names via `/sys/block/<dev>/slaves`, following nesting
/// (dm-on-md) a few levels deep. Returns `None` for plain devices.
#[cfg(target_os = "linux")]
fn sysfs_slaves(dev_path: &str) -> Option<Vec<String>> {
    // /dev/mapper/* entries are symlinks to ../dm-N — resolve to the
    // kernel name that /sys/block uses.
    let kernel_name = match std::fs::read_link(dev_path) {
        Ok(target) => target.file_name()?.to_string_lossy().into_owned(),
        Err(_) => dev_path.trim_start_matches("/dev/").to_string(),
    };

    fn expand(name: &str, depth: u8, out: &mut Vec<String>) {
        let slaves_dir = format!("/sys/block/{}/slaves", name);
        let entries: Vec<String> = std::fs::read_dir(&slaves_dir)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        if entries.is_empty() || depth == 0 {
            out.push(short_name(name));
            return;
        }
        for slave in entries {
            // Slaves may be partitions (sda1) — fold to the parent disk.
            expand(&short_name(&slave), depth - 1, out);
        }
    }

    if !std::path::Path::new(&format!("/sys/block/{}/slaves", kernel_name)).is_dir() {
        return None;
    }
    let mut out = Vec::new();
    expand(&kernel_name, 4, &mut out);
    Some(out)
}

/// Map every bcachefs member device (short name) to the full, sorted set of
/// member devices of the filesystem it belongs to, read from the authoritative
/// `/sys/fs/bcachefs/<uuid>/dev-*/block` symlinks.
///
/// This is the source of truth for multi-device attribution (issue #5): it is
/// independent of however the filesystem happens to be mounted, so it works
/// even when a post-reboot remount presents a single member as the mount
/// source instead of the colon-joined device list. Single-device bcachefs
/// filesystems are skipped (nothing to expand). Returns an empty map when
/// bcachefs isn't in use or the sysfs interface is absent, in which case
/// attribution falls back to the mount-source string unchanged.
#[cfg(target_os = "linux")]
fn bcachefs_member_map() -> HashMap<String, Vec<String>> {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    let Ok(fs_dirs) = std::fs::read_dir("/sys/fs/bcachefs") else {
        return map;
    };
    for fs in fs_dirs.flatten() {
        let fs_path = fs.path();
        if !fs_path.is_dir() {
            continue;
        }
        let mut members: Vec<String> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&fs_path) {
            for e in entries.flatten() {
                let fname = e.file_name();
                let fname = fname.to_string_lossy();
                if !fname.starts_with("dev-") {
                    continue;
                }
                // dev-N/block → symlink to the member's block device sysfs
                // node (…/block/sda); its file name is the kernel device.
                if let Ok(target) = std::fs::read_link(e.path().join("block")) {
                    if let Some(dev) = target.file_name() {
                        members.push(short_name(&dev.to_string_lossy()));
                    }
                }
            }
        }
        members.sort();
        members.dedup();
        // Single-device (or unreadable) filesystems need no expansion.
        if members.len() < 2 {
            continue;
        }
        for m in &members {
            map.insert(m.clone(), members.clone());
        }
    }
    map
}

/// Returns (physical-or-container-disk, used_bytes) per sysinfo mount,
/// already deduped by short name. On macOS the disk is the synthesized
/// container (e.g. `disk3`); the caller maps it to the physical.
///
/// `sysinfo::Disk::name()` is platform-dependent: on Linux it returns
/// the device source (`/dev/sda1`), on macOS it returns the volume
/// label (`Macintosh HD`). The macOS path resolves the device path via
/// the `mount` command's mount-point → device mapping; the Linux path
/// keeps the existing behavior.
#[cfg(not(target_os = "linux"))]
fn sysinfo_mount_used() -> Vec<(String, u64)> {
    let disks = Disks::new_with_refreshed_list();
    #[cfg(target_os = "macos")]
    let mount_table = macos_mount_table();

    let mut by_disk: HashMap<String, u64> = HashMap::new();
    for d in disks.list() {
        let used = d.total_space().saturating_sub(d.available_space());
        if used == 0 {
            continue;
        }
        #[cfg(target_os = "macos")]
        let device_source = {
            let mp = d.mount_point().to_string_lossy().to_string();
            match mount_table.get(&mp) {
                Some(dev) => dev.clone(),
                None => continue, // virtual fs / devfs / unmapped
            }
        };
        #[cfg(not(target_os = "macos"))]
        let device_source = d.name().to_string_lossy().to_string();

        let name = short_name(&device_source);
        let entry = by_disk.entry(name).or_insert(0);
        // APFS containers report the same free across all volumes — max
        // avoids over-counting. On Linux the caller sums instead.
        if used > *entry {
            *entry = used;
        }
    }
    by_disk.into_iter().collect()
}

#[cfg(target_os = "macos")]
fn macos_mount_table() -> HashMap<String, String> {
    use std::process::Command;
    let Ok(out) = Command::new("/sbin/mount").output() else {
        return HashMap::new();
    };
    if !out.status.success() {
        return HashMap::new();
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut map = HashMap::new();
    for line in text.lines() {
        // Each line: "<device> on <mount> (fs, opts...)"
        let Some(on_idx) = line.find(" on ") else {
            continue;
        };
        let device = line[..on_idx].trim().to_string();
        let after = &line[on_idx + 4..];
        let Some(paren) = after.rfind(" (") else {
            continue;
        };
        let mount = after[..paren].trim().to_string();
        map.insert(mount, device);
    }
    map
}

/// Fold a device path or partition name to its whole-disk name:
/// `/dev/sda1` → `sda`, `nvme0n1p2` → `nvme0n1`, `mmcblk0p2` → `mmcblk0`,
/// `disk3s1` → `disk3` (macOS). Names with no partition suffix pass
/// through unchanged (`md0`, `mmcblk0boot0`, `overlay`).
fn short_name(raw: &str) -> String {
    let s = raw.trim_start_matches("/dev/");
    // macOS: diskNsM → diskN
    if let Some(rest) = s.strip_prefix("disk") {
        let n: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !n.is_empty() {
            return format!("disk{}", n);
        }
    }
    // "<base>p<digits>" where base ends in a digit → base
    // (nvme0n1p2 → nvme0n1, mmcblk0p2 → mmcblk0, md0p1 → md0)
    if let Some(idx) = s.rfind('p') {
        let (base, part) = (&s[..idx], &s[idx + 1..]);
        if base.ends_with(|c: char| c.is_ascii_digit())
            && !part.is_empty()
            && part.chars().all(|c| c.is_ascii_digit())
        {
            return base.to_string();
        }
    }
    // Trailing partition digits on letter-named disks (sda1 → sda,
    // vdb2 → vdb, xvda1 → xvda). Digits are part of the name elsewhere
    // (md0, loop3), so only these prefixes are folded.
    if ["sd", "hd", "vd", "xvd"].iter().any(|p| s.starts_with(p)) {
        return s.chars().take_while(|c| !c.is_ascii_digit()).collect();
    }
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_name_folds_partitions() {
        assert_eq!(short_name("/dev/sda1"), "sda");
        assert_eq!(short_name("/dev/sda"), "sda");
        assert_eq!(short_name("/dev/nvme0n1p2"), "nvme0n1");
        assert_eq!(short_name("/dev/nvme0n1"), "nvme0n1");
        assert_eq!(short_name("/dev/mmcblk0p2"), "mmcblk0");
        assert_eq!(short_name("/dev/mmcblk0"), "mmcblk0");
        assert_eq!(short_name("/dev/mmcblk0boot0"), "mmcblk0boot0");
        assert_eq!(short_name("/dev/md0"), "md0");
        assert_eq!(short_name("/dev/md0p1"), "md0");
        assert_eq!(short_name("/dev/vdb2"), "vdb");
        assert_eq!(short_name("/dev/xvda1"), "xvda");
        assert_eq!(short_name("/dev/disk3s1"), "disk3");
        assert_eq!(short_name("overlay"), "overlay");
    }

    fn no_slaves(_: &str) -> Option<Vec<String>> {
        None
    }

    fn no_bcachefs(_: &str) -> Option<Vec<String>> {
        None
    }

    /// Build a bcachefs member-map lookup: any listed member resolves to the
    /// full set. Mirrors what `bcachefs_member_map` reads from sysfs.
    fn bcachefs(members: &'static [&'static str]) -> impl Fn(&str) -> Option<Vec<String>> {
        move |m: &str| {
            members
                .contains(&m)
                .then(|| members.iter().map(|s| s.to_string()).collect())
        }
    }

    fn sources(list: &[(&str, u64)]) -> HashMap<String, u64> {
        list.iter().map(|(s, u)| (s.to_string(), *u)).collect()
    }

    #[test]
    fn plain_partition_attributes_to_parent_disk() {
        let by_source = sources(&[("/dev/mmcblk0p2", 27_000), ("/dev/mmcblk0p1", 300)]);
        let out = attribute_sources(&by_source, &no_slaves, &no_bcachefs);
        assert_eq!(out.get("mmcblk0"), Some(&27_300));
    }

    #[test]
    fn bcachefs_multi_device_splits_across_members() {
        // Issue #4: 8× 480 GB bcachefs. v0.1.1 credited the whole fs to
        // every attached device; the fs usage must be split across its
        // members and land nowhere else.
        let members = (b'a'..=b'h')
            .map(|c| format!("/dev/sd{}", c as char))
            .collect::<Vec<_>>()
            .join(":");
        let by_source = sources(&[(members.as_str(), 640_000_000)]);
        let out = attribute_sources(&by_source, &no_slaves, &no_bcachefs);
        assert_eq!(out.get("sda"), Some(&80_000_000));
        assert_eq!(out.get("sdh"), Some(&80_000_000));
        assert_eq!(out.len(), 8);
    }

    #[test]
    fn md_array_splits_across_slaves() {
        // Issue #4 (second report): RAID0 across USB disks. The array's
        // usage splits across its members via the slaves map.
        let slaves = |dev: &str| -> Option<Vec<String>> {
            (dev == "/dev/md0")
                .then(|| vec!["sdd".into(), "sdb".into(), "sde".into(), "sdc".into()])
        };
        let by_source = sources(&[("/dev/md0", 4_000_000), ("/dev/nvme0n1p1", 5_000)]);
        let out = attribute_sources(&by_source, &slaves, &no_bcachefs);
        assert_eq!(out.get("sdd"), Some(&1_000_000));
        assert_eq!(out.get("sdc"), Some(&1_000_000));
        assert_eq!(out.get("nvme0n1"), Some(&5_000));
        assert_eq!(out.get("md0"), None);
    }

    #[test]
    fn virtual_sources_attribute_to_nothing() {
        let by_source = sources(&[
            ("overlay", 640_000_000),
            ("tmpfs", 1_000),
            ("pool/dataset", 9_000),
        ]);
        let out = attribute_sources(&by_source, &no_slaves, &no_bcachefs);
        assert!(out.is_empty());
    }

    #[test]
    fn same_disk_partitions_sum() {
        let by_source = sources(&[("/dev/sda1", 100), ("/dev/sda2", 250)]);
        let out = attribute_sources(&by_source, &no_slaves, &no_bcachefs);
        assert_eq!(out.get("sda"), Some(&350));
    }

    #[test]
    fn bcachefs_single_device_mount_expands_to_all_members() {
        // Issue #5: after a reboot the fs remounts showing a single member
        // as the source instead of the colon-joined list, so v0.1.2 dumped
        // the whole filesystem onto one disk (>100% used). The sysfs member
        // map expands the lone member back to the full set so usage still
        // splits evenly across all four disks.
        let members: &[&str] = &["sda", "sdb", "sdc", "sdd"];
        let by_source = sources(&[("/dev/sda", 4_000_000)]);
        let out = attribute_sources(&by_source, &no_slaves, &bcachefs(members));
        assert_eq!(out.get("sda"), Some(&1_000_000));
        assert_eq!(out.get("sdd"), Some(&1_000_000));
        assert_eq!(out.len(), 4);
    }

    #[test]
    fn bcachefs_colon_source_still_splits_with_member_map() {
        // The historical colon-joined form keeps working, and the member
        // map is idempotent — union of the same set is the same set.
        let members: &[&str] = &["sda", "sdb"];
        let by_source = sources(&[("/dev/sda:/dev/sdb", 2_000_000)]);
        let out = attribute_sources(&by_source, &no_slaves, &bcachefs(members));
        assert_eq!(out.get("sda"), Some(&1_000_000));
        assert_eq!(out.get("sdb"), Some(&1_000_000));
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn bcachefs_subvolume_double_mount_not_double_counted() {
        // The same multi-device fs mounted twice (root + a subvolume, or a
        // colon-list mount plus a single-member remount) reports the same
        // fs-wide statfs usage under different sources. It must be counted
        // once, not summed — otherwise each disk reads 2× and can exceed
        // 100%.
        let members: &[&str] = &["sda", "sdb"];
        let by_source = sources(&[("/dev/sda:/dev/sdb", 2_000_000), ("/dev/sda", 2_000_000)]);
        let out = attribute_sources(&by_source, &no_slaves, &bcachefs(members));
        assert_eq!(out.get("sda"), Some(&1_000_000));
        assert_eq!(out.get("sdb"), Some(&1_000_000));
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn independent_filesystems_on_shared_members_sum() {
        // Two LVs on one VG spanning two PVs: `slaves` resolves both to the
        // same member set, but they are independent filesystems. v0.1.3
        // grouped by member set and took the max, silently dropping the
        // smaller LV — 1.5 TB of usage reported as 1.0 TB.
        let slaves = |dev: &str| -> Option<Vec<String>> {
            dev.starts_with("/dev/mapper/vg-")
                .then(|| vec!["sda".into(), "sdb".into()])
        };
        let by_source = sources(&[
            ("/dev/mapper/vg-lv1", 1_000_000),
            ("/dev/mapper/vg-lv2", 500_000),
        ]);
        let out = attribute_sources(&by_source, &slaves, &no_bcachefs);
        assert_eq!(out.get("sda"), Some(&750_000));
        assert_eq!(out.get("sdb"), Some(&750_000));
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn bcachefs_and_an_unrelated_fs_on_one_member_both_land() {
        // A disk that is both a bcachefs member and hosts a separate
        // partition: the bcachefs share and the partition's own usage add
        // up on that disk rather than one displacing the other.
        let members: &[&str] = &["sda", "sdb"];
        let by_source = sources(&[("/dev/sda:/dev/sdb", 2_000_000), ("/dev/sdc1", 400)]);
        let out = attribute_sources(&by_source, &no_slaves, &bcachefs(members));
        assert_eq!(out.get("sda"), Some(&1_000_000));
        assert_eq!(out.get("sdc"), Some(&400));
        assert_eq!(out.len(), 3);
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn classify_by_name(name: &str) -> DeviceKind {
    if name.starts_with("nvme") {
        DeviceKind::Nvme
    } else if name.starts_with("sd") {
        DeviceKind::Ssd
    } else {
        DeviceKind::Unknown
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn bus_hint(k: &DeviceKind) -> String {
    match k {
        DeviceKind::Nvme => "PCIe".to_string(),
        DeviceKind::Ssd => "SATA / Internal".to_string(),
        DeviceKind::Hdd => "SATA".to_string(),
        DeviceKind::UsbMassStorage => "USB".to_string(),
        DeviceKind::Unknown => "—".to_string(),
    }
}
