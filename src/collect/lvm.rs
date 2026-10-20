//! LVM volume groups with their logical and physical volumes (Linux), from
//! `vgs`, `lvs` and `pvs` JSON reports (issue #29).
//!
//! Exact invocations, all with `--reportformat json --units b --nosuffix`
//! so every size is a byte count:
//!
//! - `vgs -o vg_name,vg_size,vg_free,vg_attr,vg_missing_pv_count`
//! - `lvs -o vg_name,lv_name,lv_size,segtype,lv_attr,lv_health_status,
//!   sync_percent,data_percent,metadata_percent,pool_lv,origin`
//! - `pvs -o pv_name,vg_name,pv_size,pv_free,pv_attr`
//!
//! Every value in the report is a string; a percentage that doesn't apply
//! (an inactive LV, a linear LV's sync) is `""`. JSON reports arrived in
//! lvm2 2.02.158 (2016).
//!
//! The reporting tools need root: as a normal user they can't take the
//! global lock or talk to device-mapper, and exit 5 with an empty report.
//! Most people run diskwatch unprivileged, so then the active LVs are read
//! from sysfs instead, which anyone can: a device-mapper node whose
//! `dm/uuid` starts `LVM-` is an LV, `dm/name` is `<vg>-<lv>` and `slaves`
//! names the PVs it sits on. That gives names, sizes and PVs, but not free
//! space, health or pool usage, and the note says so.

#[cfg(target_os = "linux")]
use std::time::Duration;

#[derive(Debug, Clone, Default)]
pub struct LvmVg {
    pub name: String,
    /// 0 when only sysfs was readable: the VG's size isn't there.
    pub size_bytes: u64,
    pub free_bytes: u64,
    /// `wz--n-`; the fourth character is `p` while a PV is missing.
    pub attr: String,
    pub missing_pvs: u32,
    pub lvs: Vec<LvmLv>,
    pub pvs: Vec<LvmPv>,
}

#[derive(Debug, Clone, Default)]
pub struct LvmLv {
    pub name: String,
    pub size_bytes: u64,
    /// `linear`, `striped`, `raid1`, `thin-pool`, `thin`, `cache`, …
    pub segtype: String,
    /// `-wi-ao----`; see lvs(8). Empty from sysfs.
    pub attr: String,
    /// `partial`, `refresh needed`, `mismatches exist`, `failed`,
    /// `out of data`, `metadata read only`, or empty when healthy.
    pub health: String,
    /// RAID and mirror resync progress.
    pub sync_percent: Option<f32>,
    /// How full a thin pool, thin volume or snapshot is.
    pub data_percent: Option<f32>,
    /// Thin and cache pool metadata usage.
    pub metadata_percent: Option<f32>,
    pub pool: Option<String>,
    pub origin: Option<String>,
    /// Kernel device it's active as (`dm-3`), filled in at collection.
    pub dev: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct LvmPv {
    /// `/dev/sda2`, or `[unknown]` for a PV that has gone missing.
    pub path: String,
    pub size_bytes: u64,
    pub free_bytes: u64,
    pub missing: bool,
}

impl LvmVg {
    /// A PV has gone missing: LVs with extents on it are partial or gone.
    pub fn is_partial(&self) -> bool {
        self.missing_pvs > 0 || self.attr.chars().nth(3) == Some('p')
    }
}

impl LvmLv {
    fn attr_at(&self, i: usize) -> Option<char> {
        self.attr.chars().nth(i)
    }

    /// Active in the kernel. sysfs only lists active LVs.
    pub fn is_active(&self) -> bool {
        self.attr.is_empty() || self.attr_at(4) == Some('a')
    }

    /// A classic (non-thin) snapshot that filled up and was dropped.
    pub fn is_invalid_snapshot(&self) -> bool {
        self.attr_at(4) == Some('I')
    }

    /// A classic copy-on-write snapshot. Thin snapshots are thin volumes.
    pub fn is_snapshot(&self) -> bool {
        matches!(self.attr_at(0), Some('s' | 'S'))
    }

    pub fn is_thin_pool(&self) -> bool {
        self.segtype == "thin-pool"
    }

