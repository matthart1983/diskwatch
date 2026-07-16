//! SMART attribute collector via the `smartctl` binary.
//!
//! If `smartctl` is on PATH (`brew install smartmontools` on macOS),
//! `smartctl -A --json <device>` returns NVMe SMART data as JSON. We
//! parse the headline fields: temperature, power-on hours, power cycles,
//! and the NVMe-specific data points (percentage_used, available_spare,
//! data_units_*).
//!
//! When smartctl is absent the tab falls back to whatever each platform
//! exposes through cheaper paths (diskutil "SMART Status: Verified" on
//! macOS, already wired into `DeviceTick.smart_ok`).

use std::collections::HashMap;
use std::process::Command;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Default)]
pub struct SmartTick {
    /// Reserved for cross-device diff views — not read in the per-device
    /// SMART panel that looks up its tick by key.
    #[allow(dead_code)]
    pub device: String,
    pub temperature_c: Option<i16>,
    pub power_on_hours: Option<u64>,
    pub power_cycles: Option<u64>,
    pub percentage_used: Option<u8>,
    pub available_spare: Option<u8>,
    pub data_units_read: Option<u64>,
    pub data_units_written: Option<u64>,
    /// Free-form attributes for ATA drives — name → (raw, value).
    pub ata_attrs: Vec<AtaAttr>,
}

#[derive(Debug, Clone, Default)]
pub struct AtaAttr {
    pub id: u8,
    pub name: String,
    pub value: u32,
    pub worst: u32,
    pub thresh: Option<u32>,
    pub raw: String,
}

pub struct SmartCollector {
    /// `None` until first probe; `Some(false)` if probe failed.
    have_smartctl: Option<bool>,
    pub by_device: HashMap<String, SmartTick>,
    last_refresh: Instant,
    /// Configurable poll interval. Defaults to 5 minutes; lowered by the
    /// `+` / `-` keys in the TUI for live temperature monitoring.
    interval: Duration,
    /// Time of the most recent successful refresh — exposed so the SMART
    /// tab can render a "next refresh in Ns" countdown without re-querying
    /// the collector's internals.
    pub last_refresh_at: Option<Instant>,
}

impl SmartCollector {
    pub fn new() -> Self {
        Self {
            have_smartctl: None,
            by_device: HashMap::new(),
            last_refresh: Instant::now() - Duration::from_secs(3600),
            interval: Duration::from_secs(300),
            last_refresh_at: None,
        }
    }

    pub fn smartctl_available(&self) -> bool {
        matches!(self.have_smartctl, Some(true))
    }

    pub fn current_interval(&self) -> Duration {
        self.interval
    }

    pub fn set_interval(&mut self, d: Duration) {
        self.interval = d;
    }

    /// Seconds until the next automatic refresh is due. Returns 0 when
    /// the interval has already elapsed (i.e. the next tick will refresh).
    pub fn secs_until_next_refresh(&self) -> u64 {
        let elapsed = self.last_refresh.elapsed();
        if elapsed >= self.interval {
            0
        } else {
            (self.interval - elapsed).as_secs()
        }
    }

    /// Bypass the cadence gate and refresh every device immediately.
    /// Used by the `r` hotkey in the TUI.
    pub fn force_refresh(&mut self, devices: &[crate::collect::DeviceTick]) {
        if self.have_smartctl.is_none() {
            self.have_smartctl = Some(probe_smartctl());
        }
        if !matches!(self.have_smartctl, Some(true)) {
            return;
        }
        self.last_refresh = Instant::now();
        self.last_refresh_at = Some(self.last_refresh);
        for d in devices {
            if let Some(tick) = query_device(&d.name) {
                self.by_device.insert(d.name.clone(), tick);
            }
        }
    }

    /// Called periodically (default 5 min, lowered for live monitoring).
    /// Refreshes SMART data for every device in the list when the
    /// configured interval has elapsed.
    pub fn refresh_if_due(&mut self, devices: &[crate::collect::DeviceTick]) {
        if self.have_smartctl.is_none() {
            self.have_smartctl = Some(probe_smartctl());
        }
        if !matches!(self.have_smartctl, Some(true)) {
            return;
        }
        if self.last_refresh.elapsed() < self.interval {
            return;
        }
        self.last_refresh = Instant::now();
        self.last_refresh_at = Some(self.last_refresh);
        for d in devices {
            if let Some(tick) = query_device(&d.name) {
                self.by_device.insert(d.name.clone(), tick);
            }
        }
    }
}

