//! Moving averages and Bollinger Bands (roadmap 10a) on the chart's bars ([`crate::bars`]).
//!
//! - **SMA**: the mean of the last `length` bars' source values.
//! - **EMA**: `ema = prev + 2/(length+1) · (x − prev)`, seeded with the SMA of the first
//!   `length` bars (no value before them).
//! - **Bollinger Bands**: the SMA ± `sigma` × the population standard deviation of the same
//!   window.
//!
//! **Exactness** (as VWAP, decision 0017): bar prices are exact integers, and the window sums
//! are exact `i128` (the source in a scaled form: `HL2` as `H+L`, `HLC3` as `H+L+C`, `OHLC4`
//! as `O+H+L+C`, so no source value is rounded). An SMA is one division of exact sums, and the
//! variance is `n·Σx² − (Σx)²` in integers before its square root. The values are derived
//! analytics carried as `f64` in price units, never snapped to the tick grid or used as an
//! order price. An EMA is a recursion with a non-terminating ratio: it is `f64` throughout.
//!
//! **No resets**: averages run across session breaks (the bars themselves break there).
//!
//! **Incremental**: an update recomputes from the bar holding the first change (the forming
//! bar on a live tick), continuing the window sums and the EMA from the bars before it.

use lines_core::{Bar, UtcMillis};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::bars::BarsOut;
use crate::vwap::Point;

/// What a bar contributes to an average.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    #[default]
    Close,
    Open,
    High,
    Low,
    /// (H + L) / 2.
    Hl2,
    /// (H + L + C) / 3.
    Hlc3,
    /// (O + H + L + C) / 4.
    Ohlc4,
}

impl Source {
    pub const ALL: [Source; 7] = [
        Source::Close,
        Source::Open,
        Source::High,
        Source::Low,
        Source::Hl2,
        Source::Hlc3,
        Source::Ohlc4,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Source::Close => "close",
            Source::Open => "open",
            Source::High => "high",
            Source::Low => "low",
            Source::Hl2 => "HL2",
            Source::Hlc3 => "HLC3",
            Source::Ohlc4 => "OHLC4",
        }
    }

    /// The value × [`Source::div`], exact.
    pub fn scaled(self, b: &Bar) -> i128 {
        let (o, h, l, c) = (
            b.open.0 as i128,
            b.high.0 as i128,
            b.low.0 as i128,
            b.close.0 as i128,
        );
        match self {
            Source::Close => c,
            Source::Open => o,
            Source::High => h,
            Source::Low => l,
            Source::Hl2 => h + l,
            Source::Hlc3 => h + l + c,
            Source::Ohlc4 => o + h + l + c,
        }
    }

    pub fn div(self) -> i128 {
        match self {
            Source::Close | Source::Open | Source::High | Source::Low => 1,
            Source::Hl2 => 2,
            Source::Hlc3 => 3,
            Source::Ohlc4 => 4,
        }
    }
}

/// Which average.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum AverageKind {
    Sma,
    Ema,
    Bollinger,
}

/// A band width in tenths of a standard deviation (`20` = 2σ), `1..=50`. Saved as a number
/// of standard deviations (`2.0`).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Sigma(pub u8);

impl Sigma {
    pub const TWO: Sigma = Sigma(20);

    pub fn get(self) -> f64 {
        f64::from(self.0) / 10.0
    }

    fn clamped(self) -> Sigma {
        Sigma(self.0.clamp(1, 50))
    }
}

impl Default for Sigma {
    fn default() -> Self {
        Sigma::TWO
    }
}

impl std::fmt::Display for Sigma {
    /// `2σ`, `2.5σ`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0 % 10 == 0 {
            write!(f, "{}σ", self.0 / 10)
        } else {
            write!(f, "{}.{}σ", self.0 / 10, self.0 % 10)
        }
    }
}

impl Serialize for Sigma {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_f64(self.get())
    }
}

impl<'de> Deserialize<'de> for Sigma {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = f64::deserialize(d)?;
        if !v.is_finite() {
            return Err(serde::de::Error::custom("sigma must be a number"));
        }
        Ok(Sigma((v * 10.0).round().clamp(1.0, 50.0) as u8))
    }
}

/// The longest average (bars).
pub const MAX_LENGTH: u16 = 1000;

/// An average's identity: with the feed and the chart's bars it keys the shared node.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct AverageSpec {
    pub kind: AverageKind,
    /// Bars in the window, `1..=MAX_LENGTH`.
    pub length: u16,
    pub source: Source,
    /// Bollinger Bands only.
    pub sigma: Sigma,
}

