//! Replay-driven paper trading: a recorded session goes through the real feed driver (replay
//! transport, `lines` provider, the listener tap), scripted orders go into the trading core at
//! fixed times, and the fills and P&L come out exactly. Plus the threaded service: click →
//! order-sent latency, and live market data through the feed bridge (the CQG mock).

use std::time::{Duration, Instant};

use lines_core::{ContractKey, Month, Price, Qty, UtcMillis};
use lines_feed::native::{self, NativeEvent};
use lines_feed::provider::Frame;
use lines_feed::recording::{Direction, FORMAT_VERSION, Header, Writer};
use lines_feed::{FeedConfig, Source as FeedSource, catalog, new_store, run_feed};
use lines_trading::market::from_events;
use lines_trading::{
    AccountConfig, AccountId, Command, Event, FeedBridge, FillModel, Money, OrderKind, OrderSpec,
    OrderState, PaperBroker, PaperConfig, Side, Source, TradingCore, TradingService,
};
use tokio::sync::watch;

fn es() -> ContractKey {
    ContractKey::monthly(catalog::ES, 2026, Month::new(12).unwrap())
}

fn px(p: f64) -> Price {
    Price((p * 100.0).round() as i64)
}

fn acct() -> AccountId {
    AccountId::new("paper-1")
}

fn at(iso: &str) -> UtcMillis {
    UtcMillis::parse_iso(iso).unwrap()
}

