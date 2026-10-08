//! ZFS pool layout and allocation, from `zpool list -v`.
//!
//! A ZFS dataset is mounted from `pool/dataset`, not from a `/dev/` node,
//! so the mount table never says which disks a pool lives on. Every
//! dataset was attributed to nothing, and a root-on-ZFS machine showed its
//! disks 0% used (issue #25). `zpool list -v` has what's missing: each
//! pool, its top-level vdevs with their allocated bytes, and the member
//! devices under each. It reads the pool config from the kernel and needs
//! no root.
//!
//! Exact invocation: `zpool list -v -P -p -o name,size,allocated,free,health`.
//!
//! - `-p` prints exact byte counts, `-P` full device paths (with the
//!   partition: `/dev/disk/by-id/nvme-…-part3`, not `nvme-…`).
//! - `-o` pins the pool columns. OpenZFS ≤ 2.3 ignores it on vdev rows and
//!   prints its nine fixed columns there; 2.4 honours it. Both put SIZE,
//!   ALLOC, FREE first and HEALTH last, which is all the parser reads.
//! - Not `-H`: scripted mode prefixes every vdev with one tab whatever its
//!   depth, which loses the tree. The human layout indents by depth (2 per
//!   level), and depth is the only thing that separates a mirror's member
//!   from the two halves of a `spare-N` swap inside a raidz.
//!
//! Formats per `print_list_stats()` (≤ 2.3) and `collect_list_stats()`
//! (2.4) in `cmd/zpool/zpool_main.c`: a header row, then per pool a pool
//! row at column 0, its data vdevs at depth 2 and their members at depth 4.
//! Allocation classes follow as an all-dash row at column 0 (`dedup`,
//! `special`, `logs`, then `cache` and `spare`) with their vdevs under it.
//! Only top-level vdevs carry ALLOC; members print `-`.

#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};

/// What a top-level vdev is for. Only data-bearing classes count toward
/// a disk's used bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VdevClass {
    #[default]
    Data,
    Special,
    Dedup,
    Log,
    Cache,
    Spare,
}

impl VdevClass {
    pub fn label(self) -> &'static str {
        match self {
            VdevClass::Data => "data",
            VdevClass::Special => "special",
            VdevClass::Dedup => "dedup",
            VdevClass::Log => "log",
            VdevClass::Cache => "cache",
            VdevClass::Spare => "spare",
        }
    }

    /// The class names `zpool list -v` prints as section rows.
    fn from_section(name: &str) -> Option<VdevClass> {
        match name {
            "special" => Some(VdevClass::Special),
            "dedup" => Some(VdevClass::Dedup),
            "logs" => Some(VdevClass::Log),
            "cache" => Some(VdevClass::Cache),
            "spare" => Some(VdevClass::Spare),
            _ => None,
        }
    }

    /// L2ARC holds evictable copies of blocks that live elsewhere in the
    /// pool, and an idle spare holds nothing, so neither is pool data.
    /// Special, dedup and log vdevs are where those blocks actually live.
    fn holds_pool_data(self) -> bool {
        !matches!(self, VdevClass::Cache | VdevClass::Spare)
    }
}

#[derive(Debug, Clone, Default)]
pub struct ZfsPool {
    pub name: String,
    pub size_bytes: u64,
    pub alloc_bytes: u64,
    pub free_bytes: u64,
    /// ONLINE, DEGRADED, FAULTED, SUSPENDED, …
    pub health: String,
    pub vdevs: Vec<ZfsVdev>,
}

#[derive(Debug, Clone, Default)]
pub struct ZfsVdev {
    /// `mirror-0`, `raidz1-0`, `draid2:4d:12c:1s-0`, or the device path
    /// of a single-disk vdev.
    pub name: String,
    pub class: VdevClass,
    pub size_bytes: u64,
    /// `None` where zpool prints `-` (idle spares).
    pub alloc_bytes: Option<u64>,
    pub health: String,
    /// Leaf devices, in zpool's order. A single-disk vdev is its own member.
    pub members: Vec<ZfsMember>,
}

#[derive(Debug, Clone, Default)]
pub struct ZfsMember {
    /// As zpool printed it: `/dev/disk/by-id/…-part3`, `/dev/sdb1`, or a
    /// bare GUID for a device that is no longer present.
    pub path: String,
    pub health: String,
    /// Which child of the vdev this member stores. Members share a slot
    /// only inside a `spare-N` / `replacing-N`, where two devices stand in
    /// for one column of a raidz.
    pub slot: usize,
    /// Kernel block device the path resolves to (`nvme0n1p3`), filled in
    /// at collection. `None` when it doesn't resolve to a `/dev/` node.
    pub dev: Option<String>,
}

impl ZfsMember {
    /// Short name for display: the kernel name when resolved, else the last
    /// path component (a by-id name, or the GUID of a missing device).
    pub fn display(&self) -> &str {
        match &self.dev {
            Some(d) => d,
            None => self.path.rsplit('/').next().unwrap_or(&self.path),
        }
    }
}

