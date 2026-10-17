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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
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
    /// A background install is still running: `watch_roots` and `error`
    /// are not known yet, so the tabs say "installing" instead of
    /// claiming roots that may not exist.
    pub installing: bool,
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
///
/// Installed either synchronously ([`Self::start`]) or in the background
/// ([`Self::start_background`]); both go through `install`, so root
/// filtering, error reporting and cleanup cannot drift between them.
/// With no roots neither builds anything — see [`Self::idle`].
///
/// Cancellation is strict for the metadata, not for the whole state.
/// Dropping retires `watch_roots` / `error` under the same gate that
/// serializes publication, so once `Drop` returns those two fields are
/// final: they can never be republished, not even by a publication that
/// was already in flight. The rest of the state keeps its normal life —
/// `activity` and `total_events` may still take updates from events
/// already in flight while a detached worker holds the watcher's last
/// reference, and until the worker finishes initialization `watch_roots`
/// is empty and `installing` is set. Release the state lock
/// before dropping the watcher; `Drop` needs that lock to retire the
/// metadata.
pub struct HotFileWatcher {
    pub state: Arc<Mutex<HotFileState>>,
    _watcher: Arc<Mutex<Option<notify::RecommendedWatcher>>>,
    /// Cancellation request, read outside `stop_gate` only as a hint for
    /// aborting work in progress — between roots, at the end of a root's
    /// work, and between directories of the readable-tree fallback. Such a
    /// read may see a stale `false` without consequence: every decision
    /// that publishes metadata or hands over the watcher re-reads it inside
    /// the gate, and the gate's release/acquire pair carries the store to
    /// whoever decides next. That is why the store needs no stronger
    /// ordering than `Relaxed` to be seen by the check that matters.
    stop: Arc<AtomicBool>,
    /// Serializes every write that decides what the state claims: `Drop`'s
    /// cancellation and state retirement, `install`'s success and
    /// initialization-error publications, and the worker's takeover of the
    /// watcher. Held only across atomic loads/stores and brief state
    /// writes — never across installs, walks or joins.
    ///
    /// Lock order is gate before state, everywhere, and nothing takes the
    /// gate while holding the state lock, so the two can never form a
    /// cycle. That is what keeps the event callback — which needs the
    /// state lock and nothing else — clear of a publication: the install
    /// thread takes the gate and the state lock together, and never holds
    /// the state lock across a `watch()` call, which on inotify blocks on
    /// the very thread the callback runs on.
    stop_gate: Arc<Mutex<()>>,
    handle: Option<thread::JoinHandle<()>>,
}

/// Test-only hook run by [`HotFileWatcher::start_background_inner`] after
/// each root has been handled, while the watcher is still alive.
type RootHook = Box<dyn FnMut(&Path) + Send + 'static>;

impl HotFileWatcher {
    /// A watcher for an empty root list: no worker, no OS watcher, and the
    /// same empty state a synchronous install would have published.
    /// Asking for nothing must not cost an inotify instance (or FSEvents
    /// stream) that would only be dropped again unused.
    fn idle() -> Self {
        Self {
            state: Arc::new(Mutex::new(HotFileState::default())),
            _watcher: Arc::new(Mutex::new(None)),
            stop: Arc::new(AtomicBool::new(false)),
            stop_gate: Arc::new(Mutex::new(())),
            handle: None,
        }
    }