fn probe_smartctl() -> bool {
    Command::new("smartctl")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    // We deliberately don't print to stderr on failure — the SMART tab
    // surfaces the missing-binary state via its own banner.
}

fn query_device(name: &str) -> Option<SmartTick> {
    let dev = format!("/dev/{}", name);
    let out = Command::new("smartctl")
        .args(["-A", "--json", &dev])
        .output()
        .ok()?;
    // smartctl returns nonzero exit on warning-class issues but still
    // emits valid JSON; parse the output regardless of status.
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;

    let mut tick = SmartTick {
        device: name.to_string(),
        ..Default::default()
    };

    // Top-level temperature summary that smartctl emits for both NVMe
    // (under nvme_smart_health_information_log) and ATA (as
    // `.temperature.current`). This is the value the SMART tab's
    // headline row and the Overview page's TEMP column render.
    if let Some(t) = v.get("temperature").and_then(|x| x.get("current")).and_then(|x| x.as_i64()) {
        tick.temperature_c = Some(t as i16);
    }
    if let Some(t) = v.get("temperature").and_then(|x| x.as_i64()) {
        // Some smartctl versions (notably 7.4) emit just `.temperature`
        // as a bare integer rather than nested `.temperature.current`.
        if tick.temperature_c.is_none() {
            tick.temperature_c = Some(t as i16);
        }
    }

    // NVMe path.
    if let Some(log) = v.get("nvme_smart_health_information_log") {
        tick.temperature_c = log
            .get("temperature")
            .and_then(|x| x.as_i64())
            .map(|n| n as i16);
        tick.power_on_hours = log.get("power_on_hours").and_then(|x| x.as_u64());
        tick.power_cycles = log.get("power_cycles").and_then(|x| x.as_u64());
        tick.percentage_used = log
            .get("percentage_used")
            .and_then(|x| x.as_u64())
            .map(|n| n as u8);
        tick.available_spare = log
            .get("available_spare")
            .and_then(|x| x.as_u64())
            .map(|n| n as u8);
        tick.data_units_read = log.get("data_units_read").and_then(|x| x.as_u64());
        tick.data_units_written = log.get("data_units_written").and_then(|x| x.as_u64());
    }

    // ATA / SATA path.
    if let Some(attrs) = v
        .get("ata_smart_attributes")
        .and_then(|x| x.get("table"))
        .and_then(|x| x.as_array())
    {
        for a in attrs {
            let Some(id) = a.get("id").and_then(|x| x.as_u64()) else {
                continue;
            };
            let name = a
                .get("name")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let value = a.get("value").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
            let worst = a.get("worst").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
            let thresh = a.get("thresh").and_then(|x| x.as_u64()).map(|n| n as u32);
            let raw = a
                .get("raw")
                .and_then(|x| x.get("string"))
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            tick.ata_attrs.push(AtaAttr {
                id: id as u8,
                name,
                value,
                worst,
                thresh,
                raw,
            });

            // Lift a few headline attributes into SmartTick so the
            // Summary block at the top of the SMART panel has the same
            // fields populated for ATA drives as for NVMe. The raw
            // value for these attributes is always an integer (for the
            // ones we lift), parsed via `raw` as string — we look at the
            // first numeric token instead, which sidesteps ATA's
            // tradition of suffixing raw values with units ("33 (Min/Max
            // 33/33)" etc.).
            let raw_int: Option<u64> = a
                .get("raw")
                .and_then(|x| x.get("value"))
                .and_then(|x| x.as_u64());
            match id as u8 {
                0x09 if tick.power_on_hours.is_none() => {
                    tick.power_on_hours = raw_int;
                }
                0x0C if tick.power_cycles.is_none() => {
                    tick.power_cycles = raw_int;
                }
                0x05 => {
                    // Reallocated_Sector_Ct — surface as a "wear"
                    // proxy (any non-zero count is a bad sign; the
                    // SMART tab's existing table renders the raw value
                    // separately). We don't override NVMe's
                    // percentage_used.
                    tick.percentage_used = if let Some(v) = raw_int {
                        if v == 0 { tick.percentage_used } else { Some(100) }
                    } else {
                        tick.percentage_used
                    };
                }
                _ => {}
            }
        }
    }
    Some(tick)
}