impl ZfsVdev {
    /// `mirror`, `raidz1`, `draid2`, or `disk` for a single device.
    pub fn layout(&self) -> &str {
        if !is_group(&self.name) {
            return "disk";
        }
        // draid2:4d:12c:1s-0 → draid2; raidz1-0 → raidz1
        let base = self.name.split(':').next().unwrap_or(&self.name);
        base.rsplit_once('-').map_or(base, |(kind, _)| kind)
    }

    /// Bytes each member holds of this vdev's allocation.
    ///
    /// A mirror stores a full copy on every member, so each one holds the
    /// whole allocation. raidz and dRAID stripe it, parity included, across
    /// their children, so each holds an even share. The split is by child
    /// slot, not device count: the two halves of a spare swap each hold the
    /// column they cover instead of diluting everyone else's. A
    /// missing member's share is dropped, not piled onto the survivors:
    /// they hold exactly what they held before it went.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn member_shares(&self) -> Vec<(&ZfsMember, u64)> {
        let Some(alloc) = self.alloc_bytes else {
            return Vec::new();
        };
        if !self.class.holds_pool_data() || self.members.is_empty() {
            return Vec::new();
        }
        let layout = self.layout();
        let share = if layout.starts_with("raidz") || layout.starts_with("draid") {
            let slots = self.members.iter().map(|m| m.slot).max().unwrap_or(0) + 1;
            alloc / slots as u64
        } else {
            alloc
        };
        self.members.iter().map(|m| (m, share)).collect()
    }
}

/// True for vdev names zpool synthesises for a group of devices rather
/// than a device path: `mirror-0`, `raidz2-1`, `draid1:…-0`, and the
/// interior `replacing-N` / `spare-N` that appear while a device is being
/// replaced or a hot spare is covering for it. A dRAID's distributed spare
/// (`draid1-0-0`) has no colon and is a leaf, like a device.
fn is_group(name: &str) -> bool {
    (name.starts_with("draid") && name.contains(':'))
        || ["mirror-", "raidz", "replacing-", "spare-"]
            .iter()
            .any(|p| name.starts_with(p))
}

/// Parse `zpool list -v -P -p -o name,size,allocated,free,health` output.
///
/// Pure and cfg-free so the fixtures below run on any platform.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn parse_zpool_list(text: &str) -> Vec<ZfsPool> {
    fn bytes(s: &str) -> Option<u64> {
        s.parse().ok()
    }

    let mut pools: Vec<ZfsPool> = Vec::new();
    let mut class = VdevClass::Data;
    // Child slots handed out so far in the current top-level vdev.
    let mut slots = 0usize;
    // Set while inside a removed (indirect) vdev, whose rows are skipped.
    let mut skipping = false;

    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        // Every data row has at least name, size, alloc, free and health;
        // this also drops "no pools available" and blank lines.
        if fields.len() < 5 {
            continue;
        }
        let name = fields[0];
        let health = fields[fields.len() - 1].to_string();
        let depth = line.len() - line.trim_start_matches(' ').len();

        if depth == 0 {
            if name == "NAME" && fields[1] == "SIZE" {
                continue;
            }
            // Section rows are the class name followed by dashes only. A
            // pool row always has a health, even when its sizes are `-`.
            if fields[1..].iter().all(|f| *f == "-") {
                class = VdevClass::from_section(name).unwrap_or(VdevClass::Data);
                continue;
            }
            pools.push(ZfsPool {
                name: name.to_string(),
                size_bytes: bytes(fields[1]).unwrap_or(0),
                alloc_bytes: bytes(fields[2]).unwrap_or(0),
                free_bytes: bytes(fields[3]).unwrap_or(0),
                health,
                vdevs: Vec::new(),
            });
            class = VdevClass::Data;
            continue;
        }

        let Some(pool) = pools.last_mut() else {
            continue;
        };
        if depth == 2 {
            // Device removal leaves an `indirect-N` vdev behind; it maps
            // old blocks to their new home and owns no disk.
            skipping = name.starts_with("indirect-");
            if skipping {
                continue;
            }
            slots = 0;
            let members = if is_group(name) {
                Vec::new()
            } else {
                vec![ZfsMember {
                    path: name.to_string(),
                    health: health.clone(),
                    slot: 0,
                    dev: None,
                }]
            };
            pool.vdevs.push(ZfsVdev {
                name: name.to_string(),
                class,
                size_bytes: bytes(fields[1]).unwrap_or(0),
                alloc_bytes: bytes(fields[2]),
                health,
                members,
            });
            continue;
        }

        if skipping {
            continue;
        }
        let Some(vdev) = pool.vdevs.last_mut() else {
            continue;
        };
        // Depth 4 is a child of the top-level vdev and takes the next
        // slot; anything deeper sits under a spare-N / replacing-N child
        // and shares that child's slot.
        let slot = if depth == 4 {
            slots += 1;
            slots - 1
        } else {
            slots.saturating_sub(1)
        };
        if is_group(name) {
            continue;
        }
        vdev.members.push(ZfsMember {
            path: name.to_string(),
            health,
            slot,
            dev: None,
        });
    }
    pools
}

