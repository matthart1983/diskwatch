//! Hot Files collector — FSEvents on macOS, inotify on Linux,
//! via the `notify` crate.
//!
//! ## What FSEvents gives us
//! - File path
//! - Event kind (create / modify / metadata / rename / remove)
//! - Approximate timestamp
//!
//! ## What FSEvents doesn't give us
//! - **Bytes written** — FSEvents reports that a file changed, not how
//!   much. Per-byte attribution requires `fs_usage -e -w` (root) or
//!   eBPF biosnoop on Linux.
//! - **Process attribution** — FSEvents doesn't carry the originating
//!   pid. macOS's Endpoint Security framework does, but that's
//!   entitlement-gated.
//!
//! So our "Hot Files" view shows the most-modified paths by event
//! count, not by throughput, and we surface that limitation in the UI.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use notify::{
    event::{EventKind, ModifyKind},
    RecursiveMode, Watcher,
};

/// Decay factor applied each second so the EWMA half-life is ~5s.
/// `0.87^5 ≈ 0.5` — a file that stops being written drops to half rate
/// after 5 seconds of silence.
const EWMA_DECAY_PER_SEC: f64 = 0.87;

/// Entries idle for longer than this are dropped from the map on the
/// next prune pass.
const PRUNE_IDLE: Duration = Duration::from_secs(30);

/// Soft cap on tracked paths. Beyond this we prune by age.
const MAX_TRACKED: usize = 4096;

/// Per-path rate history, in 1 Hz samples. Long enough to fill the Lite
/// row sparkline on a wide terminal; the sparkline buckets whatever it
/// is given, so an over-long ring costs nothing but memory.
pub const HISTORY_LEN: usize = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityKind {
    Modified,
    Created,
    Removed,
    Metadata,
    Renamed,
    Other,
}

impl ActivityKind {
    pub fn label(&self) -> &'static str {
        match self {
            ActivityKind::Modified => "modify",
            ActivityKind::Created => "create",
            ActivityKind::Removed => "remove",
            ActivityKind::Metadata => "meta",
            ActivityKind::Renamed => "rename",
            ActivityKind::Other => "other",
        }
    }

    fn from_event(kind: &EventKind) -> Self {
        match kind {
            EventKind::Create(_) => ActivityKind::Created,
            EventKind::Remove(_) => ActivityKind::Removed,
            EventKind::Modify(ModifyKind::Name(_)) => ActivityKind::Renamed,
            EventKind::Modify(ModifyKind::Metadata(_)) => ActivityKind::Metadata,
            EventKind::Modify(_) => ActivityKind::Modified,
            _ => ActivityKind::Other,
        }
    }
}

#[derive(Debug, Clone)]
pub struct FileActivity {
    pub path: PathBuf,
    /// Events per second, exponentially smoothed.
    pub events_per_sec: f64,
    pub total_events: u64,
    pub last_kind: ActivityKind,
    pub last_seen: Instant,
    /// One `events_per_sec` reading per second, oldest first. Written
    /// by `decay()`, which the App calls at 1 Hz.
    pub history: VecDeque<f64>,
    /// Readings ever pushed, including those that have aged out — see
    /// `DeviceHistory::pushed` for why a sparkline needs this.
    pub pushed: u64,
}

#[derive(Default)]
pub struct HotFileState {
    pub activity: HashMap<PathBuf, FileActivity>,
    /// Total events forwarded since the watcher started — useful as a
    /// "did we hook up correctly" sanity reading.
    pub total_events: u64,
    /// Paths the watcher is rooted on. Used by the UI banner.
    pub watch_roots: Vec<PathBuf>,
    /// `None` until `start()` succeeds; carries a human-readable reason
    /// for failure so the tab can explain it.
    pub error: Option<String>,
}

impl HotFileState {
    fn record(&mut self, path: PathBuf, kind: ActivityKind) {
        self.total_events += 1;
        let now = Instant::now();
        let entry = self
            .activity
            .entry(path.clone())
            .or_insert_with(|| FileActivity {
                path,
                events_per_sec: 0.0,
                total_events: 0,
                last_kind: kind,
                last_seen: now,
                history: VecDeque::new(),
                pushed: 0,
            });
        entry.total_events += 1;
        entry.last_kind = kind;
        // Each event contributes +1 / interval to the smoothed rate; the
        // App's tick will decay it back down. We pre-add 1.0 here so the
        // event counts even if the next tick is a moment away.
        entry.events_per_sec += 1.0;
        entry.last_seen = now;
    }
}

