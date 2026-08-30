//! Linux device enumeration via sysfs.
//!
//! Reads `/sys/block/<dev>/` for every whole-disk entry:
//! - `size` — number of 512-byte sectors (kernel API constant).
//! - `removable` — 0/1.
//! - `ro` — 0/1.
//! - `queue/rotational` — 1 = HDD, 0 = SSD / NVMe.
//! - `device/model`, `device/serial`, `device/firmware_rev`,
//!   `device/vendor` — populated when the kernel sysfs node exists
//!   (NVMe and SATA both expose these, USB mass storage usually does).
//!
//! SMART status from `smartctl` is consumed cross-platform by the
//! existing `collect::smart` collector — no Linux-specific work here.
//!
//! Note: this file is only compiled on Linux. The cfg gate lives at the
//! module declaration in `collect/mod.rs`.

use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Default)]
pub struct LinuxDevice {
    pub name: String,
    pub kind: LinuxKind,
    pub model: String,
    pub firmware: Option<String>,
    pub serial: Option<String>,
    pub size_bytes: u64,
    pub removable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LinuxKind {
    Nvme,
    Ssd,
    Hdd,
    UsbMassStorage,
    #[default]
    Unknown,
}

pub fn collect() -> Vec<LinuxDevice> {
    let Ok(entries) = fs::read_dir("/sys/block") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // Skip loopback / ram / device-mapper / md (md handled by
        // volumes collector).
        if name.starts_with("loop")
            || name.starts_with("ram")
            || name.starts_with("dm-")
            || name.starts_with("md")
            || name.starts_with("zd")
        // ZFS zvols
        {
            continue;
        }
        let path = entry.path();
        let device = parse_block(&path, &name);
        out.push(device);
    }
    // Largest first, matching the macOS ordering.
    out.sort_by_key(|d| std::cmp::Reverse(d.size_bytes));
    out
}

fn parse_block(base: &Path, name: &str) -> LinuxDevice {
    const SECTOR_BYTES: u64 = 512;
    let size_sectors = read_u64(&base.join("size")).unwrap_or(0);
    let removable = read_u64(&base.join("removable")).unwrap_or(0) == 1;
    let rotational = read_u64(&base.join("queue/rotational")).unwrap_or(0) == 1;
    let model = read_trim(&base.join("device/model")).unwrap_or_default();
    let vendor = read_trim(&base.join("device/vendor")).unwrap_or_default();
    let firmware = read_trim(&base.join("device/firmware_rev"));
    let serial = read_trim(&base.join("device/serial")).or_else(|| {
        // NVMe namespaces expose the controller serial via the parent.
        if name.starts_with("nvme") {
            // /sys/block/nvme0n1/device → /sys/class/nvme/nvme0
            read_trim(&base.join("device/serial"))
        } else {
            None
        }
    });

    // Compose a model string that includes the vendor when both are
    // present and the vendor isn't already a prefix of the model.
    let composed_model = if !vendor.is_empty()
        && !model
            .to_ascii_lowercase()
            .starts_with(&vendor.to_ascii_lowercase())
    {
        format!("{} {}", vendor, model)
    } else if model.is_empty() {
        "—".to_string()
    } else {
        model.clone()
    };

    let kind = if name.starts_with("nvme") {
        LinuxKind::Nvme
    } else if removable {
        LinuxKind::UsbMassStorage
    } else if rotational {
        LinuxKind::Hdd
    } else if !model.is_empty() {
        LinuxKind::Ssd
    } else {
        LinuxKind::Unknown
    };

    LinuxDevice {
        name: name.to_string(),
        kind,
        model: composed_model,
        firmware,
        serial,
        size_bytes: size_sectors.saturating_mul(SECTOR_BYTES),
        removable,
    }
}

fn read_trim(path: &Path) -> Option<String> {
    let s = fs::read_to_string(path).ok()?;
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

fn read_u64(path: &Path) -> Option<u64> {
    let s = fs::read_to_string(path).ok()?;
    s.trim().parse().ok()
}