/// Imported pools, plus a word when `zpool` couldn't list them: why they're
/// missing, or that they're from the last listing that worked.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Default)]
pub struct ZfsProbe {
    pub pools: Vec<ZfsPool>,
    /// One line for the Volumes tab. `None` when zpool answered, or when
    /// ZFS isn't loaded at all. A machine without ZFS hears nothing.
    pub note: Option<String>,
}

/// Every imported pool, member paths resolved to kernel device names.
///
/// Called once a second from the usage refresh, so the result is cached
/// for `FRESH`. After a timeout the next try waits `RETRY_AFTER_TIMEOUT`
/// instead: a suspended pool can hang zpool, and a stuck process every few
/// seconds would pile up. A failed or timed-out run keeps the pools from
/// the last run that worked; see `settle`.
#[cfg(target_os = "linux")]
pub fn probe() -> ZfsProbe {
    use std::sync::Mutex;

    const FRESH: Duration = Duration::from_secs(5);
    const RETRY_AFTER_TIMEOUT: Duration = Duration::from_secs(300);

    struct Cached {
        at: Instant,
        timed_out: bool,
        probe: ZfsProbe,
    }
    static CACHE: Mutex<Option<Cached>> = Mutex::new(None);

    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(c) = cache.as_ref() {
        let ttl = if c.timed_out {
            RETRY_AFTER_TIMEOUT
        } else {
            FRESH
        };
        if c.at.elapsed() < ttl {
            return c.probe.clone();
        }
    }
    let outcome = run_zpool();
    let timed_out = matches!(outcome, Run::TimedOut);
    // The previous probe's pools are the last ones zpool listed: a failed
    // run carries them over, so they outlast any number of failures.
    let last = cache.take().map(|c| c.probe.pools).unwrap_or_default();
    let zfs_loaded = std::path::Path::new("/sys/module/zfs").exists();
    let probe = settle(outcome, last, zfs_loaded);
    *cache = Some(Cached {
        at: Instant::now(),
        timed_out,
        probe: probe.clone(),
    });
    probe
}

/// How long `zpool list` gets before it's abandoned.
#[cfg(target_os = "linux")]
const ZPOOL_TIMEOUT: Duration = Duration::from_secs(2);

#[cfg(target_os = "linux")]
fn run_zpool() -> Run {
    // zpool lives in sbin, which isn't on a normal user's PATH on Debian.
    const ZPOOL: [&str; 3] = ["zpool", "/usr/sbin/zpool", "/sbin/zpool"];
    const ARGS: [&str; 6] = [
        "list",
        "-v",
        "-P",
        "-p",
        "-o",
        "name,size,allocated,free,health",
    ];

    let mut outcome = Run::Missing;
    for program in ZPOOL {
        outcome = run_with_timeout(program, &ARGS, ZPOOL_TIMEOUT);
        if !matches!(outcome, Run::Missing) {
            break;
        }
    }
    outcome
}

/// Turn a run of zpool into the probe the rest of diskwatch sees. `last`
/// is the pools the previous probe reported.
///
/// A run that fails or times out keeps `last`, with a note saying they're
/// from the last listing that worked. zpool can stall past its 2s on a
/// busy pool, and that's no reason to drop the pools: every ZFS disk would
/// read 0% used again (issue #25) and the Volumes tab would lose them until
/// the next try. Only when zpool has never listed anything are there no
/// pools to show.
#[cfg(target_os = "linux")]
fn settle(outcome: Run, last: Vec<ZfsPool>, zfs_loaded: bool) -> ZfsProbe {
    let text = match outcome {
        Run::Ok(text) => text,
        failed => {
            let kept = !last.is_empty();
            // Only worth a word if ZFS is actually in use here.
            return ZfsProbe {
                note: (kept || zfs_loaded).then(|| failure_note(&failed, kept)),
                pools: last,
            };
        }
    };

    let mut pools = parse_zpool_list(&text);
    for m in pools
        .iter_mut()
        .flat_map(|p| p.vdevs.iter_mut())
        .flat_map(|v| v.members.iter_mut())
    {
        m.dev = kernel_name(&m.path);
    }
    ZfsProbe { pools, note: None }
}

/// `/dev/disk/by-id/nvme-…-part3` → `nvme0n1p3`, `/dev/mapper/crypt` →
/// `dm-2`. `None` for a GUID (device gone), a file vdev, or anything that
/// doesn't land in `/dev/`.
#[cfg(target_os = "linux")]
fn kernel_name(path: &str) -> Option<String> {
    if !path.starts_with("/dev/") {
        return None;
    }
    let real = std::fs::canonicalize(path).ok()?;
    let name = real.to_str()?.strip_prefix("/dev/")?;
    (!name.is_empty() && !name.contains('/')).then(|| name.to_string())
}

