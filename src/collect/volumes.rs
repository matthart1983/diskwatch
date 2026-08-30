//! Volumes collector — APFS containers (macOS), parsed from
//! `diskutil apfs list` text output.
//!
//! Linux mdraid / ZFS / LVM collection is deferred. On non-macOS this
//! returns an empty list; the tab renders an explanatory banner.

#[derive(Debug, Clone, Default)]
pub struct VolumeTick {
    pub containers: Vec<ApfsContainer>,
    pub mdraid: Vec<MdRaidArray>,
}

#[derive(Debug, Clone, Default)]
pub struct MdRaidArray {
    pub name: String,  // "md0"
    pub level: String, // "raid10", "raid1", ...
    pub state: String, // "active", "inactive", ...
    pub size_bytes: u64,
    /// "[4/4]" — total / present.
    pub members_total: u32,
    pub members_present: u32,
    /// "[UUUU]" — one char per slot. 'U' = up.
    pub member_state: String,
    /// "sda1[0]", "sdb1[1]", …
    pub members: Vec<MdRaidMember>,
    /// In-progress resync/recovery: (operation, percent, eta).
    pub progress: Option<MdRaidProgress>,
}

#[derive(Debug, Clone, Default)]
pub struct MdRaidMember {
    pub device: String,
    pub index: u32,
    pub flag: Option<String>, // "(F)" failed, "(S)" spare, "(W)" write-mostly
}

#[derive(Debug, Clone, Default)]
pub struct MdRaidProgress {
    pub op: String,
    pub percent: f32,
    pub eta: String,
    pub speed: String,
}

#[derive(Debug, Clone, Default)]
pub struct ApfsContainer {
    pub bsd: String,
    pub size_bytes: u64,
    pub used_bytes: u64,
    pub physical_store: Option<String>,
    pub volumes: Vec<ApfsVolume>,
}

#[derive(Debug, Clone, Default)]
pub struct ApfsVolume {
    pub bsd: String,
    pub name: String,
    pub role: String,
    pub mount_point: Option<String>,
    pub consumed_bytes: u64,
}

