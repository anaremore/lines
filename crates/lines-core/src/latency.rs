//! Latency over the last 60 seconds (roadmap 6b, the latency HUD): fixed, allocation-free
//! histograms, one per second, so recording a sample and reading p50/p99 never allocate.
//!
//! **Buckets** split each octave of microseconds in four equal parts: bucket `4e + f` holds
//! `[2^e·(4+f)/4, 2^e·(5+f)/4)` µs (at most 25% wide), from 1 µs to about 1.2 hours. A quantile is reported as its bucket's
//! upper edge, so a shown p99 is never below the true value by more than one bucket.
//!
//! **Time** is whatever clock the caller passes in milliseconds (the wall clock live, the
//! recorded clock in a replay, the core's `now`), so windows are deterministic in tests and
//! replays. A sample older than 60 s by that clock drops out.
//!
//! **Clock offset** ([`ClockOffset`]): the smallest `receive − exchange` time of the last
//! 60 s. Transit is never negative, so it bounds the local clock's lead over the exchange's:
//! `offset ≤ min(receive − exchange)`. Negative means the local clock is behind by at least
//! that much; a large positive value is a clock running ahead or a slow link.
//!
//! **Off by default** ([`set_enabled`]): while the HUD is hidden nothing records, so it costs
//! one relaxed atomic load per feed frame or paint.

use std::sync::atomic::{AtomicBool, Ordering};

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Turn latency recording on or off (the HUD's visibility).
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

/// Is latency being recorded (the HUD is shown)?
#[inline]
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// p50 and p99 of a window, with its sample count.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub p50_us: Option<u64>,
    pub p99_us: Option<u64>,
    pub n: u64,
}

/// Seconds kept.
pub const WINDOW_S: usize = 60;
/// Quarter-octave buckets: 4 per doubling, 32 doublings.
pub const BUCKETS: usize = 128;

/// The bucket holding `us` microseconds.
pub fn bucket_of(us: u64) -> usize {
    if us <= 1 {
        return 0;
    }
    // The octave from the bit length, the quarter from the two bits below the top bit.
    let lz = 63 - us.leading_zeros() as usize; // floor(log2)
    let frac = if lz >= 2 {
        ((us >> (lz - 2)) & 0b11) as usize
    } else {
        ((us << (2 - lz)) & 0b11) as usize
    };
    (lz * 4 + frac).min(BUCKETS - 1)
}

/// The upper edge of bucket `k`, in microseconds (what a quantile in it reports).
pub fn bucket_upper(k: usize) -> u64 {
    let (e, f) = ((k / 4).min(61) as u32, (k % 4) as u64);
    if e < 2 {
        // The first octaves hold whole microseconds only.
        return (1u64 << e) * (5 + f) / 4 + 1;
    }
    (1u64 << e) / 4 * (5 + f)
}

/// Samples of one second.
#[derive(Clone, Copy)]
struct Second {
    /// Which second (`ms / 1000`), or `i64::MIN` when empty.
    at: i64,
    counts: [u32; BUCKETS],
    n: u32,
}

impl Default for Second {
    fn default() -> Self {
        Second {
            at: i64::MIN,
            counts: [0; BUCKETS],
            n: 0,
        }
    }
}

/// A rolling 60-second latency histogram. Allocates once (in `new`), never after.
#[derive(Clone)]
pub struct LatencyWindow {
    seconds: Box<[Second; WINDOW_S]>,
}

impl Default for LatencyWindow {
    fn default() -> Self {
        LatencyWindow::new()
    }
}

impl std::fmt::Debug for LatencyWindow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LatencyWindow").finish_non_exhaustive()
    }
}

impl LatencyWindow {
    pub fn new() -> LatencyWindow {
        LatencyWindow {
            seconds: Box::new([Second::default(); WINDOW_S]),
        }
    }

    /// Record `us` microseconds at `now_ms`.
    pub fn record(&mut self, now_ms: i64, us: u64) {
        let s = now_ms.div_euclid(1000);
        let slot = &mut self.seconds[s.rem_euclid(WINDOW_S as i64) as usize];
        if slot.at != s {
            if slot.at > s {
                return; // older than what the slot holds: outside the window
            }
            *slot = Second {
                at: s,
                ..Second::default()
            };
        }
        slot.counts[bucket_of(us)] += 1;
        slot.n += 1;
    }

