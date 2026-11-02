//! The mount list, read so that one mount can't stall the program.
//!
//! sysinfo reads every mount's size with `statvfs` in one pass, on whatever
//! thread asks. A mount that never answers, such as a stuck FUSE daemon, an
//! ntfs-3g volume or an automount waiting on a disk, holds that call in
//! state D. diskwatch made it on the main thread before printing or drawing
//! anything, so one such mount meant the TUI and `--diag` never started
//! (#34). On Linux this module lists mounts the way sysinfo 0.32 does, then
//! asks each one on its own thread with a time limit. A mount that doesn't
//! answer is left out of the list and named by [`not_responding`]. Its
//! thread is left waiting rather than joined by another, so a dead mount
//! costs one thread, not one per refresh.
//!
//! Other platforms still use sysinfo directly.

use std::path::PathBuf;
#[cfg(any(target_os = "linux", test))]
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::{mpsc, Arc, Mutex, MutexGuard},
    thread,
    time::{Duration, Instant},
};

/// One mounted filesystem: the fields diskwatch used to read from
/// `sysinfo::Disk`.
#[derive(Debug, Clone)]
pub struct Mount {
    /// The mount source, such as `/dev/nvme0n1p2`.
    pub device: String,
    pub mount_point: PathBuf,
    pub fs_type: String,
    pub total: u64,
    pub available: u64,
    pub removable: bool,
    pub read_only: bool,
}

/// How long a refresh waits for the slowest mount. A healthy `statvfs`
/// takes microseconds, so this only ever matters for a mount that's stuck.
#[cfg(target_os = "linux")]
const BUDGET: Duration = Duration::from_secs(1);

/// What one `statvfs` returned.
#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Space {
    total: u64,
    available: u64,
    read_only: bool,
}

#[cfg(any(target_os = "linux", test))]
#[derive(Default)]
struct Probes {
    /// Mount points whose `statvfs` hasn't returned yet.
    waiting: HashSet<PathBuf>,
    /// The mounts the last list left out because they didn't answer.
    #[cfg(target_os = "linux")]
    not_responding: Vec<PathBuf>,
}

#[cfg(any(target_os = "linux", test))]
fn lock(probes: &Mutex<Probes>) -> MutexGuard<'_, Probes> {
    // A probe thread holds the lock only to update two collections, so a
    // panic elsewhere can't leave them half-written.
    probes.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(target_os = "linux")]
fn shared() -> &'static Arc<Mutex<Probes>> {
    static PROBES: std::sync::OnceLock<Arc<Mutex<Probes>>> = std::sync::OnceLock::new();
    PROBES.get_or_init(Default::default)
}

/// Ask `probe` about each of `points` on its own thread, and wait up to
/// `budget` in total. A point missing from the result didn't answer in
/// time, or is still waiting from an earlier call, in which case it isn't
/// asked again. `None` means it answered with an error.
#[cfg(any(target_os = "linux", test))]
fn ask_all<F>(
    points: &[PathBuf],
    probe: F,
    budget: Duration,
    probes: &Arc<Mutex<Probes>>,
) -> HashMap<PathBuf, Option<Space>>
where
    F: Fn(&Path) -> Option<Space> + Clone + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    let mut asked = 0;
    for point in points {
        if !lock(probes).waiting.insert(point.clone()) {
            continue;
        }
        let (tx, probe, mine, path) =
            (tx.clone(), probe.clone(), Arc::clone(probes), point.clone());
        let spawned = thread::Builder::new()
            .name("statvfs".into())
            .spawn(move || {
                let space = probe(&path);
                lock(&mine).waiting.remove(&path);
                // The caller may have stopped waiting; that's fine.
                let _ = tx.send((path, space));
            });
        match spawned {
            Ok(_) => asked += 1,
            Err(_) => {
                lock(probes).waiting.remove(point);
            }
        }
    }
    drop(tx);

    let deadline = Instant::now() + budget;
    let mut answers = HashMap::new();
    while asked > 0 {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok((point, space)) => {
                answers.insert(point, space);
                asked -= 1;
            }
            Err(_) => break,
        }
    }
    answers
}

/// The mounts the last [`list`] left out because they didn't answer, in
/// mount-table order.
pub fn not_responding() -> Vec<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        lock(shared()).not_responding.clone()
    }
    #[cfg(not(target_os = "linux"))]
    {
        Vec::new()
    }
}