    /// What it is, for the KIND column: `thin pool`, `snapshot`, `raid1`.
    pub fn kind(&self) -> String {
        if self.is_snapshot() {
            return "snapshot".into();
        }
        match self.segtype.as_str() {
            "thin" if self.origin.is_some() => "thin snap".into(),
            "" => "lv".into(),
            other => other.replace('-', " "),
        }
    }

    /// The fuller of data and metadata, for a pool; data otherwise.
    pub fn fullest_percent(&self) -> Option<f32> {
        match (self.data_percent, self.metadata_percent) {
            (Some(d), Some(m)) => Some(d.max(m)),
            (d, m) => d.or(m),
        }
    }
}

/// Rows of one report kind (`vg`, `lv`, `pv`) from an lvm2 JSON report.
/// lvm2 prints one report object per command (or per VG under
/// `--configreport`), so rows are gathered from all of them.
fn report_rows(text: &str, kind: &str) -> Vec<serde_json::Map<String, serde_json::Value>> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return Vec::new();
    };
    v.get("report")
        .and_then(|r| r.as_array())
        .into_iter()
        .flatten()
        .filter_map(|r| r.get(kind)?.as_array())
        .flatten()
        .filter_map(|row| row.as_object().cloned())
        .collect()
}

fn field<'a>(row: &'a serde_json::Map<String, serde_json::Value>, key: &str) -> &'a str {
    row.get(key).and_then(|v| v.as_str()).unwrap_or("").trim()
}

fn bytes(row: &serde_json::Map<String, serde_json::Value>, key: &str) -> u64 {
    // --nosuffix leaves plain integers, but tolerate a stray unit suffix.
    field(row, key)
        .trim_end_matches(|c: char| c.is_ascii_alphabetic())
        .parse()
        .unwrap_or(0)
}

fn percent(row: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<f32> {
    field(row, key).parse().ok()
}

fn non_empty(s: &str) -> Option<String> {
    (!s.is_empty()).then(|| s.to_string())
}

/// Join the three reports into VGs, in `vgs` order. LVs and PVs follow the
/// order their reports list them in. PVs not in any VG are left out: they
/// hold nothing yet.
///
/// Pure and cfg-free so the fixtures below run on any platform.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn parse_reports(vgs: &str, lvs: &str, pvs: &str) -> Vec<LvmVg> {
    let mut out: Vec<LvmVg> = report_rows(vgs, "vg")
        .iter()
        .map(|r| LvmVg {
            name: field(r, "vg_name").to_string(),
            size_bytes: bytes(r, "vg_size"),
            free_bytes: bytes(r, "vg_free"),
            attr: field(r, "vg_attr").to_string(),
            missing_pvs: field(r, "vg_missing_pv_count").parse().unwrap_or(0),
            ..Default::default()
        })
        .filter(|vg| !vg.name.is_empty())
        .collect();

    for r in report_rows(lvs, "lv") {
        let Some(vg) = out.iter_mut().find(|vg| vg.name == field(&r, "vg_name")) else {
            continue;
        };
        vg.lvs.push(LvmLv {
            name: field(&r, "lv_name").to_string(),
            size_bytes: bytes(&r, "lv_size"),
            segtype: field(&r, "segtype").to_string(),
            attr: field(&r, "lv_attr").to_string(),
            health: field(&r, "lv_health_status").to_string(),
            sync_percent: percent(&r, "sync_percent"),
            data_percent: percent(&r, "data_percent"),
            metadata_percent: percent(&r, "metadata_percent"),
            pool: non_empty(field(&r, "pool_lv")),
            origin: non_empty(field(&r, "origin")),
            dev: None,
        });
    }

    for r in report_rows(pvs, "pv") {
        let Some(vg) = out.iter_mut().find(|vg| vg.name == field(&r, "vg_name")) else {
            continue;
        };
        let attr = field(&r, "pv_attr");
        vg.pvs.push(LvmPv {
            path: field(&r, "pv_name").to_string(),
            size_bytes: bytes(&r, "pv_size"),
            free_bytes: bytes(&r, "pv_free"),
            missing: attr.chars().nth(2) == Some('m'),
        });
    }
    out
}