pub fn collect() -> VolumeTick {
    #[cfg(target_os = "macos")]
    {
        macos_collect()
    }
    #[cfg(target_os = "linux")]
    {
        VolumeTick {
            mdraid: linux_mdraid(),
            ..Default::default()
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        VolumeTick::default()
    }
}

#[cfg(target_os = "macos")]
fn macos_collect() -> VolumeTick {
    use std::process::Command;
    let Ok(out) = Command::new("diskutil").args(["apfs", "list"]).output() else {
        return VolumeTick::default();
    };
    if !out.status.success() {
        return VolumeTick::default();
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut result = VolumeTick::default();
    let mut cur_container: Option<ApfsContainer> = None;
    let mut cur_volume: Option<ApfsVolume> = None;

    for line in text.lines() {
        let trimmed = line.trim_start();

        // Container header: "+-- Container disk3 <uuid>"
        if let Some(rest) = trimmed.strip_prefix("+-- Container ") {
            // Push any in-flight volume / container.
            if let Some(v) = cur_volume.take() {
                if let Some(c) = cur_container.as_mut() {
                    c.volumes.push(v);
                }
            }
            if let Some(c) = cur_container.take() {
                result.containers.push(c);
            }
            let bsd: String = rest.chars().take_while(|c| !c.is_whitespace()).collect();
            cur_container = Some(ApfsContainer {
                bsd,
                ..Default::default()
            });
            continue;
        }

        // Volume header: "+-> Volume disk3s1 <uuid>"
        if let Some(rest) = trimmed.strip_prefix("+-> Volume ") {
            if let Some(v) = cur_volume.take() {
                if let Some(c) = cur_container.as_mut() {
                    c.volumes.push(v);
                }
            }
            let bsd: String = rest.chars().take_while(|c| !c.is_whitespace()).collect();
            cur_volume = Some(ApfsVolume {
                bsd,
                ..Default::default()
            });
            continue;
        }

        // Physical store: "+-< Physical Store disk0s2 <uuid>"
        if let Some(rest) = trimmed.strip_prefix("+-< Physical Store ") {
            let store: String = rest.chars().take_while(|c| !c.is_whitespace()).collect();
            if let Some(c) = cur_container.as_mut() {
                c.physical_store = Some(store);
            }
            continue;
        }

        // Container "Size (Capacity Ceiling):  994662584320 B (994.7 GB)"
        if let Some(rest) = trimmed.strip_prefix("Size (Capacity Ceiling):") {
            if let Some(c) = cur_container.as_mut() {
                if cur_volume.is_none() {
                    c.size_bytes = first_byte_count(rest);
                }
            }
            continue;
        }
        // "Capacity In Use By Volumes:   319709114368 B (319.7 GB) (32.1% used)"
        if let Some(rest) = trimmed.strip_prefix("Capacity In Use By Volumes:") {
            if let Some(c) = cur_container.as_mut() {
                c.used_bytes = first_byte_count(rest);
            }
            continue;
        }

        // Volume role: "APFS Volume Disk (Role):   disk3s1 (System)"
        if let Some(rest) = trimmed.strip_prefix("APFS Volume Disk (Role):") {
            if let Some(v) = cur_volume.as_mut() {
                if let Some(open) = rest.find('(') {
                    if let Some(close) = rest[open..].find(')') {
                        v.role = rest[open + 1..open + close].to_string();
                    }
                }
            }
            continue;
        }
        // Volume name: "Name:                      Macintosh HD (Case-insensitive)"
        if let Some(rest) = trimmed.strip_prefix("Name:") {
            if let Some(v) = cur_volume.as_mut() {
                let name = rest.trim();
                let name = name.split_once(" (").map(|(a, _)| a).unwrap_or(name);
                v.name = name.trim().to_string();
            }
            continue;
        }
        // Mount point: "Mount Point:               /System/Volumes/Data"
        if let Some(rest) = trimmed.strip_prefix("Mount Point:") {
            if let Some(v) = cur_volume.as_mut() {
                let mp = rest.trim();
                if !mp.is_empty() && mp != "Not Mounted" {
                    v.mount_point = Some(mp.to_string());
                }
            }
            continue;
        }
        // "Capacity Consumed:         17797750784 B (17.8 GB)"
        if let Some(rest) = trimmed.strip_prefix("Capacity Consumed:") {
            if let Some(v) = cur_volume.as_mut() {
                v.consumed_bytes = first_byte_count(rest);
            }
            continue;
        }
    }

    // Flush trailing.
    if let Some(v) = cur_volume.take() {
        if let Some(c) = cur_container.as_mut() {
            c.volumes.push(v);
        }
    }
    if let Some(c) = cur_container.take() {
        result.containers.push(c);
    }
    result
}

#[cfg(target_os = "linux")]
fn linux_mdraid() -> Vec<MdRaidArray> {
    let Ok(text) = std::fs::read_to_string("/proc/mdstat") else {
        return Vec::new();
    };
    parse_mdstat(&text)
}

/// Pure parser for `/proc/mdstat` content. Kept cfg-free so it can be
/// exercised in tests from any platform.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn parse_mdstat(text: &str) -> Vec<MdRaidArray> {
    let mut out = Vec::new();
    let mut cur: Option<MdRaidArray> = None;

    for raw in text.lines() {
        let line = raw.trim_end();
        if line.starts_with("Personalities") || line.starts_with("unused devices") {
            continue;
        }
        // New array header: "md0 : active raid10 sda1[0] sdb1[1] …"
        if let Some(colon) = line.find(" : ") {
            // Flush any prior array.
            if let Some(prev) = cur.take() {
                out.push(prev);
            }
            let name = line[..colon].trim().to_string();
            let rest = &line[colon + 3..];
            let mut tokens = rest.split_whitespace();
            let state = tokens.next().unwrap_or("").to_string();
            let level = tokens.next().unwrap_or("").to_string();
            let mut members = Vec::new();
            for tok in tokens {
                if let Some(member) = parse_member(tok) {
                    members.push(member);
                }
            }
            cur = Some(MdRaidArray {
                name,
                level,
                state,
                members,
                ..Default::default()
            });
            continue;
        }

        let Some(arr) = cur.as_mut() else { continue };

        // Status line: "      7813767168 blocks super 1.2 256K chunks 2 near-copies [4/4] [UUUU]"
        if line.trim_start().starts_with(|c: char| c.is_ascii_digit()) && line.contains("blocks") {
            let mut tokens = line.split_whitespace();
            if let Some(blocks) = tokens.next().and_then(|s| s.parse::<u64>().ok()) {
                arr.size_bytes = blocks.saturating_mul(1024);
            }
            if let Some(slash) = find_slot_pair(line) {
                arr.members_total = slash.0;
                arr.members_present = slash.1;
            }
            if let Some(state) = find_member_state(line) {
                arr.member_state = state;
            }
            continue;
        }

        // Progress line: "      [=====>...........]  resync = 15.0% (…) finish=89.3min speed=123776K/sec"
        let trimmed = line.trim_start();
        if trimmed.starts_with('[')
            && (trimmed.contains("resync")
                || trimmed.contains("recovery")
                || trimmed.contains("reshape")
                || trimmed.contains("check"))
        {
            arr.progress = parse_progress(line);
            continue;
        }
    }

    if let Some(last) = cur.take() {
        out.push(last);
    }
    out
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_member(tok: &str) -> Option<MdRaidMember> {
    // Forms: "sda1[0]", "sdb1[1](F)", "sdc1[2](S)", "sdd1[3](W)"
    let lb = tok.find('[')?;
    let rb = tok.find(']')?;
    let device = tok[..lb].to_string();
    let idx_str = &tok[lb + 1..rb];
    let index = idx_str.parse().ok()?;
    let flag = if tok.len() > rb + 1 {
        Some(tok[rb + 1..].to_string())
    } else {
        None
    };
    Some(MdRaidMember {
        device,
        index,
        flag,
    })
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn find_slot_pair(line: &str) -> Option<(u32, u32)> {
    // Look for "[N/M]" near the end of the line.
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'[' {
            let close = line[i..].find(']').map(|j| i + j)?;
            let inside = &line[i + 1..close];
            if let Some(slash) = inside.find('/') {
                if let (Ok(a), Ok(b)) = (inside[..slash].parse(), inside[slash + 1..].parse()) {
                    return Some((a, b));
                }
            }
            i = close + 1;
        } else {
            i += 1;
        }
    }
    None
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn find_member_state(line: &str) -> Option<String> {
    // "[UUUU]" — the second bracketed block on a status line (the first
    // is the [present/total] pair). We look for one whose content is
    // all 'U' / '_' characters.
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'[' {
            let close = line[i..].find(']').map(|j| i + j)?;
            let inside = &line[i + 1..close];
            if !inside.is_empty() && inside.chars().all(|c| matches!(c, 'U' | '_' | 'B')) {
                return Some(inside.to_string());
            }
            i = close + 1;
        } else {
            i += 1;
        }
    }
    None
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_progress(line: &str) -> Option<MdRaidProgress> {
    let op = if line.contains("resync") {
        "resync"
    } else if line.contains("recovery") {
        "recovery"
    } else if line.contains("reshape") {
        "reshape"
    } else if line.contains("check") {
        "check"
    } else {
        return None;
    };
    // The "=" inside the progress bar (`[===>....]`) confounds a naive
    // split-on-equals. Anchor the percent parse to the op keyword.
    let needle = format!("{} = ", op);
    let percent = line
        .find(&needle)
        .and_then(|i| line[i + needle.len()..].split('%').next())
        .and_then(|s| s.trim().parse::<f32>().ok())
        .unwrap_or(0.0);
    let eta = line
        .split("finish=")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .unwrap_or("")
        .to_string();
    let speed = line
        .split("speed=")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .unwrap_or("")
        .to_string();
    Some(MdRaidProgress {
        op: op.to_string(),
        percent,
        eta,
        speed,
    })
}

/// Extracts the first byte count from a line like
/// "   319709114368 B (319.7 GB) (32.1% used)" → 319_709_114_368.
#[cfg(target_os = "macos")]
fn first_byte_count(s: &str) -> u64 {
    let mut digits = String::new();
    for ch in s.chars() {
        if ch.is_ascii_digit() {
            digits.push(ch);
        } else if !digits.is_empty() {
            break;
        }
    }
    digits.parse().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_two_active_arrays() {
        // Real `/proc/mdstat` shape from a healthy host.
        let text = "\
Personalities : [raid1] [raid10] [raid0]
md0 : active raid10 sda1[0] sdb1[1] sdc1[2] sdd1[3]
      7813767168 blocks super 1.2 256K chunks 2 near-copies [4/4] [UUUU]
      bitmap: 0/59 pages [0KB], 65536KB chunk

md1 : active raid1 sde1[0] sdf1[1]
      1953382464 blocks super 1.2 [2/2] [UU]
      bitmap: 0/15 pages [0KB], 65536KB chunk

unused devices: <none>
";
        let arrays = parse_mdstat(text);
        assert_eq!(arrays.len(), 2);

        let md0 = &arrays[0];
        assert_eq!(md0.name, "md0");
        assert_eq!(md0.level, "raid10");
        assert_eq!(md0.state, "active");
        assert_eq!(md0.members.len(), 4);
        assert_eq!(md0.members[0].device, "sda1");
        assert_eq!(md0.members[0].index, 0);
        assert!(md0.members[0].flag.is_none());
        assert_eq!(md0.size_bytes, 7_813_767_168u64 * 1024);
        assert_eq!(md0.members_total, 4);
        assert_eq!(md0.members_present, 4);
        assert_eq!(md0.member_state, "UUUU");
        assert!(md0.progress.is_none());

        let md1 = &arrays[1];
        assert_eq!(md1.members.len(), 2);
        assert_eq!(md1.member_state, "UU");
    }

    #[test]
    fn parses_degraded_with_resync() {
        let text = "\
Personalities : [raid10]
md0 : active raid10 sda1[0] sdb1[1] sdc1[2] sdd1[3](F)
      7813767168 blocks super 1.2 256K chunks 2 near-copies [4/3] [UUU_]
      [===>.................]  resync = 15.0% (1176224256/7813767168) finish=89.3min speed=123776K/sec
      bitmap: 0/59 pages [0KB], 65536KB chunk

unused devices: <none>
";
        let arrays = parse_mdstat(text);
        assert_eq!(arrays.len(), 1);
        let a = &arrays[0];
        assert_eq!(a.members_total, 4);
        assert_eq!(a.members_present, 3);
        assert_eq!(a.member_state, "UUU_");
        let failed = a.members.iter().find(|m| m.device == "sdd1").unwrap();
        assert_eq!(failed.flag.as_deref(), Some("(F)"));
        let prog = a.progress.as_ref().expect("resync progress");
        assert_eq!(prog.op, "resync");
        assert!((prog.percent - 15.0).abs() < 0.001);
        assert_eq!(prog.eta, "89.3min");
        assert_eq!(prog.speed, "123776K/sec");
    }

    #[test]
    fn parses_empty_when_no_arrays() {
        let text = "Personalities : [raid1]\n\nunused devices: <none>\n";
        let arrays = parse_mdstat(text);
        assert!(arrays.is_empty());
    }
}
