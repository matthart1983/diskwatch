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

    pub fn snapshot_meta(&self) -> (u64, Vec<PathBuf>, Option<String>) {
        let s = self.state.lock().unwrap();
        (s.total_events, s.watch_roots.clone(), s.error.clone())
    }
}

/// Sensible default roots that show real user activity without drowning
/// in /System churn. /private/tmp and /private/var/log are useful on
/// macOS; on Linux we want /home and /var/log.
pub fn default_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(PathBuf::from(home));
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
    roots
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

/// The roots to hand [`HotFileWatcher::start`], given what the user asked
/// for. `replace` (from `--watch` or the config's `watch_paths`) stands in
/// for [`default_roots`] entirely; `extra` (from `--watch-add` or
/// `extra_watch_paths`) is added to whichever list won.
///
/// Duplicates are dropped, keeping first position. Watching the same tree
/// twice is not harmful — `notify` collapses it — but it doubles the path
/// up in the tab's "watch" banner, which reads as a bug.
pub fn resolve_roots(replace: Option<Vec<PathBuf>>, extra: &[PathBuf]) -> Vec<PathBuf> {
    let mut roots = replace.unwrap_or_else(default_roots);
    roots.extend_from_slice(extra);
    let mut seen = std::collections::HashSet::new();
    roots.retain(|p| seen.insert(p.clone()));
    roots
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
}