/// Split a device-mapper name into VG and LV. dm escapes a `-` inside
/// either name as `--`, so the separator is the first lone `-`:
/// `my--vg-root--fs` → (`my-vg`, `root-fs`).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn split_dm_name(name: &str) -> Option<(String, String)> {
    let b = name.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'-' {
            if b.get(i + 1) == Some(&b'-') {
                i += 2;
                continue;
            }
            let (vg, lv) = (&name[..i], &name[i + 1..]);
            if vg.is_empty() || lv.is_empty() {
                return None;
            }
            return Some((vg.replace("--", "-"), lv.replace("--", "-")));
        }
        i += 1;
    }
    None
}

/// One active LV as sysfs shows it.
#[derive(Debug, Clone)]
pub(crate) struct SysfsLv {
    /// Kernel name, `dm-3`.
    pub dev: String,
    /// `dm/name`, `vg-lv`.
    pub dm_name: String,
    /// `dm/uuid`, `LVM-<vg uuid><lv uuid>[-suffix]`.
    pub uuid: String,
    pub size_bytes: u64,
    /// `slaves/`, the devices it sits on: `sda2`, `dm-0`, `md0`.
    pub slaves: Vec<String>,
}

/// VGs as far as sysfs can tell. An LV's own uuid is `LVM-` plus two
/// 32-character uuids; anything after that (`-tpool`, `-real`, `-cow`,
/// `-pool`, `_rimage_0`'s `-…`) is an internal sub-device of a thin pool,
/// snapshot or RAID LV, and is skipped. The PVs of a VG are the slaves of
/// its LVs that aren't LVs of the same VG themselves.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn from_sysfs(lvs: &[SysfsLv]) -> Vec<LvmVg> {
    const LV_UUID_LEN: usize = 4 + 32 + 32;
    let mut out: Vec<LvmVg> = Vec::new();
    for lv in lvs {
        if !lv.uuid.starts_with("LVM-") || lv.uuid.len() != LV_UUID_LEN {
            continue;
        }
        let Some((vg_name, lv_name)) = split_dm_name(&lv.dm_name) else {
            continue;
        };
        let pos = match out.iter().position(|v| v.name == vg_name) {
            Some(p) => p,
            None => {
                out.push(LvmVg {
                    name: vg_name.clone(),
                    ..Default::default()
                });
                out.len() - 1
            }
        };
        out[pos].lvs.push(LvmLv {
            name: lv_name,
            size_bytes: lv.size_bytes,
            dev: Some(lv.dev.clone()),
            ..Default::default()
        });
    }
    // PVs: what this VG's LVs sit on, following internal sub-devices
    // (a RAID LV sits on its rimage devices, which sit on the PVs).
    let by_dev: std::collections::HashMap<&str, &SysfsLv> =
        lvs.iter().map(|l| (l.dev.as_str(), l)).collect();
    for vg in &mut out {
        let mut pvs: Vec<String> = Vec::new();
        let mut stack: Vec<&str> = vg.lvs.iter().filter_map(|l| l.dev.as_deref()).collect();
        let mut seen = std::collections::HashSet::new();
        while let Some(dev) = stack.pop() {
            if !seen.insert(dev) {
                continue;
            }
            let Some(lv) = by_dev.get(dev) else { continue };
            for s in &lv.slaves {
                let same_vg = by_dev.get(s.as_str()).is_some_and(|sub| {
                    sub.uuid.starts_with("LVM-")
                        && split_dm_name(&sub.dm_name).is_some_and(|(v, _)| v == vg.name)
                });
                if same_vg {
                    stack.push(s);
                } else if !pvs.contains(s) {
                    pvs.push(s.clone());
                }
            }
        }
        pvs.sort();
        vg.pvs = pvs
            .into_iter()
            .map(|p| LvmPv {
                path: format!("/dev/{p}"),
                ..Default::default()
            })
            .collect();
    }
    out
}

/// Volume groups, plus a word when they couldn't be read in full.
#[derive(Debug, Clone, Default)]
pub struct LvmProbe {
    pub vgs: Vec<LvmVg>,
    /// One line for the Volumes tab. `None` when the lvm2 tools answered,
    /// or when there's no LVM here at all.
    pub note: Option<String>,
}

#[cfg(target_os = "linux")]
const LVM_TIMEOUT: Duration = Duration::from_secs(2);

