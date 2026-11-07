//! The latency HUD (roadmap 6b): how fresh the data is and how fast orders leave, in the chart
//! header's performance-stats slot. Off by default (a workspace preference); while it is
//! hidden nothing is recorded (`lines_core::latency::enabled`).
//!
//! Each measure shows p50/p99 over the last 60 s and a sparkline of its p99 (one cell per
//! 6 s, oldest left; every measure on one log scale, 100 µs to 1 s; `░` = no samples):
//! - **feed**: exchange time → receive, per feed (trades carry the exchange's time). In a
//!   replay this is the recording's own receive time, so it is what was measured live. A
//!   source that stamps trades on receipt reads `n/a`, never a flattering `<1 ms`.
//! - **paint**: receive → paint, per chart (the age of the newest chart update when this
//!   chart first paints it).
//! - **order**: input → order sent (the click to `Broker::request`, decision 0012).
//! - **ack (sim)**: the broker's ack round trip; the app's paper broker simulates it.
//! - **clock**: the local clock's offset against the feed's exchange time, as the bound
//!   `min(receive − exchange)` (`lines_core::latency::ClockOffset`); live wall-clock feeds
//!   only, with a warning beyond [`CLOCK_WARN_MS`].
//!
//! Values are copied out of their windows ([`Snapshot`]) and the locks dropped before any
//! text is built. The text is built at most once a second at render time; paint only records
//! (no allocation: [`PaintLatency::on_paint`]).

use std::fmt::Write;

use lines_core::latency::{CLOCK_WARN_MS, LatencyWindow, Summary, WINDOW_S, bucket_upper};

/// Sparkline cells: one per 6 s of the minute.
pub const SPARK: usize = 10;
/// Bars for cells with samples, low to high (the lowest still a visible bar).
const BARS: [char; 7] = ['▂', '▃', '▄', '▅', '▆', '▇', '█'];
/// A sparkline cell without samples: a texture, never a bar.
const EMPTY: char = '░';
/// The sparkline's shared log scale (µs): the lowest bar at or below, the highest at or above.
const SCALE_LO_US: f64 = 100.0;
const SCALE_HI_US: f64 = 1_000_000.0;
/// Fewer samples than this show their count (`(n=3)`): a p99 of three samples is not a p99.
const FEW: u64 = 5;

/// A measure's values, copied out of its window (so no lock is held while formatting).
#[derive(Clone, Copy, Debug)]
pub struct Snapshot {
    pub summary: Summary,
    /// p99 per second, oldest first (0 = no samples).
    pub per_sec: [u64; WINDOW_S],
}

impl Snapshot {
    pub fn of(w: &LatencyWindow, now_ms: i64) -> Snapshot {
        let mut per_sec = [0u64; WINDOW_S];
        w.sparkline(now_ms, 0.99, &mut per_sec);
        Snapshot {
            summary: w.summary(now_ms),
            per_sec,
        }
    }
}

/// One measure in the HUD.
pub struct Measure {
    pub label: &'static str,
    /// `None`: no data source here (shown as `—`).
    pub data: Option<Snapshot>,
    /// Timed in whole milliseconds (feed, ack): below 1 ms reads `<1`, never a false µs.
    pub ms_resolution: bool,
    /// The source has no separate exchange time (feed: all samples 0 and a zero clock-offset
    /// bound), so all-zero samples read `n/a`.
    pub zero_is_unknown: bool,
}

/// A latency in a unit that suits the larger value (`p99`), both values in that unit.
fn write_pair(out: &mut String, p50: u64, p99: u64, ms_resolution: bool) {
    if ms_resolution && p99 < 1_000 {
        out.push_str("<1/<1 ms");
        return;
    }
    let (div, decimals, unit) = match p99 {
        0..1_000 => (1.0, 0, "µs"),
        1_000..10_000 => (1_000.0, 1, "ms"),
        10_000..10_000_000 => (1_000.0, 0, "ms"),
        _ => (1_000_000.0, 1, "s"),
    };
    if ms_resolution && p50 < 1_000 {
        out.push_str("<1");
    } else {
        let _ = write!(out, "{:.*}", decimals, p50 as f64 / div);
    }
    let _ = write!(out, "/{:.*} {unit}", decimals, p99 as f64 / div);
}

