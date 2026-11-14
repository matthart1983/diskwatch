//! Process attribution for Hot Files.
//!
//! Neither of the event sources behind the Hot Files tab carries a pid:
//! inotify doesn't report one at all, and FSEvents only would through the
//! Endpoint Security framework, which is entitlement-gated. So the answer
//! to "which process is writing this file" has to be assembled from two
//! other readings, both of them unprivileged:
//!
//! - **Per-process byte rates**, from `sysinfo`, which reads
//!   `/proc/<pid>/io` on Linux and `proc_pidinfo` on macOS. This says who
//!   is doing IO, in bytes, but not to which path.
//! - **Open file descriptors**, from `/proc/<pid>/fd` on Linux and
//!   `PROC_PIDLISTFDS` on macOS. This says who is holding a path open,
//!   but not whether they are writing it.
//!
//! Crossing the two gives a per-path list of candidate processes ordered
//! by how much they are writing. That is an inference, not a measurement,
//! and it is wrong in two ways worth naming to the user rather than
//! papering over:
//!
//! 1. **It is sampled.** The fd scan runs every [`SCAN_INTERVAL`]. A
//!    process that opens a file, writes it and closes it between two scans
//!    never appears, even though its events do. Short-lived writers —
//!    compilers, `git`, package managers — are exactly the ones this
//!    misses.
//! 2. **Without root it is partial.** `/proc/<pid>/io` and `/proc/<pid>/fd`
//!    are readable only for your own uid. On a typical desktop that hides
//!    three quarters of the process table, and every daemon on a server.
//!    [`ProcessCollector::coverage`] reports how many were unreadable so
//!    the tab can say so instead of quietly showing you a fraction.
//!
//! Getting a real pid per event needs fanotify with `FAN_REPORT_PID`
//! (CAP_SYS_ADMIN) or eBPF (root) on Linux. Both are deliberate
//! non-goals here: diskwatch's whole premise is that it runs as you,
//! with no system dependencies.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};

/// How often the per-process byte rates are re-read. Matches the App's
/// 1 Hz usage tick.
const RATE_INTERVAL: Duration = Duration::from_millis(1000);

/// How often the fd tables are walked. A full scan of ~550 processes and
/// ~3000 descriptors costs about 9 ms, which is affordable at 1 Hz but
/// pointless: an fd table changes far more slowly than a byte rate, and
/// the scan is the part that touches every process on the box.
const SCAN_INTERVAL: Duration = Duration::from_millis(2000);

/// Soft cap on the reverse index. A build tree can have tens of thousands
/// of files open across a job server; we only ever look up paths that are
/// already hot, so an unbounded map is pure cost.
const MAX_OWNED_PATHS: usize = 8192;

/// One process, as far as disk IO is concerned.
#[derive(Debug, Clone)]
pub struct ProcessTick {
    pub pid: u32,
    /// Executable name, not the full command line — the tab has one
    /// column for this and a Chrome tab title is not useful in it.
    pub name: String,
    pub read_bps: f64,
    pub write_bps: f64,
}

impl ProcessTick {
    /// Read plus write. What the Hot Files tab sorts owners by: a file
    /// being read hard is as interesting as one being written.
    pub fn total_bps(&self) -> f64 {
        self.read_bps + self.write_bps
    }

    /// `name (pid)`, clipped to `width`. Shared by all three views so a
    /// process is identified the same way whichever one you are in.
    ///
    /// The pid survives clipping and the name gives way: the pid is what
    /// you type into `kill`, and it is the shorter half.
    pub fn label(&self, width: usize) -> String {
        use crate::ui::format::{cells, take_cells};
        let full = format!("{} ({})", self.name, self.pid);
        if cells(&full) <= width {
            return full;
        }
        let pid = format!(" ({})", self.pid);
        let room = width.saturating_sub(cells(&pid) + 1);
        if room == 0 {
            // Narrower than the pid itself: the name is gone entirely and
            // an ellipsis in front of a bare pid is noise.
            return self.pid.to_string().chars().take(width).collect();
        }
        let (name, _) = take_cells(&self.name, room);
        format!("{name}…{pid}")
    }
}

/// How much of the process table we could actually see.
#[derive(Debug, Clone, Copy, Default)]
pub struct Coverage {
    /// Processes whose fd table we read.
    pub visible: usize,
    /// Processes that exist but denied us — almost always another uid.
    pub hidden: usize,
}