/// Every VG the lvm2 tools report, or the active LVs from sysfs when they
/// can't (no root, not installed, too slow). Called from the 30s metadata
/// refresh, so it runs the tools each time without caching.
#[cfg(target_os = "linux")]
pub fn probe() -> LvmProbe {
    use crate::collect::zfs::Run;

    const OPTS: [&str; 5] = ["--reportformat", "json", "--units", "b", "--nosuffix"];
    let run = |tool: &str, fields: &str| -> Run {
        let mut args: Vec<&str> = OPTS.to_vec();
        args.extend(["-o", fields]);
        // The tools live in sbin, which isn't on a normal user's PATH on
        // Debian.
        let mut outcome = Run::Missing;
        for program in [
            tool.to_string(),
            format!("/usr/sbin/{tool}"),
            format!("/sbin/{tool}"),
        ] {
            outcome = crate::collect::zfs::run_with_timeout(&program, &args, LVM_TIMEOUT);
            if !matches!(outcome, Run::Missing) {
                break;
            }
        }
        outcome
    };

    let sysfs = || from_sysfs(&read_sysfs());
    let vgs = match run("vgs", "vg_name,vg_size,vg_free,vg_attr,vg_missing_pv_count") {
        Run::Ok(text) => text,
        failed => return settle_failure(&failed, sysfs(), is_root()),
    };
    let lvs = match run(
        "lvs",
        "vg_name,lv_name,lv_size,segtype,lv_attr,lv_health_status,sync_percent,data_percent,metadata_percent,pool_lv,origin",
    ) {
        Run::Ok(text) => text,
        failed => return settle_failure(&failed, sysfs(), is_root()),
    };
    let pvs = match run("pvs", "pv_name,vg_name,pv_size,pv_free,pv_attr") {
        Run::Ok(text) => text,
        failed => return settle_failure(&failed, sysfs(), is_root()),
    };

    let mut vgs = parse_reports(&vgs, &lvs, &pvs);
    for vg in &mut vgs {
        for lv in &mut vg.lvs {
            if lv.is_active() {
                lv.dev = kernel_name(&format!("/dev/{}/{}", vg.name, lv.name));
            }
        }
    }
    LvmProbe { vgs, note: None }
}

/// The tools couldn't report: fall back to what sysfs shows, and say why
/// it's partial. A machine with no active LVs hears nothing; lvm2 is
/// installed on plenty of systems that don't use it.
#[cfg(target_os = "linux")]
fn settle_failure(run: &crate::collect::zfs::Run, vgs: Vec<LvmVg>, root: bool) -> LvmProbe {
    use crate::collect::zfs::Run;
    if vgs.is_empty() {
        return LvmProbe::default();
    }
    let why = match run {
        Run::Ok(_) => return LvmProbe { vgs, note: None },
        Run::Missing => "LVM is in use but the lvm2 tools were not found".to_string(),
        Run::TimedOut => format!("lvm2 did not answer within {}s", LVM_TIMEOUT.as_secs()),
        Run::Failed(_) if !root => "lvm2 reports need root".to_string(),
        Run::Failed(Some(code)) => format!("lvm2 reports failed (exit {code})"),
        Run::Failed(None) => "lvm2 reports failed".to_string(),
    };
    LvmProbe {
        vgs,
        note: Some(format!(
            "{why}; showing active LVs from sysfs, without free space or health"
        )),
    }
}

#[cfg(target_os = "linux")]
fn is_root() -> bool {
    use std::os::unix::fs::MetadataExt;
    // /proc/self is owned by the effective uid.
    std::fs::metadata("/proc/self").is_ok_and(|m| m.uid() == 0)
}

#[cfg(target_os = "linux")]
fn read_sysfs() -> Vec<SysfsLv> {
    let read = |p: String| std::fs::read_to_string(p).map(|s| s.trim().to_string());
    let Ok(entries) = std::fs::read_dir("/sys/block") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let dev = e.file_name().to_string_lossy().into_owned();
        if !dev.starts_with("dm-") {
            continue;
        }
        let (Ok(uuid), Ok(dm_name)) = (
            read(format!("/sys/block/{dev}/dm/uuid")),
            read(format!("/sys/block/{dev}/dm/name")),
        ) else {
            continue;
        };
        if !uuid.starts_with("LVM-") {
            continue;
        }
        let size_bytes = read(format!("/sys/block/{dev}/size"))
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .map_or(0, |sectors| sectors * 512);
        let mut slaves: Vec<String> = std::fs::read_dir(format!("/sys/block/{dev}/slaves"))
            .into_iter()
            .flatten()
            .flatten()
            .map(|s| s.file_name().to_string_lossy().into_owned())
            .collect();
        slaves.sort();
        out.push(SysfsLv {
            dev,
            dm_name,
            uuid,
            size_bytes,
            slaves,
        });
    }
    out.sort_by(|a, b| a.dm_name.cmp(&b.dm_name));
    out
}