#[cfg(target_os = "linux")]
enum Run {
    Ok(String),
    Missing,
    TimedOut,
    Failed(Option<i32>),
}

/// `kept` says the pools on screen are from an earlier listing.
#[cfg(target_os = "linux")]
fn failure_note(run: &Run, kept: bool) -> String {
    let why = match run {
        Run::Ok(_) => return String::new(),
        Run::Missing => "ZFS is loaded but zpool was not found".to_string(),
        Run::TimedOut => format!(
            "zpool list did not answer within {}s",
            ZPOOL_TIMEOUT.as_secs()
        ),
        Run::Failed(Some(code)) => format!("zpool list failed (exit {code})"),
        Run::Failed(None) => "zpool list failed".to_string(),
    };
    let shown = if kept {
        "showing pools as last listed"
    } else {
        "pools not shown"
    };
    format!("{why}; {shown}")
}

/// Run a command for its stdout, giving up after `timeout`.
///
/// `LC_ALL=C` keeps state names such as ONLINE in English; libzfs passes
/// them through gettext, and the parser and the colours match on them.
/// stdout is drained on a thread so a large listing can't fill the pipe
/// and deadlock the wait. A killed child is reaped on a thread too: one
/// stuck in the kernel on a suspended pool can't die until the kernel lets
/// go, and the UI mustn't wait for that.
#[cfg(target_os = "linux")]
fn run_with_timeout(program: &str, args: &[&str], timeout: Duration) -> Run {
    use std::io::Read;
    use std::process::{Command, Stdio};

    let mut child = match Command::new(program)
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Run::Missing,
        Err(_) => return Run::Failed(None),
    };
    let Some(mut stdout) = child.stdout.take() else {
        return Run::Failed(None);
    };
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });

    let abandon = |mut child: std::process::Child| {
        let _ = child.kill();
        std::thread::spawn(move || child.wait());
        Run::TimedOut
    };
    let deadline = Instant::now() + timeout;
    let Ok(buf) = rx.recv_timeout(timeout) else {
        return abandon(child);
    };
    // stdout is closed, so the process is on its way out.
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => {
                return Run::Ok(String::from_utf8_lossy(&buf).into_owned());
            }
            Ok(Some(status)) => return Run::Failed(status.code()),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(None) => return abandon(child),
            Err(_) => return Run::Failed(None),
        }
    }
}

#[cfg(test)]
pub(crate) mod fixtures {
    //! `zpool list -v -P -p -o name,size,allocated,free,health` output for
    //! the pool shapes diskwatch has to get right. Spacing follows the
    //! printf formats in OpenZFS `zpool_main.c`; `_V24` fixtures are the
    //! 2.4 layout, where vdev rows honour `-o` and section rows come from
    //! `print_line()`, the rest are ≤ 2.3 with nine fixed vdev columns.

    /// One pool on one partition, addressed by-id.
    pub const SINGLE: &str = "\
NAME                                                                         SIZE         ALLOC          FREE    HEALTH
tank                                                                 994558394368  412334399488  582223994880    ONLINE
  /dev/disk/by-id/ata-Samsung_SSD_870_EVO_1TB_S6PTNZ0R612345X-part3  995635478528  412334399488  582223994880        -         -      4     41      -    ONLINE
";

    /// Two whole disks mirrored; zpool partitioned them, so -P shows -part1.
    pub const MIRROR: &str = "\
NAME                                                                         SIZE          ALLOC           FREE    HEALTH
backup                                                              3985729650688  2103012589568  1882717061120    ONLINE
  mirror-0                                                          3985729650688  2103012589568  1882717061120        -         -     11     52      -    ONLINE
    /dev/disk/by-id/ata-WDC_WD40EFRX-68N32N0_WD-WCC7K4XYZ123-part1  4000776716288      -      -        -         -      -      -      -    ONLINE
    /dev/disk/by-id/ata-WDC_WD40EFRX-68N32N0_WD-WCC7K7ABC456-part1  4000776716288      -      -        -         -      -      -      -    ONLINE
";

    /// raidz1 across three disks, by kernel name.
    pub const RAIDZ1: &str = "\
NAME                     SIZE          ALLOC           FREE    HEALTH
media          11995970224128  6598134841344  5397835382784    ONLINE
  raidz1-0     11995970224128  6598134841344  5397835382784        -         -      2     55      -    ONLINE
    /dev/sdb1  4000776716288      -      -        -         -      -      -      -    ONLINE
    /dev/sdc1  4000776716288      -      -        -         -      -      -      -    ONLINE
    /dev/sdd1  4000776716288      -      -        -         -      -      -      -    ONLINE
";

