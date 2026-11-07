//! VWAP against hand-computed values, across an ETH boundary, a DST change and a holiday; RTH,
//! weekly and anchored resets; bands; approximate vs exact; the graph's sharing, cycles,
//! types, bounded recompute and determinism.

use lines_core::sessions::{
    ExchangeHours, HolidayTable, Hours, LocalTime, SessionCalendar, SessionSpec,
};
use lines_core::{Bar, Price, Qty, UtcMillis};
use lines_indicators::graph::{Changed, Factory, NodeKind, make_builtin};
use lines_indicators::{
    Bands, Graph, GraphError, Indicator, Input, Output, Point, Reset, SeriesKey, Spec, VwapSpec,
};

const MIN: i64 = 60_000;

/// CME equity index hours (as in the catalog) with Thanksgiving 2026 from an inline table.
fn es_calendar() -> &'static SessionCalendar {
    let table = HolidayTable::parse(
        "format 1\nversion 2026-11-06\ncalendar cme-equity-index\ncoverage 2026 2026 unverified\n\
         source test\n2026-11-26 halt 12:00 unverified Thanksgiving\n\
         2026-11-27 early 12:15 unverified Day after Thanksgiving\n",
    )
    .unwrap();
    let spec = SessionSpec {
        time_zone: "America/Chicago",
        hours: Hours::Exchange(ExchangeHours {
            eth_open: LocalTime::hm(17, 0),
            eth_close: LocalTime::hm(16, 0),
            rth: Some((LocalTime::hm(8, 30), LocalTime::hm(15, 0))),
        }),
        holidays: Some("cme-equity-index"),
    };
    Box::leak(Box::new(SessionCalendar::new(spec, Some(&table)).unwrap()))
}

fn at(iso: &str) -> UtcMillis {
    UtcMillis::parse_iso(iso).unwrap()
}

fn trade(iso: &str, price: f64, qty: i64) -> Input {
    Input::Trade {
        time: at(iso),
        price: Price((price * 100.0).round() as i64),
        qty: Qty(qty),
        aggressor: None,
    }
}

struct Rig {
    g: Graph,
    cal: &'static SessionCalendar,
}

impl Rig {
    fn new() -> Rig {
        let mut r = Rig {
            g: Graph::new(),
            cal: es_calendar(),
        };
        r.node(Spec::Market { period_ms: MIN });
        r
    }

    fn node(&mut self, spec: Spec) -> usize {
        let cal = self.cal;
        let mut f = |k: &SeriesKey| make_builtin(k, Some(cal), 10_000);
        self.g.acquire(SeriesKey { feed: 1, spec }, &mut f).unwrap()
    }

    fn vwap(&mut self, reset: Reset, bands: Bands) -> usize {
        self.node(Spec::Vwap {
            period_ms: MIN,
            vwap: VwapSpec { reset, bands },
        })
    }

    fn points(&self, id: usize) -> Vec<Point> {
        match self.g.output(id) {
            Some(Output::Line(p)) => p.to_vec(),
            _ => panic!("not a line"),
        }
    }

    fn push(&mut self, inputs: &[Input]) -> Vec<Changed> {
        self.g.push(1, inputs)
    }
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-6
}

/// Values are price units (cents); ES 6000.00 = 600000.
#[test]
fn eth_rth_weekly_and_bands_across_the_eth_boundary() {
    let mut r = Rig::new();
    let eth = r.vwap(Reset::Eth, Bands::ALL);
    let rth = r.vwap(Reset::Rth, Bands::NONE);
    let week = r.vwap(Reset::Weekly, Bands::NONE);
    // Tuesday 2026-11-10 (CST = UTC-6).
    r.push(&[
        Input::Connect {
            expect_history: false,
        },
        trade("2026-11-10T14:00:00Z", 6000.00, 2), // 08:00 CT: ETH only
        trade("2026-11-10T15:00:00Z", 6004.00, 1), // 09:00 CT: ETH and RTH
        trade("2026-11-10T21:59:00Z", 6002.00, 3), // 15:59 CT: ETH only (RTH closed 15:00)
        trade("2026-11-10T23:01:00Z", 6010.00, 5), // 17:01 CT: Wednesday's ETH session
    ]);
    let e = r.points(eth);
    assert_eq!(e.len(), 4);
    assert!(close(e[0].vwap, 600_000.0));
    // (2·600000 + 600400) / 3
    assert!(close(e[1].vwap, 1_800_400.0 / 3.0));
    // (1200000 + 600400 + 1800600) / 6 = 3601000 / 6
    assert!(close(e[2].vwap, 3_601_000.0 / 6.0));
    // Deviations from 600166.67: −166.67 ×2, +233.33 ×1, +33.33 ×3 → var = 113333.33 / 6.
    assert!(
        close(e[2].sd, (113_333.333_333_333_3_f64 / 6.0).sqrt()),
        "{}",
        e[2].sd
    );
    assert!(close(e[3].vwap, 601_000.0), "reset at the 17:00 CT open");
    assert_ne!(e[3].period, e[2].period);
    assert_eq!(e[3].sd, 0.0);
    assert!(e[3].forming && !e[2].forming);
    assert!(e.iter().all(|p| !p.approx), "all from trades");

    let rt = r.points(rth);
    assert_eq!(rt.len(), 1, "only 09:00 CT is inside RTH");
    assert_eq!(rt[0].time, at("2026-11-10T15:00:00Z"));
    assert!(close(rt[0].vwap, 600_400.0));

    // Tuesday and Wednesday trade dates share the week: no reset at 17:00.
    let w = r.points(week);
    assert!(close(w[3].vwap, (3_601_000.0 + 3_005_000.0) / 11.0));
    assert_eq!(w[3].period, w[0].period);
}

