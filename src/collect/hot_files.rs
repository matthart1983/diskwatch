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
        let mut s = state.lock().unwrap();
        s.watch_roots = roots.iter().map(|p| p.to_path_buf()).collect();
        drop(s);

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

        for r in roots {
            if let Err(e) = watcher.watch(r, RecursiveMode::Recursive) {
                state.lock().unwrap().error =
                    Some(format!("failed to watch {}: {}", r.display(), e));
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