    /// A mirror for data plus every other class: a special mirror, a log,
    /// an L2ARC cache device and an idle hot spare.
    pub const CLASSES: &str = "\
NAME                         SIZE          ALLOC           FREE    HEALTH
tank                4485716738048  1250157830144  3235558907904    ONLINE
  mirror-0          3985729650688  1201865441280  2783864209408        -         -      7     30      -    ONLINE
    /dev/sda1       4000776716288      -      -        -         -      -      -      -    ONLINE
    /dev/sdb1       4000776716288      -      -        -         -      -      -      -    ONLINE
special                 -      -      -        -         -      -      -      -         -
  mirror-1          499987087360  48292388864  451694698496        -         -      3      9      -    ONLINE
    /dev/nvme0n1p1  500105217024      -      -        -         -      -      -      -    ONLINE
    /dev/nvme1n1p1  500105217024      -      -        -         -      -      -      -    ONLINE
logs                    -      -      -        -         -      -      -      -         -
  /dev/nvme2n1p1    17179869184  1310720  16641687552        -         -      0      0      -    ONLINE
cache                   -      -      -        -         -      -      -      -         -
  /dev/nvme2n1p2    233001975808  101468766208  131441754112        -         -      0     43      -    ONLINE
spare                   -      -      -        -         -      -      -      -         -
  /dev/sdc1         4000776716288      -      -        -         -      -      -      -     AVAIL
";

    /// The same pool as OpenZFS 2.4 prints it.
    pub const CLASSES_V24: &str = "\
NAME                         SIZE          ALLOC           FREE    HEALTH
tank                4485716738048  1250157830144  3235558907904    ONLINE
  mirror-0          3985729650688  1201865441280  2783864209408    ONLINE
    /dev/sda1       4000776716288      -      -    ONLINE
    /dev/sdb1       4000776716288      -      -    ONLINE
special                         -              -              -         -
  mirror-1          499987087360  48292388864  451694698496    ONLINE
    /dev/nvme0n1p1  500105217024      -      -    ONLINE
    /dev/nvme1n1p1  500105217024      -      -    ONLINE
logs                            -              -              -         -
  /dev/nvme2n1p1    17179869184  1310720  16641687552    ONLINE
cache                           -              -              -         -
  /dev/nvme2n1p2    233001975808  101468766208  131441754112    ONLINE
spare                           -              -              -         -
  /dev/sdc1         4000776716288      -      -     AVAIL
";

    /// Issue #25: Ubuntu root-on-ZFS. bpool and rpool are each mirrored
    /// across the same-numbered partitions of two NVMe disks; /boot/efi is
    /// a plain vfat partition beside them.
    pub const UBUNTU: &str = "\
NAME                                                                           SIZE        ALLOC          FREE    HEALTH
bpool                                                                    4160749568    412090368    3748659200    ONLINE
  mirror-0                                                             4160749568  412090368  3748659200        -         -      0      9      -    ONLINE
    /dev/disk/by-id/nvme-Sabrent_SB-RKT4P-1TB_48790469800117-part2     4294967296      -      -        -         -      -      -      -    ONLINE
    /dev/disk/by-id/nvme-Lexar_SSD_NM790_1TB_QLD587H007580P2202-part2  4294967296      -      -        -         -      -      -      -    ONLINE
rpool                                                                  991111553024  58128883712  932982669312    ONLINE
  mirror-0                                                             991111553024  58128883712  932982669312        -         -      3      5      -    ONLINE
    /dev/disk/by-id/nvme-Sabrent_SB-RKT4P-1TB_48790469800117-part3     994835120128      -      -        -         -      -      -      -    ONLINE
    /dev/disk/by-id/nvme-Lexar_SSD_NM790_1TB_QLD587H007580P2202-part3  994835120128      -      -        -         -      -      -      -    ONLINE
";

    /// Issue #25's pools as OpenZFS 2.4 prints them.
    pub const UBUNTU_V24: &str = "\
NAME                                                                           SIZE        ALLOC          FREE    HEALTH
bpool                                                                    4160749568    412090368    3748659200    ONLINE
  mirror-0                                                             4160749568  412090368  3748659200    ONLINE
    /dev/disk/by-id/nvme-Sabrent_SB-RKT4P-1TB_48790469800117-part2     4294967296      -      -    ONLINE
    /dev/disk/by-id/nvme-Lexar_SSD_NM790_1TB_QLD587H007580P2202-part2  4294967296      -      -    ONLINE
rpool                                                                  991111553024  58128883712  932982669312    ONLINE
  mirror-0                                                             991111553024  58128883712  932982669312    ONLINE
    /dev/disk/by-id/nvme-Sabrent_SB-RKT4P-1TB_48790469800117-part3     994835120128      -      -    ONLINE
    /dev/disk/by-id/nvme-Lexar_SSD_NM790_1TB_QLD587H007580P2202-part3  994835120128      -      -    ONLINE
";