/// Owner of the watcher + shared state. Drop this to stop watching.
pub struct HotFileWatcher {
    pub state: Arc<Mutex<HotFileState>>,
    _watcher: Option<notify::RecommendedWatcher>,
}

impl HotFileWatcher {
    pub fn start(roots: &[&Path]) -> Self {
        let state = Arc::new(Mutex::new(HotFileState::default()));
        let state_w = state.clone();
        let watcher_result =
            notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
                let Ok(event) = result else { return };
                let kind = ActivityKind::from_event(&event.kind);
                // notify can emit multiple paths per event (e.g. rename). We
                // record each path once.
                let Ok(mut s) = state_w.lock() else { return };
                for p in event.paths {
                    s.record(p, kind);
                }
            });

        let mut watcher = match watcher_result {
            Ok(w) => w,
            Err(e) => {
                state.lock().unwrap().error = Some(format!("watcher init failed: {}", e));
                return Self {
                    state,
                    _watcher: None,
                };
            }
        };

        // One error slot, many roots. Assigning per-failure would keep
        // only the last one, and a *later success* used to leave a stale
        // error sitting in the slot with no failing root to explain it.
        // With user-supplied paths a typo is the common case, not the
        // exotic one, so collect every failure and report them together.
        let mut errors: Vec<String> = Vec::new();
        let mut watched: Vec<PathBuf> = Vec::new();
        for r in roots {
            if !r.exists() {
                errors.push(format!("{}: no such path", r.display()));
                continue;
            }
            match watcher.watch(r, RecursiveMode::Recursive) {
                Ok(()) => watched.push(r.to_path_buf()),
                // `notify` abandons a whole recursive root at the first
                // directory it can't open (rootless-container storage under
                // $HOME is enough), leaving a random partial watch behind.
                // Walk it ourselves and skip what we can't read instead.
                Err(e) if is_unreadable_descendant(r, &e) => {
                    let skipped = watch_readable_tree(&mut watcher, r);
                    watched.push(r.to_path_buf());
                    if let Some(first) = skipped.first() {
                        errors.push(format!(
                            "{}: {} unreadable director{} not watched (e.g. {})",
                            r.display(),
                            skipped.len(),
                            if skipped.len() == 1 { "y" } else { "ies" },
                            first.display()
                        ));
                    }
                }
                Err(e) => errors.push(describe_watch_error(r, &e)),
            }
        }
        {
            let mut s = state.lock().unwrap();
            // Report the roots we are actually watching, not the ones we
            // were asked to: the tab draws this as "watch <paths>", and
            // listing a path no event will ever come from reads as a bug
            // in the watcher rather than a bad line in a config file.
            s.watch_roots = watched;
            if !errors.is_empty() {
                s.error = Some(errors.join("; "));
            }
        }

        Self {
            state,
            _watcher: Some(watcher),
        }
    }

    /// Called from the App tick. Decays per-file rates back toward zero
    /// based on elapsed time and prunes idle / overflowed entries.
    pub fn decay(&self, elapsed: Duration) {
        let mut s = self.state.lock().unwrap();
        let now = Instant::now();
        let factor = EWMA_DECAY_PER_SEC.powf(elapsed.as_secs_f64());
        s.activity.retain(|_, a| {
            if now.duration_since(a.last_seen) > PRUNE_IDLE {
                return false;
            }
            // Sample before decaying: `events_per_sec` currently holds
            // the events this interval accumulated, which is the rate
            // for the second just ended. Decaying first would record
            // every sample already faded.
            if a.history.len() == HISTORY_LEN {
                a.history.pop_front();
            }
            a.history.push_back(a.events_per_sec);
            a.pushed += 1;

            a.events_per_sec *= factor;
            if a.events_per_sec < 0.01 {
                a.events_per_sec = 0.0;
            }
            true
        });
        // Hard cap — if we somehow exceed it, drop the oldest entries.
        if s.activity.len() > MAX_TRACKED {
            let mut by_age: Vec<(PathBuf, Instant)> = s
                .activity
                .iter()
                .map(|(k, v)| (k.clone(), v.last_seen))
                .collect();
            by_age.sort_by_key(|(_, t)| *t);
            let drop_n = s.activity.len() - MAX_TRACKED;
            for (k, _) in by_age.into_iter().take(drop_n) {
                s.activity.remove(&k);
            }
        }
    }

    /// Returns a snapshot of the top N most-active files sorted by
    /// events-per-second descending.
    pub fn top(&self, n: usize) -> Vec<FileActivity> {
        let s = self.state.lock().unwrap();
        let mut v: Vec<FileActivity> = s.activity.values().cloned().collect();
        v.sort_by(|a, b| {
            b.events_per_sec
                .partial_cmp(&a.events_per_sec)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.total_events.cmp(&a.total_events))
        });
        v.truncate(n);
        v
    }

    /// Hot-file activity per ZFS pool. `fs` is the current mount table.
    pub fn pool_activity(&self, fs: &[crate::collect::FsTick]) -> HashMap<String, PoolHot> {
        let s = self.state.lock().unwrap();
        let active: Vec<(PathBuf, f64)> = s
            .activity
            .values()
            .filter(|a| a.events_per_sec > 0.0)
            .map(|a| (a.path.clone(), a.events_per_sec))
            .collect();
        attribute_to_pools(&active, fs, &s.watch_roots)
    }

    pub fn snapshot_meta(&self) -> (u64, Vec<PathBuf>, Option<String>) {
        let s = self.state.lock().unwrap();
        (s.total_events, s.watch_roots.clone(), s.error.clone())
    }

    /// Total tracked paths, regardless of how many a caller will actually
    /// render — `top` truncates to a page size, and scrolling needs the
    /// real count to clamp against.
    pub fn active_count(&self) -> usize {
        self.state.lock().unwrap().activity.len()
    }
}