/// `/dev/vg/lv` → `dm-3`.
#[cfg(target_os = "linux")]
fn kernel_name(path: &str) -> Option<String> {
    let real = std::fs::canonicalize(path).ok()?;
    let name = real.to_str()?.strip_prefix("/dev/")?;
    (!name.is_empty() && !name.contains('/')).then(|| name.to_string())
}

#[cfg(test)]
pub(crate) mod fixtures {
    //! lvm2 JSON reports as `vgs`/`lvs`/`pvs --reportformat json --units b
    //! --nosuffix -o <the fields probe() asks for>` print them: two-space
    //! indent, every value a string, one report object per command.

    /// A typical install: one VG on one partition with root and swap, and
    /// a data VG over two disks with a thin pool, a thin volume, a RAID1
    /// LV and a classic snapshot.
    pub const VGS: &str = r#"  {
      "report": [
          {
              "vg": [
                  {"vg_name":"data", "vg_size":"1000203091968", "vg_free":"536870912000", "vg_attr":"wz--n-", "vg_missing_pv_count":"0"},
                  {"vg_name":"ubuntu-vg", "vg_size":"254435868672", "vg_free":"0", "vg_attr":"wz--n-", "vg_missing_pv_count":"0"}
              ]
          }
      ]
  }
"#;

    pub const LVS: &str = r#"  {
      "report": [
          {
              "lv": [
                  {"vg_name":"data", "lv_name":"home", "lv_size":"107374182400", "segtype":"linear", "lv_attr":"owi-aos---", "lv_health_status":"", "sync_percent":"", "data_percent":"", "metadata_percent":"", "pool_lv":"", "origin":""},
                  {"vg_name":"data", "lv_name":"home_snap", "lv_size":"10737418240", "segtype":"linear", "lv_attr":"swi-a-s---", "lv_health_status":"", "sync_percent":"", "data_percent":"83.42", "metadata_percent":"", "pool_lv":"", "origin":"home"},
                  {"vg_name":"data", "lv_name":"mirror", "lv_size":"53687091200", "segtype":"raid1", "lv_attr":"rwi-aor---", "lv_health_status":"", "sync_percent":"42.17", "data_percent":"", "metadata_percent":"", "pool_lv":"", "origin":""},
                  {"vg_name":"data", "lv_name":"pool", "lv_size":"214748364800", "segtype":"thin-pool", "lv_attr":"twi-aotz--", "lv_health_status":"", "sync_percent":"", "data_percent":"61.04", "metadata_percent":"12.50", "pool_lv":"", "origin":""},
                  {"vg_name":"data", "lv_name":"vm1", "lv_size":"107374182400", "segtype":"thin", "lv_attr":"Vwi-aotz--", "lv_health_status":"", "sync_percent":"", "data_percent":"70.11", "metadata_percent":"", "pool_lv":"pool", "origin":""},
                  {"vg_name":"ubuntu-vg", "lv_name":"root", "lv_size":"252287336448", "segtype":"linear", "lv_attr":"-wi-ao----", "lv_health_status":"", "sync_percent":"", "data_percent":"", "metadata_percent":"", "pool_lv":"", "origin":""},
                  {"vg_name":"ubuntu-vg", "lv_name":"swap_1", "lv_size":"2147483648", "segtype":"linear", "lv_attr":"-wi-ao----", "lv_health_status":"", "sync_percent":"", "data_percent":"", "metadata_percent":"", "pool_lv":"", "origin":""}
              ]
          }
      ]
  }