impl Coverage {
    pub fn total(&self) -> usize {
        self.visible + self.hidden
    }

    /// True when we are seeing so little of the box that the Hot Files
    /// owner column is more misleading than useful, and the tab should
    /// say why rather than show a mostly-empty column.
    pub fn is_partial(&self) -> bool {
        self.hidden > 0
    }
}

#[derive(Default)]
pub struct ProcessCollector {
    sys: System,
    /// Latest rates, by pid.
    procs: HashMap<u32, ProcessTick>,
    /// Reverse index: absolute path -> pids holding it open.
    owners: HashMap<PathBuf, Vec<u32>>,
    coverage: Coverage,
    last_rates: Option<Instant>,
    last_scan: Option<Instant>,
}

impl ProcessCollector {
    pub fn new() -> Self {
        Self {
            sys: System::new(),
            ..Default::default()
        }
    }

    /// Refresh both readings if their intervals have elapsed. Safe to
    /// call every frame; it rate-limits internally the way `IoCollector`
    /// does.
    pub fn refresh(&mut self) {
        let now = Instant::now();

        let rates_due = self
            .last_rates
            .is_none_or(|t| now.duration_since(t) >= RATE_INTERVAL);
        if rates_due {
            // Elapsed is measured, not assumed: a paused app or a busy
            // frame makes the real gap longer than RATE_INTERVAL, and
            // dividing by the nominal interval would inflate every rate.
            let elapsed = self
                .last_rates
                .map(|t| now.duration_since(t).as_secs_f64())
                .unwrap_or(0.0);
            self.refresh_rates(elapsed);
            self.last_rates = Some(now);
        }

        let scan_due = self
            .last_scan
            .is_none_or(|t| now.duration_since(t) >= SCAN_INTERVAL);
        if scan_due {
            let (owners, coverage) = scan_open_files();
            self.owners = owners;
            self.coverage = coverage;
            self.last_scan = Some(now);
        }
    }

    fn refresh_rates(&mut self, elapsed_secs: f64) {
        self.sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::new().with_disk_usage(),
        );
        self.procs.clear();
        // The first refresh has no previous sample to difference
        // against, so sysinfo reports the process's lifetime totals as
        // the delta. Publishing that as a rate would put a browser that
        // has written 4 GB since login at the top of the table forever.
        if elapsed_secs <= 0.0 {
            return;
        }
        let me = std::process::id();
        for (pid, proc) in self.sys.processes() {
            let pid = pid.as_u32();
            // diskwatch's own reads, its directory walks and /proc scans,
            // ranked it the busiest process on its own screen.
            if pid == me {
                continue;
            }
            // Idle processes stay in. One holding a hot file open is still
            // the answer for that file's PROCESS column; leaving them out
            // left the column blank on every row.
            let usage = proc.disk_usage();
            self.procs.insert(
                pid,
                ProcessTick {
                    pid,
                    name: proc.name().to_string_lossy().into_owned(),
                    read_bps: usage.read_bytes as f64 / elapsed_secs,
                    write_bps: usage.written_bytes as f64 / elapsed_secs,
                },
            );
        }
    }

    /// Processes holding `path` open, busiest first. Empty when nothing
    /// visible has it open — which, unprivileged, is not the same as
    /// nothing having it open.
    pub fn owners_of(&self, path: &Path) -> Vec<&ProcessTick> {
        let Some(pids) = self.owners.get(path) else {
            return Vec::new();
        };
        let mut v: Vec<&ProcessTick> = pids.iter().filter_map(|p| self.procs.get(p)).collect();
        v.sort_by(|a, b| {
            b.total_bps()
                .partial_cmp(&a.total_bps())
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.pid.cmp(&b.pid))
        });
        v
    }

    /// The single best guess at who is responsible for `path`, or `None`.
    ///
    /// "Best" is the holder doing the most IO overall, which is a guess:
    /// we know it has the file open and we know it is busy, but not that
    /// the two facts are connected. A holder doing no measurable IO is
    /// still returned when it is the only one — an idle holder is a
    /// better answer than a blank column.
    pub fn likely_owner(&self, path: &Path) -> Option<&ProcessTick> {
        self.owners_of(path).into_iter().next()
    }

    /// Busiest processes by total bytes per second, regardless of path.
    /// This is the reading that survives when the fd scan misses a
    /// short-lived writer, so the tab shows it alongside the per-path
    /// column rather than instead of it.
    pub fn top(&self, n: usize) -> Vec<ProcessTick> {
        let mut v: Vec<ProcessTick> = self
            .procs
            .values()
            .filter(|p| p.total_bps() > 0.0)
            .cloned()
            .collect();
        v.sort_by(|a, b| {
            b.total_bps()
                .partial_cmp(&a.total_bps())
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.pid.cmp(&b.pid))
        });
        v.truncate(n);
        v
    }

    pub fn coverage(&self) -> Coverage {
        self.coverage
    }
}