/// The p99 alone, in its unit (compact form).
fn write_one(out: &mut String, p99: u64, ms_resolution: bool) {
    if ms_resolution && p99 < 1_000 {
        out.push_str("<1 ms");
        return;
    }
    let (div, decimals, unit) = match p99 {
        0..1_000 => (1.0, 0, "µs"),
        1_000..10_000 => (1_000.0, 1, "ms"),
        10_000..10_000_000 => (1_000.0, 0, "ms"),
        _ => (1_000_000.0, 1, "s"),
    };
    let _ = write!(out, "{:.*} {unit}", decimals, p99 as f64 / div);
}

/// `label p50/p99 unit` and its sparkline (`compact`: `label p99 unit` only), `label —`
/// without samples, `label n/a` when the source can't be measured.
pub fn write_measure(out: &mut String, m: &Measure, compact: bool) {
    let _ = write!(out, "{} ", m.label);
    let Some(d) = &m.data else {
        out.push('—');
        return;
    };
    let (Some(p50), Some(p99)) = (d.summary.p50_us, d.summary.p99_us) else {
        out.push('—');
        return;
    };
    if m.zero_is_unknown && p99 <= bucket_upper(0) {
        out.push_str("n/a (stamped on receipt)");
        return;
    }
    if compact {
        write_one(out, p99, m.ms_resolution);
    } else {
        write_pair(out, p50, p99, m.ms_resolution);
        out.push(' ');
        write_sparkline(out, &d.per_sec);
    }
    if d.summary.n < FEW {
        let _ = write!(out, " (n={})", d.summary.n);
    }
}

/// [`SPARK`] cells from 60 per-second values (0 = no samples), each the largest of its
/// seconds, on the shared log scale.
pub fn write_sparkline(out: &mut String, per_sec: &[u64; WINDOW_S]) {
    let per = WINDOW_S / SPARK;
    let (lo, hi) = (SCALE_LO_US.log10(), SCALE_HI_US.log10());
    for cell in per_sec.chunks(per) {
        let v = cell.iter().copied().max().unwrap_or(0);
        if v == 0 {
            out.push(EMPTY);
        } else {
            let x = ((v as f64).max(1.0).log10() - lo) / (hi - lo);
            let k = (x.clamp(0.0, 1.0) * (BARS.len() - 1) as f64).round() as usize;
            out.push(BARS[k.min(BARS.len() - 1)]);
        }
    }
}

/// The clock status from the offset bound (ms): its text, and whether it warns.
pub fn write_clock(out: &mut String, bound_ms: i64) -> bool {
    if bound_ms > CLOCK_WARN_MS {
        let _ = write!(out, "⚠ clock ahead or slow link: +{bound_ms} ms");
        true
    } else if bound_ms < -CLOCK_WARN_MS {
        let _ = write!(out, "⚠ clock behind by {} ms or more", -bound_ms);
        true
    } else if bound_ms >= 0 {
        let _ = write!(out, "clock ≤ +{bound_ms} ms");
        false
    } else {
        let _ = write!(out, "clock behind ≥ {} ms", -bound_ms);
        false
    }
}

/// Receive → paint for one pane. The window is allocated at render time when the HUD is
/// shown (and dropped when it is hidden); paint only records.
#[derive(Default)]
pub struct PaintLatency {
    window: Option<LatencyWindow>,
    seen: u64,
}

impl PaintLatency {
    /// Render side: `on` = the HUD is shown and the clock isn't frozen; `rev` = the store's
    /// revision now (updates painted before the HUD was shown don't count).
    pub fn prepare(&mut self, on: bool, rev: u64) {
        match (on, self.window.is_some()) {
            (true, false) => {
                self.window = Some(LatencyWindow::new());
                self.seen = rev;
            }
            (false, true) => self.window = None,
            _ => {}
        }
    }

