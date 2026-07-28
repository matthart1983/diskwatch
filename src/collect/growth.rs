//! Per-mount capacity growth trend and time-to-full projection.
//!
//! The Lite view's capacity line is built on the premise that "11 days
//! left" is more actionable than "94% full" — which means the trend is
//! the load-bearing part, not the gauge. Nothing else in diskwatch
//! computed it: `collect::filesystems` documented growth as something
//! "the App computes from a snapshot ring", and the FS tab renders a
//! placeholder. This is that ring.
//!
//! ## How the trend is measured
//! One `used_bytes` sample per mount per second, kept for
//! [`WINDOW`]. The trend is the slope across the window — last sample
//! minus first, over the elapsed time between them.
//!
//! A session EWMA would be cheaper to state but jumpier to read: a
//! single `cargo build` moves it hard and it takes a long time to come
//! back. A window slope answers a question the user can actually check
//! ("over the last ten minutes"), and recovers as soon as the burst
//! leaves the window.
//!
//! ## Why nothing is projected early
//! Extrapolating a slope measured over three seconds produces
//! "4 hours left" during any build, which is worse than silence. We
//! report no trend at all until [`MIN_OBSERVATION`] has elapsed, and
//! suppress the projection whenever the trend is flat or negative —
//! a shrinking filesystem has no time-to-full.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// How far back the trend looks. Long enough to ride out a single
/// build or download, short enough to react within a coffee break.
const WINDOW: Duration = Duration::from_secs(600);

/// Minimum observation span before any trend is reported. Below this
/// the slope is noise amplified by extrapolation.
pub const MIN_OBSERVATION: Duration = Duration::from_secs(60);

/// Minimum interval between retained samples. The App ticks usage at
/// 1 Hz; this guards against a faster caller inflating the ring.
const SAMPLE_INTERVAL: Duration = Duration::from_millis(950);

/// Trend for one mount.
#[derive(Debug, Clone, Copy)]
pub struct Growth {
    /// Signed bytes/day. Negative when the filesystem is shrinking.
    pub bytes_per_day: f64,
    /// Days until `used` reaches `size` at the current trend. `None`
    /// when the trend is flat or negative — there is no deadline to
    /// project.
    pub days_until_full: Option<f64>,
}

#[derive(Debug)]
struct Series {
    samples: Vec<(Instant, u64)>,
    last_push: Instant,
}

#[derive(Default)]
pub struct GrowthTracker {
    by_mount: HashMap<String, Series>,
}

