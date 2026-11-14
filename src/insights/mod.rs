//! Insights engine — pure functions over collected ticks.
//!
//! Each rule reads the latest snapshots and returns `Option<Insight>`.
//! Severity choices follow the BRIEF: CRIT for imminent failure, WARN
//! for trending toward bad, INFO for noteworthy state.

use crate::collect::hot_files::HotFileWatcher;
use crate::collect::lvm::LvmVg;
use crate::collect::{DeviceTick, FsTick, IoTick, SmartCollector, VolumeTick};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Crit,
    Warn,
    Info,
}

impl Severity {
    pub fn label(&self) -> &'static str {
        match self {
            Severity::Crit => "CRIT",
            Severity::Warn => "WARN",
            Severity::Info => "INFO",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Insight {
    pub sev: Severity,
    pub title: String,
    pub body: Vec<String>,
    pub suggested_tab: &'static str,
}

pub fn evaluate(
    devices: &[DeviceTick],
    filesystems: &[FsTick],
    io: &[IoTick],
    smart: &SmartCollector,
    hot_files: &HotFileWatcher,
    volumes: &VolumeTick,
    not_responding: &[std::path::PathBuf],
) -> Vec<Insight> {
    let mut out = Vec::new();
    if let Some(i) = mounts_not_responding(not_responding) {
        out.push(i);
    }
    if let Some(i) = lvm_unhealthy(&volumes.lvm) {
        out.push(i);
    }
    if let Some(i) = lvm_space_running_out(&volumes.lvm) {
        out.push(i);
    }
    if let Some(i) = capacity_critical(filesystems) {
        out.push(i);
    }
    if let Some(i) = capacity_warning(filesystems) {
        out.push(i);
    }
    if let Some(i) = smart_failing(devices) {
        out.push(i);
    }
    if let Some(i) = nvme_wear_high(smart) {
        out.push(i);
    }
    if let Some(i) = nvme_spare_low(smart) {
        out.push(i);
    }
    if let Some(i) = drive_temperature_high(smart) {
        out.push(i);
    }
    if let Some(i) = io_dominant_device(io) {
        out.push(i);
    }
    if let Some(i) = io_latency_outlier(io) {
        out.push(i);
    }
    if let Some(i) = hot_file_runaway(hot_files) {
        out.push(i);
    }
    if let Some(i) = removable_present(devices) {
        out.push(i);
    }
    if out.is_empty() {
        out.push(all_clear(devices, smart));
    }
    // Stable ordering: crit > warn > info.
    out.sort_by_key(|i| match i.sev {
        Severity::Crit => 0,
        Severity::Warn => 1,
        Severity::Info => 2,
    });
    out
}

/// A mount that didn't answer is left out of every list, so say so rather
/// than let it look as if it isn't there.
fn mounts_not_responding(mounts: &[std::path::PathBuf]) -> Option<Insight> {
    if mounts.is_empty() {
        return None;
    }
    let mut body = vec![format!(
        "{} mount{} didn't answer within a second, so {} size is unknown.",
        mounts.len(),
        if mounts.len() == 1 { "" } else { "s" },
        if mounts.len() == 1 { "its" } else { "their" }
    )];
    for m in mounts.iter().take(3) {
        body.push(format!("  {}", m.display()));
    }
    body.push("diskwatch keeps running and asks again on the next refresh.".into());
    Some(Insight {
        sev: Severity::Warn,
        title: "Mount not responding".into(),
        body,
        suggested_tab: "fs",
    })
}

fn capacity_critical(fs: &[FsTick]) -> Option<Insight> {
    let crit: Vec<&FsTick> = fs
        .iter()
        .filter(|m| !m.ignored && m.size_bytes > 0 && used_pct(m) >= 90)
        .collect();
    if crit.is_empty() {
        return None;
    }
    let mut body = vec![format!(
        "{} mount{} above 90% capacity.",
        crit.len(),
        if crit.len() == 1 { "" } else { "s" }
    )];
    for m in crit.iter().take(3) {
        body.push(format!("  {} — {}% used", m.mount, used_pct(m)));
    }
    Some(Insight {
        sev: Severity::Crit,
        title: "Filesystem critically full".into(),
        body,
        suggested_tab: "fs",
    })
}

fn capacity_warning(fs: &[FsTick]) -> Option<Insight> {
    let warn: Vec<&FsTick> = fs
        .iter()
        .filter(|m| {
            !m.ignored && m.size_bytes > 0 && {
                let p = used_pct(m);
                (80..90).contains(&p)
            }
        })
        .collect();
    if warn.is_empty() {
        return None;
    }
    let mut body = vec![format!(
        "{} mount{} trending toward full (80–89%).",
        warn.len(),
        if warn.len() == 1 { "" } else { "s" }
    )];
    for m in warn.iter().take(3) {
        body.push(format!("  {} — {}% used", m.mount, used_pct(m)));
    }
    Some(Insight {
        sev: Severity::Warn,
        title: "Filesystem nearing capacity".into(),
        body,
        suggested_tab: "fs",
    })
}

/// A VG missing a PV, or an LV lvm2 reports unhealthy. CRIT where data is
/// unreachable or a write will fail; WARN where it still works but needs
/// a hand (`lvchange --refresh`, a scrub that found mismatches).
fn lvm_unhealthy(vgs: &[LvmVg]) -> Option<Insight> {
    let mut sev = Severity::Warn;
    let mut body = Vec::new();
    for vg in vgs {
        if vg.is_partial() {
            sev = Severity::Crit;
            let n = vg.missing_pvs.max(1);
            body.push(format!(
                "  VG {} \u{2014} {} PV{} missing",
                vg.name,
                n,
                if n == 1 { "" } else { "s" }
            ));
        }
        for lv in &vg.lvs {
            let what = if lv.is_invalid_snapshot() {
                "snapshot overflowed and was invalidated"
            } else if lv.health.is_empty() {
                continue;
            } else {
                lv.health.as_str()
            };
            if lv.is_invalid_snapshot()
                || matches!(
                    lv.health.as_str(),
                    "partial" | "failed" | "out of data" | "metadata read only"
                )
            {
                sev = Severity::Crit;
            }
            body.push(format!("  {}/{} \u{2014} {}", vg.name, lv.name, what));
        }
    }
    if body.is_empty() {
        return None;
    }
    body.truncate(4);
    Some(Insight {
        sev,
        title: "LVM volume unhealthy".into(),
        body,
        suggested_tab: "volumes",
    })
}

/// Thin pools and classic snapshots that are filling. Neither degrades
/// gracefully: a full thin pool stops all writes to every thin volume in
/// it, and a full snapshot is invalidated.
fn lvm_space_running_out(vgs: &[LvmVg]) -> Option<Insight> {
    let mut hits: Vec<(f32, String)> = Vec::new();
    for vg in vgs {
        for lv in &vg.lvs {
            if lv.is_invalid_snapshot() {
                continue; // already reported as unhealthy
            }
            let line = if lv.is_thin_pool() {
                let (d, m) = (
                    lv.data_percent.unwrap_or(0.0),
                    lv.metadata_percent.unwrap_or(0.0),
                );
                let worst = d.max(m);
                if worst < 80.0 {
                    continue;
                }
                (
                    worst,
                    format!(
                        "  thin pool {}/{} \u{2014} data {d:.0}%, metadata {m:.0}%",
                        vg.name, lv.name
                    ),
                )
            } else if lv.is_snapshot() {
                let d = lv.data_percent.unwrap_or(0.0);
                if d < 80.0 {
                    continue;
                }
                (
                    d,
                    format!("  snapshot {}/{} \u{2014} {d:.0}% full", vg.name, lv.name),
                )
            } else {
                continue;
            };
            hits.push(line);
        }
    }
    if hits.is_empty() {
        return None;
    }
    hits.sort_by(|a, b| b.0.total_cmp(&a.0));
    let sev = if hits[0].0 >= 95.0 {
        Severity::Crit
    } else {
        Severity::Warn
    };
    let mut body = vec!["Writes stop when a thin pool fills; a full snapshot is dropped.".into()];
    body.extend(hits.into_iter().take(3).map(|(_, l)| l));
    Some(Insight {
        sev,
        title: "LVM space running out".into(),
        body,
        suggested_tab: "volumes",
    })
}

fn smart_failing(devices: &[DeviceTick]) -> Option<Insight> {
    let bad: Vec<&DeviceTick> = devices
        .iter()
        .filter(|d| matches!(d.smart_ok, Some(false)))
        .collect();
    if bad.is_empty() {
        return None;
    }
    let body = bad
        .iter()
        .map(|d| format!("  {} ({}) — SMART status FAILING", d.name, d.model))
        .collect();
    Some(Insight {
        sev: Severity::Crit,
        title: "Drive reporting SMART failure".into(),
        body,
        suggested_tab: "smart",
    })
}

fn nvme_wear_high(smart: &SmartCollector) -> Option<Insight> {
    let bad: Vec<(&String, u8)> = smart
        .by_device
        .iter()
        .filter_map(|(name, t)| t.percentage_used.map(|p| (name, p)))
        .filter(|(_, p)| *p >= 80)
        .collect();
    if bad.is_empty() {
        return None;
    }
    let body = bad
        .iter()
        .map(|(n, p)| format!("  {} — {}% of write-endurance budget used", n, p))
        .collect();
    Some(Insight {
        sev: Severity::Warn,
        title: "NVMe wear approaching rated limit".into(),
        body,
        suggested_tab: "smart",
    })
}

fn nvme_spare_low(smart: &SmartCollector) -> Option<Insight> {
    let bad: Vec<(&String, u8)> = smart
        .by_device
        .iter()
        .filter_map(|(name, t)| t.available_spare.map(|p| (name, p)))
        .filter(|(_, p)| *p <= 10)
        .collect();
    if bad.is_empty() {
        return None;
    }
    let body = bad
        .iter()
        .map(|(n, p)| format!("  {} — only {}% spare blocks left", n, p))
        .collect();
    Some(Insight {
        sev: Severity::Crit,
        title: "NVMe spare-block reserve nearly exhausted".into(),
        body,
        suggested_tab: "smart",
    })
}

fn drive_temperature_high(smart: &SmartCollector) -> Option<Insight> {
    let bad: Vec<(&String, i16)> = smart
        .by_device
        .iter()
        .filter_map(|(name, t)| t.temperature_c.map(|c| (name, c)))
        .filter(|(_, c)| *c >= 70)
        .collect();
    if bad.is_empty() {
        return None;
    }
    let body = bad
        .iter()
        .map(|(n, c)| format!("  {} — {}°C  (warning > 70°C)", n, c))
        .collect();
    Some(Insight {
        sev: Severity::Warn,
        title: "Drive running hot".into(),
        body,
        suggested_tab: "smart",
    })
}

/// What to say when nothing fired. "All systems nominal" only when SMART
/// was read for every disk: without root it isn't, and saying nominal beside
/// a HEALTH card reading "unknown" claimed a check that never ran.
fn all_clear(devices: &[DeviceTick], smart: &SmartCollector) -> Insight {
    let unread: Vec<&DeviceTick> = devices
        .iter()
        .filter(|d| !crate::collect::devices::is_memory_backed(&d.name))
        .filter(|d| !smart_was_read(d, smart))
        .collect();
    if unread.is_empty() {
        return Insight {
            sev: Severity::Info,
            title: "All systems nominal".into(),
            body: vec![
                "No filesystems above 80% capacity.".into(),
                "No SMART warnings on attached devices.".into(),
            ],
            suggested_tab: "",
        };
    }
    let names: Vec<&str> = unread.iter().map(|d| d.name.as_str()).collect();
    let why = if unread.iter().any(|d| smart.needs_root(&d.name)) {
        "smartctl needs root (sudo diskwatch)"
    } else if !smart.smartctl_available() && unread.iter().all(|d| d.smart_ok.is_none()) {
        "smartctl isn't installed"
    } else {
        "no SMART data came back"
    };
    Insight {
        sev: Severity::Info,
        title: "No problems found, but SMART wasn't read".into(),
        body: vec![
            "No filesystems above 80% capacity.".into(),
            format!("No SMART reading for {}: {why}.", names.join(", ")),
        ],
        suggested_tab: "smart",
    }
}

/// Whether diskwatch has any SMART reading for `d`: a health verdict (macOS,
/// Windows) or smartctl data (Linux).
fn smart_was_read(d: &DeviceTick, smart: &SmartCollector) -> bool {
    d.smart_ok.is_some()
        || smart.by_device.get(&d.name).is_some_and(|t| {
            !t.needs_root
                && (t.temperature_c.is_some()
                    || t.power_on_hours.is_some()
                    || t.percentage_used.is_some()
                    || !t.ata_attrs.is_empty())
        })
}

fn io_dominant_device(io: &[IoTick]) -> Option<Insight> {
    // Physical disks only. zram is swap, which is memory traffic, and a
    // stacked device (dm, md) carries the same blocks as the disk under it,
    // so either one made a single disk look dominant. With one disk left
    // there's nothing for it to dominate.
    let disks: Vec<&IoTick> = io
        .iter()
        .filter(|t| crate::collect::io::is_summed(&t.device))
        .collect();
    if disks.len() < 2 {
        return None;
    }
    let total: f64 = disks.iter().map(|t| t.bps).sum();
    if total < 1_000_000.0 {
        return None; // Don't fire on idle hosts.
    }
    let dominant = disks.iter().max_by(|a, b| {
        a.bps
            .partial_cmp(&b.bps)
            .unwrap_or(std::cmp::Ordering::Equal)
    })?;
    let share = dominant.bps / total;
    if share < 0.80 {
        return None;
    }
    let mb = dominant.bps / 1_000_000.0;
    Some(Insight {
        sev: Severity::Info,
        title: format!("{} is dominating disk IO", dominant.device),
        body: vec![format!(
            "{:.1} MB/s — {:.0}% of IO across the disks",
            mb,
            share * 100.0
        )],
        suggested_tab: "io",
    })
}

fn io_latency_outlier(io: &[IoTick]) -> Option<Insight> {
    // Fires when any device's tick-averaged p99 exceeds 10ms, which is
    // already 50× a fast-NVMe steady state. Real per-op p99 requires
    // eBPF / IOReport — see io collector docs.
    let mut bad: Vec<(&str, f64, f64)> = Vec::new();
    for t in io {
        let Some(pct) = t.latency_pct else { continue };
        if pct.p99_r >= 10_000.0 || pct.p99_w >= 10_000.0 {
            bad.push((t.device.as_str(), pct.p99_r, pct.p99_w));
        }
    }
    if bad.is_empty() {
        return None;
    }
    let worst = bad
        .iter()
        .map(|(_, r, w)| r.max(*w))
        .fold(0.0_f64, f64::max);
    let sev = if worst >= 100_000.0 {
        Severity::Crit
    } else {
        Severity::Warn
    };
    let mut body = vec![format!(
        "{} device{} with p99 latency >10ms (60s window).",
        bad.len(),
        if bad.len() == 1 { "" } else { "s" }
    )];
    for (n, r, w) in bad.iter().take(3) {
        body.push(format!(
            "  {} — read p99 {:.1}ms  write p99 {:.1}ms",
            n,
            r / 1_000.0,
            w / 1_000.0
        ));
    }
    Some(Insight {
        sev,
        title: "Disk p99 latency elevated".into(),
        body,
        suggested_tab: "io",
    })
}

fn hot_file_runaway(watcher: &HotFileWatcher) -> Option<Insight> {
    let top = watcher.top(3);
    let runaway: Vec<_> = top.iter().filter(|f| f.events_per_sec >= 20.0).collect();
    if runaway.is_empty() {
        return None;
    }
    let mut body = vec![format!(
        "{} path{} exceeding 20 events/sec.",
        runaway.len(),
        if runaway.len() == 1 { "" } else { "s" }
    )];
    for f in runaway.iter().take(3) {
        body.push(format!(
            "  {} — {:.1} ev/s  ({} total)",
            f.path.display(),
            f.events_per_sec,
            f.total_events
        ));
    }
    Some(Insight {
        sev: Severity::Info,
        title: "File-modification rate spike".into(),
        body,
        suggested_tab: "hot",
    })
}

fn removable_present(devices: &[DeviceTick]) -> Option<Insight> {
    let r: Vec<&DeviceTick> = devices.iter().filter(|d| d.is_removable).collect();
    if r.is_empty() {
        return None;
    }
    let body = r
        .iter()
        .map(|d| {
            if d.bus.is_empty() {
                format!("  {} — {}", d.name, d.model)
            } else {
                format!("  {} — {} ({})", d.name, d.model, d.bus)
            }
        })
        .collect();
    Some(Insight {
        sev: Severity::Info,
        title: format!(
            "{} removable drive{} attached",
            r.len(),
            if r.len() == 1 { "" } else { "s" }
        ),
        body,
        suggested_tab: "devices",
    })
}

fn used_pct(m: &FsTick) -> u32 {
    if m.size_bytes == 0 {
        return 0;
    }
    (m.used_bytes as f64 / m.size_bytes as f64 * 100.0).round() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn io(device: &str, bps: f64) -> IoTick {
        IoTick {
            device: device.into(),
            bps,
            ..Default::default()
        }
    }

    #[test]
    fn dominating_needs_two_physical_disks() {
        // The plan review's two misreads: zram, and a one-disk host.
        assert!(io_dominant_device(&[io("zram0", 158.8e6), io("nvme0n1", 2e6)]).is_none());
        assert!(io_dominant_device(&[io("nvme0n1", 17.5e6)]).is_none());
        // LUKS: dm-0 carries the same blocks as the disk under it.
        assert!(io_dominant_device(&[io("nvme0n1", 20e6), io("dm-0", 20e6)]).is_none());
        let two = io_dominant_device(&[io("nvme0n1", 90e6), io("sda", 5e6)]).unwrap();
        assert_eq!(two.title, "nvme0n1 is dominating disk IO");
    }

    fn device(name: &str, smart_ok: Option<bool>) -> DeviceTick {
        DeviceTick {
            name: name.into(),
            kind: crate::collect::devices::DeviceKind::Nvme,
            model: String::new(),
            bus: String::new(),
            size_bytes: 0,
            used_bytes: 0,
            is_removable: false,
            firmware: None,
            serial: None,
            smart_ok,
            idle: false,
        }
    }

    #[test]
    fn nominal_only_when_smart_was_read() {
        let mut smart = SmartCollector::new();
        smart.by_device.insert(
            "nvme0n1".into(),
            crate::collect::smart::SmartTick {
                needs_root: true,
                ..Default::default()
            },
        );
        let devices = [device("nvme0n1", None), device("zram0", None)];
        let refused = all_clear(&devices, &smart);
        assert_ne!(refused.title, "All systems nominal");
        assert!(refused.body[1].contains("nvme0n1"), "{:?}", refused.body);
        assert!(refused.body[1].contains("sudo"), "{:?}", refused.body);
        assert!(
            !refused.body[1].contains("zram0"),
            "zram has no SMART to read"
        );

        smart.by_device.insert(
            "nvme0n1".into(),
            crate::collect::smart::SmartTick {
                temperature_c: Some(41),
                ..Default::default()
            },
        );
        assert_eq!(all_clear(&devices, &smart).title, "All systems nominal");
        // macOS and Windows carry a verdict on the device instead.
        assert_eq!(
            all_clear(&[device("disk0", Some(true))], &SmartCollector::new()).title,
            "All systems nominal"
        );
    }

    #[test]
    fn a_mount_that_does_not_answer_is_named() {
        let stuck = [std::path::PathBuf::from("/mnt/NTFS1")];
        let i = mounts_not_responding(&stuck).expect("insight");
        assert_eq!(i.sev, Severity::Warn);
        assert!(
            i.body.iter().any(|l| l.contains("/mnt/NTFS1")),
            "{:?}",
            i.body
        );
        assert!(mounts_not_responding(&[]).is_none());
    }

    fn fs(mount: &str, pct: u64, ignored: bool) -> FsTick {
        FsTick {
            mount: mount.to_string(),
            device: "test".into(),
            fs_type: "test".into(),
            size_bytes: 100,
            used_bytes: pct,
            avail_bytes: 100 - pct,
            inode_pct: None,
            is_removable: false,
            is_system: false,
            ro_image: ignored,
            ignored,
        }
    }

    #[test]
    fn a_healthy_lvm_setup_raises_nothing() {
        let vgs = crate::collect::lvm::fixtures::vgs();
        assert!(lvm_unhealthy(&vgs).is_none());
        // The snapshot at 83% is worth a warning, the pool at 61% isn't.
        let i = lvm_space_running_out(&vgs).expect("snapshot warning");
        assert_eq!(i.sev, Severity::Warn);
        assert!(i.body.iter().any(|l| l.contains("snapshot data/home_snap")));
        assert!(!i.body.iter().any(|l| l.contains("thin pool data/")));
    }

    #[test]
    fn a_degraded_vg_is_critical() {
        let vgs = crate::collect::lvm::fixtures::degraded();
        let i = lvm_unhealthy(&vgs).expect("unhealthy");
        assert_eq!(i.sev, Severity::Crit);
        let body = i.body.join("\n");
        assert!(body.contains("VG data \u{2014} 1 PV missing"), "{body}");
        assert!(body.contains("data/mirror \u{2014} partial"), "{body}");
        assert!(
            body.contains("data/old_snap \u{2014} snapshot overflowed"),
            "{body}"
        );

        let i = lvm_space_running_out(&vgs).expect("pool nearly full");
        assert_eq!(i.sev, Severity::Crit, "96% data");
        assert!(
            !i.body.iter().any(|l| l.contains("old_snap")),
            "reported once"
        );
    }

    #[test]
    fn refresh_needed_is_a_warning() {
        let mut vgs = crate::collect::lvm::fixtures::vgs();
        vgs[0].lvs[2].health = "refresh needed".into();
        let i = lvm_unhealthy(&vgs).expect("unhealthy");
        assert_eq!(i.sev, Severity::Warn);
    }

    #[test]
    fn ignored_mounts_raise_no_capacity_alerts() {
        let mounts = [fs("/tmp/.mount_app", 100, true), fs("/mnt/old", 85, true)];
        assert!(capacity_critical(&mounts).is_none());
        assert!(capacity_warning(&mounts).is_none());
    }

    #[test]
    fn real_mounts_still_alert_beside_ignored_ones() {
        let mounts = [fs("/tmp/.mount_app", 100, true), fs("/data", 95, false)];
        let crit = capacity_critical(&mounts).expect("critical insight");
        assert!(crit.body.iter().any(|l| l.contains("/data")));
        assert!(!crit.body.iter().any(|l| l.contains(".mount_app")));
    }
}
