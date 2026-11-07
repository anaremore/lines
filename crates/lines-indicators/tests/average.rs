//! Roadmap 10a: SMA, EMA and Bollinger Bands through the graph, on the chart's bars: 1 minute,
//! a higher timeframe derived in the engine (breaking at the session close), and trade-built
//! bars (tick, volume, range) set from outside; gaps; sharing; incremental = full.

use lines_core::sessions::{ExchangeHours, Hours, LocalTime, SessionCalendar, SessionSpec};
use lines_core::{
    Bar, BarKind, BarScale, BarSeries, Derived, Price, Qty, TickBars, TickReset, Timeframe,
    UtcMillis,
};
use lines_indicators::graph::make_builtin;
use lines_indicators::{
    Average, AverageKind, AverageSpec, BarsOut, ChartBars, Graph, Input, Output, Point,
    SeriesKey, Sigma, Source, Spec,
};

const MIN: i64 = 60_000;

fn calendar() -> &'static SessionCalendar {
    let spec = SessionSpec {
        time_zone: "America/Chicago",
        hours: Hours::Exchange(ExchangeHours {
            eth_open: LocalTime::hm(17, 0),
            eth_close: LocalTime::hm(16, 0),
            rth: Some((LocalTime::hm(8, 30), LocalTime::hm(15, 0))),
        }),
        holidays: None,
    };
    Box::leak(Box::new(SessionCalendar::new(spec, None).unwrap()))
}

fn at(iso: &str) -> UtcMillis {
    UtcMillis::parse_iso(iso).unwrap()
}

/// One trade a minute from `from` for `n` minutes, prices wandering on the grid.
fn minutes(from: UtcMillis, n: i64, seed: i64) -> Vec<Input> {
    (0..n)
        .map(|k| Input::Trade {
            time: UtcMillis(from.0 + k * MIN + 5_000),
            price: Price(600_000 + ((k * 37 + seed) % 23) * 25),
            qty: Qty(1 + k % 4),
            aggressor: None,
        })
        .collect()
}

fn start() -> Vec<Input> {
    vec![
        Input::Connect {
            expect_history: false,
        },
        Input::TradesFrom(UtcMillis(0)),
    ]
}

struct Rig {
    g: Graph,
    cal: &'static SessionCalendar,
}

impl Rig {
    fn new() -> Rig {
        let mut r = Rig {
            g: Graph::new(),
            cal: calendar(),
        };
        r.node(Spec::Market { period_ms: MIN });
        r
    }

    fn node(&mut self, spec: Spec) -> usize {
        let cal = self.cal;
        let mut make = |k: &SeriesKey| make_builtin(k, Some(cal), 10_000);
        self.g
            .acquire(SeriesKey { feed: 1, spec }, &mut make)
            .unwrap()
    }

    fn points(&self, id: usize) -> Vec<Point> {
        match self.g.output(id) {
            Some(Output::Line(p)) => p.to_vec(),
            _ => panic!("not a line"),
        }
    }

    fn base_bars(&self) -> Vec<Bar> {
        match self.g.output(0) {
            Some(Output::Market(m)) => m.bar_series().bars().to_vec(),
            _ => panic!(),
        }
    }
}

fn avg(kind: AverageKind, length: u16) -> AverageSpec {
    AverageSpec::new(kind, length, Source::Close, Sigma::TWO)
}

fn spec(bars: ChartBars, average: AverageSpec) -> Spec {
    Spec::Average {
        period_ms: MIN,
        bars,
        average,
    }
}

/// The same average computed in one pass over `bars`.
fn direct(average: AverageSpec, bars: &[Bar]) -> Vec<Point> {
    let mut a = Average::new(average);
    a.update(BarsOut { bars, trimmed: 0 }, UtcMillis(i64::MIN));
    a.points().to_vec()
}

const KINDS: [AverageKind; 3] = [AverageKind::Sma, AverageKind::Ema, AverageKind::Bollinger];