#[test]
fn dst_change_moves_the_open_in_utc() {
    let mut r = Rig::new();
    let eth = r.vwap(Reset::Eth, Bands::NONE);
    r.push(&[
        Input::Connect {
            expect_history: false,
        },
        // Sunday 2026-10-25 17:30 CDT (UTC-5) = 22:30Z: Monday 10-26's session (opened 22:00Z).
        trade("2026-10-25T22:30:00Z", 6000.00, 1),
        // Sunday 2026-11-01 22:30Z = 16:30 CST (UTC-6): before the 17:00 CST open: closed.
        trade("2026-11-01T22:30:00Z", 6100.00, 1),
        // 23:00Z = 17:00 CST: Monday 11-02's session starts here, an hour later in UTC.
        trade("2026-11-01T23:00:00Z", 6200.00, 1),
    ]);
    let e = r.points(eth);
    assert_eq!(
        e.iter().map(|p| p.time).collect::<Vec<_>>(),
        vec![at("2026-10-25T22:30:00Z"), at("2026-11-01T23:00:00Z")],
        "no VWAP while closed"
    );
    assert!(close(e[1].vwap, 620_000.0), "a new session");
    assert_ne!(e[0].period, e[1].period);
}

#[test]
fn thanksgiving_halt_carries_the_session_and_the_early_close_ends_it() {
    let mut r = Rig::new();
    let eth = r.vwap(Reset::Eth, Bands::NONE);
    r.push(&[
        Input::Connect {
            expect_history: false,
        },
        trade("2026-11-25T16:00:00Z", 6010.00, 1), // Wed 10:00 CT: Wednesday's session
        trade("2026-11-26T00:00:00Z", 6020.00, 1), // Wed 18:00 CT: Friday's session (Thu is a halt day)
        trade("2026-11-26T17:00:00Z", 6030.00, 1), // Thu 11:00 CT: before the 12:00 halt
        trade("2026-11-26T23:30:00Z", 6040.00, 2), // Thu 17:30 CT: after the halt, same trade date
        trade("2026-11-27T18:20:00Z", 6050.00, 1), // Fri 12:20 CT: after the 12:15 early close
    ]);
    let e = r.points(eth);
    assert_eq!(e.len(), 4, "nothing after the early close");
    assert!(close(e[0].vwap, 601_000.0));
    assert_ne!(e[1].period, e[0].period, "reset at Wednesday's 17:00 open");
    assert_eq!(e[1].period, e[3].period, "the halt doesn't reset");
    // (602000 + 603000 + 2·604000) / 4
    assert!(close(e[3].vwap, 2_413_000.0 / 4.0));
}

#[test]
fn anchored_starts_at_its_bar() {
    let mut r = Rig::new();
    let a = r.vwap(
        Reset::Anchored {
            from: at("2026-11-10T15:02:00Z").0,
        },
        Bands(0b001),
    );
    let mut inputs = vec![Input::Connect {
        expect_history: false,
    }];
    for (m, p) in [(0, 100.0), (1, 101.0), (2, 102.0), (3, 104.0), (4, 100.0)] {
        inputs.push(trade(&format!("2026-11-10T15:0{m}:00Z"), 6000.0 + p, 1));
    }
    r.push(&inputs);
    let pts = r.points(a);
    assert_eq!(pts.len(), 3, "from the anchor bar on");
    assert_eq!(pts[0].time, at("2026-11-10T15:02:00Z"));
    assert!(close(pts[2].vwap, 610_200.0)); // (102 + 104 + 100) / 3 over 6000
    // Prices 610200, 610400, 610000 around 610200: var = (0 + 200² + 200²) / 3.
    assert!(close(pts[2].sd, (80_000.0f64 / 3.0).sqrt()));
}