/// Every mounted filesystem with a size, as sysinfo would list it, minus any
/// that didn't answer.
#[cfg(target_os = "linux")]
pub fn list() -> Vec<Mount> {
    let entries = parse_mounts(&std::fs::read_to_string("/proc/mounts").unwrap_or_default());
    let mut points: Vec<PathBuf> = Vec::new();
    for e in &entries {
        if !points.contains(&e.mount_point) {
            points.push(e.mount_point.clone());
        }
    }
    let answers = ask_all(&points, statvfs, BUDGET, shared());
    let removable = removable_devices();

    let mut out = Vec::new();
    let mut not_responding: Vec<PathBuf> = Vec::new();
    for e in entries {
        match answers.get(&e.mount_point) {
            None => {
                if !not_responding.contains(&e.mount_point) {
                    not_responding.push(e.mount_point);
                }
            }
            // sysinfo leaves out a mount whose statvfs fails or reports no
            // blocks, so this does too.
            Some(None) => {}
            Some(Some(space)) if space.total == 0 => {}
            Some(Some(space)) => out.push(Mount {
                removable: removable.iter().any(|r| r.as_os_str() == e.device.as_str()),
                device: e.device,
                mount_point: e.mount_point,
                fs_type: e.fs_type,
                total: space.total,
                available: space.available,
                read_only: space.read_only,
            }),
        }
    }
    lock(shared()).not_responding = not_responding;
    out
}

#[cfg(not(target_os = "linux"))]
pub fn list() -> Vec<Mount> {
    sysinfo::Disks::new_with_refreshed_list()
        .list()
        .iter()
        .map(|d| Mount {
            device: d.name().to_string_lossy().into_owned(),
            mount_point: d.mount_point().to_path_buf(),
            fs_type: d.file_system().to_string_lossy().into_owned(),
            total: d.total_space(),
            available: d.available_space(),
            removable: d.is_removable(),
            read_only: d.is_read_only(),
        })
        .collect()
}

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, PartialEq, Eq)]
struct Entry {
    device: String,
    mount_point: PathBuf,
    fs_type: String,
}

/// `/proc/mounts`, filtered the way sysinfo 0.32 filters it with its default
/// features, so the list keeps the mounts diskwatch has always shown.
#[cfg(any(target_os = "linux", test))]
fn parse_mounts(content: &str) -> Vec<Entry> {
    content
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let device = fields.next()?;
            let mount = fields
                .next()?
                .replace("\\134", "\\")
                .replace("\\040", " ")
                .replace("\\011", "\t")
                .replace("\\012", "\n");
            let fs_type = fields.next().unwrap_or("");
            let skipped_type = matches!(
                fs_type,
                "rootfs"
                    | "sysfs"
                    | "proc"
                    | "devtmpfs"
                    | "cgroup"
                    | "cgroup2"
                    | "pstore"
                    | "squashfs"
                    | "rpc_pipefs"
                    | "iso9660"
                    | "tmpfs"
                    // statvfs on these can hang; sysinfo skips them unless
                    // built with `linux-netdevs`.
                    | "cifs"
                    | "nfs"
                    | "nfs4"
            );
            let skipped_place = mount.starts_with("/sys")
                || mount.starts_with("/proc")
                || (mount.starts_with("/run") && !mount.starts_with("/run/media"))
                || device.starts_with("sunrpc");
            (!skipped_type && !skipped_place).then(|| Entry {
                device: device.to_string(),
                mount_point: PathBuf::from(mount),
                fs_type: fs_type.to_string(),
            })
        })
        .collect()
}

/// Devices with a `usb-` name under /dev/disk/by-id, which is how sysinfo
/// decides a disk is removable.
#[cfg(target_os = "linux")]
fn removable_devices() -> Vec<PathBuf> {
    let Ok(dir) = std::fs::read_dir("/dev/disk/by-id/") else {
        return Vec::new();
    };
    dir.flatten()
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n.starts_with("usb-"))
        })
        .filter_map(|e| e.path().canonicalize().ok())
        .collect()
}