    fn live(&self, now_ms: i64) -> impl Iterator<Item = &Second> {
        let now = now_ms.div_euclid(1000);
        self.seconds
            .iter()
            .filter(move |s| s.n > 0 && s.at <= now && s.at > now - WINDOW_S as i64)
    }

    /// Samples in the last 60 s.
    pub fn count(&self, now_ms: i64) -> u64 {
        self.live(now_ms).map(|s| u64::from(s.n)).sum()
    }

    fn merged(&self, now_ms: i64) -> ([u64; BUCKETS], u64) {
        let mut merged = [0u64; BUCKETS];
        let mut n = 0u64;
        for s in self.live(now_ms) {
            for (m, c) in merged.iter_mut().zip(s.counts.iter()) {
                *m += u64::from(*c);
            }
            n += u64::from(s.n);
        }
        (merged, n)
    }

    /// The `q` quantile (0..=1) of the last 60 s, as its bucket's upper edge (µs).
    pub fn quantile(&self, now_ms: i64, q: f64) -> Option<u64> {
        let (merged, n) = self.merged(now_ms);
        quantile_of(&merged, n, q)
    }

    /// p50 and p99 of the last 60 s in one pass.
    pub fn summary(&self, now_ms: i64) -> Summary {
        let (merged, n) = self.merged(now_ms);
        Summary {
            p50_us: quantile_of(&merged, n, 0.5),
            p99_us: quantile_of(&merged, n, 0.99),
            n,
        }
    }

    /// One value per second for a sparkline, oldest first: that second's `q` quantile (µs),
    /// 0 for a second without samples.
    pub fn sparkline(&self, now_ms: i64, q: f64, out: &mut [u64; WINDOW_S]) {
        let now = now_ms.div_euclid(1000);
        for (i, o) in out.iter_mut().enumerate() {
            let at = now - (WINDOW_S as i64 - 1) + i as i64;
            let s = &self.seconds[at.rem_euclid(WINDOW_S as i64) as usize];
            *o = if s.at == at && s.n > 0 {
                let mut c = [0u64; BUCKETS];
                for (d, v) in c.iter_mut().zip(s.counts.iter()) {
                    *d = u64::from(*v);
                }
                quantile_of(&c, u64::from(s.n), q).unwrap_or(0)
            } else {
                0
            };
        }
    }
}

fn quantile_of(counts: &[u64; BUCKETS], n: u64, q: f64) -> Option<u64> {
    if n == 0 {
        return None;
    }
    // The smallest bucket whose cumulative count reaches ceil(q · n) (at least 1).
    let rank = ((q.clamp(0.0, 1.0) * n as f64).ceil() as u64).max(1);
    let mut cum = 0u64;
    for (k, c) in counts.iter().enumerate() {
        cum += c;
        if cum >= rank {
            return Some(bucket_upper(k));
        }
    }
    Some(bucket_upper(BUCKETS - 1))
}

/// The smallest `receive − exchange` time (ms) per second over the last 60 s: a bound on
/// the local clock's lead over the exchange's (see the module doc).
#[derive(Clone, Debug)]
pub struct ClockOffset {
    mins: [(i64, i64); WINDOW_S],
}

impl Default for ClockOffset {
    fn default() -> Self {
        ClockOffset {
            mins: [(i64::MIN, i64::MAX); WINDOW_S],
        }
    }
}

impl ClockOffset {
    /// A message received at `recv_ms` (local) stamped `exchange_ms` by the exchange.
    pub fn record(&mut self, recv_ms: i64, exchange_ms: i64) {
        let s = recv_ms.div_euclid(1000);
        let slot = &mut self.mins[s.rem_euclid(WINDOW_S as i64) as usize];
        let d = recv_ms.saturating_sub(exchange_ms);
        if slot.0 != s {
            if slot.0 > s {
                return;
            }
            *slot = (s, d);
        } else {
            slot.1 = slot.1.min(d);
        }
    }

    /// `min(receive − exchange)` over the last 60 s (ms), if any message was seen.
    pub fn bound_ms(&self, now_ms: i64) -> Option<i64> {
        let now = now_ms.div_euclid(1000);
        self.mins
            .iter()
            .filter(|(s, _)| *s <= now && *s > now - WINDOW_S as i64)
            .map(|(_, d)| *d)
            .min()
    }
}