/// On 1 minute the averages read the base series' bars; live trades update them
/// incrementally to the same values a single pass gives.
#[test]
fn one_minute_averages_follow_live_trades() {
    let mut r = Rig::new();
    let bars = ChartBars::default();
    let ids: Vec<usize> = KINDS.map(|k| r.node(spec(bars, avg(k, 5)))).to_vec();
    r.g.push(1, &start());
    let trades = minutes(at("2026-11-04T15:00:00Z"), 40, 3);
    for chunk in trades.chunks(3) {
        r.g.push(1, chunk);
    }
    let base = r.base_bars();
    assert_eq!(base.len(), 40);
    for (k, id) in KINDS.iter().zip(&ids) {
        let p = r.points(*id);
        assert_eq!(p, direct(avg(*k, 5), &base), "{k:?}");
        assert_eq!(p.len(), 36, "values from the 5th bar");
        assert!(p.last().unwrap().forming && !p[0].forming);
        assert!(p.iter().all(|p| !p.approx), "bars are exact");
    }
}

/// A 15-minute chart: the engine derives the chart's bars (they break at the 16:00 CT close
/// and start again at the 17:00 open) and the average runs across that break.
#[test]
fn a_higher_timeframe_is_derived_and_breaks_at_the_session_close() {
    let mut r = Rig::new();
    let tf = ChartBars::new(Timeframe::Minutes(15), TickReset::Session);
    let ema = r.node(spec(tf, avg(AverageKind::Ema, 3)));
    r.g.push(1, &start());
    // 15:00-16:00 CT (21:00-22:00 UTC) and 17:00-18:00 CT the same evening.
    r.g.push(1, &minutes(at("2026-11-04T21:00:00Z"), 60, 1));
    r.g.push(1, &minutes(at("2026-11-04T23:00:00Z"), 60, 9));
    let mut series = BarSeries::new(MIN, 10_000);
    for b in r.base_bars() {
        series.upsert(b);
    }
    let mut chart = Derived::new(Timeframe::Minutes(15), series.capacity());
    chart.update(&series, Some(r.cal));
    assert_eq!(chart.bars().len(), 8, "four bars each side of the break");
    let p = r.points(ema);
    assert_eq!(p, direct(avg(AverageKind::Ema, 3), chart.bars()));
    assert_eq!(p.len(), 6, "no reset: the EMA carries across the break");
    // Each point sits on its chart bar's start (what the chart pairs it with).
    for p in &p {
        assert!(chart.bars().iter().any(|b| b.time == p.time));
    }
    // A gap: trades lost, bars still the chart's bars (the stitched ones); values agree.
    r.g.push(1, &[Input::Gap]);
    r.g.push(1, &minutes(at("2026-11-05T00:00:00Z"), 20, 4));
    for b in r.base_bars() {
        series.upsert(b);
    }
    chart.update(&series, Some(r.cal));
    assert_eq!(r.points(ema), direct(avg(AverageKind::Ema, 3), chart.bars()));
}

/// A contract change starts the bars and the averages over.
#[test]
fn a_reset_starts_over() {
    let mut r = Rig::new();
    let tf = ChartBars::new(Timeframe::Minutes(5), TickReset::Session);
    let sma = r.node(spec(tf, avg(AverageKind::Sma, 2)));
    r.g.push(1, &start());
    r.g.push(1, &minutes(at("2026-11-04T15:00:00Z"), 30, 0));
    assert_eq!(r.points(sma).len(), 5);
    r.g.push(1, &[Input::Reset]);
    assert!(r.points(sma).is_empty());
    r.g.push(1, &start());
    r.g.push(1, &minutes(at("2026-11-04T16:00:00Z"), 10, 5));
    assert_eq!(r.points(sma).len(), 1);
}