// The fields are u64 on 64-bit targets and narrower on armv5te.
#[cfg(target_os = "linux")]
#[allow(clippy::unnecessary_cast)]
fn statvfs(path: &Path) -> Option<Space> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: an all-zero statvfs is a valid value for libc to overwrite.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    loop {
        // SAFETY: `path` is NUL-terminated and outlives the call, and `st`
        // is a valid, writable statvfs.
        if unsafe { libc::statvfs(path.as_ptr(), &mut st) } == 0 {
            break;
        }
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return None;
        }
    }
    let bsize = st.f_bsize as u64;
    Some(Space {
        total: bsize.saturating_mul(st.f_blocks as u64),
        available: bsize.saturating_mul(st.f_bavail as u64),
        read_only: st.f_flag & libc::ST_RDONLY != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const OK: Space = Space {
        total: 100,
        available: 40,
        read_only: false,
    };

    #[test]
    fn parses_and_filters_like_sysinfo() {
        let table = "\
/dev/nvme0n1p3 / btrfs rw,relatime 0 0
proc /proc proc rw 0 0
tmpfs /tmp tmpfs rw 0 0
/dev/sda1 /mnt/My\\040Disk ntfs3 rw 0 0
portal /run/user/1000/doc fuse.portal rw 0 0
/dev/sdb1 /run/media/me/USB vfat rw 0 0
server:/x /mnt/nfs nfs4 rw 0 0
sunrpc /var/lib/nfs/rpc_pipefs rpc_pipefs rw 0 0
/dev/sdc1 /mnt/NTFS1 fuseblk rw,allow_other 0 0
systemd-1 /mnt/auto autofs rw 0 0
";
        let got: Vec<(String, String)> = parse_mounts(table)
            .into_iter()
            .map(|e| (e.mount_point.display().to_string(), e.fs_type))
            .collect();
        let want = [
            ("/", "btrfs"),
            ("/mnt/My Disk", "ntfs3"),
            ("/run/media/me/USB", "vfat"),
            ("/mnt/NTFS1", "fuseblk"),
            ("/mnt/auto", "autofs"),
        ];
        assert_eq!(
            got,
            want.map(|(m, t)| (m.to_string(), t.to_string())).to_vec()
        );
    }

    /// A probe that blocks until the test lets it go, standing in for a
    /// mount whose daemon never answers.
    fn stuck_probe(
        stuck: &'static str,
        calls: Arc<AtomicUsize>,
        release: Arc<Mutex<mpsc::Receiver<()>>>,
    ) -> impl Fn(&Path) -> Option<Space> + Clone + Send + 'static {
        move |p: &Path| {
            if p == Path::new(stuck) {
                calls.fetch_add(1, Ordering::SeqCst);
                let _ = release.lock().unwrap().recv();
            }
            Some(OK)
        }
    }

    #[test]
    fn a_stuck_mount_costs_one_budget_and_one_thread() {
        let probes = Arc::new(Mutex::new(Probes::default()));
        let calls = Arc::new(AtomicUsize::new(0));
        let (release_tx, release_rx) = mpsc::channel();
        let probe = stuck_probe("/stuck", calls.clone(), Arc::new(Mutex::new(release_rx)));
        let points = [
            PathBuf::from("/"),
            PathBuf::from("/stuck"),
            PathBuf::from("/home"),
        ];

        let started = Instant::now();
        let first = ask_all(&points, probe.clone(), Duration::from_millis(200), &probes);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(first.len(), 2, "{first:?}");
        assert!(!first.contains_key(Path::new("/stuck")));

        // Still stuck: answered at once, and no second thread joins it.
        let started = Instant::now();
        let second = ask_all(&points, probe.clone(), Duration::from_secs(5), &probes);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(second.len(), 2);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Once it answers, the next refresh asks it again.
        release_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while lock(&probes).waiting.contains(Path::new("/stuck")) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        let probe = stuck_probe(
            "/never",
            calls.clone(),
            Arc::new(Mutex::new(mpsc::channel().1)),
        );
        let third = ask_all(&points, probe, Duration::from_secs(5), &probes);
        assert_eq!(third.get(Path::new("/stuck")), Some(&Some(OK)));
    }

    #[test]
    fn an_error_is_an_answer() {
        let probes = Arc::new(Mutex::new(Probes::default()));
        let got = ask_all(
            &[PathBuf::from("/gone")],
            |_: &Path| None,
            Duration::from_secs(5),
            &probes,
        );
        assert_eq!(got.get(Path::new("/gone")), Some(&None));
        assert!(lock(&probes).waiting.is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn statvfs_reads_the_root_filesystem() {
        let root = statvfs(Path::new("/")).expect("statvfs /");
        assert!(root.total > 0 && root.available <= root.total, "{root:?}");
    }
}