"#;

    pub const PVS: &str = r#"  {
      "report": [
          {
              "pv": [
                  {"pv_name":"/dev/nvme0n1p3", "vg_name":"ubuntu-vg", "pv_size":"254435868672", "pv_free":"0", "pv_attr":"a--"},
                  {"pv_name":"/dev/sda", "vg_name":"data", "pv_size":"500101545984", "pv_free":"268435456000", "pv_attr":"a--"},
                  {"pv_name":"/dev/sdb", "vg_name":"data", "pv_size":"500101545984", "pv_free":"268435456000", "pv_attr":"a--"},
                  {"pv_name":"/dev/sdc1", "vg_name":"", "pv_size":"1000204886016", "pv_free":"1000204886016", "pv_attr":"---"}
              ]
          }
      ]
  }
"#;

    /// `data` after `sdb` died: the VG is partial, the RAID1 LV has lost a
    /// leg, and the missing PV is listed as `[unknown]`.
    pub const VGS_DEGRADED: &str = r#"  {
      "report": [
          {
              "vg": [
                  {"vg_name":"data", "vg_size":"1000203091968", "vg_free":"536870912000", "vg_attr":"wz-pn-", "vg_missing_pv_count":"1"}
              ]
          }
      ]
  }
"#;

    pub const LVS_DEGRADED: &str = r#"  {
      "report": [
          {
              "lv": [
                  {"vg_name":"data", "lv_name":"mirror", "lv_size":"53687091200", "segtype":"raid1", "lv_attr":"rwi-aor-p-", "lv_health_status":"partial", "sync_percent":"100.00", "data_percent":"", "metadata_percent":"", "pool_lv":"", "origin":""},
                  {"vg_name":"data", "lv_name":"pool", "lv_size":"214748364800", "segtype":"thin-pool", "lv_attr":"twi-aotz--", "lv_health_status":"", "sync_percent":"", "data_percent":"96.30", "metadata_percent":"40.00", "pool_lv":"", "origin":""},
                  {"vg_name":"data", "lv_name":"old_snap", "lv_size":"10737418240", "segtype":"linear", "lv_attr":"swi-I-s---", "lv_health_status":"", "sync_percent":"", "data_percent":"100.00", "metadata_percent":"", "pool_lv":"", "origin":"home"}
              ]
          }
      ]
  }
"#;

    pub const PVS_DEGRADED: &str = r#"  {
      "report": [
          {
              "pv": [
                  {"pv_name":"/dev/sda", "vg_name":"data", "pv_size":"500101545984", "pv_free":"268435456000", "pv_attr":"a--"},
                  {"pv_name":"[unknown]", "vg_name":"data", "pv_size":"500101545984", "pv_free":"268435456000", "pv_attr":"a-m"}
              ]
          }
      ]
  }