#[test]
fn bar_history_is_approximate_until_trades_take_over() {
    let mut r = Rig::new();
    let eth = r.vwap(Reset::Eth, Bands::NONE);
    let hist = Bar {
        time: at("2026-11-10T14:00:00Z"),
        open: Price(600_000),
        high: Price(600_300),
        low: Price(599_700),
        close: Price(600_000),
        volume: Qty(10),
    };
    r.push(&[
        Input::Connect {
            expect_history: true,
        },
        Input::History {
            bars: vec![hist],
            as_of: Some(UtcMillis(hist.time.0 + MIN - 1)),
        },
        Input::HistoryComplete,
        trade("2026-11-10T14:01:00Z", 6003.00, 10),
    ]);
    let e = r.points(eth);
    assert!(e[0].approx, "from a bar's typical price");
    assert!(close(e[0].vwap, 600_000.0)); // (600300 + 599700 + 600000) / 3
    assert!(e[1].approx, "the session still includes the bar");
    assert!(close(e[1].vwap, (6_000_000.0 + 6_003_000.0) / 20.0));
    // The next session is all trades: exact again.
    r.push(&[trade("2026-11-10T23:00:00Z", 6010.00, 1)]);
    assert!(!r.points(eth)[2].approx);
}

#[test]
fn late_history_recomputes_only_from_what_changed() {
    let mut r = Rig::new();
    let eth = r.vwap(Reset::Eth, Bands::NONE);
    let mut inputs = vec![Input::Connect {
        expect_history: true,
    }];
    inputs.push(Input::History {
        bars: vec![],
        as_of: Some(at("2026-11-10T13:59:59.999Z")),
    });
    for m in 0..30 {
        inputs.push(trade(
            &format!("2026-11-10T14:{m:02}:00Z"),
            6000.0 + m as f64,
            1,
        ));
    }
    r.push(&inputs);
    // A correction-style snapshot replacing minute 20 only.
    let fix = Bar {
        time: at("2026-11-10T14:20:00Z"),
        open: Price(600_000),
        high: Price(600_000),
        low: Price(600_000),
        close: Price(600_000),
        volume: Qty(1),
    };
    let changed = r.push(&[Input::History {
        bars: vec![fix],
        as_of: Some(UtcMillis(fix.time.0 + MIN - 1)),
    }]);
    let c = changed.iter().find(|c| c.node == eth).unwrap();
    // Point 19 is re-sent for its `forming` flag; everything before it is untouched.
    assert_eq!(
        c.first, 19,
        "points before the changed bucket are untouched"
    );
    let pts = r.points(eth);
    assert!(
        pts[..20].iter().all(|p| !p.approx),
        "earlier trades stay exact"
    );
    assert!(pts[20].approx, "the replaced bucket is bar-derived now");
}

/// A named indicator for the graph tests: the spread between two VWAPs.
struct Spread {
    a: Spec,
    b: Spec,
    out: Vec<Point>,
}

impl Indicator for Spread {
    fn inputs(&self) -> Vec<Spec> {
        vec![self.a.clone(), self.b.clone()]
    }
    fn update(&mut self, inputs: &[Output<'_>], _from: UtcMillis) -> usize {
        let (Output::Line(a), Output::Line(b)) = (inputs[0], inputs[1]) else {
            return 0;
        };
        self.out.clear();
        for p in a {
            if let Some(q) = b.iter().find(|q| q.time == p.time) {
                self.out.push(Point {
                    vwap: p.vwap - q.vwap,
                    ..*p
                });
            }
        }
        0
    }
    fn output(&self) -> Output<'_> {
        Output::Line(&self.out)
    }
}

fn eth_spec() -> Spec {
    Spec::Vwap {
        period_ms: MIN,
        vwap: VwapSpec::default(),
    }
}

fn rth_spec() -> Spec {
    Spec::Vwap {
        period_ms: MIN,
        vwap: VwapSpec {
            reset: Reset::Rth,
            bands: Bands::NONE,
        },
    }
}

fn factory(cal: &'static SessionCalendar) -> impl FnMut(&SeriesKey) -> Option<NodeKind> {
    move |k: &SeriesKey| match &k.spec {
        Spec::Named(n) if n == "spread" => Some(NodeKind::indicator(Box::new(Spread {
            a: eth_spec(),
            b: rth_spec(),
            out: Vec::new(),
        }))),
        Spec::Named(n) if n == "a" => Some(NodeKind::indicator(Box::new(Spread {
            a: Spec::Named("b".into()),
            b: eth_spec(),
            out: Vec::new(),
        }))),
        Spec::Named(n) if n == "b" => Some(NodeKind::indicator(Box::new(Spread {
            a: Spec::Named("a".into()),
            b: eth_spec(),
            out: Vec::new(),
        }))),
        // A named series that is really the market series: the wrong shape for a consumer.
        Spec::Named(n) if n == "market" => make_builtin(
            &SeriesKey {
                feed: k.feed,
                spec: Spec::Market { period_ms: MIN },
            },
            Some(cal),
            100,
        ),
        Spec::Named(n) if n == "bad" => Some(NodeKind::indicator(Box::new(Spread {
            a: Spec::Named("market".into()),
            b: eth_spec(),
            out: Vec::new(),
        }))),
        _ => make_builtin(k, Some(cal), 10_000),
    }
}

