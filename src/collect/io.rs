//! Per-device IO sampling at 5Hz.
//!
//! Both supported platforms expose cumulative split-direction byte +
//! operation + service-time counters:
//! - **macOS**: `IOBlockStorageDriver` Statistics dict via
//!   `collect::iokit` (`ioreg -c IOBlockStorageDriver -r -l -w 0`).
//! - **Linux**: `/proc/diskstats` columns 5/9 (sectors) and 6/10
//!   (milliseconds spent on IO).
//!
//! Each sample at 5Hz computes the avg per-op service time (Total Time
//! Δ / Operations Δ) for the interval. We retain the last
//! `WINDOW_SAMPLES` of those observations per device and surface
//! `p50 / p99 / p999` against that rolling window.
//!
//! **Honest scope:** these are *percentiles of per-tick averages*, not
//! of individual operations. They catch sustained slow stretches; they
//! cannot see a single 50ms outlier hiding inside an otherwise-fast
//! 200ms-sample window. Real per-op p99 needs eBPF biolatency (Linux)
//! or IOReport subscription (macOS), both deferred.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

/// Minimum interval between `sample()` calls actually doing work.
/// 200ms = 5Hz, matching the technical doc's per-device IO loop.
const SAMPLE_INTERVAL: Duration = Duration::from_millis(200);

/// Sparkline ring length — 240 samples at 5Hz = 48s of throughput.
/// Large enough that on a 130-wide terminal the visible window already
/// has real samples once the ring is warm.
const RING_LEN: usize = 240;

/// Host-wide 1 Hz ring length, in seconds of history.
///
/// The Lite view draws one sample per chart column and its charts are
/// as wide as the terminal, so this has to cover the widest sensible
/// terminal rather than Lite's 78-column reference grid. Lite takes the
/// rightmost `content_w` samples and left-pads if the ring is still
/// warming.
pub const AGG_RING_SECS: usize = 300;

/// Latency observation window — 300 samples at 5Hz = 60s of percentile
/// history per device per direction.
const LATENCY_WINDOW: usize = 300;

#[derive(Debug, Default, Clone)]
pub struct IoTick {
    pub device: String,
    /// Combined read + write bytes/sec.
    pub bps: f64,
    /// Per-direction byte rates.
    pub split: Option<(f64, f64)>,
    /// Avg per-op service time for the most recent interval, in µs,
    /// (read, write). `None` when no ops happened. Kept for callers
    /// that want the most-recent observation rather than the windowed
    /// percentile (e.g. drill-in views).
    #[allow(dead_code)]
    pub latency_avg: Option<(f64, f64)>,
    /// Percentiles of avg-per-op samples over the last `LATENCY_WINDOW`
    /// observations. See module docs for what this measures.
    pub latency_pct: Option<LatencyPct>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct LatencyPct {
    pub p50_r: f64,
    pub p99_r: f64,
    /// p99.9 — surfaces with a 300-sample window what p99 can't catch
    /// with only 100. Not yet displayed; reserved for a drill-in view.
    #[allow(dead_code)]
    pub p999_r: f64,
    pub p50_w: f64,
    pub p99_w: f64,
    #[allow(dead_code)]
    pub p999_w: f64,
}

#[derive(Debug, Default, Clone)]
pub struct DeviceHistory {
    pub combined: VecDeque<f64>,
    /// Per-tick avg read latency in µs.
    pub read_us: VecDeque<f64>,
    /// Per-tick avg write latency in µs.
    pub write_us: VecDeque<f64>,
}

#[derive(Debug, Default, Clone, Copy)]
struct DeviceTotals {
    bytes_read: u64,
    bytes_written: u64,
    ops_read: u64,
    ops_written: u64,
    total_time_read_ns: u64,
    total_time_write_ns: u64,
}

/// Host-wide read/write byte rates at 1 Hz, summed across devices.
///
/// The per-device `history` rings are 5 Hz and per-device; the Lite
/// view wants one aggregate sample per chart column per second. Rather
/// than resample per frame, we accumulate the 5 Hz samples and emit
/// their mean once a second — the mean, not the last value, because a
/// second of throughput is the average over that second and taking the
/// final 200ms slice would show phantom idle gaps between bursts.
#[derive(Debug, Default)]
pub struct AggHistory {
    pub read_bps: VecDeque<f64>,
    pub write_bps: VecDeque<f64>,
    /// Accumulator for the second currently in progress.
    acc_read: f64,
    acc_write: f64,
    acc_n: u32,
    last_emit: Option<Instant>,
}

impl AggHistory {
    fn accumulate(&mut self, read_bps: f64, write_bps: f64, now: Instant) {
        self.acc_read += read_bps;
        self.acc_write += write_bps;
        self.acc_n += 1;

        let last = *self.last_emit.get_or_insert(now);
        if now.duration_since(last) < Duration::from_secs(1) {
            return;
        }
        let n = self.acc_n.max(1) as f64;
        push_ring(&mut self.read_bps, self.acc_read / n, AGG_RING_SECS);
        push_ring(&mut self.write_bps, self.acc_write / n, AGG_RING_SECS);
        self.acc_read = 0.0;
        self.acc_write = 0.0;
        self.acc_n = 0;
        self.last_emit = Some(now);
    }
}

pub struct IoCollector {
    last_sample: Instant,
    prev_totals: HashMap<String, DeviceTotals>,
    pub history: HashMap<String, DeviceHistory>,
    /// Host-wide 1 Hz rings. See [`AggHistory`].
    pub agg: AggHistory,
    pub latest: Vec<IoTick>,
}

impl IoCollector {
    pub fn new() -> Self {
        Self {
            // Offset the baseline back so the first `sample()` actually runs.
            last_sample: Instant::now() - SAMPLE_INTERVAL,
            prev_totals: HashMap::new(),
            history: HashMap::new(),
            agg: AggHistory::default(),
            latest: Vec::new(),
        }
    }

