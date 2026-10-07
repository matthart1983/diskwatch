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
    /// smartctl was refused the device for lack of privilege, so every
    /// field above is empty because of permissions, not the drive.
    pub needs_root: bool,
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

    /// True when the last poll of `device` was refused for lack of root.
    pub fn needs_root(&self, device: &str) -> bool {
        self.by_device.get(device).is_some_and(|t| t.needs_root)
    }

    /// Render tests run where smartctl may not be installed.
    #[cfg(test)]
    pub fn assume_smartctl(&mut self) {
        self.have_smartctl = Some(true);
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
    Some(parse_smartctl(name, &v))
}

/// True when smartctl was refused the device for lack of privilege. As a
/// normal user it exits 2 with no data and says why in `smartctl.messages`:
/// "Smartctl open device: /dev/nvme0n1 failed: Permission denied" when the
/// node is root-only, or EPERM from the admin passthrough when it isn't.
fn refused(v: &serde_json::Value) -> bool {
    let Some(messages) = v.pointer("/smartctl/messages").and_then(|m| m.as_array()) else {
        return false;
    };
    messages.iter().any(|m| {
        m.get("string").and_then(|s| s.as_str()).is_some_and(|s| {
            s.contains("Permission denied") || s.contains("Operation not permitted")
        })
    })
}

/// Pull the headline fields out of `smartctl -A --json` output.
fn parse_smartctl(name: &str, v: &serde_json::Value) -> SmartTick {
    let mut tick = SmartTick {
        device: name.to_string(),
        needs_root: refused(v),
        ..Default::default()
    };

    // Top-level temperature summary that smartctl emits for both NVMe
    // (under nvme_smart_health_information_log) and ATA (as
    // `.temperature.current`). This is the value the SMART tab's
    // headline row and the Overview page's TEMP column render.
    if let Some(t) = v
        .get("temperature")
        .and_then(|x| x.get("current"))
        .and_then(|x| x.as_i64())
    {
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
        // Only override the top-level temperature parsed above when the
        // NVMe log actually carries the field, so a log missing it can't
        // clobber a good value with None.
        if let Some(t) = log.get("temperature").and_then(|x| x.as_i64()) {
            tick.temperature_c = Some(t as i16);
        }
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
            // Reallocated_Sector_Ct (0x05) is deliberately NOT lifted into
            // `percentage_used`: that field is the NVMe write-endurance
            // gauge (consumed by the `nvme_wear_high` insight at >=80%), and
            // a reallocated-sector *count* is an unrelated metric. Mapping
            // any non-zero count to 100% falsely flagged healthy ATA/SATA
            // drives — including spinning HDDs — as "worn out". The raw
            // count still appears in the attribute table below.
            match id as u8 {
                0x09 if tick.power_on_hours.is_none() => {
                    tick.power_on_hours = raw_int;
                }
                0x0C if tick.power_cycles.is_none() => {
                    tick.power_cycles = raw_int;
                }
                _ => {}
            }
        }
    }
    tick
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `smartctl -A --json /dev/nvme0n1` as a normal user, verbatim from
    /// smartctl 7.5 on Fedora 44.
    const REFUSED: &str = r#"{
  "json_format_version": [
    1,
    0
  ],
  "smartctl": {
    "version": [
      7,
      5
    ],
    "pre_release": false,
    "svn_revision": "5714",
    "platform_info": "x86_64-linux-7.1.13-200.fc44.x86_64",
    "build_info": "(local build)",
    "argv": [
      "smartctl",
      "-A",
      "--json",
      "/dev/nvme0n1"
    ],
    "messages": [
      {
        "string": "Smartctl open device: /dev/nvme0n1 failed: Permission denied",
        "severity": "error"
      }
    ],
    "exit_status": 2
  },
  "local_time": {
    "time_t": 1791358805,
    "asctime": "Wed Oct  7 18:40:05 2026 AEDT"
  }
}"#;

    /// The same query as root, trimmed to the fields diskwatch reads.
    const NVME: &str = r#"{
  "smartctl": { "version": [7, 4], "exit_status": 0 },
  "device": { "name": "/dev/nvme0n1", "type": "nvme", "protocol": "NVMe" },
  "nvme_smart_health_information_log": {
    "critical_warning": 0,
    "temperature": 41,
    "available_spare": 100,
    "percentage_used": 2,
    "data_units_read": 19446553,
    "data_units_written": 25771349,
    "power_cycles": 412,
    "power_on_hours": 3150
  },
  "temperature": { "current": 41 },
  "power_cycle_count": 412,
  "power_on_time": { "hours": 3150 }
}"#;

    fn parse(text: &str) -> SmartTick {
        parse_smartctl("nvme0n1", &serde_json::from_str(text).unwrap())
    }

    #[test]
    fn a_permission_refusal_is_told_apart_from_no_data() {
        // Issue #25: without sudo every SMART field read "—" with nothing
        // to say that root was the reason.
        let t = parse(REFUSED);
        assert!(t.needs_root);
        assert_eq!(t.temperature_c, None);
        assert_eq!(t.power_on_hours, None);
    }

    #[test]
    fn a_successful_read_does_not_need_root() {
        let t = parse(NVME);
        assert!(!t.needs_root);
        assert_eq!(t.temperature_c, Some(41));
        assert_eq!(t.percentage_used, Some(2));
        assert_eq!(t.power_on_hours, Some(3150));
    }

    #[test]
    fn other_failures_are_not_blamed_on_root() {
        let t = parse(
            r#"{"smartctl": {"messages": [{"string": "/dev/zram0: Unable to detect device type", "severity": "error"}], "exit_status": 1}}"#,
        );
        assert!(!t.needs_root);
    }
}