"#;

    pub fn vgs() -> Vec<super::LvmVg> {
        super::parse_reports(VGS, LVS, PVS)
    }

    pub fn degraded() -> Vec<super::LvmVg> {
        super::parse_reports(VGS_DEGRADED, LVS_DEGRADED, PVS_DEGRADED)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_join_into_vgs() {
        let vgs = fixtures::vgs();
        assert_eq!(
            vgs.iter().map(|v| v.name.as_str()).collect::<Vec<_>>(),
            ["data", "ubuntu-vg"]
        );
        let data = &vgs[0];
        assert_eq!(data.size_bytes, 1_000_203_091_968);
        assert_eq!(data.free_bytes, 536_870_912_000);
        assert!(!data.is_partial());
        assert_eq!(data.lvs.len(), 5);
        assert_eq!(
            data.pvs.iter().map(|p| p.path.as_str()).collect::<Vec<_>>(),
            ["/dev/sda", "/dev/sdb"],
            "a PV in no VG holds nothing and is left out"
        );
        let root = vgs[1].lvs.iter().find(|l| l.name == "root").unwrap();
        assert_eq!(root.size_bytes, 252_287_336_448);
        assert!(root.is_active());
        assert_eq!(root.kind(), "linear");
    }

    #[test]
    fn lv_kinds_and_usage() {
        let vgs = fixtures::vgs();
        let lv = |n: &str| vgs[0].lvs.iter().find(|l| l.name == n).unwrap();
        assert_eq!(lv("pool").kind(), "thin pool");
        assert!(lv("pool").is_thin_pool());
        assert_eq!(lv("pool").data_percent, Some(61.04));
        assert_eq!(lv("pool").metadata_percent, Some(12.5));
        assert_eq!(lv("pool").fullest_percent(), Some(61.04));
        assert_eq!(lv("vm1").kind(), "thin");
        assert_eq!(lv("vm1").pool.as_deref(), Some("pool"));
        assert_eq!(lv("home_snap").kind(), "snapshot");
        assert_eq!(lv("home_snap").origin.as_deref(), Some("home"));
        assert_eq!(lv("mirror").kind(), "raid1");
        assert_eq!(lv("mirror").sync_percent, Some(42.17));
        assert_eq!(lv("home").sync_percent, None, "\"\" means not applicable");
    }

    #[test]
    fn a_missing_pv_marks_the_vg_partial() {
        let vgs = fixtures::degraded();
        let data = &vgs[0];
        assert!(data.is_partial());
        assert_eq!(data.missing_pvs, 1);
        assert!(data.pvs.iter().any(|p| p.missing && p.path == "[unknown]"));
        let mirror = data.lvs.iter().find(|l| l.name == "mirror").unwrap();
        assert_eq!(mirror.health, "partial");
        let snap = data.lvs.iter().find(|l| l.name == "old_snap").unwrap();
        assert!(snap.is_invalid_snapshot());
    }

    #[test]
    fn garbage_and_empty_reports_give_nothing() {
        assert!(parse_reports("", "", "").is_empty());
        assert!(parse_reports("not json", "{}", "[]").is_empty());
        let empty = r#"  {"report": [{"vg": []}]}"#;
        assert!(parse_reports(empty, empty, empty).is_empty());
    }

    #[test]
    fn dm_names_split_on_the_lone_dash() {
        let s = |n: &str| split_dm_name(n);
        assert_eq!(s("fedora-root"), Some(("fedora".into(), "root".into())));
        assert_eq!(
            s("ubuntu--vg-ubuntu--lv"),
            Some(("ubuntu-vg".into(), "ubuntu-lv".into()))
        );
        assert_eq!(s("vg-lv--x--y"), Some(("vg".into(), "lv-x-y".into())));
        assert_eq!(s("nodash"), None);
        assert_eq!(s("luks--abc"), None, "only escaped dashes: no separator");
    }

    fn sysfs_lv(dev: &str, name: &str, uuid_suffix: &str, gib: u64, slaves: &[&str]) -> SysfsLv {
        SysfsLv {
            dev: dev.into(),
            dm_name: name.into(),
            uuid: format!("LVM-{}{}{}", "a".repeat(32), "b".repeat(32), uuid_suffix),
            size_bytes: gib << 30,
            slaves: slaves.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn sysfs_gives_lvs_and_their_pvs() {
        let lvs = vec![
            sysfs_lv("dm-1", "ubuntu--vg-root", "", 200, &["dm-0"]),
            sysfs_lv("dm-2", "ubuntu--vg-swap_1", "", 2, &["dm-0"]),
            // A RAID1 LV: its rimage sub-devices sit on the PVs.
            sysfs_lv("dm-5", "data-mirror_rimage_0", "-rimage_0", 50, &["sda"]),
            sysfs_lv("dm-6", "data-mirror_rimage_1", "-rimage_1", 50, &["sdb"]),
            sysfs_lv("dm-7", "data-mirror", "", 50, &["dm-5", "dm-6"]),
        ];
        let vgs = from_sysfs(&lvs);
        assert_eq!(vgs.len(), 2);
        let ubuntu = vgs.iter().find(|v| v.name == "ubuntu-vg").unwrap();
        assert_eq!(
            ubuntu
                .lvs
                .iter()
                .map(|l| l.name.as_str())
                .collect::<Vec<_>>(),
            ["root", "swap_1"]
        );
        assert_eq!(ubuntu.lvs[0].size_bytes, 200 << 30);
        assert_eq!(ubuntu.lvs[0].dev.as_deref(), Some("dm-1"));
        // On LUKS: the PV is the dm-crypt device, which isn't an LV.
        assert_eq!(ubuntu.pvs[0].path, "/dev/dm-0");
        assert_eq!(ubuntu.size_bytes, 0, "sysfs doesn't know VG size");

        let data = vgs.iter().find(|v| v.name == "data").unwrap();
        assert_eq!(
            data.lvs.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(),
            ["mirror"],
            "sub-devices are not LVs of their own"
        );
        assert_eq!(
            data.pvs.iter().map(|p| p.path.as_str()).collect::<Vec<_>>(),
            ["/dev/sda", "/dev/sdb"]
        );
        assert!(data.lvs[0].is_active());
        assert_eq!(data.lvs[0].kind(), "lv");
    }

    #[test]
    fn non_lvm_dm_devices_are_ignored() {
        let crypt = SysfsLv {
            dev: "dm-0".into(),
            dm_name: "luks-1234".into(),
            uuid: "CRYPT-LUKS2-1234-luks-1234".into(),
            size_bytes: 1 << 30,
            slaves: vec!["nvme0n1p3".into()],
        };
        assert!(from_sysfs(&[crypt]).is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_failed_report_without_lvm_in_use_says_nothing() {
        use crate::collect::zfs::Run;
        let p = settle_failure(&Run::Failed(Some(5)), Vec::new(), false);
        assert!(p.vgs.is_empty() && p.note.is_none());
        let p = settle_failure(&Run::Missing, Vec::new(), false);
        assert!(p.note.is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_failed_report_with_lvs_in_sysfs_explains_itself() {
        use crate::collect::zfs::Run;
        let vgs = from_sysfs(&[sysfs_lv("dm-1", "fedora-root", "", 10, &["sda2"])]);
        let p = settle_failure(&Run::Failed(Some(5)), vgs.clone(), false);
        assert_eq!(p.vgs.len(), 1);
        let note = p.note.unwrap();
        assert!(note.starts_with("lvm2 reports need root"), "{note}");
        let p = settle_failure(&Run::Failed(Some(5)), vgs.clone(), true);
        assert!(p.note.unwrap().contains("exit 5"));
        let p = settle_failure(&Run::TimedOut, vgs, true);
        assert!(p.note.unwrap().contains("within 2s"));
    }

    /// Run by the `lvm` CI job as root, against loop-device VGs it builds:
    /// the parser must agree with the lvm2 that's actually installed.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore]
    fn real_lvm_reports_parse() {
        let p = probe();
        assert!(p.note.is_none(), "{:?}", p.note);
        let vg = p
            .vgs
            .iter()
            .find(|v| v.name == "dwtest")
            .unwrap_or_else(|| panic!("no dwtest VG in {:?}", p.vgs));
        println!("{vg:#?}");
        assert!(vg.size_bytes > 0 && vg.free_bytes > 0 && vg.free_bytes < vg.size_bytes);
        assert_eq!(vg.pvs.len(), 3);
        assert!(vg
            .pvs
            .iter()
            .all(|pv| pv.path.starts_with("/dev/loop") && !pv.missing));
        let lv = |n: &str| {
            vg.lvs
                .iter()
                .find(|l| l.name == n)
                .unwrap_or_else(|| panic!("no LV {n} in {:?}", vg.lvs))
        };
        assert_eq!(lv("lin").kind(), "linear");
        assert!(lv("lin")
            .dev
            .as_deref()
            .is_some_and(|d| d.starts_with("dm-")));
        assert_eq!(lv("mirr").kind(), "raid1");
        assert!(lv("mirr").sync_percent.is_some());
        assert!(lv("pool").is_thin_pool());
        assert!(lv("pool").data_percent.is_some_and(|d| d > 10.0));
        assert!(lv("pool").metadata_percent.is_some());
        assert_eq!(lv("thinvol").pool.as_deref(), Some("pool"));
        assert!(lv("snap").is_snapshot());
        assert!(lv("snap").data_percent.is_some_and(|d| d > 50.0));
        assert!(!vg.is_partial());

        // The no-root fallback, from the same live devices.
        let sys = from_sysfs(&read_sysfs());
        let svg = sys
            .iter()
            .find(|v| v.name == "dwtest")
            .expect("dwtest in sysfs");
        println!("{svg:#?}");
        for n in ["lin", "mirr", "thinvol", "snap"] {
            assert!(
                svg.lvs.iter().any(|l| l.name == n),
                "{n} missing from sysfs"
            );
        }
        assert!(svg.pvs.iter().all(|pv| pv.path.starts_with("/dev/loop")));
    }
}