/// Walk every process's open files and invert it into path -> pids.
fn scan_open_files() -> (HashMap<PathBuf, Vec<u32>>, Coverage) {
    let mut owners: HashMap<PathBuf, Vec<u32>> = HashMap::new();
    let mut coverage = Coverage::default();
    for pid in list_pids() {
        match open_files(pid) {
            Some(paths) => {
                coverage.visible += 1;
                for p in paths {
                    if owners.len() >= MAX_OWNED_PATHS && !owners.contains_key(&p) {
                        continue;
                    }
                    let e = owners.entry(p).or_default();
                    // A process holding the same file on several
                    // descriptors is one owner, not three.
                    if !e.contains(&pid) {
                        e.push(pid);
                    }
                }
            }
            None => coverage.hidden += 1,
        }
    }
    (owners, coverage)
}

/// Paths worth indexing. Sockets, pipes and anonymous inodes have no
/// path; `/proc`, `/sys` and `/dev` have one but never appear in Hot
/// Files, since the watcher is rooted on real directories.
#[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
fn is_indexable(path: &Path) -> bool {
    let Some(s) = path.to_str() else {
        // A non-UTF-8 filename is legal and we keep it: the Hot Files
        // map is keyed on PathBuf, so it will still match.
        return true;
    };
    s.starts_with('/')
        && !s.starts_with("/proc/")
        && !s.starts_with("/sys/")
        && !s.starts_with("/dev/")
}

#[cfg(target_os = "linux")]
fn list_pids() -> Vec<u32> {
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    dir.filter_map(|e| e.ok()?.file_name().to_str()?.parse::<u32>().ok())
        .collect()
}

/// `None` when the process denied us — the caller counts that as hidden
/// rather than as a process with nothing open.
#[cfg(target_os = "linux")]
fn open_files(pid: u32) -> Option<Vec<PathBuf>> {
    let dir = std::fs::read_dir(format!("/proc/{pid}/fd")).ok()?;
    let mut out = Vec::new();
    for entry in dir.flatten() {
        // A process can exit mid-walk; a dead descriptor is not a denial.
        let Ok(target) = std::fs::read_link(entry.path()) else {
            continue;
        };
        // The kernel appends " (deleted)" to an unlinked file's link
        // target. The path is still the one the events named right up
        // until it was removed, so strip the marker and keep it.
        let target = match target.to_str() {
            Some(s) => match s.strip_suffix(" (deleted)") {
                Some(stripped) => PathBuf::from(stripped),
                None => target,
            },
            None => target,
        };
        if is_indexable(&target) {
            out.push(target);
        }
    }
    Some(out)
}

#[cfg(target_os = "macos")]
mod darwin {
    use libc::{c_int, off_t, vnode_info_path};

    pub const PROC_ALL_PIDS: u32 = 1;
    pub const PROC_PIDFDVNODEPATHINFO: c_int = 2;

    /// `struct proc_fileinfo` from `sys/proc_info.h`. libc binds
    /// `proc_fdinfo` and `vnode_info_path` but not this one, and
    /// `vnode_fdinfowithpath` leads with it, so the offset to the path
    /// is wrong without it.
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct ProcFileInfo {
        pub fi_openflags: u32,
        pub fi_status: u32,
        pub fi_offset: off_t,
        pub fi_type: i32,
        pub fi_guardflags: u32,
    }

    /// `struct vnode_fdinfowithpath`, the payload of a
    /// `PROC_PIDFDVNODEPATHINFO` query.
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct VnodeFdInfoWithPath {
        pub pfi: ProcFileInfo,
        pub pvip: vnode_info_path,
    }
}

