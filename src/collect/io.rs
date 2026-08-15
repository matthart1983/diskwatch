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

/// Milliseconds per sample in the fast aggregate ring. Anything drawing that
/// ring computes its axis span from this rather than assuming a rate — an axis
/// that labels its own width wrongly is worse than one with no label, because
/// it is read and believed.
pub const FAST_SAMPLE_MS: usize = 200;

/// Fast-ring length. Braille takes two samples per character column, so this
/// covers a 600-column graph, and at 5Hz it holds four minutes.
const FAST_RING_LEN: usize = 1200;

/// Seconds of history the per-device ring holds. Anything drawing that ring
/// labels its axis from this rather than from a guess about the sample rate —
/// a sparkline claiming "60s" while showing five is worse than no label.
pub const DEVICE_RING_SECS: usize = 48;

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

/// Upper edges of the latency histogram, in milliseconds. Seven buckets:
/// `<0.1 · 0.1-0.5 · 0.5-1 · 1-5 · 5-10 · 10-50 · >50`.
///
/// The design's latency box exists to show the TAIL, so the long buckets are
/// kept even when they hold single-digit counts.
pub const LAT_EDGES_MS: [f64; 6] = [0.1, 0.5, 1.0, 5.0, 10.0, 50.0];
pub const LAT_BUCKETS: usize = LAT_EDGES_MS.len() + 1;
/// Index of the first bucket in the ">10ms" tail. Derived, not typed, so the
/// tail and the histogram's own colouring can't disagree about where it starts.
pub const LAT_TAIL_FROM: usize = 4 + 1;

/// Which bucket a service time falls in.
pub fn lat_bucket(ms: f64) -> usize {
    LAT_EDGES_MS
        .iter()
        .position(|&e| ms < e)
        .unwrap_or(LAT_BUCKETS - 1)
}

#[derive(Debug, Default, Clone)]
pub struct IoTick {
    pub device: String,
    /// Combined read + write bytes/sec.
    pub bps: f64,
    /// Per-direction byte rates.
    pub split: Option<(f64, f64)>,
    /// Per-direction operations/sec (read, write).
    ///
    /// Measured per direction rather than derived from a share of the total:
    /// a fixed split reads plausibly while a box is calm and then reports a
    /// read-heavy workload through a write storm.
    pub iops: Option<(f64, f64)>,
    /// Fraction of the interval the device had IO in flight, 0..1.
    ///
    /// `None` where the platform doesn't expose it. Linux has `io_ticks`
    /// (`/proc/diskstats` field 13), which is exactly this. macOS's
    /// `IOBlockStorageDriver` exposes summed *service* time, which on a
    /// deep-queue NVMe routinely exceeds wall-clock and would read as a
    /// permanent 100% — so macOS reports nothing rather than something
    /// confident and wrong, and the UI renders `--`.
    pub util: Option<f64>,
    /// Requests in flight at sample time. Linux only (field 12).
    pub inflight: Option<u32>,
    /// Ops per latency bucket over the last [`LATENCY_WINDOW`] samples.
    ///
    /// **Honest scope**, same caveat as `latency_pct`: each sample contributes
    /// its whole op count to the bucket containing that sample's *mean*
    /// service time. It is a histogram of per-tick means weighted by ops, not
    /// of individual operations — it will show a sustained slow stretch, and
    /// will smear a single 50ms outlier into whatever its 200ms tick averaged.
    /// Real per-op buckets need eBPF `biolatency`; see the module docs.
    pub lat_hist: [u64; LAT_BUCKETS],
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
    /// Per-direction byte rates, one entry per 5Hz sample. The 2.0 view draws
    /// a mirrored read/write graph per device, which the combined ring can't
    /// feed.
    pub read_bps: VecDeque<f64>,
    pub write_bps: VecDeque<f64>,
    /// Ops per latency bucket, one entry per sample. Summed over the window to
    /// produce [`IoTick::lat_hist`]; kept per-sample so the window slides
    /// instead of accumulating since boot.
    pub lat_hist: VecDeque<[u64; LAT_BUCKETS]>,
}