#[test]
fn identical_nodes_are_shared_cycles_and_bad_types_rejected() {
    let cal = es_calendar();
    let mut g = Graph::new();
    let mut f = factory(cal);
    let f: &mut Factory<'_> = &mut f;
    let key = |spec| SeriesKey { feed: 1, spec };
    // Two charts show ETH VWAP; a custom indicator consumes it too.
    let chart1 = g.acquire(key(eth_spec()), f).unwrap();
    let chart2 = g.acquire(key(eth_spec()), f).unwrap();
    assert_eq!(chart1, chart2, "one node");
    let spread = g.acquire(key(Spec::Named("spread".into())), f).unwrap();
    assert_eq!(g.len(), 4, "market, ETH VWAP, RTH VWAP, spread");
    let before = g.evaluations(chart1);
    g.push(
        1,
        &[
            Input::Connect {
                expect_history: false,
            },
            trade("2026-11-10T15:00:00Z", 6004.0, 1),
        ],
    );
    assert_eq!(
        g.evaluations(chart1),
        before + 1,
        "computed once for two charts and the spread"
    );
    match g.output(spread) {
        Some(Output::Line(p)) => assert_eq!(p.len(), 1),
        _ => panic!(),
    }
    // Other feeds are separate nodes.
    assert_ne!(
        g.acquire(
            SeriesKey {
                feed: 2,
                spec: eth_spec()
            },
            f
        )
        .unwrap(),
        chart1
    );

    assert!(matches!(
        g.acquire(key(Spec::Named("a".into())), f),
        Err(GraphError::Cycle(_))
    ));
    assert!(matches!(
        g.acquire(key(Spec::Named("bad".into())), f),
        Err(GraphError::Type { .. })
    ));
    assert!(matches!(
        g.acquire(key(Spec::Named("nope".into())), f),
        Err(GraphError::Unknown(_))
    ));

    // Releasing: the shared node lives while anyone uses it.
    g.release(spread);
    g.release(chart1);
    assert!(g.output(chart2).is_some());
    g.release(chart2);
    assert!(g.output(chart2).is_none(), "gone with its last consumer");
}

#[test]
fn deterministic_for_the_same_inputs() {
    let run = || {
        let mut r = Rig::new();
        let ids = [
            r.vwap(Reset::Eth, Bands::ALL),
            r.vwap(Reset::Rth, Bands(0b010)),
        ];
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        for chunk in 0..50 {
            let mut batch = vec![];
            if chunk == 0 {
                batch.push(Input::Connect {
                    expect_history: false,
                });
            }
            for k in 0..200 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let t = at("2026-11-10T14:00:00Z").0 + (chunk * 200 + k) * 997;
                batch.push(Input::Trade {
                    time: UtcMillis(t),
                    price: Price(600_000 + (x % 400) as i64 * 25),
                    qty: Qty(1 + (x % 9) as i64),
                    aggressor: None,
                });
            }
            r.push(&batch);
        }
        ids.map(|id| r.points(id))
    };
    assert_eq!(run(), run());
}

#[test]
fn rth_from_history_bars_is_approximate_too() {
    let mut r = Rig::new();
    let rth = r.vwap(Reset::Rth, Bands::NONE);
    let bar = |iso: &str| Bar {
        time: at(iso),
        open: Price(600_000),
        high: Price(600_100),
        low: Price(599_900),
        close: Price(600_000),
        volume: Qty(5),
    };
    // 09:00 and 09:01 CT (inside RTH) from history; a live trade after the snapshot.
    r.push(&[
        Input::Connect {
            expect_history: true,
        },
        Input::History {
            bars: vec![bar("2026-11-10T15:00:00Z"), bar("2026-11-10T15:01:00Z")],
            as_of: Some(at("2026-11-10T15:01:59.999Z")),
        },
        Input::HistoryComplete,
        trade("2026-11-10T15:02:00Z", 6001.00, 1),
    ]);
    let p = r.points(rth);
    assert_eq!(p.len(), 3);
    assert!(p.iter().all(|p| p.approx), "{p:?}");
}