impl GrowthTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the current usage for every mount. Safe to call every
    /// tick — it rate-limits internally. Mounts that disappear (an
    /// unmounted volume) are dropped so their history can't be
    /// resurrected against a different filesystem later.
    pub fn observe(&mut self, filesystems: &[crate::collect::FsTick]) {
        let now = Instant::now();
        for fs in filesystems {
            // A mount reporting zero total space is a synthetic entry
            // (devfs, map auto_home); it has no capacity to fill.
            if fs.size_bytes == 0 {
                continue;
            }
            let series = self
                .by_mount
                .entry(fs.mount.clone())
                .or_insert_with(|| Series {
                    samples: Vec::new(),
                    last_push: now - SAMPLE_INTERVAL,
                });
            if now.duration_since(series.last_push) < SAMPLE_INTERVAL {
                continue;
            }
            series.last_push = now;
            series.samples.push((now, fs.used_bytes));
            series
                .samples
                .retain(|(t, _)| now.duration_since(*t) <= WINDOW);
        }

        let live: std::collections::HashSet<&str> =
            filesystems.iter().map(|f| f.mount.as_str()).collect();
        self.by_mount
            .retain(|mount, _| live.contains(mount.as_str()));
    }

    /// Trend for one mount, or `None` while the window is still too
    /// short to say anything honest.
    pub fn growth(&self, mount: &str, used_bytes: u64, size_bytes: u64) -> Option<Growth> {
        let series = self.by_mount.get(mount)?;
        let first = series.samples.first()?;
        let last = series.samples.last()?;
        let span = last.0.duration_since(first.0);
        if span < MIN_OBSERVATION {
            return None;
        }

        let delta = last.1 as f64 - first.1 as f64;
        let bytes_per_sec = delta / span.as_secs_f64();
        let bytes_per_day = bytes_per_sec * 86_400.0;

        // Sub-KB/s drift is measurement noise on a live filesystem, not
        // a trend worth extrapolating to a deadline.
        let days_until_full = if bytes_per_sec > 1024.0 {
            let free = size_bytes.saturating_sub(used_bytes) as f64;
            Some(free / (bytes_per_sec * 86_400.0))
        } else {
            None
        };

        Some(Growth {
            bytes_per_day,
            days_until_full,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fs(mount: &str, used: u64, size: u64) -> crate::collect::FsTick {
        crate::collect::FsTick {
            mount: mount.to_string(),
            device: "test".into(),
            fs_type: "test".into(),
            size_bytes: size,
            used_bytes: used,
            avail_bytes: size - used,
            inode_pct: None,
            is_removable: false,
            is_system: false,
        }
    }

    /// Build a tracker with a synthetic history, bypassing the 1 Hz
    /// rate limit so tests don't have to sleep.
    fn seeded(mount: &str, points: &[(u64, u64)]) -> GrowthTracker {
        let now = Instant::now();
        let mut t = GrowthTracker::new();
        t.by_mount.insert(
            mount.to_string(),
            Series {
                samples: points
                    .iter()
                    .map(|(secs_ago, used)| (now - Duration::from_secs(*secs_ago), *used))
                    .collect(),
                last_push: now,
            },
        );
        t
    }

    #[test]
    fn reports_nothing_before_the_minimum_observation() {
        // 10s of history extrapolates to nonsense — the whole reason
        // the guard exists.
        let t = seeded("/", &[(10, 0), (0, 1_000_000_000)]);
        assert!(t.growth("/", 1_000_000_000, 2_000_000_000).is_none());
    }

    #[test]
    fn projects_days_until_full_from_the_window_slope() {
        // 100 MB over 100s = 1 MB/s. 500 MB free → ~500s ≈ 0.0058 days.
        let t = seeded("/", &[(100, 0), (0, 100_000_000)]);
        let g = t
            .growth("/", 100_000_000, 600_000_000)
            .expect("window is long enough");
        assert!((g.bytes_per_day - 86_400_000_000.0).abs() < 1e6);
        let days = g.days_until_full.expect("growing, so there is a deadline");
        assert!((days - 500.0 / 86_400.0).abs() < 1e-4, "got {days}");
    }

    #[test]
    fn a_shrinking_filesystem_has_no_deadline() {
        let t = seeded("/", &[(100, 500_000_000), (0, 400_000_000)]);
        let g = t
            .growth("/", 400_000_000, 1_000_000_000)
            .expect("has window");
        assert!(g.bytes_per_day < 0.0);
        assert!(g.days_until_full.is_none());
    }

    #[test]
    fn flat_usage_has_no_deadline() {
        // Byte-level jitter must not become "full in 3 days".
        let t = seeded("/", &[(100, 500_000_000), (0, 500_000_100)]);
        let g = t
            .growth("/", 500_000_100, 1_000_000_000)
            .expect("has window");
        assert!(g.days_until_full.is_none());
    }

    #[test]
    fn unmounted_volumes_are_forgotten() {
        let mut t = GrowthTracker::new();
        t.observe(&[fs("/", 1, 100), fs("/mnt/usb", 1, 100)]);
        assert_eq!(t.by_mount.len(), 2);
        t.observe(&[fs("/", 1, 100)]);
        assert_eq!(t.by_mount.len(), 1);
        assert!(!t.by_mount.contains_key("/mnt/usb"));
    }

    #[test]
    fn zero_sized_mounts_are_ignored() {
        let mut t = GrowthTracker::new();
        t.observe(&[fs("/dev", 0, 0)]);
        assert!(t.by_mount.is_empty());
    }
}