/// Beyond this (either way) the HUD warns about the clock (or the link).
pub const CLOCK_WARN_MS: i64 = 250;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_are_quarter_octaves_and_cover_their_values() {
        for us in [
            1u64, 2, 3, 7, 100, 999, 1_000, 25_000, 1_000_000, 60_000_000,
        ] {
            let k = bucket_of(us);
            assert!(
                bucket_upper(k) >= us,
                "{us} in bucket {k} ≤ {}",
                bucket_upper(k)
            );
            assert!(
                k == 0 || bucket_upper(k - 1) <= us + 1,
                "{us}: bucket {k} too high"
            );
        }
        // At most 25% per bucket.
        for k in 12..100 {
            let w = bucket_upper(k) as f64 / bucket_upper(k - 1) as f64;
            assert!(w > 1.0 && w <= 1.25 + 1e-9, "{k}: {w}");
        }
        assert_eq!(bucket_of(u64::MAX), BUCKETS - 1);
    }

    #[test]
    fn quantiles_of_a_known_distribution() {
        let mut w = LatencyWindow::new();
        // 990 samples at 100 µs, 10 at 10 ms.
        for i in 0..1000 {
            w.record(5_000 + i, if i < 990 { 100 } else { 10_000 });
        }
        assert_eq!(w.count(6_000), 1000);
        let p50 = w.quantile(6_000, 0.5).unwrap();
        let p99 = w.quantile(6_000, 0.99).unwrap();
        let p999 = w.quantile(6_000, 0.999).unwrap();
        assert!((100..=120).contains(&p50), "{p50}");
        assert!((100..=120).contains(&p99), "990/1000 at 100 µs: {p99}");
        assert!((10_000..=12_000).contains(&p999), "{p999}");
        assert_eq!(LatencyWindow::new().quantile(0, 0.5), None);
        let s = w.summary(6_000);
        assert_eq!((s.p50_us, s.p99_us, s.n), (Some(p50), Some(p99), 1000));
        assert_eq!(LatencyWindow::new().summary(0), Summary::default());
    }

    #[test]
    fn samples_older_than_60_s_drop_out() {
        let mut w = LatencyWindow::new();
        w.record(0, 50_000);
        w.record(30_000, 100);
        assert_eq!(w.count(59_999), 2);
        assert_eq!(w.count(60_000), 1, "the first second left the window");
        assert!(w.quantile(60_000, 1.0).unwrap() < 200);
        // A slot reused 60 s later starts over.
        w.record(60_500, 7);
        assert_eq!(w.count(60_500), 2);
        // A late sample for a second already replaced is ignored, never mixed in.
        w.record(500, 9_999_999);
        assert!(w.quantile(60_500, 1.0).unwrap() < 200);
    }

    #[test]
    fn sparkline_has_one_value_per_second_oldest_first() {
        let mut w = LatencyWindow::new();
        w.record(100_000, 1_000);
        w.record(158_000, 5_000);
        let mut out = [0u64; WINDOW_S];
        w.sparkline(159_999, 0.5, &mut out);
        assert!(out[WINDOW_S - 1] == 0, "no samples this second");
        assert!((5_000..6_000).contains(&out[WINDOW_S - 2]));
        assert!((1_000..1_200).contains(&out[0]), "{:?}", &out[..3]);
        assert_eq!(out.iter().filter(|v| **v > 0).count(), 2);
    }

    #[test]
    fn clock_offset_is_the_smallest_receive_minus_exchange() {
        let mut c = ClockOffset::default();
        assert_eq!(c.bound_ms(1_000), None);
        // Local clock 300 ms ahead; transit 20-80 ms.
        for (i, transit) in [80, 20, 55].into_iter().enumerate() {
            let exch = 10_000 + i as i64 * 400;
            c.record(exch + 300 + transit, exch);
        }
        assert_eq!(c.bound_ms(11_000), Some(320));
        // Behind: negative.
        let mut b = ClockOffset::default();
        b.record(10_000, 10_150);
        assert_eq!(b.bound_ms(10_000), Some(-150));
        // Old seconds leave the window.
        assert_eq!(c.bound_ms(80_000), None);
    }

    #[test]
    fn recording_and_reading_do_not_allocate() {
        // A window allocates once, in `new`; record/quantile/sparkline use fixed arrays.
        // (The counting allocator check for this runs in `tests/no_alloc.rs`.)
        let mut w = LatencyWindow::new();
        let mut out = [0u64; WINDOW_S];
        for i in 0..10_000 {
            w.record(i * 7, (i as u64 * 37) % 50_000);
        }
        w.sparkline(70_000, 0.99, &mut out);
        assert!(w.quantile(70_000, 0.99).is_some());
    }
}