impl AverageSpec {
    pub fn new(kind: AverageKind, length: u16, source: Source, sigma: Sigma) -> AverageSpec {
        AverageSpec {
            kind,
            length: length.clamp(1, MAX_LENGTH),
            source,
            sigma: if kind == AverageKind::Bollinger {
                sigma.clamped()
            } else {
                Sigma::TWO
            },
        }
    }

    /// `EMA 20`, `SMA 50 HL2`, `Bollinger 20 2σ` (the source when it isn't the close).
    pub fn label(&self) -> String {
        let name = match self.kind {
            AverageKind::Sma => "SMA",
            AverageKind::Ema => "EMA",
            AverageKind::Bollinger => "Bollinger",
        };
        let mut s = format!("{name} {}", self.length);
        if self.source != Source::Close {
            s.push(' ');
            s.push_str(self.source.label());
        }
        if self.kind == AverageKind::Bollinger {
            s.push_str(&format!(" {}", self.sigma));
        }
        s
    }
}

/// Exact window sums of scaled source values.
#[derive(Copy, Clone, Debug, Default)]
struct Window {
    sum: i128,
    sum_sq: i128,
    /// `sum_sq` overflowed: the variance falls back to `f64`.
    overflow: bool,
}

impl Window {
    fn add(&mut self, x: i128) {
        self.sum += x;
        match x.checked_mul(x).and_then(|q| self.sum_sq.checked_add(q)) {
            Some(s) => self.sum_sq = s,
            None => self.overflow = true,
        }
    }

    fn remove(&mut self, x: i128) {
        self.sum -= x;
        if !self.overflow {
            self.sum_sq -= x * x;
        }
    }

    /// Population standard deviation of `n` scaled values, unscaled by `div`.
    fn std_dev(&self, n: i128, div: i128, xs: impl Iterator<Item = i128>) -> f64 {
        let scale = (n * div) as f64;
        let exact = self
            .sum_sq
            .checked_mul(n)
            .zip(self.sum.checked_mul(self.sum))
            .filter(|_| !self.overflow)
            .map(|(a, b)| (a - b).max(0) as f64);
        match exact {
            Some(v) => v.sqrt() / scale,
            None => {
                let mean = self.sum as f64 / n as f64;
                let var = xs.map(|x| (x as f64 - mean).powi(2)).sum::<f64>() / n as f64;
                var.sqrt() / div as f64
            }
        }
    }
}

pub struct Average {
    spec: AverageSpec,
    /// The EMA after each bar (aligned with the input bars; `None` before the seed).
    ema: Vec<Option<f64>>,
    points: Vec<Point>,
    /// For each bar, the number of points before it (to truncate `points` exactly).
    points_before: Vec<u32>,
    /// Input bars dropped from the front so far (seen).
    seen_drops: u64,
    pub evaluations: u64,
}

impl Average {
    pub fn new(spec: AverageSpec) -> Average {
        Average {
            spec: AverageSpec::new(spec.kind, spec.length, spec.source, spec.sigma),
            ema: Vec::new(),
            points: Vec::new(),
            points_before: Vec::new(),
            seen_drops: 0,
            evaluations: 0,
        }
    }

    pub fn spec(&self) -> &AverageSpec {
        &self.spec
    }

    pub fn points(&self) -> &[Point] {
        &self.points
    }