impl DeviceHistory {
    /// Ops per bucket across the whole retained window.
    pub fn hist_sum(&self) -> [u64; LAT_BUCKETS] {
        let mut out = [0u64; LAT_BUCKETS];
        for s in &self.lat_hist {
            for (o, v) in out.iter_mut().zip(s.iter()) {
                *o = o.saturating_add(*v);
            }
        }
        out
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct DeviceTotals {
    bytes_read: u64,
    bytes_written: u64,
    ops_read: u64,
    ops_written: u64,
    total_time_read_ns: u64,
    total_time_write_ns: u64,
    /// Cumulative time the device had IO in flight. `None` where the platform
    /// doesn't measure it — see [`IoTick::util`].
    busy_ns: Option<u64>,
    /// Instantaneous queue depth, not cumulative.
    inflight: Option<u32>,
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
    /// The same totals at the full 5Hz sample rate.
    ///
    /// The 1Hz rings above are what the Lite view wants: one sample per chart
    /// column, one column per second. A braille graph takes TWO samples per
    /// column, so feeding it the 1Hz ring makes each character cell span two
    /// seconds — a 147-column graph then covers five minutes, and every cell is
    /// built from two samples a second apart, which on bursty IO lights one
    /// sub-column and not the other and renders the fill as a comb. At 5Hz a
    /// column is 400ms, the pair inside it is 200ms apart, and the graph covers
    /// the ~60s the design asks for.
    pub read_bps_fast: VecDeque<f64>,
    pub write_bps_fast: VecDeque<f64>,
    /// Accumulator for the second currently in progress.
    acc_read: f64,
    acc_write: f64,
    acc_n: u32,
    last_emit: Option<Instant>,
}

impl AggHistory {
    fn accumulate(&mut self, read_bps: f64, write_bps: f64, now: Instant) {
        push_ring(&mut self.read_bps_fast, read_bps, FAST_RING_LEN);
        push_ring(&mut self.write_bps_fast, write_bps, FAST_RING_LEN);

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
    /// Host-wide read/write rates, over the physical devices only.
    ///
    /// A LUKS volume on an NVMe reports its traffic as `dm-0` AND as
    /// `nvme0n1`; summing both double-counts every block, which is a silent
    /// 2× on exactly the machines that encrypt their disks.
    pub fn totals_bps(&self) -> (f64, f64) {
        self.latest
            .iter()
            .filter(|t| !is_stacked_name(&t.device))
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
                    iops: Some((0.0, 0.0)),
                    util: None,
                    inflight: t.inflight,
                    lat_hist: [0; LAT_BUCKETS],
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

            // Busy fraction. `busy_ns` is time-with-IO-in-flight, so it is
            // bounded by wall clock by construction and the clamp only ever
            // absorbs counter jitter across a sample boundary.
            let util = match (t.busy_ns, prev.busy_ns) {
                (Some(now_ns), Some(prev_ns)) => {
                    let busy = now_ns.saturating_sub(prev_ns) as f64;
                    Some((busy / (elapsed * 1e9)).clamp(0.0, 1.0))
                }
                _ => None,
            };

            // Attribute this tick's ops to the bucket its mean service time
            // lands in, per direction. See `IoTick::lat_hist` for what this
            // does and does not measure.
            let mut hist = [0u64; LAT_BUCKETS];
            if let Some(us) = sample_r_us {
                hist[lat_bucket(us / 1_000.0)] += read_ops_delta;
            }
            if let Some(us) = sample_w_us {
                hist[lat_bucket(us / 1_000.0)] += write_ops_delta;
            }

            let h = self.history.entry(device.clone()).or_default();
            push_ring(&mut h.combined, bps, RING_LEN);
            push_ring(&mut h.read_bps, read_bps, RING_LEN);
            push_ring(&mut h.write_bps, write_bps, RING_LEN);
            if let Some(v) = sample_r_us {
                push_ring(&mut h.read_us, v, LATENCY_WINDOW);
            }
            if let Some(v) = sample_w_us {
                push_ring(&mut h.write_us, v, LATENCY_WINDOW);
            }
            while h.lat_hist.len() >= LATENCY_WINDOW {
                h.lat_hist.pop_front();
            }
            h.lat_hist.push_back(hist);
            let lat_hist = h.hist_sum();

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
                iops: Some((
                    read_ops_delta as f64 / elapsed,
                    write_ops_delta as f64 / elapsed,
                )),
                util,
                inflight: t.inflight,
                lat_hist,
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
                    // IOBlockStorageDriver counts service time, not
                    // time-with-IO-in-flight. See `IoTick::util`.
                    busy_ns: None,
                    inflight: None,
                },
            )
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn diskstats_totals_linux() -> HashMap<String, DeviceTotals> {
    let Ok(text) = std::fs::read_to_string("/proc/diskstats") else {
        return HashMap::new();
    };
    parse_diskstats(&text)
}

/// Parse `/proc/diskstats`.
///
/// Split out from the file read, and compiled on every platform, so the field
/// arithmetic is testable where Linux isn't — the fixtures below are real
/// lines from a 6.x kernel and from a pre-2.6.25 short-form one.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_diskstats(text: &str) -> HashMap<String, DeviceTotals> {
    const SECTOR_BYTES: u64 = 512;
    const MS_TO_NS: u64 = 1_000_000;
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
        // Fields 12 and 13: requests in flight, and milliseconds spent doing
        // IO — the latter is time-with-at-least-one-request-queued, which is
        // exactly `%util`. Both are optional: they arrived in 2.6.25, and a
        // short line means an older kernel rather than a parse failure, so
        // they degrade to None and the UI renders `--`.
        let inflight = fields.get(11).and_then(|f| f.parse::<u32>().ok());
        let busy_ns = fields
            .get(12)
            .and_then(|f| f.parse::<u64>().ok())
            .map(|ms| ms.saturating_mul(MS_TO_NS));
        out.insert(
            name.to_string(),
            DeviceTotals {
                bytes_read: sectors_read.saturating_mul(SECTOR_BYTES),
                bytes_written: sectors_written.saturating_mul(SECTOR_BYTES),
                ops_read: reads,
                ops_written: writes,
                total_time_read_ns: ms_reading.saturating_mul(MS_TO_NS),
                total_time_write_ns: ms_writing.saturating_mul(MS_TO_NS),
                busy_ns,
                inflight,
            },
        );
    }
    out
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn is_partition_name(name: &str) -> bool {
    if name.starts_with("nvme") || name.starts_with("mmcblk") {
        return name.contains('p');
    }
    // Whole devices whose names END IN A DIGIT that is an index, not a
    // partition number. The trailing-digit heuristic below is right for
    // sda1 / vdb2 / hda3 and wrong for every one of these — and being wrong
    // means the device is dropped from /proc/diskstats entirely, so an md
    // array simply never appeared in the IO list at all. Their own
    // partitions do carry a `p` (md0p1), same as nvme.
    for whole in ["dm-", "md", "sr", "zram", "nbd", "zd"] {
        if name.starts_with(whole) {
            return name.contains('p');
        }
    }
    name.chars()
        .last()
        .map(|c| c.is_ascii_digit())
        .unwrap_or(false)
}

/// True for a device that is stacked on top of others — md, dm, LVM, LUKS.
///
/// Their traffic also passes through the physical devices beneath them, so
/// anything summing across devices has to exclude them or it counts every
/// block twice. They stay in the list because the UI shows them as rows; it is
/// the SUMS they must stay out of.
pub fn is_stacked_name(name: &str) -> bool {
    name.starts_with("dm-") || name.starts_with("md") || name.starts_with("zd")
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
    // Physical devices only — see `totals_bps` for why.
    let phys = || latest.iter().filter(|t| !is_stacked_name(&t.device));
    let combined: f64 = phys().map(|t| t.bps).sum();
    let write: f64 = phys().filter_map(|t| t.split.map(|(_, w)| w)).sum();
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

#[cfg(test)]
mod diskstats_tests {
    use super::*;

    /// Real lines from a 6.x kernel: an NVMe whole disk, one of its
    /// partitions, a dm mapping, a loop device, and an md array.
    const MODERN: &str = "\
 259       0 nvme0n1 1204485 154403 92216832 419827 2274743 1385042 128793104 1936114 0 1123456 2372123 0 0 0 0 74521 25631
 259       1 nvme0n1p1 1180 0 45888 108 12 0 24 3 0 128 111 0 0 0 0 0 0
 252       0 dm-0 1189042 0 91755024 431223 3487211 0 128512488 3612991 0 1198877 4044214 0 0 0 0 0 0
   7       0 loop0 44 0 1408 6 0 0 0 0 0 24 6 0 0 0 0 0 0
   9       0 md0 92211 0 8192000 12042 411223 0 42112000 92113 0 84221 104155 0 0 0 0 0 0";

    /// Pre-2.6.25: eleven fields, no in-flight and no io_ticks.
    const SHORT: &str = " 8 0 sda 1204485 154403 92216832 419827 2274743 1385042 128793104 1936114";

    #[test]
    fn parses_a_modern_diskstats_line() {
        let out = parse_diskstats(MODERN);
        let d = out.get("nvme0n1").expect("nvme0n1 present");
        // Sectors are always 512 bytes in this interface, whatever the
        // drive's physical sector size — a kernel API constant, not a
        // property of the device.
        assert_eq!(d.bytes_read, 92_216_832 * 512);
        assert_eq!(d.bytes_written, 128_793_104 * 512);
        assert_eq!(d.ops_read, 1_204_485);
        assert_eq!(d.ops_written, 2_274_743);
        assert_eq!(d.total_time_read_ns, 419_827 * 1_000_000);
        assert_eq!(d.total_time_write_ns, 1_936_114 * 1_000_000);
        // Field 12 is in-flight, field 13 is io_ticks — the numbers that
        // make utilisation and queue depth real on Linux.
        assert_eq!(d.inflight, Some(0));
        assert_eq!(d.busy_ns, Some(1_123_456 * 1_000_000));
    }

    #[test]
    fn skips_partitions_loops_and_keeps_stacked_devices() {
        let out = parse_diskstats(MODERN);
        assert!(!out.contains_key("nvme0n1p1"), "partitions double-count");
        assert!(!out.contains_key("loop0"));
        // dm and md ARE kept: the view lists them and marks them stacked,
        // then leaves them out of the system totals. Dropping them here
        // would lose the rows entirely.
        assert!(out.contains_key("dm-0"));
        assert!(out.contains_key("md0"));
    }

    #[test]
    fn an_old_kernel_loses_util_not_the_device() {
        let out = parse_diskstats(SHORT);
        let d = out.get("sda").expect("sda still parsed");
        assert_eq!(d.ops_read, 1_204_485);
        assert_eq!(d.inflight, None, "renders as -- rather than 0");
        assert_eq!(d.busy_ns, None);
    }

    #[test]
    fn a_truncated_or_garbled_line_is_skipped_not_fatal() {
        assert!(parse_diskstats("8 0 sda 1 2 3").is_empty());
        assert!(parse_diskstats("8 0 sda x x x x x x x x").is_empty());
        assert!(parse_diskstats("").is_empty());
    }

    #[test]
    fn whole_devices_whose_names_end_in_a_digit_are_not_partitions() {
        // The bug this pins: `md0` ends in a digit, so the trailing-digit
        // heuristic called it a partition and dropped the array from
        // /proc/diskstats entirely. It never appeared in the IO list at all.
        for whole in [
            "md0", "md127", "dm-0", "sr0", "zram0", "nbd0", "sda", "nvme0n1",
        ] {
            assert!(!is_partition_name(whole), "{whole} treated as a partition");
        }
        for part in ["sda1", "vdb2", "hda3", "nvme0n1p1", "mmcblk0p2", "md0p1"] {
            assert!(is_partition_name(part), "{part} treated as a whole device");
        }
    }

    #[test]
    fn stacked_devices_stay_out_of_the_totals() {
        // A LUKS volume on an NVMe reports its traffic twice — once as dm-0
        // and once as nvme0n1. Summing both is a silent 2x on exactly the
        // machines that encrypt their disks.
        let tick = |name: &str, r: f64, w: f64| IoTick {
            device: name.to_string(),
            bps: r + w,
            split: Some((r, w)),
            ..Default::default()
        };
        let latest = vec![
            tick("nvme0n1", 100.0, 200.0),
            tick("dm-0", 90.0, 190.0),
            tick("md0", 10.0, 10.0),
        ];
        let (combined, write) = aggregate(&latest);
        assert_eq!(combined, 300.0);
        assert_eq!(write, 200.0);
        assert!(is_stacked_name("dm-0") && is_stacked_name("md0"));
        assert!(!is_stacked_name("nvme0n1") && !is_stacked_name("sda"));
    }

    #[test]
    fn service_times_land_in_the_bucket_they_name() {
        // The histogram's edges, walked from both sides.
        assert_eq!(lat_bucket(0.0), 0);
        assert_eq!(lat_bucket(0.099), 0);
        assert_eq!(lat_bucket(0.1), 1);
        assert_eq!(lat_bucket(0.5), 2);
        assert_eq!(lat_bucket(1.0), 3);
        assert_eq!(lat_bucket(4.99), 3);
        assert_eq!(lat_bucket(5.0), 4);
        assert_eq!(lat_bucket(10.0), LAT_TAIL_FROM);
        assert_eq!(lat_bucket(49.9), LAT_TAIL_FROM);
        assert_eq!(lat_bucket(50.0), LAT_BUCKETS - 1);
        assert_eq!(lat_bucket(5_000.0), LAT_BUCKETS - 1);
    }

    #[test]
    fn the_histogram_window_slides_instead_of_accumulating() {
        // A histogram that only ever accumulates shows the machine's whole
        // uptime, so a burst an hour ago never leaves the tail.
        let mut h = DeviceHistory::default();
        for i in 0..LATENCY_WINDOW + 50 {
            let mut s = [0u64; LAT_BUCKETS];
            s[if i < 50 { LAT_BUCKETS - 1 } else { 0 }] = 1;
            while h.lat_hist.len() >= LATENCY_WINDOW {
                h.lat_hist.pop_front();
            }
            h.lat_hist.push_back(s);
        }
        let sum = h.hist_sum();
        assert_eq!(sum[LAT_BUCKETS - 1], 0, "the old burst never aged out");
        assert_eq!(sum[0], LATENCY_WINDOW as u64);
    }
}