    /// Paint side, once per painted frame (call only when the frame really draws the store's
    /// data): record the age of the data being painted the first time a revision is
    /// painted. Never allocates.
    #[inline]
    pub fn on_paint(&mut self, rev: u64, age_us: Option<u64>, now_ms: i64) {
        let Some(w) = self.window.as_mut() else {
            return;
        };
        if rev == self.seen {
            return;
        }
        self.seen = rev;
        if let Some(a) = age_us {
            w.record(now_ms, a);
        }
    }

    /// While the pane is hidden (it doesn't paint): updates that arrive count as seen, so
    /// the wait for the tab to be shown again is never recorded as paint latency.
    #[inline]
    pub fn hidden(&mut self, rev: u64) {
        if self.window.is_some() {
            self.seen = rev;
        }
    }

    pub fn window(&self) -> Option<&LatencyWindow> {
        self.window.as_ref()
    }
}

/// The HUD's text: `latency p50/p99 · feed … · paint … · …` (`compact`: `latency p99 · …`,
/// no sparklines, for narrow charts), and the clock status separately
/// (`Some((text, warn))`) when the feed's clock is known.
pub fn write_hud(
    out: &mut String,
    measures: &[Measure],
    compact: bool,
    clock_bound_ms: Option<i64>,
) -> Option<(String, bool)> {
    out.push_str(if compact {
        "latency p99"
    } else {
        "latency p50/p99"
    });
    for m in measures {
        out.push_str("  ·  ");
        write_measure(out, m, compact);
    }
    clock_bound_ms.map(|b| {
        let mut s = String::new();
        let warn = write_clock(&mut s, b);
        (s, warn)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(p50: u64, p99: u64, n: u64) -> Snapshot {
        Snapshot {
            summary: Summary {
                p50_us: Some(p50),
                p99_us: Some(p99),
                n,
            },
            per_sec: [0; WINDOW_S],
        }
    }

    fn measure(label: &'static str, data: Option<Snapshot>, ms: bool) -> Measure {
        Measure {
            label,
            data,
            ms_resolution: ms,
            zero_is_unknown: false,
        }
    }

    fn text(m: &Measure, compact: bool) -> String {
        let mut o = String::new();
        write_measure(&mut o, m, compact);
        o
    }

    #[test]
    fn units_follow_the_p99() {
        let t = |p50, p99| text(&measure("feed", Some(snap(p50, p99, 100)), false), false);
        assert_eq!(t(120, 300), "feed 120/300 µs ░░░░░░░░░░");
        assert_eq!(t(800, 2_500), "feed 0.8/2.5 ms ░░░░░░░░░░");
        assert_eq!(t(12_000, 40_000), "feed 12/40 ms ░░░░░░░░░░");
        assert_eq!(t(900_000, 12_000_000), "feed 0.9/12.0 s ░░░░░░░░░░");
        assert_eq!(text(&measure("feed", None, false), false), "feed —");
    }

    #[test]
    fn whole_millisecond_measures_and_unmeasurable_feeds() {
        let ms = |p50, p99| text(&measure("ack", Some(snap(p50, p99, 100)), true), false);
        assert!(ms(2, 2).starts_with("ack <1/<1 ms "), "{}", ms(2, 2));
        assert!(
            ms(2, 25_000).starts_with("ack <1/25 ms "),
            "{}",
            ms(2, 25_000)
        );
        // A source stamping trades on receipt: every sample exactly 0.
        let feed = Measure {
            zero_is_unknown: true,
            ..measure("feed", Some(snap(2, 2, 100)), true)
        };
        assert_eq!(text(&feed, false), "feed n/a (stamped on receipt)");
    }

    #[test]
    fn few_samples_show_their_count_and_compact_keeps_the_p99() {
        let m = measure("ack", Some(snap(29_000, 29_000, 1)), true);
        assert_eq!(text(&m, false), "ack 29/29 ms ░░░░░░░░░░ (n=1)");
        assert_eq!(text(&m, true), "ack 29 ms (n=1)");
        let m = measure("feed", Some(snap(25_000, 328_000, 600)), true);
        assert_eq!(text(&m, true), "feed 328 ms");
        let m = measure("paint", Some(snap(900, 3_100, 600)), false);
        assert_eq!(text(&m, true), "paint 3.1 ms");
    }

    #[test]
    fn sparkline_is_on_one_log_scale_with_empty_cells_distinct() {
        let mut v = [0u64; WINDOW_S];
        v[0] = 100; // 100 µs: the lowest bar (still a bar)
        v[30] = 10_000; // 10 ms: the middle
        v[59] = 1_000_000; // 1 s: the top
        let mut o = String::new();
        write_sparkline(&mut o, &v);
        assert_eq!(o.chars().count(), SPARK);
        assert_eq!(o, "▂░░░░▅░░░█");
        // The same value draws the same bar whatever else is in the minute.
        let mut w = [0u64; WINDOW_S];
        w[30] = 10_000;
        let mut o2 = String::new();
        write_sparkline(&mut o2, &w);
        assert_eq!(o2.chars().nth(5), o.chars().nth(5));
    }

    #[test]
    fn the_clock_warns_beyond_250_ms_either_way() {
        let c = |b| {
            let mut o = String::new();
            let w = write_clock(&mut o, b);
            (o, w)
        };
        assert_eq!(c(12), ("clock ≤ +12 ms".into(), false));
        assert_eq!(c(250), ("clock ≤ +250 ms".into(), false));
        assert_eq!(c(-40), ("clock behind ≥ 40 ms".into(), false));
        assert_eq!(c(320), ("⚠ clock ahead or slow link: +320 ms".into(), true));
        assert_eq!(c(-400), ("⚠ clock behind by 400 ms or more".into(), true));
    }

    #[test]
    fn paint_latency_is_off_until_prepared_and_counts_each_revision_once() {
        let mut p = PaintLatency::default();
        p.on_paint(5, Some(1_000), 10_000);
        assert!(
            p.window().is_none(),
            "hidden: nothing allocated or recorded"
        );
        p.prepare(true, 5);
        p.on_paint(5, Some(9_999_999), 10_000);
        assert_eq!(
            p.window().unwrap().count(10_000),
            0,
            "already painted before"
        );
        p.on_paint(6, Some(2_000), 10_016);
        p.on_paint(6, Some(2_000), 10_032); // the same update repainted
        p.on_paint(7, None, 10_048);
        assert_eq!(p.window().unwrap().count(10_100), 1);
        // Hidden at 10 s; an update arrives at 30 s; shown again at 35 s. The 5 s it waited
        // for the tab is not paint latency.
        p.hidden(8);
        p.on_paint(8, Some(5_000_000), 35_000);
        assert_eq!(p.window().unwrap().count(35_000), 1, "not recorded");
        p.on_paint(9, Some(3_000), 35_016);
        assert_eq!(p.window().unwrap().count(35_016), 2);
        p.prepare(false, 7);
        assert!(p.window().is_none(), "hidden again: freed");
    }

    #[test]
    fn hud_text_lists_every_measure() {
        let mut feed = LatencyWindow::new();
        for i in 0..10 {
            feed.record(10_000 + i, 12_000);
        }
        let ms = vec![
            Measure {
                zero_is_unknown: true,
                ..measure("feed", Some(Snapshot::of(&feed, 10_500)), true)
            },
            measure("paint", None, false),
            measure("ack (sim)", None, true),
        ];
        let mut o = String::new();
        let clock = write_hud(&mut o, &ms, false, Some(8));
        assert!(o.starts_with("latency p50/p99  ·  feed 12/12 ms "), "{o}");
        assert!(o.ends_with("paint —  ·  ack (sim) —"), "{o}");
        assert_eq!(clock, Some(("clock ≤ +8 ms".into(), false)));
        let mut o = String::new();
        write_hud(&mut o, &ms, true, None);
        assert_eq!(o, "latency p99  ·  feed 12 ms  ·  paint —  ·  ack (sim) —");
        let mut o = String::new();
        assert_eq!(write_hud(&mut o, &[], false, None), None);
        assert_eq!(o, "latency p50/p99");
    }
}
