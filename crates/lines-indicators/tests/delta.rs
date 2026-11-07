//! Cumulative delta through the graph: ETH and RTH resets on the CME calendar, exact sums,
//! node sharing.

use lines_core::sessions::{ExchangeHours, Hours, LocalTime, SessionCalendar, SessionSpec};
use lines_core::{Aggressor, Price, Qty, UtcMillis};
use lines_indicators::graph::make_builtin;
use lines_indicators::{DeltaPoint, DeltaReset, DeltaSpec, Graph, Input, Output, SeriesKey, Spec};

const MIN: i64 = 60_000;

fn es_calendar() -> &'static SessionCalendar {
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

fn trade(iso: &str, qty: i64, a: Aggressor) -> Input {
    Input::Trade {
        time: at(iso),
        price: Price(600_000),
        qty: Qty(qty),
        aggressor: Some(a),
    }
}

struct Rig {
    graph: Graph,
}

impl Rig {
    fn new() -> Rig {
        let mut graph = Graph::new();
        let cal = es_calendar();
        let mut make = |k: &SeriesKey| make_builtin(k, Some(cal), 10_000);
        graph
            .acquire(
                SeriesKey {
                    feed: 1,
                    spec: Spec::Market { period_ms: MIN },
                },
                &mut make,
            )
            .unwrap();
        graph.push(
            1,
            &[
                Input::Connect {
                    expect_history: false,
                },
                Input::TradesFrom(at("2026-11-09T21:00:00Z")),
            ],
        );
        Rig { graph }
    }

    fn delta(&mut self, reset: DeltaReset) -> usize {
        let cal = es_calendar();
        let mut make = |k: &SeriesKey| make_builtin(k, Some(cal), 10_000);
        self.graph
            .acquire(
                SeriesKey {
                    feed: 1,
                    spec: Spec::CumDelta {
                        period_ms: MIN,
                        delta: DeltaSpec { reset },
                    },
                },
                &mut make,
            )
            .unwrap()
    }

    fn points(&self, id: usize) -> Vec<DeltaPoint> {
        match self.graph.output(id) {
            Some(Output::Candles(p)) => p.to_vec(),
            _ => panic!("not candles"),
        }
    }
}

#[test]
fn eth_resets_at_the_session_open_rth_only_inside_rth_none_never() {
    let mut r = Rig::new();
    let (eth, rth, none) = (
        r.delta(DeltaReset::Eth),
        r.delta(DeltaReset::Rth),
        r.delta(DeltaReset::None),
    );
    // Monday 2026-11-09 (CST, UTC-6): RTH 14:30–21:00Z, the ETH close 22:00Z, the next ETH
    // (trade date Tuesday) opens 23:00Z.
    r.graph.push(
        1,
        &[
            trade("2026-11-09T21:30:00Z", 10, Aggressor::Buy), // after RTH, in ETH
            trade("2026-11-09T21:31:00Z", 4, Aggressor::Sell),
            trade("2026-11-09T23:00:00Z", 7, Aggressor::Sell), // the new session
            trade("2026-11-10T15:00:00Z", 3, Aggressor::Buy),  // Tuesday RTH
        ],
    );
    let closes = |p: &[DeltaPoint]| p.iter().map(|x| x.close).collect::<Vec<_>>();
    // ETH: 10, 6 in Monday's session; Tuesday's starts over: -7, -4.
    assert_eq!(closes(&r.points(eth)), [10, 6, -7, -4]);
    let e = r.points(eth);
    assert_eq!(e[2].open, 0, "a reset candle opens at 0");
    assert_ne!(e[1].period, e[2].period);
    // RTH: only the trade inside RTH has a value.
    let p = r.points(rth);
    assert_eq!(closes(&p), [3]);
    assert_eq!(p[0].time, at("2026-11-10T15:00:00Z"));
    // No reset: one running sum.
    assert_eq!(closes(&r.points(none)), [10, 6, -1, 2]);
    assert!(r.points(none).iter().all(|p| !p.approx));
}

#[test]
fn the_same_delta_is_one_shared_node() {
    let mut r = Rig::new();
    let a = r.delta(DeltaReset::Eth);
    let b = r.delta(DeltaReset::Eth);
    let c = r.delta(DeltaReset::Rth);
    assert_eq!(a, b);
    assert_ne!(a, c);
    r.graph
        .push(1, &[trade("2026-11-10T15:00:00Z", 3, Aggressor::Buy)]);
    assert_eq!(
        r.graph.evaluations(a),
        2,
        "acquire + one push, once for both"
    );
}