    /// Recompute from the bar holding `from`; returns the index of the first point that may
    /// have changed (the point before the recomputed ones included: its `forming` flag).
    pub fn update(&mut self, input: BarsOut<'_>, from: UtcMillis) -> usize {
        self.evaluations += 1;
        let drops = input.trimmed;
        if drops < self.seen_drops {
            // The bars started over: so do we.
            self.seen_drops = drops;
            self.ema.clear();
            self.points.clear();
            self.points_before.clear();
        }
        if drops > self.seen_drops {
            let k = ((drops - self.seen_drops) as usize).min(self.ema.len());
            self.seen_drops = drops;
            self.ema.drain(..k);
            let gone = if k < self.points_before.len() {
                self.points_before[k] as usize
            } else {
                self.points.len()
            };
            self.points_before.drain(..k);
            self.points.drain(..gone.min(self.points.len()));
            for n in &mut self.points_before {
                *n -= gone as u32;
            }
        }
        let bars = input.bars;
        // The bar holding `from` (an input's change may fall inside a bar), so one earlier.
        let start = bars
            .partition_point(|b| b.time < from)
            .saturating_sub(1)
            .min(self.ema.len());
        let keep = if start < self.points_before.len() {
            self.points_before[start] as usize
        } else {
            self.points.len()
        };
        self.ema.truncate(start);
        self.points_before.truncate(start);
        self.points.truncate(keep);

        let n = self.spec.length as usize;
        let src = self.spec.source;
        let div = src.div();
        let alpha = 2.0 / (n as f64 + 1.0);
        // The window before `start`: the n − 1 bars that precede it.
        let mut w = Window::default();
        for b in &bars[start.saturating_sub(n - 1)..start.min(bars.len())] {
            w.add(src.scaled(b));
        }
        let last = bars.len().saturating_sub(1);
        for i in start..bars.len() {
            self.points_before.push(self.points.len() as u32);
            // Before this step the window holds the (up to) n − 1 bars before `i`.
            let x = src.scaled(&bars[i]);
            w.add(x);
            let full = i + 1 >= n;
            let sma = full.then(|| w.sum as f64 / (n as i128 * div) as f64);
            let ema = match (self.ema.last().copied().flatten(), sma) {
                (Some(prev), _) => Some(prev + alpha * (x as f64 / div as f64 - prev)),
                (None, seed) => seed,
            };
            self.ema.push(ema);
            let (value, sd) = match self.spec.kind {
                AverageKind::Sma => (sma, 0.0),
                AverageKind::Ema => (ema, 0.0),
                AverageKind::Bollinger => {
                    let sd = if full {
                        let xs = bars[i + 1 - n..=i].iter().map(|b| src.scaled(b));
                        w.std_dev(n as i128, div, xs) * self.spec.sigma.get()
                    } else {
                        0.0
                    };
                    (sma, sd)
                }
            };
            if full {
                w.remove(src.scaled(&bars[i + 1 - n]));
            }
            if let Some(value) = value {
                self.points.push(Point {
                    time: bars[i].time,
                    vwap: value,
                    sd,
                    approx: false,
                    period: 0,
                    forming: i == last,
                });
            }
        }
        if keep > 0
            && let Some(p) = self.points.get_mut(keep - 1)
        {
            p.forming = false;
            return keep - 1;
        }
        keep
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lines_core::{Price, Qty};

    fn bar(i: i64, o: i64, h: i64, l: i64, c: i64) -> Bar {
        Bar {
            time: UtcMillis(i * 60_000),
            open: Price(o),
            high: Price(h),
            low: Price(l),
            close: Price(c),
            volume: Qty(1),
        }
    }

    fn closes(cs: &[i64]) -> Vec<Bar> {
        cs.iter()
            .enumerate()
            .map(|(i, &c)| bar(i as i64, c, c + 2, c - 2, c))
            .collect()
    }

    fn all(spec: AverageSpec, bars: &[Bar]) -> Vec<Point> {
        let mut a = Average::new(spec);
        a.update(BarsOut { bars, trimmed: 0 }, UtcMillis(i64::MIN));
        a.points().to_vec()
    }

    fn spec(kind: AverageKind, length: u16) -> AverageSpec {
        AverageSpec::new(kind, length, Source::Close, Sigma::TWO)
    }

    #[test]
    fn sma_is_the_exact_window_mean() {
        let bars = closes(&[10, 11, 12, 13, 15]);
        let p = all(spec(AverageKind::Sma, 3), &bars);
        let v: Vec<f64> = p.iter().map(|p| p.vwap).collect();
        assert_eq!(v, [11.0, 12.0, 40.0 / 3.0]);
        assert_eq!(p[0].time, bars[2].time, "first value at the length-th bar");
        assert!(p[2].forming && !p[1].forming);
    }

    #[test]
    fn scaled_sources_are_exact() {
        // HL2 of (12, 7) is 9.5: kept as 19 / 2, never rounded to the grid.
        let bars = vec![bar(0, 8, 12, 7, 9), bar(1, 9, 13, 8, 10)];
        let s = AverageSpec::new(AverageKind::Sma, 2, Source::Hl2, Sigma::TWO);
        assert_eq!(all(s, &bars)[0].vwap, 10.0); // (19 + 21) / 4
        let s = AverageSpec::new(AverageKind::Sma, 1, Source::Ohlc4, Sigma::TWO);
        assert_eq!(all(s, &bars)[0].vwap, 9.0); // 36 / 4
        let s = AverageSpec::new(AverageKind::Sma, 1, Source::Hlc3, Sigma::TWO);
        assert_eq!(all(s, &bars)[0].vwap, 28.0 / 3.0);
    }

    #[test]
    fn ema_seeds_with_the_sma_and_recurses() {
        let bars = closes(&[10, 20, 30, 40]);
        let p = all(spec(AverageKind::Ema, 3), &bars);
        // Seed: SMA(10, 20, 30) = 20; alpha 0.5: 20 + 0.5·(40 − 20) = 30.
        assert_eq!(p.iter().map(|p| p.vwap).collect::<Vec<_>>(), [20.0, 30.0]);
    }

    #[test]
    fn bollinger_is_the_sma_plus_minus_sigma_population_sd() {
        let bars = closes(&[2, 4, 4, 4, 5, 5, 7, 9]);
        let s = AverageSpec::new(AverageKind::Bollinger, 8, Source::Close, Sigma(25));
        let p = all(s, &bars);
        assert_eq!(p.len(), 1);
        // Mean 5, population σ 2 (the textbook set); 2.5σ = 5.
        assert_eq!(p[0].vwap, 5.0);
        assert_eq!(p[0].sd, 5.0);
    }

    #[test]
    fn a_huge_price_falls_back_to_f64_variance() {
        let big = i64::MAX / 2;
        let bars = closes(&[big, big - 2, big + 2]);
        let s = AverageSpec::new(AverageKind::Bollinger, 3, Source::Ohlc4, Sigma(10));
        let p = all(s, &bars);
        assert!(p[0].sd.is_finite());
    }

    #[test]
    fn incremental_updates_equal_a_full_recompute() {
        let mut bars = closes(&[100, 102, 101, 105, 104, 108, 107, 110]);
        for kind in [AverageKind::Sma, AverageKind::Ema, AverageKind::Bollinger] {
            let s = spec(kind, 3);
            let mut a = Average::new(s);
            a.update(
                BarsOut {
                    bars: &bars[..5],
                    trimmed: 0,
                },
                UtcMillis(i64::MIN),
            );
            // Ticks on the forming bar, then new bars.
            for n in 5..=bars.len() {
                let first = a.update(
                    BarsOut {
                        bars: &bars[..n],
                        trimmed: 0,
                    },
                    bars[n - 1].time,
                );
                assert!(first <= a.points().len());
                assert_eq!(a.points(), &all(s, &bars[..n])[..], "{kind:?} at {n}");
            }
            // A change inside an old bar (history back-fill) recomputes from it.
            bars[2].close = Price(90);
            a.update(
                BarsOut {
                    bars: &bars,
                    trimmed: 0,
                },
                UtcMillis(bars[2].time.0 + 30_000),
            );
            assert_eq!(a.points(), &all(s, &bars)[..], "{kind:?} back-fill");
            bars[2].close = Price(101);
        }
    }

    #[test]
    fn front_drops_keep_points_aligned() {
        let bars = closes(&[1, 2, 3, 4, 5, 6, 7, 8]);
        let s = spec(AverageKind::Sma, 2);
        let mut a = Average::new(s);
        a.update(
            BarsOut {
                bars: &bars[..6],
                trimmed: 0,
            },
            UtcMillis(i64::MIN),
        );
        // Three dropped from the front, two new.
        a.update(
            BarsOut {
                bars: &bars[3..],
                trimmed: 3,
            },
            bars[6].time,
        );
        let v: Vec<f64> = a.points().iter().map(|p| p.vwap).collect();
        // The point at the first kept bar was computed before the drop (with bar 2).
        assert_eq!(v, [3.5, 4.5, 5.5, 6.5, 7.5]);
        // A smaller trim count: the bars started over.
        let fresh = closes(&[50, 60]);
        a.update(
            BarsOut {
                bars: &fresh,
                trimmed: 0,
            },
            UtcMillis(i64::MIN),
        );
        assert_eq!(a.points().len(), 1);
        assert_eq!(a.points()[0].vwap, 55.0);
    }

    #[test]
    fn sigma_saves_as_a_number_and_clamps() {
        assert_eq!(serde_json::to_string(&Sigma(25)).unwrap(), "2.5");
        assert_eq!(serde_json::from_str::<Sigma>("2").unwrap(), Sigma(20));
        assert_eq!(serde_json::from_str::<Sigma>("99").unwrap(), Sigma(50));
        assert_eq!(serde_json::from_str::<Sigma>("0").unwrap(), Sigma(1));
        assert_eq!(Sigma(25).to_string(), "2.5σ");
        assert_eq!(Sigma(20).to_string(), "2σ");
    }

    #[test]
    fn labels() {
        assert_eq!(spec(AverageKind::Ema, 20).label(), "EMA 20");
        assert_eq!(
            AverageSpec::new(AverageKind::Sma, 50, Source::Hl2, Sigma::TWO).label(),
            "SMA 50 HL2"
        );
        assert_eq!(
            AverageSpec::new(AverageKind::Bollinger, 20, Source::Close, Sigma(25)).label(),
            "Bollinger 20 2.5σ"
        );
        assert_eq!(spec(AverageKind::Sma, 0).length, 1);
    }
}