#[cfg(target_os = "macos")]
fn list_pids() -> Vec<u32> {
    use std::mem::size_of;
    // Two calls: the first with a null buffer asks how much is needed.
    // The table can grow between them, so the buffer is oversized and the
    // second call's return value, not the first's, decides the length.
    let needed = unsafe { libc::proc_listpids(darwin::PROC_ALL_PIDS, 0, std::ptr::null_mut(), 0) };
    if needed <= 0 {
        return Vec::new();
    }
    let cap = (needed as usize / size_of::<libc::c_int>()) + 64;
    let mut buf: Vec<libc::c_int> = vec![0; cap];
    let got = unsafe {
        libc::proc_listpids(
            darwin::PROC_ALL_PIDS,
            0,
            buf.as_mut_ptr() as *mut libc::c_void,
            (cap * size_of::<libc::c_int>()) as libc::c_int,
        )
    };
    if got <= 0 {
        return Vec::new();
    }
    let n = got as usize / size_of::<libc::c_int>();
    buf.truncate(n);
    // proc_listpids pads the tail with zeroes when the table shrank.
    buf.into_iter()
        .filter(|p| *p > 0)
        .map(|p| p as u32)
        .collect()
}

#[cfg(target_os = "macos")]
fn open_files(pid: u32) -> Option<Vec<PathBuf>> {
    use std::mem::size_of;

    let bytes = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDLISTFDS,
            0,
            std::ptr::null_mut(),
            0,
        )
    };
    // 0 is a process with nothing open; negative is a denial. Only the
    // second is "hidden" — conflating them would report every empty
    // process as one we were refused.
    if bytes < 0 {
        return None;
    }
    if bytes == 0 {
        return Some(Vec::new());
    }
    let count = bytes as usize / size_of::<libc::proc_fdinfo>();
    let mut fds: Vec<libc::proc_fdinfo> = vec![
        libc::proc_fdinfo {
            proc_fd: 0,
            proc_fdtype: 0,
        };
        count + 32
    ];
    let got = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDLISTFDS,
            0,
            fds.as_mut_ptr() as *mut libc::c_void,
            (fds.len() * size_of::<libc::proc_fdinfo>()) as libc::c_int,
        )
    };
    if got < 0 {
        return None;
    }
    fds.truncate(got as usize / size_of::<libc::proc_fdinfo>());

    let mut out = Vec::new();
    for fd in fds {
        if fd.proc_fdtype != libc::PROX_FDTYPE_VNODE as u32 {
            continue;
        }
        let mut info: darwin::VnodeFdInfoWithPath = unsafe { std::mem::zeroed() };
        let n = unsafe {
            libc::proc_pidfdinfo(
                pid as libc::c_int,
                fd.proc_fd,
                darwin::PROC_PIDFDVNODEPATHINFO,
                &mut info as *mut _ as *mut libc::c_void,
                size_of::<darwin::VnodeFdInfoWithPath>() as libc::c_int,
            )
        };
        // A per-descriptor refusal is not a per-process one: a sandboxed
        // fd inside an otherwise readable process just has no path.
        if n < size_of::<darwin::VnodeFdInfoWithPath>() as libc::c_int {
            continue;
        }
        if let Some(p) = vnode_path(&info) {
            if is_indexable(&p) {
                out.push(p);
            }
        }
    }
    Some(out)
}

/// libc declares `vip_path` as `[[c_char; 32]; 32]` to stay on an old
/// rustc, so it needs flattening back into one NUL-terminated buffer.
#[cfg(target_os = "macos")]
fn vnode_path(info: &darwin::VnodeFdInfoWithPath) -> Option<PathBuf> {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let raw = &info.pvip.vip_path;
    let flat: Vec<u8> = raw
        .iter()
        .flat_map(|chunk| chunk.iter())
        .map(|c| *c as u8)
        .collect();
    let end = flat.iter().position(|b| *b == 0).unwrap_or(flat.len());
    if end == 0 {
        return None;
    }
    Some(PathBuf::from(OsStr::from_bytes(&flat[..end])))
}