    /// Current host-wide (read, write) byte rates, summed across devices.
    pub fn totals_bps(&self) -> (f64, f64) {
        self.latest
            .iter()
            .filter_map(|t| t.split)
            .fold((0.0, 0.0), |(r, w), (dr, dw)| (r + dr, w + dw))
    }

    /// Called from the main loop. Internally rate-limits to 5Hz, so
    /// it's safe to call as often as the loop tick fires.
    pub fn sample(&mut self) {
        let now = Instant::now();
        let elapsed_dur = now - self.last_sample;
        if elapsed_dur < SAMPLE_INTERVAL {
            return;
        }
        let elapsed = elapsed_dur.as_secs_f64().max(0.001);
        self.last_sample = now;

        let totals = self.read_totals();
        let mut new_latest: Vec<IoTick> = Vec::new();
        for (device, t) in &totals {
            // The counters are cumulative since boot. On the first
            // sighting of a device there is nothing to subtract, so a
            // zero baseline would report every byte written since boot
            // as this tick's throughput — a 400 GB/s spike that then
            // sets the scale for every chart and poisons peak/avg for
            // the rest of the session. Record the baseline, report
            // nothing, and start measuring on the next tick.
            let Some(prev) = self.prev_totals.get(device).copied() else {
                new_latest.push(IoTick {
                    device: device.clone(),
                    bps: 0.0,
                    split: Some((0.0, 0.0)),
                    latency_avg: None,
                    latency_pct: None,
                });
                continue;
            };

            let read_bytes_delta = t.bytes_read.saturating_sub(prev.bytes_read) as f64;
            let write_bytes_delta = t.bytes_written.saturating_sub(prev.bytes_written) as f64;
            let read_ops_delta = t.ops_read.saturating_sub(prev.ops_read);
            let write_ops_delta = t.ops_written.saturating_sub(prev.ops_written);
            let read_time_delta = t.total_time_read_ns.saturating_sub(prev.total_time_read_ns);
            let write_time_delta = t
                .total_time_write_ns
                .saturating_sub(prev.total_time_write_ns);

            let read_bps = read_bytes_delta / elapsed;
            let write_bps = write_bytes_delta / elapsed;
            let bps = read_bps + write_bps;

            let (latency_avg, sample_r_us, sample_w_us) = if read_ops_delta + write_ops_delta == 0 {
                (None, None, None)
            } else {
                let r_us = if read_ops_delta > 0 {
                    Some((read_time_delta as f64 / read_ops_delta as f64) / 1_000.0)
                } else {
                    None
                };
                let w_us = if write_ops_delta > 0 {
                    Some((write_time_delta as f64 / write_ops_delta as f64) / 1_000.0)
                } else {
                    None
                };
                (Some((r_us.unwrap_or(0.0), w_us.unwrap_or(0.0))), r_us, w_us)
            };

            let h = self.history.entry(device.clone()).or_default();
            push_ring(&mut h.combined, bps, RING_LEN);
            if let Some(v) = sample_r_us {
                push_ring(&mut h.read_us, v, LATENCY_WINDOW);
            }
            if let Some(v) = sample_w_us {
                push_ring(&mut h.write_us, v, LATENCY_WINDOW);
            }

            // Recompute percentiles from the windows. Sorts a copy each
            // time — cheap at this scale (≤300 samples).
            let latency_pct = if !h.read_us.is_empty() || !h.write_us.is_empty() {
                let (p50_r, p99_r, p999_r) = percentiles(&h.read_us);
                let (p50_w, p99_w, p999_w) = percentiles(&h.write_us);
                Some(LatencyPct {
                    p50_r,
                    p99_r,
                    p999_r,
                    p50_w,
                    p99_w,
                    p999_w,
                })
            } else {
                None
            };

            new_latest.push(IoTick {
                device: device.clone(),
                bps,
                split: Some((read_bps, write_bps)),
                latency_avg,
                latency_pct,
            });
        }
        new_latest.sort_by(|a, b| a.device.cmp(&b.device));

        // Host-wide rings, summed over this tick's devices. Done here
        // rather than from `latest` on demand so a paused UI doesn't
        // leave a hole in the series it later scrolls through.
        let (agg_r, agg_w) = new_latest
            .iter()
            .filter_map(|t| t.split)
            .fold((0.0, 0.0), |(r, w), (dr, dw)| (r + dr, w + dw));
        self.agg.accumulate(agg_r, agg_w, now);

        self.latest = new_latest;
        self.prev_totals = totals;
    }