/// Tick, volume and range bars: set from the store's builder (`sync_bars`), only what
/// changed, and the averages equal a single pass over the same bars; session resets close the
/// forming bar at the open and the average carries on.
#[test]
fn trade_built_bars_from_outside() {
    let cal = calendar();
    for kind in [BarKind::Ticks(7), BarKind::Volume(9), BarKind::Range(4)] {
        let mut r = Rig::new();
        let tf = match kind {
            BarKind::Ticks(n) => Timeframe::Ticks(n),
            BarKind::Volume(n) => Timeframe::Volume(n),
            BarKind::Range(n) => Timeframe::Range(n),
            BarKind::Seconds(n) => Timeframe::Seconds(n),
        };
        let cb = ChartBars::new(tf, TickReset::Session);
        let ids: Vec<usize> = KINDS.map(|k| r.node(spec(cb, avg(k, 4)))).to_vec();
        let bars_id =
            r.g.id(&SeriesKey {
                feed: 1,
                spec: Spec::Bars {
                    period_ms: MIN,
                    bars: cb,
                },
            })
            .expect("shared bars node");
        let scale = BarScale {
            tick: 25,
            qty_unit: 1,
        };
        let mut tb = TickBars::with_kind(kind, TickReset::Session, Some(cal), scale, 10_000);
        // Before and after the 16:00 CT close and the 17:00 open (session reset).
        let mut trades = minutes(at("2026-11-04T21:30:00Z"), 30, 2);
        trades.extend(minutes(at("2026-11-04T23:00:00Z"), 30, 7));
        let mut syncs = 0;
        for t in &trades {
            let Input::Trade {
                time, price, qty, ..
            } = t
            else {
                continue;
            };
            tb.push(*time, *price, *qty, None);
            if let Some(from) = r.g.sync_bars(bars_id, tb.bars(), tb.trimmed()) {
                r.g.bars_changed(bars_id, from);
                syncs += 1;
            }
        }
        assert_eq!(syncs, trades.len(), "{kind:?}: every trade changes a bar");
        assert!(tb.len() > 4, "{kind:?}");
        assert!((0..tb.len()).any(|i| i > 0 && tb.starts_session(i)), "{kind:?}");
        for (k, id) in KINDS.iter().zip(&ids) {
            assert_eq!(r.points(*id), direct(avg(*k, 4), tb.bars()), "{kind:?} {k:?}");
        }
        // Nothing new: nothing to do.
        assert_eq!(r.g.sync_bars(bars_id, tb.bars(), tb.trimmed()), None);
        // A rebuild (the store replaced its builder): everything again.
        let mut rebuilt = TickBars::with_kind(kind, TickReset::Session, Some(cal), scale, 10_000);
        for t in trades.iter().skip(3) {
            if let Input::Trade {
                time, price, qty, ..
            } = t
            {
                rebuilt.push(*time, *price, *qty, None);
            }
        }
        let from = r.g.sync_bars(bars_id, rebuilt.bars(), rebuilt.trimmed());
        assert_eq!(from, Some(UtcMillis(i64::MIN)), "{kind:?}");
        r.g.bars_changed(bars_id, from.unwrap());
        assert_eq!(
            r.points(ids[0]),
            direct(avg(KINDS[0], 4), rebuilt.bars()),
            "{kind:?}"
        );
    }
}

/// Two charts showing the same bars share the bars node and an identical average.
#[test]
fn identical_studies_share_nodes() {
    let mut r = Rig::new();
    let tf = ChartBars::new(Timeframe::Minutes(5), TickReset::Continuous);
    let a = r.node(spec(tf, avg(AverageKind::Ema, 9)));
    let n = r.g.len();
    let b = r.node(spec(
        ChartBars::new(Timeframe::Minutes(5), TickReset::Session),
        avg(AverageKind::Ema, 9),
    ));
    assert_eq!(a, b);
    assert_eq!(r.g.len(), n);
    // A different length is another average on the same bars.
    r.node(spec(tf, avg(AverageKind::Ema, 21)));
    assert_eq!(r.g.len(), n + 1);
}