/// Hot-file activity attributed to one ZFS pool.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PoolHot {
    /// Summed event rate of the pool's active files.
    pub events_per_sec: f64,
    /// Busiest files first, at most [`POOL_HOT_TOP`].
    pub top: Vec<(PathBuf, f64)>,
    /// Some mountpoint of the pool lies under a watched root. When false
    /// the pool shows no activity because nothing is looking, not because
    /// it is idle.
    pub watched: bool,
}

/// Files listed per pool.
pub const POOL_HOT_TOP: usize = 3;

/// Mountpoints of ZFS datasets worth watching by default. `/` is left out:
/// a root-on-ZFS install would otherwise watch the whole system, and
/// `$HOME` beneath it is already a default root.
pub fn zfs_mount_roots(fs: &[crate::collect::FsTick]) -> Vec<PathBuf> {
    fs.iter()
        .filter(|f| f.fs_type == "zfs" && f.mount != "/")
        .map(|f| PathBuf::from(&f.mount))
        .collect()
}

/// Pool a dataset belongs to: `tank/data/db` → `tank`.
fn pool_of(dataset: &str) -> &str {
    dataset.split('/').next().unwrap_or(dataset)
}

/// Attribute hot paths to ZFS pools by the deepest mount each lies under,
/// so a non-ZFS mount nested inside a dataset (or the reverse) is credited
/// to the right filesystem.
///
/// A path that is the parent of another active path is skipped: a write to
/// `/tank/db/x` also raises an event on `/tank/db`, and counting both
/// would double the pool's rate.
pub fn attribute_to_pools(
    activity: &[(PathBuf, f64)],
    fs: &[crate::collect::FsTick],
    roots: &[PathBuf],
) -> HashMap<String, PoolHot> {
    let mut out: HashMap<String, PoolHot> = HashMap::new();
    // Every pool with a mount gets an entry, so "idle" is distinguishable
    // from "unknown".
    for f in fs.iter().filter(|f| f.fs_type == "zfs") {
        let mount = Path::new(&f.mount);
        let covered = roots
            .iter()
            .any(|r| mount.starts_with(r) || r.starts_with(mount));
        let e = out.entry(pool_of(&f.device).to_string()).or_default();
        e.watched |= covered;
    }
    if out.is_empty() {
        return out;
    }

    let mut parents: std::collections::HashSet<&Path> = std::collections::HashSet::new();
    for (path, _) in activity {
        for a in path.ancestors().skip(1) {
            if !parents.insert(a) {
                break; // everything above is already recorded
            }
        }
    }

    for (path, rate) in activity {
        if parents.contains(path.as_path()) {
            continue;
        }
        let owner = fs
            .iter()
            .filter(|f| path.starts_with(&f.mount))
            .max_by_key(|f| Path::new(&f.mount).components().count());
        let Some(owner) = owner.filter(|f| f.fs_type == "zfs") else {
            continue;
        };
        let e = out.entry(pool_of(&owner.device).to_string()).or_default();
        e.events_per_sec += rate;
        e.top.push((path.clone(), *rate));
    }
    for e in out.values_mut() {
        e.top
            .sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        e.top.truncate(POOL_HOT_TOP);
    }
    out
}