/// Every other platform gets an empty attribution rather than a build
/// failure: the Hot Files tab already renders a blank owner column, and
/// the banner already explains it.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn list_pids() -> Vec<u32> {
    Vec::new()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn open_files(_pid: u32) -> Option<Vec<PathBuf>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tick(pid: u32, name: &str, read: f64, write: f64) -> ProcessTick {
        ProcessTick {
            pid,
            name: name.to_string(),
            read_bps: read,
            write_bps: write,
        }
    }

    fn collector_with(procs: Vec<ProcessTick>, owners: &[(&str, Vec<u32>)]) -> ProcessCollector {
        let mut c = ProcessCollector::new();
        c.procs = procs.into_iter().map(|p| (p.pid, p)).collect();
        c.owners = owners
            .iter()
            .map(|(p, pids)| (PathBuf::from(p), pids.clone()))
            .collect();
        c
    }

    /// The point of crossing the two readings: of everything holding a
    /// file open, name the one actually moving bytes.
    #[test]
    fn the_busiest_holder_wins_the_owner_column() {
        let c = collector_with(
            vec![
                tick(1, "tail", 0.0, 0.0),
                tick(2, "postgres", 0.0, 9_000_000.0),
                tick(3, "grep", 200.0, 0.0),
            ],
            &[("/var/lib/db/wal", vec![1, 2, 3])],
        );
        let owner = c.likely_owner(Path::new("/var/lib/db/wal")).unwrap();
        assert_eq!(owner.name, "postgres");
        assert_eq!(c.owners_of(Path::new("/var/lib/db/wal")).len(), 3);
    }

    /// An idle holder is still the answer when it is the only one. The
    /// alternative is a blank column on a file we can in fact attribute.
    #[test]
    fn a_holder_doing_no_io_is_better_than_no_owner_at_all() {
        let c = collector_with(
            vec![tick(1, "tail", 0.0, 0.0)],
            &[("/var/log/syslog", vec![1])],
        );
        assert_eq!(
            c.likely_owner(Path::new("/var/log/syslog")).unwrap().name,
            "tail"
        );
    }

    /// The sampling gap, made explicit: the fd scan can name a pid that
    /// the rate refresh has already dropped because the process exited.
    /// That must read as "no owner", not panic or resurrect a stale name.
    #[test]
    fn a_holder_that_has_since_exited_is_dropped_not_reported() {
        let c = collector_with(vec![tick(1, "tail", 0.0, 0.0)], &[("/tmp/gone", vec![99])]);
        assert!(c.likely_owner(Path::new("/tmp/gone")).is_none());
        assert!(c.owners_of(Path::new("/tmp/gone")).is_empty());
    }

    #[test]
    fn an_unwatched_path_has_no_owner() {
        let c = collector_with(vec![tick(1, "tail", 0.0, 0.0)], &[]);
        assert!(c.likely_owner(Path::new("/nothing/here")).is_none());
    }

    /// Sorting by read+write, not by writes: a file being read at
    /// 100 MB/s is the answer to "why is the disk busy" too.
    #[test]
    fn owners_rank_on_reads_as_well_as_writes() {
        let c = collector_with(
            vec![
                tick(1, "rsync", 50_000_000.0, 0.0),
                tick(2, "vim", 0.0, 4096.0),
            ],
            &[("/srv/backup.tar", vec![1, 2])],
        );
        assert_eq!(
            c.likely_owner(Path::new("/srv/backup.tar")).unwrap().name,
            "rsync"
        );
    }

    /// `top()` is the fallback for writers the fd scan never caught, so
    /// it must not be restricted to processes that hold an indexed path.
    #[test]
    fn top_writers_are_reported_without_any_path_attribution() {
        let c = collector_with(
            vec![
                tick(1, "dd", 0.0, 800_000_000.0),
                tick(2, "vim", 0.0, 4096.0),
            ],
            &[],
        );
        let top = c.top(2);
        assert_eq!(top[0].name, "dd");
        assert_eq!(top[1].name, "vim");
    }

    #[test]
    fn a_label_never_exceeds_the_column_it_was_given() {
        let long = tick(1234, "systemd-journald-with-a-silly-name", 0.0, 0.0);
        let short = tick(9, "vim", 0.0, 0.0);
        for w in 1..40usize {
            assert!(long.label(w).chars().count() <= w, "long at {w}");
            assert!(short.label(w).chars().count() <= w, "short at {w}");
        }
        // The pid is the half worth keeping, so it survives while there is
        // room for it at all.
        assert!(long.label(22).contains("(1234)"));
        assert_eq!(short.label(20), "vim (9)");
    }

    #[test]
    fn coverage_counts_what_we_could_not_read() {
        let c = Coverage {
            visible: 125,
            hidden: 431,
        };
        assert_eq!(c.total(), 556);
        assert!(c.is_partial());
        assert!(!Coverage {
            visible: 10,
            hidden: 0
        }
        .is_partial());
    }

    /// Paths with no bearing on Hot Files must stay out of the index —
    /// it is capped, and /proc alone would fill a good part of it.
    #[test]
    fn pseudo_filesystems_are_not_indexed() {
        assert!(is_indexable(Path::new("/home/matt/src/main.rs")));
        assert!(is_indexable(Path::new("/var/log/syslog")));
        assert!(!is_indexable(Path::new("/proc/406494/fd")));
        assert!(!is_indexable(Path::new("/sys/block/nvme0n1/stat")));
        assert!(!is_indexable(Path::new("/dev/null")));
        // notify reports absolute paths, so a relative one (a socket or
        // pipe link target) can never match and is not worth indexing.
        assert!(!is_indexable(Path::new("socket:[2235587]")));
        assert!(!is_indexable(Path::new("anon_inode:[eventpoll]")));
    }

    /// The real scan, against the process we are running in. It must find
    /// this test binary's own open file and count itself as visible.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn scanning_our_own_process_finds_a_file_we_hold_open() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("diskwatch-proc-test-{}", std::process::id()));
        let f = std::fs::File::create(&path).expect("temp file");
        let me = std::process::id();

        let held = open_files(me).expect("our own fd table must be readable");
        // The path we opened may be a symlink target (/tmp -> /private/tmp
        // on macOS), so compare on the file name we chose.
        let name = path.file_name().unwrap();
        assert!(
            held.iter().any(|p| p.file_name() == Some(name)),
            "expected our own open file in {held:?}"
        );

        drop(f);
        let _ = std::fs::remove_file(&path);
    }

    /// End to end, against the real kernel: hold a file open, then check
    /// the collector names this process as its owner. This is the whole
    /// feature in one test — the fd scan, the reverse index and the join
    /// all have to work for it to pass.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_file_we_hold_open_is_attributed_to_us() {
        use std::io::Write;

        let path = std::env::temp_dir().join(format!("diskwatch-attrib-{}", std::process::id()));
        let mut f = std::fs::File::create(&path).expect("temp file");
        f.write_all(&vec![0u8; 1 << 20]).expect("write");
        f.sync_all().expect("sync");

        let mut c = ProcessCollector::new();
        c.refresh();

        // The scan resolves symlinks the temp dir may sit behind
        // (/tmp -> /private/tmp on macOS), so match on the name we chose
        // rather than on the path we asked for.
        let name = path.file_name().unwrap();
        let indexed = c
            .owners
            .keys()
            .find(|p| p.file_name() == Some(name))
            .cloned()
            .expect("our open file should be in the reverse index");

        let owners = c.owners_of(&indexed);
        // `procs` leaves diskwatch itself out, and the first refresh has no
        // interval to difference against, so the join is empty here. The
        // index is the part under test; that it contains our pid is the
        // claim.
        assert!(
            c.owners[&indexed].contains(&std::process::id()),
            "expected our pid among {:?}",
            c.owners[&indexed]
        );
        assert!(owners.len() <= c.owners[&indexed].len());
        assert!(c.coverage().visible > 0, "we can at least read ourselves");

        drop(f);
        let _ = std::fs::remove_file(&path);
    }

    /// The first refresh has no previous sample, and sysinfo reports
    /// lifetime totals as that delta. Publishing it would pin whatever
    /// has written most since boot to the top of the table permanently.
    /// The live path, not a seeded one: after a real refresh an idle
    /// process is in the table, so it can be named as a holder, and
    /// diskwatch itself is not.
    #[test]
    fn a_real_refresh_keeps_idle_processes_and_leaves_out_diskwatch() {
        let mut c = ProcessCollector::new();
        c.refresh_rates(0.0);
        c.refresh_rates(1.0);
        assert!(!c.procs.contains_key(&std::process::id()));
        let idle = c.procs.values().filter(|p| p.total_bps() == 0.0).count();
        assert!(idle > 0, "some process on this machine did no IO");
        assert!(c.top(usize::MAX).iter().all(|p| p.total_bps() > 0.0));
    }

    #[test]
    fn the_first_refresh_publishes_no_rates() {
        let mut c = ProcessCollector::new();
        c.refresh_rates(0.0);
        assert!(c.procs.is_empty());
    }

    /// The two intervals are what keep this affordable; a regression to
    /// scanning every frame would put a full walk of /proc on the render
    /// path.
    #[test]
    fn the_fd_scan_runs_less_often_than_the_rate_refresh() {
        assert!(SCAN_INTERVAL > RATE_INTERVAL);
    }
}