    /// Two DEGRADED pools: a mirror with one member taken OFFLINE, and a
    /// raidz1 that lost one disk outright (listed by GUID) and has a hot
    /// spare covering a FAULTED one through a `spare-2` interior vdev.
    pub const DEGRADED: &str = "\
NAME                                                                             SIZE          ALLOC           FREE    HEALTH
rpool                                                                    991111553024    58128883712   932982669312  DEGRADED
  mirror-0                                                             991111553024  58128883712  932982669312        -         -      3      5      -  DEGRADED
    /dev/disk/by-id/nvme-Sabrent_SB-RKT4P-1TB_48790469800117-part3     994835120128      -      -        -         -      -      -      -   OFFLINE
    /dev/disk/by-id/nvme-Lexar_SSD_NM790_1TB_QLD587H007580P2202-part3  994835120128      -      -        -         -      -      -      -    ONLINE
media                                                                  11995970224128  6598134841344  5397835382784  DEGRADED
  raidz1-0                                                             11995970224128  6598134841344  5397835382784        -         -      2     55      -  DEGRADED
    /dev/sdb1                                                          4000776716288      -      -        -         -      -      -      -    ONLINE
    2817390427383737393                                                    -      -      -        -         -      -      -      -   UNAVAIL
    spare-2                                                                -      -      -        -         -      -      -      -  DEGRADED
      /dev/sdd1                                                        4000776716288      -      -        -         -      -      -      -   FAULTED
      /dev/sde1                                                        4000776716288      -      -        -         -      -      -      -    ONLINE
spare                                                                      -      -      -        -         -      -      -      -         -
  /dev/sde1                                                            4000776716288      -      -        -         -      -      -      -     INUSE
";

    /// Kernel names the fixture paths resolve to on the issue #25 machine
    /// (the Sabrent is nvme0n1, the Lexar nvme1n1); kernel-named paths
    /// resolve to themselves.
    pub fn resolve(path: &str) -> Option<String> {
        if let Some(part) = path
            .rsplit("-part")
            .next()
            .filter(|_| path.contains("-part"))
        {
            if path.contains("Sabrent") {
                return Some(format!("nvme0n1p{part}"));
            }
            if path.contains("Lexar") {
                return Some(format!("nvme1n1p{part}"));
            }
            if path.contains("Samsung") {
                return Some(format!("sda{part}"));
            }
            if path.contains("WCC7K4XYZ123") {
                return Some(format!("sdb{part}"));
            }
            if path.contains("WCC7K7ABC456") {
                return Some(format!("sdc{part}"));
            }
        }
        path.strip_prefix("/dev/").map(str::to_string)
    }