/// Sensible default roots that show real user activity without drowning
/// in /System churn. /private/tmp and /private/var/log are useful on
/// macOS; on Linux we want /home and /var/log; on Windows we want
/// %USERPROFILE% and %TEMP% (when distinct and not nested inside the profile).
pub fn default_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    #[cfg(not(target_os = "windows"))]
    {
        if let Some(home) = std::env::var_os("HOME") {
            roots.push(PathBuf::from(home));
        }
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(profile) = std::env::var_os("USERPROFILE") {
            roots.push(PathBuf::from(profile));
        } else if let Some(home) = std::env::var_os("HOME") {
            roots.push(PathBuf::from(home));
        }
        if let Some(temp) = std::env::var_os("TEMP") {
            roots.push(PathBuf::from(temp));
        }
    }
    #[cfg(target_os = "macos")]
    {
        roots.push(PathBuf::from("/private/var/log"));
        roots.push(PathBuf::from("/private/tmp"));
    }
    #[cfg(target_os = "linux")]
    {
        roots.push(PathBuf::from("/var/log"));
        roots.push(PathBuf::from("/tmp"));
    }
    prune_nested_roots(roots)
}

/// True when a recursive watch of `root` failed because a directory
/// *underneath* it couldn't be read — the case [`watch_readable_tree`]
/// can recover from. Out-of-watches and a bad root itself can't be.
fn is_unreadable_descendant(root: &Path, e: &notify::Error) -> bool {
    matches!(&e.kind, notify::ErrorKind::Io(io)
        if io.kind() == std::io::ErrorKind::PermissionDenied)
        && e.paths.first().is_some_and(|p| p != root)
}

/// Watch `root` and every readable directory under it, one non-recursive
/// watch each, never following symlinks. Returns the directories that
/// couldn't be read or watched; they and their subtrees are left out.
///
/// Unlike a recursive watch this does not pick up directories created
/// after startup, which is the price of not losing the whole root to one
/// unreadable directory.
fn watch_readable_tree(watcher: &mut impl Watcher, root: &Path) -> Vec<PathBuf> {
    let mut skipped = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => {
                skipped.push(dir);
                continue;
            }
        };
        match watcher.watch(&dir, RecursiveMode::NonRecursive) {
            Ok(()) => {}
            Err(e) if matches!(e.kind, notify::ErrorKind::MaxFilesWatch) => {
                // Out of descriptors: nothing further down can succeed.
                skipped.push(dir);
                break;
            }
            Err(_) => {
                skipped.push(dir);
                continue;
            }
        }
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                stack.push(entry.path());
            }
        }
    }
    skipped
}