    /// Install every watch before returning — the original entry point,
    /// kept for callers that need `watch_roots` / `error` to already be
    /// final. The TUI startup path uses [`Self::start_background`].
    ///
    /// With no roots there is nothing to install, so this returns
    /// [`Self::idle`] without creating an OS watcher.
    pub fn start(roots: &[&Path]) -> Self {
        if roots.is_empty() {
            return Self::idle();
        }
        let roots: Vec<PathBuf> = roots.iter().map(|p| p.to_path_buf()).collect();
        let state = Arc::new(Mutex::new(HotFileState::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let stop_gate = Arc::new(Mutex::new(()));
        let watcher = install(&roots, &state, &stop, &stop_gate, None);
        Self {
            state,
            _watcher: Arc::new(Mutex::new(watcher)),
            stop,
            stop_gate,
            handle: None,
        }
    }

    /// Begin watching on a background thread and return immediately.
    ///
    /// Nothing waits on the install: `installing` is set until the worker
    /// publishes the roots it actually watches. Blocking the first frame on a recursive walk of a large tree is the
    /// stall this exists to remove.
    ///
    /// With no roots there is nothing to watch; like [`Self::start`], this
    /// returns [`Self::idle`] without spawning a worker or creating the OS
    /// watcher `install` would only drop again unused.
    pub fn start_background(roots: &[&Path]) -> Self {
        // No roots is nothing to watch: fall through to the synchronous
        // path, which returns the same empty watcher without a worker.
        if roots.is_empty() {
            return Self::start(roots);
        }
        Self::start_background_inner(roots, || (), || (), None)
    }

    /// The body of the background constructor. `before_install` runs on the
    /// worker before any watch work, `on_root` after each root while the
    /// watcher is still alive, and `after_install` between `install`
    /// publishing its result and the ownership decision below. Whatever
    /// `before_install` / `after_install` return is held until the worker
    /// exits — tests use them to park the worker at a known point and to
    /// observe termination. Production passes no-ops and no hook.
    fn start_background_inner<F, G, H, I>(
        roots: &[&Path],
        before_install: F,
        after_install: H,
        mut on_root: Option<RootHook>,
    ) -> Self
    where
        F: FnOnce() -> G + Send + 'static,
        G: Send + 'static,
        H: FnOnce() -> I + Send + 'static,
        I: Send + 'static,
    {
        let roots: Vec<PathBuf> = roots.iter().map(|p| p.to_path_buf()).collect();
        let state = Arc::new(Mutex::new(HotFileState::default()));
        state.lock().unwrap().installing = true;
        let watcher = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let stop_gate = Arc::new(Mutex::new(()));

        let state_t = state.clone();
        let watcher_t = watcher.clone();
        let stop_t = stop.clone();
        let stop_gate_t = stop_gate.clone();
        let roots_t = roots.clone();
        let spawned = thread::Builder::new()
            .name("hot-files-watch".into())
            .spawn(move || {
                let _before = before_install();
                let hook: Option<&mut dyn FnMut(&Path)> = match on_root.as_mut() {
                    Some(hook) => Some(&mut **hook),
                    None => None,
                };
                let installed = install(&roots_t, &state_t, &stop_t, &stop_gate_t, hook);
                let _after = after_install();
                // Ownership transfer, serialized with `Drop` under the
                // gate. A cancellation visible here wins and the watcher is
                // dropped unowned; there is no third outcome. Publication
                // takes the same gate, so it can neither slip past a `Drop`
                // that already retired the state nor leave metadata behind
                // when this decision refuses the watcher.
                if let Some(w) = installed {
                    let _gate = stop_gate_t.lock().unwrap();
                    if !stop_t.load(Ordering::Relaxed) {
                        *watcher_t.lock().unwrap() = Some(w);
                    }
                }
            });
        let handle = match spawned {
            Ok(handle) => Some(handle),
            // No thread means no install. Report it through the same error
            // slot the tab already reads instead of panicking the TUI.
            Err(e) => {
                let mut s = state.lock().unwrap();
                s.installing = false;
                s.error = Some(format!("watcher thread failed to start: {}", e));
                None
            }
        };
        Self {
            state,
            _watcher: watcher,
            stop,
            stop_gate,
            handle,
        }
    }

    /// Block until the background worker has exited, which is the point at
    /// which `watch_roots` and `error` are final. Test-only: production
    /// never needs to wait for installation.
    ///
    /// Calling it more than once is harmless, and it returns at once for a
    /// watcher with no worker — [`Self::start`] and [`Self::idle`]. If the
    /// worker panicked, this returns when the thread is done rather than
    /// waiting forever, and `state` holds whatever the worker managed to
    /// publish.
    #[cfg(test)]
    pub fn wait(&self) {
        if let Some(handle) = &self.handle {
            while !handle.is_finished() {
                thread::sleep(Duration::from_millis(1));
            }
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

    /// Whether a background install is still running.
    pub fn installing(&self) -> bool {
        self.state.lock().unwrap().installing
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

impl Drop for HotFileWatcher {
    fn drop(&mut self) {
        // Cancel and retire under the gate. `install` reads `stop` between
        // roots, at the end of each root's work, and between directories of
        // the readable-tree fallback, but those reads are only abort hints —
        // the publication they protect is gated here, together with the
        // worker's ownership decision. One critical section, then there is
        // no publication left to land: one that already happened is cleared
        // here, and one that has not yet takes the gate and finds the
        // cancellation. The state lock is held only across the few field
        // writes below; callers must not hold it around this drop.
        {
            // Poison-tolerant: a panic elsewhere while a lock was held must
            // not turn into a second panic here during unwinding, which
            // aborts the process.
            let _gate = self
                .stop_gate
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            self.stop.store(true, Ordering::Relaxed);
            let mut s = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            s.watch_roots.clear();
            s.error = None;
            s.installing = false;
        }
        // Join only a worker that has already finished. One still inside
        // `notify`'s recursive walk cannot be interrupted from here — the
        // walk is synchronous and `notify` exposes no cancellation — and on
        // a large tree it can take seconds to tens of seconds. Blocking
        // shutdown on that would put back the stall this change removes.
        // The detached worker observes `stop` at its next check, drops any
        // watcher it built, and exits; the watcher lives behind an `Arc`, so
        // the last owner out releases the OS watches.
        if let Some(handle) = self.handle.take() {
            if handle.is_finished() {
                let _ = handle.join();
            }
        }
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

/// Install watches for `roots` into `state`, returning the watcher when
/// successful. Shared by the synchronous and background entry points so the
/// two cannot drift apart.
///
/// `on_root`, when given, runs after each root has been handled and while
/// the watcher is still alive. Tests use it to observe and cancel a
/// partially completed install; production passes `None`.
///
/// Cancellation is checked before each root, again at the end of every
/// root's work (a `watch()` syscall or the readable-tree fallback above
/// can be interrupted part-way), and once more under `stop_gate` before
/// the result is published. Any of those aborts the call: it returns
/// `None`, having published nothing, and dropping the watcher it was
/// building releases every watch already installed.
///
/// Both final publications — success metadata and the initialization error
/// — are serialized with cancellation under `stop_gate`, the same gate
/// `Drop` takes to cancel and retire the state. A `Drop` that already
/// retired the state cannot be followed by a publication (the recheck
/// inside the gate sees the stop), and a publication that landed before
/// `Drop` is retired by it. Once `Drop` returns, `watch_roots` and `error`
/// are final; the rest of the state is not frozen by the drop (see
/// [`HotFileWatcher`]). The gate is held only across the state writes
/// themselves — never across the walk or `notify` calls.
fn install(
    roots: &[PathBuf],
    state: &Arc<Mutex<HotFileState>>,
    stop: &AtomicBool,
    stop_gate: &Mutex<()>,
    mut on_root: Option<&mut dyn FnMut(&Path)>,
) -> Option<notify::RecommendedWatcher> {
    // Dropped before the worker got here, or cancelled outright: nothing to
    // build, and building the OS watcher would only have to be torn down.
    if stop.load(Ordering::Relaxed) {
        return None;
    }
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
            // Serialized with `Drop`: if cancellation won, publish nothing
            // — `Drop` retires whatever the state held, and a fresh error
            // for an install nobody owns must not appear after it.
            let _gate = stop_gate.lock().unwrap();
            if !stop.load(Ordering::Relaxed) {
                let mut s = state.lock().unwrap();
                s.installing = false;
                s.error = Some(format!("watcher init failed: {}", e));
            }
            return None;
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
        if stop.load(Ordering::Relaxed) {
            return None;
        }
        if !r.exists() {
            errors.push(format!("{}: no such path", r.display()));
            continue;
        }
        match watcher.watch(r, RecursiveMode::Recursive) {
            Ok(()) => watched.push(r.clone()),
            // `notify` abandons a whole recursive root at the first
            // directory it can't open (rootless-container storage under
            // $HOME is enough), leaving a random partial watch behind.
            // Walk it ourselves and skip what we can't read instead.
            Err(e) if is_unreadable_descendant(r, &e) => {
                let skipped = watch_readable_tree(&mut watcher, r, stop);
                watched.push(r.clone());
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
        // Cancellation can arrive *inside* a root rather than between
        // them: `watch()` is a syscall, and the fallback walk above can be
        // abandoned part-way down a tree. The match arms have already run
        // by this point — the fallback arm pushes the root and its skipped
        // count whatever the walk's outcome — so the local `watched` /
        // `errors` can describe a root that is only partly watched. This is
        // where that bookkeeping stops mattering: a root whose traversal
        // was cut short must not contribute to a completed installation,
        // so abandon it before processing another root or publishing any
        // metadata.
        //
        // This is the boundary the partial work is produced at. The gated
        // recheck after the loop would catch the same cancellation before
        // publishing, so removing this changes no outcome today; it is
        // kept because the invariant it maintains is local and should not
        // depend on a later check far away to hold. Note that no test can
        // reach it deterministically — cancellation arrives from `Drop` on
        // another thread, and every per-root seam the tests have runs
        // *after* this check — so it is argued, not covered.
        if stop.load(Ordering::Relaxed) {
            return None;
        }
        if let Some(hook) = on_root.as_deref_mut() {
            hook(r.as_path());
        }
    }
    {
        // The recheck and the publication share one critical section, so
        // no `Drop` can retire the state between them. A cancellation seen
        // here abandons the watcher instead of returning it: `Drop` has
        // already given up ownership, and the publication below would
        // claim roots for an install nobody is going to keep.
        let _gate = stop_gate.lock().unwrap();
        if stop.load(Ordering::Relaxed) {
            return None;
        }
        let mut s = state.lock().unwrap();
        // Report the roots we are actually watching, not the ones we
        // were asked to: the tab draws this as "watch <paths>", and
        // listing a path no event will ever come from reads as a bug
        // in the watcher rather than a bad line in a config file.
        s.watch_roots = watched;
        s.installing = false;
        if !errors.is_empty() {
            s.error = Some(errors.join("; "));
        }
    }

    Some(watcher)
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
fn watch_readable_tree(watcher: &mut impl Watcher, root: &Path, stop: &AtomicBool) -> Vec<PathBuf> {
    let mut skipped = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if stop.load(Ordering::Relaxed) {
            break;
        }
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

/// The roots to hand [`HotFileWatcher::start_background`], given what the
/// user asked for. `replace` (from `--watch` or the config's `watch_paths`)
/// stands in for [`default_roots`] entirely; `extra` (from `--watch-add` or
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
        let w = HotFileWatcher::start_background(&[dir.as_path(), missing.as_path()]);
        w.wait();
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
    /// Linux only: FSEvents watches an unreadable subtree without error,
    /// so there is no failure to recover from on macOS.
    #[cfg(target_os = "linux")]
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
        let w = HotFileWatcher::start_background(&[dir.as_path()]);
        w.wait();
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

    /// The point of the background constructor: it must not wait for the
    /// install. The worker is parked at a gate before it can touch the
    /// filesystem, and the gate is released only after the constructor has
    /// returned; if the constructor had waited for the worker, the gate
    /// timeout would expire and the flag below would fail the test instead
    /// of hanging the suite.
    #[test]
    fn background_startup_returns_before_installation_runs() {
        let dir = std::env::temp_dir().join(format!("diskwatch-gate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let _guard = TempDirGuard(dir.clone());

        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let released_by_test = Arc::new(AtomicBool::new(false));
        let released_w = released_by_test.clone();
        let w = HotFileWatcher::start_background_inner(
            &[dir.as_path()],
            move || {
                let released = release_rx.recv_timeout(Duration::from_secs(5)).is_ok();
                released_w.store(released, Ordering::SeqCst);
            },
            || (),
            None,
        );

        // Reaching this line with the worker still parked is the guarantee;
        // the flag only distinguishes that from a constructor that waited
        // and let the timeout expire. Until it runs, nothing claims a root.
        assert!(w.installing());
        assert!(w.snapshot_meta().1.is_empty());
        let _ = release_tx.send(());
        w.wait();
        assert!(!w.installing());
        assert!(
            released_by_test.load(Ordering::SeqCst),
            "start_background returned only after installation had run"
        );
        let (_, roots, err) = w.snapshot_meta();
        assert_eq!(roots, vec![dir.clone()]);
        assert!(err.is_none(), "{err:?}");
    }

    /// `wait()` is the deterministic completion signal for tests: it is
    /// repeatable, and once it returns the published roots and error are
    /// final.
    #[test]
    fn wait_is_repeatable_and_reports_the_final_state() {
        let dir = std::env::temp_dir().join(format!("diskwatch-wait-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let _guard = TempDirGuard(dir.clone());
        let missing = dir.join("diskwatch-wait-missing");

        let w = HotFileWatcher::start_background(&[dir.as_path(), missing.as_path()]);
        w.wait();
        let first = w.snapshot_meta();
        w.wait();
        let second = w.snapshot_meta();
        assert_eq!(first, second, "repeated wait() must not change the result");
        assert_eq!(second.1, vec![dir.clone()]);
        assert!(second
            .2
            .as_deref()
            .is_some_and(|e| e.contains("no such path")));
    }

    /// The synchronous entry point keeps its old contract: installation is
    /// finished by the time it returns, with no `wait()` required.
    #[test]
    fn synchronous_start_finishes_installation_before_returning() {
        let dir = std::env::temp_dir().join(format!("diskwatch-sync-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let _guard = TempDirGuard(dir.clone());
        let missing = dir.join("diskwatch-sync-missing");

        let w = HotFileWatcher::start(&[dir.as_path(), missing.as_path()]);
        let (_, roots, err) = w.snapshot_meta();
        assert_eq!(roots, vec![dir.clone()]);
        assert!(err
            .expect("missing root reported")
            .contains("diskwatch-sync-missing"));
    }

    /// Watching nothing is still a successful install: no roots, no error,
    /// through either entry point — and no OS watcher is built only to be
    /// dropped again unused.
    /// A panic while the state lock is held poisons it. `Drop` runs during
    /// that unwind, and panicking there too would abort the process.
    #[test]
    fn drop_survives_a_poisoned_state_lock() {
        let w = HotFileWatcher::start(&[]);
        let state = w.state.clone();
        let _ = std::thread::spawn(move || {
            let _held = state.lock().unwrap();
            panic!("poison the state lock");
        })
        .join();
        assert!(w.state.is_poisoned());
        drop(w);
    }

    #[test]
    fn empty_roots_finish_with_no_error() {
        for w in [
            HotFileWatcher::start_background(&[]),
            HotFileWatcher::start(&[]),
        ] {
            w.wait();
            assert!(
                w._watcher.lock().unwrap().is_none(),
                "nothing to watch must not build an OS watcher"
            );
            let (events, roots, err) = w.snapshot_meta();
            assert_eq!(events, 0);
            assert!(roots.is_empty());
            assert!(err.is_none(), "{err:?}");
        }
    }

    /// A cancelled install must not watch anything and must not publish
    /// either: the state keeps no roots and no error for work that was
    /// abandoned, and the watcher it built is dropped.
    #[test]
    fn a_cancelled_install_installs_and_publishes_nothing() {
        let dir = std::env::temp_dir().join(format!("diskwatch-cancel-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let _guard = TempDirGuard(dir.clone());

        let state = Arc::new(Mutex::new(HotFileState::default()));
        let stop = AtomicBool::new(true);
        let stop_gate = Mutex::new(());
        let w = install(std::slice::from_ref(&dir), &state, &stop, &stop_gate, None);
        assert!(w.is_none(), "a cancelled install must not return a watcher");
        let s = state.lock().unwrap();
        assert!(s.watch_roots.is_empty());
        assert!(s.error.is_none());
    }

    /// The `(device, inode)` pair for `path`, in the same representation as
    /// the watch pairs below.
    #[cfg(target_os = "linux")]
    fn file_key(path: &Path) -> (u64, u64) {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::metadata(path).expect("metadata");
        (metadata.dev(), metadata.ino())
    }

    /// `(device, inode)` pairs for every live inotify watch in this process,
    /// read from `/proc/self/fdinfo`. Matching on the pair rather than the
    /// inode alone keeps a watch on some other filesystem with a colliding
    /// inode number from passing for the one under test.
    ///
    /// `None` means the mechanism is unusable — `/proc/self/fdinfo` could
    /// not be read at all — while `Some` means it worked, *including* when
    /// the set comes back empty. An empty set is an observation ("no
    /// watches are live right now"), not the absence of one. Callers gate
    /// kernel-facing assertions on that distinction, and
    /// [`wait_for_watch_release`] re-checks it on every poll rather than
    /// once up front.
    #[cfg(target_os = "linux")]
    fn inotify_watched_files() -> Option<std::collections::HashSet<(u64, u64)>> {
        let mut files = std::collections::HashSet::new();
        for entry in std::fs::read_dir("/proc/self/fdinfo").ok()?.flatten() {
            let Ok(info) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            for line in info.lines() {
                // Watch lines look like
                // `inotify wd:1 ino:9e7e sdev:800013 mask:...`; the fdinfo
                // header's own `ino:` line does not start with `inotify`.
                let mut fields = line.split_whitespace();
                if fields.next() != Some("inotify") {
                    continue;
                }
                let (mut sdev, mut ino) = (None, None);
                for field in fields {
                    if let Some(hex) = field.strip_prefix("sdev:") {
                        sdev = u64::from_str_radix(hex, 16).ok();
                    } else if let Some(hex) = field.strip_prefix("ino:") {
                        ino = u64::from_str_radix(hex, 16).ok();
                    }
                }
                if let (Some(sdev), Some(ino)) = (sdev, ino) {
                    files.insert((new_encode_dev(sdev), ino));
                }
            }
        }
        Some(files)
    }

    /// `/proc/self/fdinfo` prints `sdev` as the kernel's raw `dev_t`, while
    /// `Metadata::dev` is its `new_encode_dev` user representation (see
    /// `include/linux/kdev_t.h`), so the two differ wherever the minor or
    /// major numbers are large. Normalizing here lets the tests compare
    /// fdinfo watches against `std::fs::metadata` directly.
    #[cfg(target_os = "linux")]
    fn new_encode_dev(dev: u64) -> u64 {
        let major = (dev >> 20) & 0xfff;
        let minor = dev & 0xfffff;
        ((minor & 0xff) | (major << 8) | ((minor & 0xfffff00) << 12)) & 0xffff_ffff
    }

    /// Wait until `key` has no live inotify watch. `notify`'s `Drop` hands
    /// the inotify fd to its event-loop thread, so the kernel-side release
    /// lands shortly after the watcher drops; wait for that observable
    /// state instead of assuming it is instantaneous.
    ///
    /// The deadline bounds the wait; it never decides the outcome on its
    /// own, which is why the result distinguishes three cases:
    ///
    /// - `Some(true)` — the watch was observed to be gone.
    /// - `Some(false)` — the deadline expired while the mechanism stayed
    ///   readable and the watch was still live. This is the only outcome
    ///   that is evidence of a leak.
    /// - `None` — `/proc/self/fdinfo` stopped being readable before the
    ///   question could be settled. Nothing was observed either way, so a
    ///   caller must not read this as a leak: whether the file was
    ///   readable when the wait started says nothing about it staying
    ///   readable throughout.
    ///
    /// Folding the last two into one boolean would let an environment that
    /// loses `/proc` mid-wait masquerade as a resource leak.
    #[cfg(target_os = "linux")]
    fn wait_for_watch_release(key: (u64, u64)) -> Option<bool> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            // An empty set is a real observation, so only `None` — `/proc`
            // unreadable — ends the wait without an answer.
            let live = inotify_watched_files()?;
            if !live.contains(&key) {
                return Some(true);
            }
            if Instant::now() >= deadline {
                return Some(false);
            }
            thread::sleep(Duration::from_millis(1));
        }
    }

    /// Cancellation arriving *after* part of the install is already on
    /// disk must take those watches down with it: `install` drops the
    /// half-built watcher, and the kernel frees every watch when its
    /// inotify instance closes. The hook cancels between the two roots —
    /// with the first already watched and the second untouched — so
    /// neither "cancelled before any work" nor timing luck can pass this.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_cancelled_install_releases_the_watches_it_already_made() {
        let dir = std::env::temp_dir().join(format!("diskwatch-partial-{}", std::process::id()));
        let first = dir.join("first");
        let second = dir.join("second");
        std::fs::create_dir_all(&first).expect("first dir");
        std::fs::create_dir_all(&second).expect("second dir");
        let _guard = TempDirGuard(dir.clone());

        let first_key = file_key(&first);
        // Precondition only: when the initial observation works, a watch
        // already on the test directory would make the release check below
        // meaningless. An unreadable `/proc/self/fdinfo` skips this one
        // check and nothing else.
        let observed = inotify_watched_files();
        if let Some(before) = &observed {
            assert!(
                !before.contains(&first_key),
                "the test directory is already watched"
            );
        }

        let state = Arc::new(Mutex::new(HotFileState::default()));
        let stop = AtomicBool::new(false);
        let stop_gate = Mutex::new(());
        let mut roots_seen = 0;
        let mut watched_at_cancel = None;
        let mut on_root = |root: &Path| {
            roots_seen += 1;
            // `watch()` is request/reply with `notify`'s event loop, so by
            // now the first root's kernel watch is live: cancelling here is
            // genuinely mid-install, not before it started.
            watched_at_cancel = inotify_watched_files().map(|w| w.contains(&file_key(root)));
            stop.store(true, Ordering::Relaxed);
        };
        let watcher = install(
            &[first.clone(), second.clone()],
            &state,
            &stop,
            &stop_gate,
            Some(&mut on_root),
        );

        assert!(watcher.is_none(), "a cancelled install must return nothing");
        assert_eq!(
            roots_seen, 1,
            "the cancelled install must not touch the second root"
        );
        // Judged on the observation taken at cancellation time, not on
        // whether `/proc` happened to be readable earlier: `Some(false)` is
        // a working observation that saw no watch, which is a defect,
        // while `None` only means the question could not be answered.
        assert_ne!(
            watched_at_cancel,
            Some(false),
            "the first root was not watched when the cancellation arrived"
        );
        // `Some(false)` is a confirmed leak; `None` only means `/proc` went
        // away mid-wait, which proves nothing either way.
        assert_ne!(
            wait_for_watch_release(first_key),
            Some(false),
            "the partial watch must be released with the cancelled install"
        );
        let s = state.lock().unwrap();
        assert!(
            s.watch_roots.is_empty(),
            "a cancelled install publishes nothing"
        );
        assert!(s.error.is_none(), "{:?}", s.error);
    }

    /// The failure this guards against. `Drop` takes the gate while the
    /// install thread is still working on one root, and the readable-tree
    /// fallback is a walk over a whole subtree that checks `stop` only
    /// between directories. The loop used to carry on from there as if the
    /// root had been watched in full — appending it to `watched`, and for
    /// the last root running the publication and returning the partially
    /// built watcher.
    ///
    /// The invariant now enforced: cancellation observed at either the
    /// end-of-root check or the final gated check makes `install` return
    /// `None`, publish nothing, and drop the watcher it was building.
    ///
    /// What this test actually forces, precisely: the per-root hook runs
    /// *after* the end-of-root check, so on this single-root list the
    /// cancellation is first seen by the **final gated check**, in the
    /// window between the last root completing and the publication. It
    /// does **not** inject cancellation into the middle of
    /// `watch_readable_tree` — a root is cancelled from outside the
    /// install thread only by `Drop`, and nothing lets a test make that
    /// land inside a synchronous traversal without adding a seam to
    /// production code. The traversal's own mid-walk abort is pinned
    /// separately by `a_cancelled_walk_stops_before_the_next_directory`.
    ///
    /// Two runs of the same tree. The first is the control: its recursive
    /// watch fails on an unreadable descendant, the recovery branch runs,
    /// and the skipped directory is reported — that is what proves the
    /// recovery path is the one under test here and not an ordinary
    /// install. The second run cancels and must come back empty-handed.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_cancelled_fallback_install_abandons_the_watcher_it_was_building() {
        use std::os::unix::fs::PermissionsExt;
        /// Gives `locked` its mode back on every way out. Declared after
        /// the temp-dir guard so it runs first: an unreadable directory
        /// left behind makes `remove_dir_all` fail, which would leak this
        /// tree into `/tmp` for every future run.
        struct RestoreMode(PathBuf);
        impl Drop for RestoreMode {
            fn drop(&mut self) {
                let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
            }
        }

        let dir =
            std::env::temp_dir().join(format!("diskwatch-fallback-cancel-{}", std::process::id()));
        let locked = dir.join("locked");
        let readable = dir.join("readable");
        std::fs::create_dir_all(locked.join("inner")).expect("locked dir");
        std::fs::create_dir_all(&readable).expect("readable dir");
        let _guard = TempDirGuard(dir.clone());
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let _mode = RestoreMode(locked.clone());
        if std::fs::read_dir(&locked).is_ok() {
            return; // running as root: the recursive watch cannot fail
        }

        // Control: the recovery branch really is the one under test here.
        let control_state = Arc::new(Mutex::new(HotFileState::default()));
        let control_stop = AtomicBool::new(false);
        let control_watcher = install(
            std::slice::from_ref(&dir),
            &control_state,
            &control_stop,
            &Mutex::new(()),
            None,
        );
        assert!(
            control_watcher.is_some(),
            "the control install must complete"
        );
        let control = control_state.lock().unwrap();
        assert_eq!(control.watch_roots, vec![dir.clone()]);
        let control_err = control
            .error
            .clone()
            .expect("the skipped directory should be reported");
        assert!(control_err.contains("locked"), "{control_err}");
        drop(control);
        drop(control_watcher);

        // There is deliberately no "not watched yet" precondition on the second
        // run: the control run just watched this very tree, and `notify`
        // releases the kernel's watches asynchronously, so demanding a
        // clean slate before the cancellation run would only add a way to
        // be flaky.

        let state = Arc::new(Mutex::new(HotFileState::default()));
        let stop = AtomicBool::new(false);
        let stop_gate = Mutex::new(());
        let mut on_root = |_: &Path| stop.store(true, Ordering::Relaxed);
        let watcher = install(
            std::slice::from_ref(&dir),
            &state,
            &stop,
            &stop_gate,
            Some(&mut on_root),
        );

        assert!(
            watcher.is_none(),
            "a cancelled install must not hand back the half-built watcher"
        );
        let s = state.lock().unwrap();
        assert!(
            s.watch_roots.is_empty(),
            "a cancelled install publishes nothing, got {:?}",
            s.watch_roots
        );
        assert!(s.error.is_none(), "{:?}", s.error);
        drop(s);
        // The kernel half of the guarantee, and the only part that needs
        // `/proc`. A confirmed leak fails; a `/proc` that went away mid-wait
        // is not a leak and is not reported as one.
        assert_ne!(
            wait_for_watch_release(file_key(&readable)),
            Some(false),
            "the abandoned fallback must release the watches it already made"
        );
    }

    /// The fallback walk checks cancellation before each directory, so a
    /// cancelled walk must not even read the root. Without cancellation the
    /// same call reports the unreadable root, which keeps the assertion
    /// from being vacuous.
    #[test]
    fn a_cancelled_walk_leaves_the_tree_alone() {
        let missing = PathBuf::from("/diskwatch-cancelled-walk-does-not-exist");
        let mut watcher =
            notify::recommended_watcher(|_: notify::Result<notify::Event>| {}).unwrap();

        let cancelled = AtomicBool::new(true);
        assert!(watch_readable_tree(&mut watcher, &missing, &cancelled).is_empty());

        let running = AtomicBool::new(false);
        assert_eq!(
            watch_readable_tree(&mut watcher, &missing, &running),
            vec![missing]
        );
    }

    /// A watcher that requests cancellation part-way through its own
    /// traversal.
    ///
    /// `watch_readable_tree` is generic over `impl Watcher` and calls
    /// `watch` once per directory it descends into, so flipping `stop` from
    /// inside `watch` puts the flag mid-traversal with no second thread and
    /// no sleeps involved. Test-only: `install` builds its own concrete
    /// watcher and offers no such seam, which is why this is where the
    /// mid-walk abort is pinned rather than through `install`.
    struct CancelAfterNWatches<'a> {
        inner: notify::RecommendedWatcher,
        stop: Option<&'a AtomicBool>,
        remaining: usize,
    }

    impl Watcher for CancelAfterNWatches<'_> {
        fn new<F: notify::EventHandler>(
            handler: F,
            config: notify::Config,
        ) -> notify::Result<Self> {
            // Never used by the walk, which only calls `watch`. Built
            // without a stop flag so the type stays constructible.
            Ok(Self {
                inner: notify::RecommendedWatcher::new(handler, config)?,
                stop: None,
                remaining: 0,
            })
        }

        fn watch(&mut self, path: &Path, mode: RecursiveMode) -> notify::Result<()> {
            let res = self.inner.watch(path, mode);
            if self.remaining > 0 {
                self.remaining -= 1;
                if self.remaining == 0 {
                    if let Some(stop) = self.stop {
                        stop.store(true, Ordering::Relaxed);
                    }
                }
            }
            res
        }

        fn unwatch(&mut self, path: &Path) -> notify::Result<()> {
            self.inner.unwatch(path)
        }

        fn kind() -> notify::WatcherKind {
            notify::RecommendedWatcher::kind()
        }
    }

    /// `watch_readable_tree`'s per-directory check, exercised where it
    /// matters: a traversal already under way.
    ///
    /// The root is the first directory watched, so the flag goes up from
    /// inside that `watch` call, while the walk is between iterations. The
    /// loop's order then decides what "stops" means precisely, and it is
    /// worth being exact about:
    ///
    /// - Iteration 1 lists the root, watches it, and queues both children.
    /// - Iteration 2 pops one child and *then* tests the flag, so a pop
    ///   does still happen. That directory is dropped without being listed
    ///   or watched — the break precedes both.
    ///
    /// So what is guaranteed is narrower than "stops before popping
    /// anything": no watch is installed on either child (asserted below
    /// from `/proc`), neither child is listed, and nothing is reported as
    /// skipped. What is *not* claimed: the children are never touched at
    /// all — the root's listing already enumerated them and stat'd their
    /// file types while queuing them.
    ///
    /// The sibling test above only cancels before the walk starts, which
    /// the check at the top of the loop catches without any iteration
    /// having begun. This one needs no scheduling assumptions either: the
    /// wrapper flips the flag from inside the walk, so the ordering is the
    /// program's, not the scheduler's.
    #[test]
    fn a_cancelled_walk_stops_before_the_next_directory() {
        let dir = std::env::temp_dir().join(format!("diskwatch-midwalk-{}", std::process::id()));
        let deep = dir.join("deep");
        let other = dir.join("other");
        std::fs::create_dir_all(&deep).expect("deep dir");
        std::fs::create_dir_all(&other).expect("other dir");
        let _guard = TempDirGuard(dir.clone());

        let stop = AtomicBool::new(false);
        let skipped = {
            let mut watcher = CancelAfterNWatches {
                inner: notify::recommended_watcher(|_: notify::Result<notify::Event>| {}).unwrap(),
                stop: Some(&stop),
                remaining: 1,
            };
            let skipped = watch_readable_tree(&mut watcher, &dir, &stop);
            assert!(
                stop.load(Ordering::Relaxed),
                "the walk must have reached the point that cancels it"
            );
            // Seen from the kernel's side while the wrapper is still
            // alive: the root it did watch, and no watch on either child.
            // This is what pins the "no watch on the children" half of the
            // contract above; the other half is the `stop` assertion.
            #[cfg(target_os = "linux")]
            if let Some(live) = inotify_watched_files() {
                assert!(
                    live.contains(&file_key(&dir)),
                    "the root should be watched by the time the walk stops"
                );
                assert!(
                    !live.contains(&file_key(&deep)),
                    "an abandoned walk must not watch a queued child"
                );
                assert!(
                    !live.contains(&file_key(&other)),
                    "an abandoned walk must not watch a queued child"
                );
            }
            skipped
        };
        assert!(
            skipped.is_empty(),
            "an abandoned walk reports nothing: {:?}",
            skipped
        );
        // The wrapper has dropped by now, so the one watch the abandoned
        // walk did make — the root's — is gone too. `Some(false)` is a
        // confirmed leak; `None` only means `/proc` went away mid-wait.
        #[cfg(target_os = "linux")]
        assert_ne!(
            wait_for_watch_release(file_key(&dir)),
            Some(false),
            "the abandoned walk's own watch must be released with it"
        );
    }

    /// Cancellation while initialization is still ahead of the watcher:
    /// dropping it must request cancellation, return without waiting out the
    /// install, and the worker must terminate instead of installing the
    /// requested roots. The gate parks the worker before `install` runs, so
    /// the drop provably lands before any watch is installed; dropping the
    /// guard signals the worker's exit.
    #[test]
    fn dropping_a_watcher_during_initialization_cancels_the_worker() {
        struct ExitSignal(std::sync::mpsc::Sender<()>);
        impl Drop for ExitSignal {
            fn drop(&mut self) {
                let _ = self.0.send(());
            }
        }

        let dir = std::env::temp_dir().join(format!("diskwatch-drop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let _guard = TempDirGuard(dir.clone());
        let missing = dir.join("diskwatch-drop-missing");

        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let (exit_tx, exit_rx) = std::sync::mpsc::channel::<()>();
        let w = HotFileWatcher::start_background_inner(
            &[dir.as_path(), missing.as_path()],
            move || {
                let _ = release_rx.recv_timeout(Duration::from_secs(5));
                ExitSignal(exit_tx)
            },
            || (),
            None,
        );
        let state = w.state.clone();
        drop(w);
        // Drop must not wait out the install: the worker is still parked in
        // the gate, so its exit signal cannot have arrived yet.
        assert!(
            exit_rx.try_recv().is_err(),
            "Drop waited for the in-flight installation"
        );
        let _ = release_tx.send(());
        exit_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the worker must terminate after Drop requests cancellation");

        // Cancellation landed before the first watch, and Drop cleared the
        // installing flag on its way out: after Drop returns the state
        // claims nothing. A completed install would have dropped the
        // missing root and reported it instead.
        let s = state.lock().unwrap();
        assert!(s.watch_roots.is_empty(), "{:?}", s.watch_roots);
        assert!(s.error.is_none(), "{:?}", s.error);
    }

    /// The gap between `install` publishing its result and the worker taking
    /// ownership of the watcher is load-bearing: a `Drop` landing in it must
    /// retire the published roots immediately — nothing may claim a watcher
    /// no one owns, not even transiently. The `after_install` seam parks the
    /// worker in exactly that gap until the test has dropped the watcher.
    #[test]
    fn a_cancelled_transfer_rolls_back_the_published_roots() {
        struct ExitSignal(std::sync::mpsc::Sender<()>);
        impl Drop for ExitSignal {
            fn drop(&mut self) {
                let _ = self.0.send(());
            }
        }

        let dir = std::env::temp_dir().join(format!("diskwatch-rollback-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let _guard = TempDirGuard(dir.clone());

        let (reached_tx, reached_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let (exit_tx, exit_rx) = std::sync::mpsc::channel::<()>();
        let w = HotFileWatcher::start_background_inner(
            &[dir.as_path()],
            || (),
            move || {
                reached_tx.send(()).expect("signal the transfer point");
                let _ = release_rx.recv_timeout(Duration::from_secs(5));
                ExitSignal(exit_tx)
            },
            None,
        );
        let state = w.state.clone();
        let watcher_slot = w._watcher.clone();

        // Parked between publication and ownership: the installed roots are
        // already visible, the watcher is not stored yet.
        reached_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the worker must reach the transfer point");
        assert_eq!(state.lock().unwrap().watch_roots, vec![dir.clone()]);
        assert!(watcher_slot.lock().unwrap().is_none());

        // Cancellation lands in the gap: Drop must request it, retire the
        // publication, and return without waiting for the parked worker.
        drop(w);
        assert!(exit_rx.try_recv().is_err(), "Drop waited out the transfer");
        {
            let s = state.lock().unwrap();
            assert!(
                s.watch_roots.is_empty(),
                "a publication that landed before Drop must be retired by it"
            );
            assert!(s.error.is_none(), "{:?}", s.error);
        }

        let _ = release_tx.send(());
        exit_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the worker must terminate after the cancellation");

        // Cancellation also wins the ownership decision: nothing stores the
        // watcher, and the state stays retired.
        assert!(
            watcher_slot.lock().unwrap().is_none(),
            "a cancelled transfer must not store the watcher"
        );
        #[cfg(target_os = "linux")]
        assert_ne!(
            wait_for_watch_release(file_key(&dir)),
            Some(false),
            "the abandoned watcher must release its watch"
        );
        let s = state.lock().unwrap();
        assert!(s.watch_roots.is_empty(), "{:?}", s.watch_roots);
        assert!(s.error.is_none(), "{:?}", s.error);
    }

    /// The state-mutex interleaving: a publication that has passed its
    /// cancellation check can still be waiting on the state lock when
    /// `Drop` runs. Without the publication half of the gate that write
    /// can land after `Drop` retired the metadata; with it, the write
    /// either happens before `Drop` (which retires it) or waits behind the
    /// gate and finds the cancellation.
    ///
    /// Every round forces the rendezvous: an outside thread holds the state
    /// lock, the worker is parked after its last root, the worker is
    /// released *before* `Drop` starts — biasing, not guaranteeing, that
    /// its check runs while the cancellation is still unpublished — and
    /// `Drop` then runs on its own thread. Channels announce every step;
    /// no sleeps.
    ///
    /// Honest scope. The retirement half of the contract is deterministic
    /// here: a `Drop` that does not retire, or a publication that slips
    /// past a retired state, fails whichever round it happens in. The exact
    /// gate-less window is *not* deterministic: the un-gated check and the
    /// state write are adjacent instructions with no seam a test can park
    /// on, and the final write-vs-retire order is a mutex wake race whose
    /// odds drift with scheduling — repetition is the only lever. Measured
    /// against a deliberately gate-less build: at 64 rounds 4/170 runs
    /// still passed; at 256 rounds 600/600 runs failed. A green run is
    /// therefore strong evidence, not proof, that the publication is
    /// gated: this is a stress detector for that window, not deterministic
    /// coverage of it.
    #[test]
    fn a_publication_racing_drop_leaves_no_stale_state() {
        struct ExitSignal(std::sync::mpsc::Sender<()>);
        impl Drop for ExitSignal {
            fn drop(&mut self) {
                let _ = self.0.send(());
            }
        }

        // Repetition is the only lever on the wake race above; see the
        // scope note in this test's docs for what the loop does and does
        // not prove.
        const ROUNDS: usize = 256;

        let dir = std::env::temp_dir().join(format!("diskwatch-racelock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let _guard = TempDirGuard(dir.clone());

        for round in 0..ROUNDS {
            let (reached_tx, reached_rx) = std::sync::mpsc::channel::<()>();
            let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
            let (exit_tx, exit_rx) = std::sync::mpsc::channel::<()>();
            let w = HotFileWatcher::start_background_inner(
                &[dir.as_path()],
                || (),
                move || ExitSignal(exit_tx),
                Some(Box::new(move |_root: &Path| {
                    reached_tx.send(()).expect("signal the last root");
                    let _ = go_rx.recv_timeout(Duration::from_secs(5));
                })),
            );
            let state = w.state.clone();

            // Hold the state lock from an outside thread: the worker's
            // publication cannot finish until this thread lets go.
            let (held_tx, held_rx) = std::sync::mpsc::channel::<()>();
            let (let_go_tx, let_go_rx) = std::sync::mpsc::channel::<()>();
            let state_holder = state.clone();
            let holder = thread::spawn(move || {
                let _guard = state_holder.lock().unwrap();
                held_tx.send(()).expect("signal the held lock");
                let _ = let_go_rx.recv_timeout(Duration::from_secs(5));
            });
            held_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("the holder must take the state lock");

            // The worker is now parked after its root, with the watch
            // already installed: the publication is the next thing it will
            // attempt. The setup repeats identically every round, so
            // checking that the watch is live once keeps the loop cheap
            // and the test honest about watching a real root.
            reached_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("the worker must reach the last root");
            if round == 0 {
                #[cfg(target_os = "linux")]
                if let Some(seen) = inotify_watched_files() {
                    assert!(
                        seen.contains(&file_key(&dir)),
                        "the root must be watched before the race starts"
                    );
                }
            }

            // Release the worker before creating the dropper. This biases
            // scheduling toward letting the worker reach its publication
            // path while cancellation is still unpublished, but does not
            // guarantee that ordering. The race remains a probabilistic
            // stress detector; see the scope note above.
            // The dropper then owns `w`.
            let (dropped_tx, dropped_rx) = std::sync::mpsc::channel::<()>();
            let _ = go_tx.send(());
            let dropper = thread::spawn(move || {
                drop(w);
                let _ = dropped_tx.send(());
            });
            let _ = let_go_tx.send(());

            dropped_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("Drop must return without waiting out the install");
            holder.join().expect("holder thread");
            dropper.join().expect("dropper thread");

            // Strict contract: once Drop has returned, no publication —
            // past or future — can claim watched roots or an error.
            {
                let s = state.lock().unwrap();
                assert!(
                    s.watch_roots.is_empty(),
                    "round {round}: roots after Drop: {:?}",
                    s.watch_roots
                );
                assert!(s.error.is_none(), "round {round}: error {:?}", s.error);
            }
            exit_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("the worker must terminate after the cancellation");
            // `Some(false)` is a confirmed leak; `None` only means `/proc`
            // went away mid-wait, which proves nothing either way.
            #[cfg(target_os = "linux")]
            assert_ne!(
                wait_for_watch_release(file_key(&dir)),
                Some(false),
                "round {round}: the abandoned watcher must release its watch"
            );
            {
                let s = state.lock().unwrap();
                assert!(
                    s.watch_roots.is_empty(),
                    "round {round}: roots after the worker exited: {:?}",
                    s.watch_roots
                );
                assert!(s.error.is_none(), "round {round}: error {:?}", s.error);
            }
        }
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