    /// Parse a fixture and resolve its member paths, as `probe()` would.
    pub fn pools(text: &str) -> Vec<super::ZfsPool> {
        let mut pools = super::parse_zpool_list(text);
        for m in pools
            .iter_mut()
            .flat_map(|p| p.vdevs.iter_mut())
            .flat_map(|v| v.members.iter_mut())
        {
            m.dev = resolve(&m.path);
        }
        pools
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    fn shares(v: &ZfsVdev) -> Vec<(String, u64)> {
        v.member_shares()
            .into_iter()
            .map(|(m, b)| (m.display().to_string(), b))
            .collect()
    }

    #[test]
    fn single_disk_pool_on_a_by_id_partition() {
        let pools = pools(SINGLE);
        assert_eq!(pools.len(), 1);
        let p = &pools[0];
        assert_eq!(p.name, "tank");
        assert_eq!(p.size_bytes, 994_558_394_368);
        assert_eq!(p.alloc_bytes, 412_334_399_488);
        assert_eq!(p.free_bytes, 582_223_994_880);
        assert_eq!(p.health, "ONLINE");
        assert_eq!(p.vdevs.len(), 1);
        let v = &p.vdevs[0];
        assert_eq!(v.layout(), "disk");
        assert_eq!(v.class, VdevClass::Data);
        assert_eq!(v.alloc_bytes, Some(412_334_399_488));
        assert_eq!(v.members.len(), 1);
        assert_eq!(
            v.members[0].path,
            "/dev/disk/by-id/ata-Samsung_SSD_870_EVO_1TB_S6PTNZ0R612345X-part3"
        );
        assert_eq!(v.members[0].display(), "sda3");
        assert_eq!(shares(v), vec![("sda3".to_string(), 412_334_399_488)]);
    }

    #[test]
    fn every_mirror_member_holds_the_whole_allocation() {
        let pools = pools(MIRROR);
        let v = &pools[0].vdevs[0];
        assert_eq!(v.name, "mirror-0");
        assert_eq!(v.layout(), "mirror");
        assert_eq!(v.members.len(), 2);
        assert_eq!(
            shares(v),
            vec![
                ("sdb1".to_string(), 2_103_012_589_568),
                ("sdc1".to_string(), 2_103_012_589_568),
            ]
        );
    }

    #[test]
    fn raidz_splits_its_allocation_evenly() {
        // ALLOC on a raidz vdev is raw, parity included, and striped over
        // every child: each disk holds a third of it.
        let pools = pools(RAIDZ1);
        let v = &pools[0].vdevs[0];
        assert_eq!(v.layout(), "raidz1");
        let s = shares(v);
        assert_eq!(s.len(), 3);
        for (dev, bytes) in &s {
            assert_eq!(*bytes, 6_598_134_841_344 / 3, "{dev}");
        }
        assert_eq!(
            s.iter().map(|(d, _)| d.as_str()).collect::<Vec<_>>(),
            ["sdb1", "sdc1", "sdd1"]
        );
    }

    #[test]
    fn allocation_classes_are_told_apart() {
        for text in [CLASSES, CLASSES_V24] {
            let pools = pools(text);
            assert_eq!(pools.len(), 1, "the section rows are not pools");
            let classes: Vec<(VdevClass, &str)> = pools[0]
                .vdevs
                .iter()
                .map(|v| (v.class, v.layout()))
                .collect();
            assert_eq!(
                classes,
                [
                    (VdevClass::Data, "mirror"),
                    (VdevClass::Special, "mirror"),
                    (VdevClass::Log, "disk"),
                    (VdevClass::Cache, "disk"),
                    (VdevClass::Spare, "disk"),
                ]
            );
            let by_class = |c: VdevClass| {
                let v = pools[0].vdevs.iter().find(|v| v.class == c).unwrap();
                shares(v)
            };
            assert_eq!(
                by_class(VdevClass::Special),
                vec![
                    ("nvme0n1p1".to_string(), 48_292_388_864),
                    ("nvme1n1p1".to_string(), 48_292_388_864),
                ]
            );
            // The log's own allocation is real data on that disk.
            assert_eq!(
                by_class(VdevClass::Log),
                vec![("nvme2n1p1".to_string(), 1_310_720)]
            );
            // L2ARC reports 101 GB allocated, but it's evictable copies of
            // blocks that already live on the data vdevs; an idle spare
            // reports nothing at all.
            assert!(by_class(VdevClass::Cache).is_empty());
            assert!(by_class(VdevClass::Spare).is_empty());
            let spare = pools[0].vdevs.last().unwrap();
            assert_eq!(spare.health, "AVAIL");
            assert_eq!(spare.alloc_bytes, None);
        }
    }

    #[test]
    fn ubuntu_bpool_and_rpool_span_both_disks() {
        for text in [UBUNTU, UBUNTU_V24] {
            let pools = pools(text);
            let names: Vec<&str> = pools.iter().map(|p| p.name.as_str()).collect();
            assert_eq!(names, ["bpool", "rpool"]);
            let rpool = &pools[1];
            assert_eq!(rpool.size_bytes, 991_111_553_024);
            assert_eq!(rpool.alloc_bytes, 58_128_883_712);
            let members: Vec<&str> = rpool.vdevs[0].members.iter().map(|m| m.display()).collect();
            assert_eq!(members, ["nvme0n1p3", "nvme1n1p3"]);
            let bpool: Vec<&str> = pools[0].vdevs[0]
                .members
                .iter()
                .map(|m| m.display())
                .collect();
            assert_eq!(bpool, ["nvme0n1p2", "nvme1n1p2"]);
        }
    }

    #[test]
    fn degraded_pools_keep_their_shape() {
        let pools = pools(DEGRADED);
        assert_eq!(pools.len(), 2);

        let rpool = &pools[0];
        assert_eq!(rpool.health, "DEGRADED");
        let mirror = &rpool.vdevs[0];
        assert_eq!(mirror.health, "DEGRADED");
        let states: Vec<&str> = mirror.members.iter().map(|m| m.health.as_str()).collect();
        assert_eq!(states, ["OFFLINE", "ONLINE"]);
        // The offline disk still holds every block written before it went
        // offline; both members keep their full copy.
        assert_eq!(shares(mirror).len(), 2);
        assert!(shares(mirror).iter().all(|(_, b)| *b == 58_128_883_712));

        let media = &pools[1];
        let raidz = &media.vdevs[0];
        assert_eq!(raidz.layout(), "raidz1");
        // spare-2 is structure, not a device; its two halves share slot 2.
        let members: Vec<(&str, &str, usize)> = raidz
            .members
            .iter()
            .map(|m| (m.display(), m.health.as_str(), m.slot))
            .collect();
        assert_eq!(
            members,
            [
                ("sdb1", "ONLINE", 0),
                ("2817390427383737393", "UNAVAIL", 1),
                ("sdd1", "FAULTED", 2),
                ("sde1", "ONLINE", 2),
            ]
        );
        // Three columns, so a third each. The spare swap doesn't dilute
        // sdb1's share to a quarter.
        let third = 6_598_134_841_344 / 3;
        assert!(shares(raidz).iter().all(|(_, b)| *b == third));
        // The GUID of a vanished disk resolves to nothing.
        assert_eq!(raidz.members[1].dev, None);

        let spare = media.vdevs.iter().find(|v| v.class == VdevClass::Spare);
        assert_eq!(spare.map(|v| v.health.as_str()), Some("INUSE"));
    }

    #[test]
    fn no_pools_parses_to_nothing() {
        // Without -H, zpool says so on stdout and exits 0.
        assert!(parse_zpool_list("no pools available\n").is_empty());
        assert!(parse_zpool_list("").is_empty());
    }

    #[test]
    fn a_faulted_pool_is_still_a_pool() {
        // An unavailable pool prints `-` for its sizes; it must not be
        // mistaken for a class section row, which is all dashes.
        let text = "\
NAME        SIZE  ALLOC   FREE    HEALTH
tank           -      -      -   FAULTED
  mirror-0     -      -      -        -         -      -      -      -   UNAVAIL
    /dev/sdb1  -      -      -        -         -      -      -      -   UNAVAIL
";
        let pools = parse_zpool_list(text);
        assert_eq!(pools.len(), 1);
        assert_eq!(pools[0].health, "FAULTED");
        assert_eq!(pools[0].size_bytes, 0);
        assert!(pools[0].vdevs[0].member_shares().is_empty());
    }

    #[test]
    fn removed_vdevs_are_skipped() {
        let text = "\
NAME          SIZE  ALLOC   FREE    HEALTH
tank         1000    400    600    ONLINE
  /dev/sdb1  1100    400    600        -         -      1     40      -    ONLINE
  indirect-1     -      -      -        -         -      -      -      -    ONLINE
";
        let pools = parse_zpool_list(text);
        assert_eq!(pools[0].vdevs.len(), 1);
        assert_eq!(pools[0].vdevs[0].name, "/dev/sdb1");
    }

    #[test]
    fn draid_names_reduce_to_their_layout() {
        let v = ZfsVdev {
            name: "draid2:4d:12c:1s-0".into(),
            ..Default::default()
        };
        assert_eq!(v.layout(), "draid2");
        let v = ZfsVdev {
            name: "raidz3-1".into(),
            ..Default::default()
        };
        assert_eq!(v.layout(), "raidz3");
        // A distributed spare is a leaf, not a group.
        assert!(!is_group("draid2-0-0"));
    }

    /// No zpool on the machine: no pools, and no note unless ZFS is loaded
    /// (it isn't, anywhere this test can run without the binary).
    #[cfg(target_os = "linux")]
    #[test]
    fn a_missing_binary_is_not_an_error() {
        assert!(matches!(
            run_with_timeout("/nonexistent/zpool", &["list"], Duration::from_millis(500)),
            Run::Missing
        ));
        if !std::path::Path::new("/sys/module/zfs").exists() {
            let outcome = run_zpool();
            assert!(!matches!(outcome, Run::TimedOut));
            let probe = settle(outcome, Vec::new(), false);
            if probe.pools.is_empty() {
                assert!(probe.note.is_none(), "{:?}", probe.note);
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_failed_run_keeps_the_last_pools() {
        // One slow zpool must not put issue #25's disks back at 0% used:
        // the pools it listed last time stand, and the tab says so.
        let last = pools(UBUNTU);
        for (failed, why) in [
            (Run::TimedOut, "zpool list did not answer within 2s"),
            (Run::Failed(Some(1)), "zpool list failed (exit 1)"),
        ] {
            let probe = settle(failed, last.clone(), true);
            let names: Vec<&str> = probe.pools.iter().map(|p| p.name.as_str()).collect();
            assert_eq!(names, ["bpool", "rpool"]);
            assert_eq!(probe.pools[1].vdevs[0].members[0].display(), "nvme0n1p3");
            assert_eq!(
                probe.note.as_deref(),
                Some(format!("{why}; showing pools as last listed").as_str())
            );
        }

        // Never listed: nothing to keep.
        let probe = settle(Run::TimedOut, Vec::new(), true);
        assert!(probe.pools.is_empty());
        assert_eq!(
            probe.note.as_deref(),
            Some("zpool list did not answer within 2s; pools not shown")
        );

        // The next run that works replaces them, down to no pools at all.
        let probe = settle(Run::Ok(MIRROR.to_string()), last.clone(), true);
        assert_eq!(probe.pools[0].name, "backup");
        assert_eq!(probe.note, None);
        let probe = settle(Run::Ok("no pools available\n".to_string()), last, true);
        assert!(probe.pools.is_empty());
        assert_eq!(probe.note, None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_hung_command_is_abandoned() {
        let start = Instant::now();
        let out = run_with_timeout("sleep", &["10"], Duration::from_millis(200));
        assert!(matches!(out, Run::TimedOut));
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(matches!(
            run_with_timeout("false", &[], Duration::from_secs(2)),
            Run::Failed(Some(1))
        ));
        assert!(matches!(
            run_with_timeout("echo", &["ok"], Duration::from_secs(2)),
            Run::Ok(s) if s == "ok\n"
        ));
    }
}