/// Turn a `notify` failure into something a user can act on.
///
/// Recursive watching costs one watch descriptor per directory underneath
/// a root, so a `watch_paths` entry pointed at a large tree hits the
/// kernel's limit readily. `notify` reports that as "OS file watch limit
/// reached", which is true but leaves the user with nowhere to go — the
/// fix is a sysctl, and it is worth naming.
fn describe_watch_error(root: &Path, e: &notify::Error) -> String {
    if matches!(e.kind, notify::ErrorKind::MaxFilesWatch) {
        let fix = if cfg!(target_os = "linux") {
            "raise fs.inotify.max_user_watches, or watch a narrower path"
        } else {
            "watch a narrower path"
        };
        return format!(
            "{}: out of file watches — watching is recursive, and costs one \
             per directory underneath this path. To fix: {fix}.",
            root.display()
        );
    }
    // `notify` walks the tree itself and gives up on the entire root if any
    // directory underneath it can't be watched — one 0700 systemd-private
    // directory is enough to lose all of /tmp. The bare error names the
    // root and buries the descendant that actually failed in a debug-
    // formatted list, which reads as "/tmp is unreadable" when it isn't.
    let reason = match &e.kind {
        notify::ErrorKind::Io(io) => io.to_string(),
        _ => e.to_string(),
    };
    match e.paths.first() {
        Some(p) if p != root => format!(
            "{}: skipped — {reason} on {} (watching is recursive, so one \
             unreadable directory underneath fails the whole root)",
            root.display(),
            p.display()
        ),
        _ => format!("{}: {reason}", root.display()),
    }
}

/// Drops exact duplicate roots and removes any root that is nested inside
/// another watched root.
///
/// Because watching is recursive ([`RecursiveMode::Recursive`]), watching a
/// directory and any of its descendants causes every file event inside the
/// descendant to be delivered twice. If canonicalization succeeds for both
/// paths, symlinks, casing, and Windows 8.3 short names (`ALEXAN~1`) are
/// normalized before checking.
pub fn prune_nested_roots(roots: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut result: Vec<PathBuf> = Vec::new();
    for candidate in roots {
        let already_covered = result.iter().any(|kept| {
            if let (Ok(c_cand), Ok(c_kept)) = (candidate.canonicalize(), kept.canonicalize()) {
                c_cand.starts_with(&c_kept)
            } else {
                candidate.starts_with(kept)
            }
        });
        if already_covered {
            continue;
        }

        result.retain(|kept| {
            let kept_is_child =
                if let (Ok(c_kept), Ok(c_cand)) = (kept.canonicalize(), candidate.canonicalize()) {
                    c_kept.starts_with(&c_cand)
                } else {
                    kept.starts_with(&candidate)
                };
            !kept_is_child
        });

        result.push(candidate);
    }
    result
}