/// Ten minutes of ES: a static book, trades every second around 6000, a dip to 5999.25 at
/// +150 s and a pop to 6001.00 at +300 s.
fn recording() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("lines-trading-session-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("es.lfr");
    let start = at("2026-11-10T15:00:00Z");
    let mut w = Writer::create(
        &path,
        &Header {
            start_wall_ms: start.0,
            provider: native::PROVIDER.into(),
            protocol: native::PROTOCOL.into(),
            product: "ES".into(),
            contract: Some(es()),
            version: FORMAT_VERSION,
        },
    )
    .unwrap();
    let us = |s: i64| (start.0 + s * 1000) * 1000;
    let mut frame = |s: i64, evs: Vec<NativeEvent>| {
        let mut buf = Vec::new();
        for e in &evs {
            native::encode(e, &mut buf);
        }
        w.write(
            Direction::FromServer,
            Duration::from_secs(s as u64),
            &Frame::binary(buf),
        )
        .unwrap();
    };
    let level = |s, side, p: f64, q| NativeEvent::BookLevel {
        time_us: us(s),
        side,
        price: px(p),
        qty: Qty(q),
        orders: 0,
    };
    use lines_core::Side::{Ask, Bid};
    frame(
        0,
        vec![
            NativeEvent::BookClear { time_us: us(0) },
            level(0, Bid, 5999.75, 10),
            level(0, Bid, 5999.50, 10),
            level(0, Bid, 5999.25, 10),
            level(0, Ask, 6000.00, 10),
            level(0, Ask, 6000.25, 10),
            level(0, Ask, 6000.50, 10),
        ],
    );
    for s in 1..600 {
        let (p, q) = match s {
            150 => (5999.25, 3),
            300 => (6001.00, 5),
            s if s % 2 == 0 => (6000.00, 1),
            _ => (5999.75, 1),
        };
        frame(
            s,
            vec![NativeEvent::Trade {
                time_us: us(s),
                price: px(p),
                qty: Qty(q),
                aggressor: native::Aggressor::Unknown,
            }],
        );
    }
    w.finish().unwrap();
    path
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replayed_session_with_scripted_orders_gives_exact_fills_and_pnl() {
    // 1. Replay through the real feed driver, catching batches with a listener.
    let config = FeedConfig::new(FeedSource::Replay {
        path: recording(),
        speed: 0.0,
        stop_at: None,
    });
    let store = new_store(&config).unwrap();
    let rx = lines_feed::listen::listen(store.feed_id());
    let (_tx, srx) = watch::channel(false);
    tokio::time::timeout(Duration::from_secs(30), run_feed(config, store, srx))
        .await
        .unwrap();
    let batches: Vec<_> = rx.try_iter().collect();
    assert_eq!(batches.len(), 600);

    // 2. A paper account; scripted orders at fixed session times.
    let mut cfg = PaperConfig {
        latency_ms: 25,
        fill_model: FillModel::TradeThrough,
        ..Default::default()
    };
    cfg.fees.insert(catalog::ES.0, Money::cents(100));
    let mut broker = PaperBroker::new(cfg);
    broker.open_account(acct(), Money::whole(50_000));
    let mut core = TradingCore::new(Box::new(broker));
    core.add_account(AccountConfig::paper("paper-1", 50_000), None);
    let start = at("2026-11-10T15:00:00Z").0;
    let script = [
        (60, Side::Buy, 5999.50), // fills on the dip at +150 s (trade-through: 5999.25)
        (200, Side::Sell, 6000.75), // fills on the pop at +300 s (6001.00)
    ];
    let mut next = 0;
    let mut events = Vec::new();
    for batch in &batches {
        let inputs = from_events(batch);
        let t = inputs
            .iter()
            .find_map(|i| match i {
                lines_trading::MarketInput::Trade { time, .. } => Some(*time),
                _ => None,
            })
            .unwrap_or(UtcMillis(start));
        while next < script.len() && start + script[next].0 * 1000 <= t.0 {
            let (s, side, p) = script[next];
            core.submit(
                UtcMillis(start + s * 1000),
                Source::Test,
                Instant::now(),
                Command::PlaceOrder(OrderSpec {
                    account: acct(),
                    contract: es(),
                    side,
                    qty: Qty(2),
                    kind: OrderKind::Limit { price: px(p) },
                }),
            );
            next += 1;
        }
        core.tick(t); // acks due by now
        core.market(t, es(), &inputs);
        events.extend(core.drain());
    }

    // 3. Exactly these fills, and this P&L.
    let fills: Vec<(i64, Price, i64)> = events
        .iter()
        .filter_map(|e| match e {
            Event::Fill(f) => Some((f.at.0 - start, f.price, f.qty.0)),
            _ => None,
        })
        .collect();
    assert_eq!(
        fills,
        vec![(150_000, px(5999.50), 2), (300_000, px(6000.75), 2)]
    );
    assert_eq!(core.position(&acct(), es()), 0);
    // +1.25 points × 2 × $50 = $125, minus 4 × $1.00 fees.
    assert_eq!(
        core.balance(&acct()),
        Some(Money::whole(50_000) + Money::whole(125) - Money::whole(4))
    );
}

fn far_limit(i: i64) -> Command {
    Command::PlaceOrder(OrderSpec {
        account: acct(),
        contract: es(),
        side: Side::Buy,
        qty: Qty(1),
        kind: OrderKind::Limit {
            price: Price(400_000 + (i % 100) * 25),
        },
    })
}

/// Clicks a millisecond apart through the threaded service (far faster than a trader), each
/// order cancelled by the next click, so the account holds what a trader would. Returns the
/// core's click → order-sent latency (p50 µs, p99 µs, samples) and how many orders were placed.
fn click_latency(n: i64) -> ((Option<u64>, Option<u64>, usize), usize) {
    let mut broker = PaperBroker::new(PaperConfig {
        check_sessions: false,
        ..Default::default()
    });
    broker.open_account(acct(), Money::whole(1_000_000));
    let mut core = TradingCore::new(Box::new(broker));
    core.add_account(AccountConfig::paper("paper-1", 1_000_000), None);
    let svc = TradingService::start(core, None).unwrap();
    let h = svc.handle();
    let events = h.subscribe();
    let mut last = None;
    let mut placed = 0;
    for i in 0..n {
        if i % 2 == 0 {
            last = Some(h.submit(Source::Test, far_limit(i)));
            placed += 1;
        } else {
            last = Some(h.submit(
                Source::Test,
                Command::CancelAll {
                    account: acct(),
                    contract: None,
                },
            ));
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    // Wait for the last command's result.
    loop {
        match events.recv_timeout(Duration::from_secs(30)) {
            Ok(Event::Command { id, result, .. }) if Some(id) == last => {
                assert!(result.is_ok());
                break;
            }
            Ok(_) => {}
            Err(e) => panic!("{e}"),
        }
    }
    (h.latency(), placed)
}

/// Decision 0012: every command sent through the threaded service records its click →
/// order-sent time. Behaviour only: the budget is a perf test (below), never a unit test
/// (wall-clock budgets fail under parallel load).
#[test]
fn click_to_order_sent_is_recorded_through_the_service() {
    let ((p50, p99, n), placed) = click_latency(100);
    assert!(n >= placed, "one sample per order sent ({n} < {placed})");
    let (p50, p99) = (p50.expect("p50"), p99.expect("p99"));
    assert!(p50 <= p99);
}

/// The click → order-sent budget (decision 0012; roadmap 7 target: under 1 ms to the core).
/// A perf test: run alone, in release, on an idle machine:
/// `cargo test --release -p lines-trading --test session -- --ignored --nocapture`.
#[test]
#[ignore = "perf: wall-clock budget; run alone in release"]
fn click_to_order_sent_latency_budget() {
    let ((p50, p99, n), _) = click_latency(2000);
    println!("click -> order sent: p50 {p50:?} us, p99 {p99:?} us over {n} orders and cancels");
    assert!(p99.unwrap() < 1_000, "p99 {p99:?} us");
}

/// Live market data through the bridge: the CQG mock's ES feed fills a paper market order.
#[test]
fn feed_bridge_brings_live_market_data_to_the_paper_broker() {
    let base = FeedConfig::new(FeedSource::CqgMock(lines_feed::cqg::mock::MockConfig {
        quotes_per_sec: 2_000,
        history_bars: 10,
        ..Default::default()
    }));
    let es = catalog::get(catalog::ES).unwrap();
    let front = lines_feed::front_month(&es, None).unwrap();
    let mut broker = PaperBroker::new(PaperConfig {
        check_sessions: false, // the mock trades whenever the test runs
        ..Default::default()
    });
    broker.open_account(acct(), Money::whole(50_000));
    let mut core = TradingCore::new(Box::new(broker));
    core.add_account(AccountConfig::paper("paper-1", 50_000), None);
    let svc = TradingService::start(core, None).unwrap();
    let h = svc.handle();
    let events = h.subscribe();
    let bridge = FeedBridge::new(lines_feed::registry::FeedRegistry::new(), base);
    bridge.follow(front, &h).unwrap();
    std::thread::sleep(Duration::from_millis(800)); // the book arrives
    let id = h.submit(
        Source::Test,
        Command::PlaceOrder(OrderSpec {
            account: acct(),
            contract: front,
            side: Side::Buy,
            qty: Qty(1),
            kind: OrderKind::Market,
        }),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut filled = false;
    while Instant::now() < deadline && !filled {
        if let Ok(e) = events.recv_timeout(Duration::from_millis(500)) {
            match e {
                Event::Command { id: i, result, .. } if i == id => {
                    assert!(result.is_ok(), "{result:?}")
                }
                Event::Order(o) if o.state == OrderState::Filled => filled = true,
                _ => {}
            }
        }
    }
    assert!(filled, "the market order filled from the mock's book");
}

/// A trigger's underlying keeps its feed with no pane open, and shares a pane's when there is one,
/// so the core never gets the same data twice. Letting go leaves the pane's running.
#[test]
fn an_underlying_can_be_held_with_no_pane_and_shares_a_panes_feed() {
    use std::collections::BTreeSet;
    let base = FeedConfig::new(FeedSource::CqgMock(lines_feed::cqg::mock::MockConfig {
        quotes_per_sec: 2_000,
        history_bars: 10,
        ..Default::default()
    }));
    let mut broker = PaperBroker::new(PaperConfig {
        check_sessions: false,
        ..Default::default()
    });
    broker.open_account(acct(), Money::whole(50_000));
    let mut core = TradingCore::new(Box::new(broker));
    core.add_account(AccountConfig::paper("paper-1", 50_000), None);
    let svc = TradingService::start(core, None).unwrap();
    let h = svc.handle();
    let registry = lines_feed::registry::FeedRegistry::new();
    let bridge = FeedBridge::new(registry.clone(), base.clone());
    let es = BTreeSet::from([catalog::ES]);
    assert!(!bridge.holding(catalog::ES));
    assert!(bridge.hold_underlyings(&es, &h).is_empty());
    assert!(bridge.holding(catalog::ES));
    // Asked again: nothing new.
    assert!(bridge.hold_underlyings(&es, &h).is_empty());
    // A pane on the product joins the same feed.
    let mut cfg = base.clone();
    cfg.target = Some(lines_feed::catalog::Target::Product(catalog::ES));
    let pane = registry.subscribe(cfg.clone()).unwrap();
    let again = registry.subscribe(cfg).unwrap();
    assert_eq!(pane.store().feed_id(), again.store().feed_id());
    drop(again);
    // Letting go leaves the pane's running.
    assert!(bridge.hold_underlyings(&BTreeSet::new(), &h).is_empty());
    assert!(!bridge.holding(catalog::ES));
    assert!(pane.store().try_read().is_some());
    // A source with no feed for a product says so rather than fail.
    let spy = BTreeSet::from([catalog::SPY]);
    assert_eq!(bridge.hold_underlyings(&spy, &h), spy);
    assert!(!bridge.holding(catalog::SPY));
}

/// The point of holding a trigger's underlying: with no pane open, its data reaches the core, so
/// paper fills (and a trigger's print) happen.
#[test]
fn a_held_underlying_feeds_the_core_with_no_pane() {
    use std::collections::BTreeSet;
    let base = FeedConfig::new(FeedSource::CqgMock(lines_feed::cqg::mock::MockConfig {
        quotes_per_sec: 2_000,
        history_bars: 10,
        ..Default::default()
    }));
    let es = catalog::get(catalog::ES).unwrap();
    let front = lines_feed::front_month(&es, None).unwrap();
    let mut broker = PaperBroker::new(PaperConfig {
        check_sessions: false,
        ..Default::default()
    });
    broker.open_account(acct(), Money::whole(50_000));
    let mut core = TradingCore::new(Box::new(broker));
    core.add_account(AccountConfig::paper("paper-1", 50_000), None);
    let svc = TradingService::start(core, None).unwrap();
    let h = svc.handle();
    let events = h.subscribe();
    let bridge = FeedBridge::new(lines_feed::registry::FeedRegistry::new(), base);
    assert!(
        bridge
            .hold_underlyings(&BTreeSet::from([catalog::ES]), &h)
            .is_empty()
    );
    std::thread::sleep(Duration::from_millis(800)); // the book arrives
    let id = h.submit(
        Source::Test,
        Command::PlaceOrder(OrderSpec {
            account: acct(),
            contract: front,
            side: Side::Buy,
            qty: Qty(1),
            kind: OrderKind::Market,
        }),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut filled = false;
    while Instant::now() < deadline && !filled {
        if let Ok(e) = events.recv_timeout(Duration::from_millis(500)) {
            match e {
                Event::Command { id: i, result, .. } if i == id => {
                    assert!(result.is_ok(), "{result:?}")
                }
                Event::Order(o) if o.state == OrderState::Filled => filled = true,
                _ => {}
            }
        }
    }
    assert!(filled, "the held feed's book filled it, with no pane");
}