    fn read_totals(&self) -> HashMap<String, DeviceTotals> {
        #[cfg(target_os = "macos")]
        {
            totals_macos()
        }
        #[cfg(target_os = "linux")]
        {
            diskstats_totals_linux()
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            HashMap::new()
        }
    }
}

#[cfg(target_os = "macos")]
fn totals_macos() -> HashMap<String, DeviceTotals> {
    let raw = crate::collect::iokit::collect();
    raw.into_iter()
        .map(|(name, s)| {
            (
                name,
                DeviceTotals {
                    bytes_read: s.bytes_read,
                    bytes_written: s.bytes_written,
                    ops_read: s.ops_read,
                    ops_written: s.ops_written,
                    total_time_read_ns: s.total_time_read_ns,
                    total_time_write_ns: s.total_time_write_ns,
                },
            )
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn diskstats_totals_linux() -> HashMap<String, DeviceTotals> {
    const SECTOR_BYTES: u64 = 512;
    const MS_TO_NS: u64 = 1_000_000;
    let Ok(text) = std::fs::read_to_string("/proc/diskstats") else {
        return HashMap::new();
    };
    let mut out = HashMap::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 11 {
            continue;
        }
        let name = fields[2];
        if name.starts_with("loop") || name.starts_with("ram") {
            continue;
        }
        if is_partition_name(name) {
            continue;
        }
        let Ok(reads) = fields[3].parse::<u64>() else {
            continue;
        };
        let Ok(sectors_read) = fields[5].parse::<u64>() else {
            continue;
        };
        let Ok(ms_reading) = fields[6].parse::<u64>() else {
            continue;
        };
        let Ok(writes) = fields[7].parse::<u64>() else {
            continue;
        };
        let Ok(sectors_written) = fields[9].parse::<u64>() else {
            continue;
        };
        let Ok(ms_writing) = fields[10].parse::<u64>() else {
            continue;
        };
        out.insert(
            name.to_string(),
            DeviceTotals {
                bytes_read: sectors_read.saturating_mul(SECTOR_BYTES),
                bytes_written: sectors_written.saturating_mul(SECTOR_BYTES),
                ops_read: reads,
                ops_written: writes,
                total_time_read_ns: ms_reading.saturating_mul(MS_TO_NS),
                total_time_write_ns: ms_writing.saturating_mul(MS_TO_NS),
            },
        );
    }
    out
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn is_partition_name(name: &str) -> bool {
    if name.starts_with("nvme") {
        return name.contains('p');
    }
    if name.starts_with("mmcblk") {
        return name.contains('p');
    }
    if name.starts_with("dm-") {
        return false;
    }
    name.chars()
        .last()
        .map(|c| c.is_ascii_digit())
        .unwrap_or(false)
}

fn push_ring(q: &mut VecDeque<f64>, v: f64, cap: usize) {
    if q.len() == cap {
        q.pop_front();
    }
    q.push_back(v);
}

/// Returns (p50, p99, p999) of the values in `samples`. Empty input
/// yields zeros so the caller can use them in arithmetic without
/// branching.
fn percentiles(samples: &VecDeque<f64>) -> (f64, f64, f64) {
    if samples.is_empty() {
        return (0.0, 0.0, 0.0);
    }
    let mut v: Vec<f64> = samples.iter().copied().collect();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let pct = |p: f64| {
        let idx = ((p / 100.0) * (v.len() - 1) as f64).round() as usize;
        v[idx.min(v.len() - 1)]
    };
    (pct(50.0), pct(99.0), pct(99.9))
}

/// Sums device rates for the Overview "AGG IO" panel.
pub fn aggregate(latest: &[IoTick]) -> (f64, f64) {
    let combined: f64 = latest.iter().map(|t| t.bps).sum();
    let write: f64 = latest.iter().filter_map(|t| t.split.map(|(_, w)| w)).sum();
    (combined, write)
}

/// Worst p99 across all devices. Reads the max of read-p99 and
/// write-p99 per device, then takes the max across devices.
pub fn worst_p99_us(latest: &[IoTick]) -> Option<f64> {
    let mut worst: Option<f64> = None;
    for t in latest {
        if let Some(pct) = t.latency_pct {
            let candidate = pct.p99_r.max(pct.p99_w);
            worst = Some(worst.map_or(candidate, |w| w.max(candidate)));
        }
    }
    worst
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sata_disks_and_partitions() {
        assert!(!is_partition_name("sda"));
        assert!(!is_partition_name("sdb"));
        assert!(is_partition_name("sda1"));
        assert!(is_partition_name("sdb12"));
    }

    #[test]
    fn nvme_disks_and_partitions() {
        assert!(!is_partition_name("nvme0n1"));
        assert!(is_partition_name("nvme0n1p1"));
        assert!(is_partition_name("nvme1n2p5"));
    }

    #[test]
    fn mmc_quirks() {
        assert!(!is_partition_name("mmcblk0"));
        assert!(is_partition_name("mmcblk0p1"));
    }

    #[test]
    fn device_mapper_is_whole() {
        assert!(!is_partition_name("dm-0"));
        assert!(!is_partition_name("dm-12"));
    }

    #[test]
    fn percentiles_basic() {
        let v: VecDeque<f64> = (1..=100).map(|x| x as f64).collect();
        let (p50, p99, p999) = percentiles(&v);
        // Nearest-rank with (N-1) indexing: idx = round(p * 99 / 100).
        // p50 → idx 50 → v[50] = 51.
        // p99 → idx 98 → v[98] = 99.
        // p999 → idx 99 → v[99] = 100.
        assert_eq!(p50, 51.0);
        assert_eq!(p99, 99.0);
        assert_eq!(p999, 100.0);
    }

    #[test]
    fn percentiles_with_outlier() {
        // 99 fast samples + 1 huge outlier. With only 100 samples, the
        // outlier surfaces at p99.9 (round(0.999 * 99) = 99 → last
        // value) but not at p99 (round(0.99 * 99) = 98 → second-last).
        // This is the limitation the IO tab footer note is about.
        let mut v: VecDeque<f64> = (0..99).map(|_| 100.0).collect();
        v.push_back(50_000.0);
        let (p50, p99, p999) = percentiles(&v);
        assert_eq!(p50, 100.0);
        assert_eq!(p99, 100.0);
        assert_eq!(p999, 50_000.0);
    }

    #[test]
    fn percentiles_empty() {
        let v: VecDeque<f64> = VecDeque::new();
        assert_eq!(percentiles(&v), (0.0, 0.0, 0.0));
    }
}
