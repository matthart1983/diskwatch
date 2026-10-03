//! Insights engine — pure functions over collected ticks.
//!
//! Each rule reads the latest snapshots and returns `Option<Insight>`.
//! Severity choices follow the BRIEF: CRIT for imminent failure, WARN
//! for trending toward bad, INFO for noteworthy state.

use crate::collect::hot_files::HotFileWatcher;
use crate::collect::{DeviceTick, FsTick, IoTick, SmartCollector};

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
) -> Vec<Insight> {
    let mut out = Vec::new();
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
        out.push(Insight {
            sev: Severity::Info,
            title: "All systems nominal".into(),
            body: vec![
                "No filesystems above 80% capacity.".into(),
                "No SMART warnings on attached devices.".into(),
            ],
            suggested_tab: "",
        });
    }
    // Stable ordering: crit > warn > info.
    out.sort_by_key(|i| match i.sev {
        Severity::Crit => 0,
        Severity::Warn => 1,
        Severity::Info => 2,
    });
    out
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

fn io_dominant_device(io: &[IoTick]) -> Option<Insight> {
    let total: f64 = io.iter().map(|t| t.bps).sum();
    if total < 1_000_000.0 {
        return None; // Don't fire on idle hosts.
    }
    let dominant = io.iter().max_by(|a, b| {
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
            "{:.1} MB/s — {:.0}% of total host IO",
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