/// The roots to hand [`HotFileWatcher::start`], given what the user asked
/// for. `replace` (from `--watch` or the config's `watch_paths`) stands in
/// for [`default_roots`] entirely; `extra` (from `--watch-add` or
/// `extra_watch_paths`) is added to whichever list won.
///
/// Duplicates and nested descendant paths are dropped. Watching the same tree
/// twice is not harmful — `notify` collapses it — but it doubles the path
/// up in the tab's "watch" banner, which reads as a bug.
pub fn resolve_roots(replace: Option<Vec<PathBuf>>, extra: &[PathBuf]) -> Vec<PathBuf> {
    let mut roots = replace.unwrap_or_else(default_roots);
    roots.extend_from_slice(extra);
    prune_nested_roots(roots)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extra_roots_add_to_the_defaults_and_replacements_stand_in_for_them() {
        let extra = vec![PathBuf::from("/srv/data")];
        let with_defaults = resolve_roots(None, &extra);
        assert!(with_defaults.len() > 1, "defaults should still be present");
        assert!(with_defaults.contains(&PathBuf::from("/srv/data")));

        let replaced = resolve_roots(Some(vec![PathBuf::from("/only")]), &extra);
        assert_eq!(
            replaced,
            vec![PathBuf::from("/only"), PathBuf::from("/srv/data")],
            "an explicit list replaces the defaults but still takes the extras"
        );
    }

    #[test]
    fn duplicate_roots_collapse_to_one_banner_entry() {
        let roots = resolve_roots(
            Some(vec![PathBuf::from("/a"), PathBuf::from("/b")]),
            &[PathBuf::from("/a")],
        );
        assert_eq!(roots, vec![PathBuf::from("/a"), PathBuf::from("/b")]);
    }

    #[test]
    fn nested_roots_collapse_into_parent() {
        let roots = resolve_roots(
            Some(vec![PathBuf::from("/a"), PathBuf::from("/a/b")]),
            &[PathBuf::from("/a/b/c")],
        );
        assert_eq!(roots, vec![PathBuf::from("/a")]);

        let inverted = resolve_roots(Some(vec![PathBuf::from("/a/b")]), &[PathBuf::from("/a")]);
        assert_eq!(inverted, vec![PathBuf::from("/a")]);
    }

    struct TempDirGuard(std::path::PathBuf);

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn nested_roots_pruning_with_real_filesystem_paths() {
        let temp_dir = std::env::temp_dir().join(format!("dw_test_{}", std::process::id()));
        let _guard = TempDirGuard(temp_dir.clone());
        let parent = temp_dir.join("parent");
        let child = parent.join("child");
        std::fs::create_dir_all(&child).unwrap();

        let roots = resolve_roots(Some(vec![parent.clone(), child.clone()]), &[]);
        assert_eq!(roots, vec![parent.clone()]);

        let inverted = resolve_roots(Some(vec![child]), std::slice::from_ref(&parent));
        assert_eq!(inverted, vec![parent]);
    }

    /// `notify` maps the inotify budget being exhausted to its own error
    /// kind rather than the kernel's ENOSPC, so matching on the io error
    /// would silently never fire and users would keep seeing "OS file
    /// watch limit reached" with no next step.
    #[test]
    fn an_exhausted_watch_budget_names_the_knob_that_fixes_it() {
        let e = notify::Error {
            kind: notify::ErrorKind::MaxFilesWatch,
            paths: Vec::new(),
        };
        let msg = describe_watch_error(Path::new("/big/tree"), &e);
        assert!(msg.contains("/big/tree"), "{msg}");
        assert!(msg.contains("recursive"), "{msg}");
        #[cfg(target_os = "linux")]
        assert!(msg.contains("max_user_watches"), "{msg}");
    }

    /// The failure that actually happens on a stock systemd box: `/tmp` is
    /// a default root, and a single root-owned `systemd-private-*`
    /// directory inside it fails the recursive watch of the whole tree.
    /// The bare error blames `/tmp`; the descendant is the real subject.
    #[test]
    fn a_permission_error_names_the_directory_that_actually_failed() {
        let e = notify::Error {
            kind: notify::ErrorKind::Io(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
            paths: vec![PathBuf::from("/tmp/systemd-private-abc")],
        };
        let msg = describe_watch_error(Path::new("/tmp"), &e);
        assert!(msg.contains("/tmp/systemd-private-abc"), "{msg}");
        assert!(msg.contains("recursive"), "{msg}");
        // The debug-formatted path list `notify` appends must not survive.
        assert!(!msg.contains('['), "{msg}");
    }

    /// A root that doesn't exist has to be reported, and it must not stop
    /// the roots on either side of it from being watched. This is the
    /// failure mode that arrives with user-supplied paths: one typo in a
    /// list of three.
    #[test]
    fn a_bad_root_is_named_without_silencing_the_good_ones() {
        // A directory of our own, not the shared temp root: watching is
        // recursive, and pointing it at whatever else is in /tmp can
        // exhaust the inotify budget and fail the good root too.
        let dir = std::env::temp_dir().join(format!("diskwatch-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let missing = dir.join("diskwatch-does-not-exist-49bd2f");
        let w = HotFileWatcher::start(&[dir.as_path(), missing.as_path()]);
        let (_, roots, err) = w.snapshot_meta();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(roots, vec![dir.clone()], "the good root is still watched");
        let err = err.expect("the missing root should be reported");
        assert!(
            err.contains("diskwatch-does-not-exist-49bd2f"),
            "the error should name the offending path, got: {err}"
        );
    }

    /// A root with one unreadable directory inside it must still be
    /// watched — and report what it skipped — rather than being dropped
    /// whole. Root can read everything, so skip when running as root.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_subdirectory_does_not_lose_the_root() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("diskwatch-unreadable-{}", std::process::id()));
        let good = dir.join("good");
        let locked = dir.join("locked");
        std::fs::create_dir_all(&good).expect("good dir");
        std::fs::create_dir_all(locked.join("inner")).expect("locked dir");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let readable = std::fs::read_dir(&locked).is_ok();
        let w = HotFileWatcher::start(&[dir.as_path()]);
        let (_, roots, err) = w.snapshot_meta();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        if readable {
            return; // running as root
        }
        assert_eq!(roots, vec![dir.clone()], "the root is still watched");
        let err = err.expect("the skipped directory should be reported");
        assert!(err.contains("locked"), "{err}");
        assert!(!err.contains("fails the whole root"), "{err}");
    }

    fn mount(dev: &str, at: &str, ty: &str) -> crate::collect::FsTick {
        crate::collect::FsTick {
            mount: at.into(),
            device: dev.into(),
            fs_type: ty.into(),
            size_bytes: 0,
            used_bytes: 0,
            avail_bytes: 0,
            inode_pct: None,
            is_removable: false,
            is_system: false,
            ro_image: false,
            ignored: false,
        }
    }

    fn act(items: &[(&str, f64)]) -> Vec<(PathBuf, f64)> {
        items.iter().map(|(p, r)| (PathBuf::from(p), *r)).collect()
    }

    #[test]
    fn activity_lands_on_the_pool_that_owns_the_mount() {
        let fs = vec![
            mount("/dev/nvme0n1p2", "/", "ext4"),
            mount("tank/data", "/tank/data", "zfs"),
            mount("bpool/boot", "/boot", "zfs"),
        ];
        let roots = vec![PathBuf::from("/tank/data"), PathBuf::from("/boot")];
        let a = act(&[
            ("/tank/data/db.sqlite", 40.0),
            ("/tank/data/log", 10.0),
            ("/boot/grub/grubenv", 2.0),
            ("/home/x/file", 99.0), // ext4: not a pool
        ]);
        let out = attribute_to_pools(&a, &fs, &roots);
        assert_eq!(out["tank"].events_per_sec, 50.0);
        assert_eq!(out["tank"].top[0].0, PathBuf::from("/tank/data/db.sqlite"));
        assert_eq!(out["bpool"].events_per_sec, 2.0);
        assert!(out["tank"].watched);
    }

    #[test]
    fn a_parent_directory_event_is_not_counted_twice() {
        let fs = vec![mount("tank/data", "/tank/data", "zfs")];
        let roots = vec![PathBuf::from("/tank/data")];
        let a = act(&[("/tank/data/db", 30.0), ("/tank/data/db/x", 30.0)]);
        let out = attribute_to_pools(&a, &fs, &roots);
        assert_eq!(out["tank"].events_per_sec, 30.0);
        assert_eq!(out["tank"].top.len(), 1);
    }

    #[test]
    fn the_deepest_mount_wins_over_an_enclosing_dataset() {
        let fs = vec![
            mount("tank/data", "/tank/data", "zfs"),
            mount("/dev/sdb1", "/tank/data/usb", "ext4"),
        ];
        let roots = vec![PathBuf::from("/tank/data")];
        let a = act(&[("/tank/data/usb/f", 5.0), ("/tank/data/g", 1.0)]);
        let out = attribute_to_pools(&a, &fs, &roots);
        assert_eq!(out["tank"].events_per_sec, 1.0);
    }

    #[test]
    fn an_unwatched_pool_is_distinguished_from_an_idle_one() {
        let fs = vec![mount("tank/data", "/tank/data", "zfs")];
        let out = attribute_to_pools(&[], &fs, &[PathBuf::from("/home/x")]);
        assert!(!out["tank"].watched);
        let out = attribute_to_pools(&[], &fs, &[PathBuf::from("/tank")]);
        assert!(out["tank"].watched);
        assert_eq!(out["tank"].events_per_sec, 0.0);
    }

    #[test]
    fn zfs_mounts_become_default_roots_except_slash() {
        let fs = vec![
            mount("rpool/ROOT", "/", "zfs"),
            mount("tank/data", "/tank/data", "zfs"),
            mount("/dev/sda1", "/mnt", "ext4"),
        ];
        assert_eq!(zfs_mount_roots(&fs), vec![PathBuf::from("/tank/data")]);
    }
}
