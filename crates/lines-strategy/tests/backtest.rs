//! Backtests (decision 0025) over a synthetic ES recording and a recorded synthetic level
//! map (decision 0010: synthetic data only, built in code).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use lines_core::{ContractKey, Month, Price, Qty, UtcMillis};
use lines_feed::catalog;
use lines_feed::levels::demo::{BODY, DemoLevels, ID as DEMO};
use lines_feed::levels::runtime::{PROTOCOL, PROVIDER, recording_path};
use lines_feed::levels::{LevelFrame, encode_batch};
use lines_feed::native::{self, Aggressor, NativeEvent};
use lines_feed::provider::Frame;
use lines_feed::recording::{Direction, FORMAT_VERSION, Header, Writer};
use lines_strategy::backtest::{self, Backtest, Report};
use lines_strategy::example::LevelTouchExt;
use lines_trading::{
    AccountConfig, HoursWindow, Money, PaperConfig, RejectCode, StrategyLimits, StrategyState,
    TradingHours,
};

fn es() -> ContractKey {
    ContractKey::monthly(catalog::ES, 2026, Month::new(12).unwrap())
}

fn ms(s: &str) -> i64 {
    UtcMillis::parse_iso(s).unwrap().0
}

/// A fresh scratch directory for one test.
fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("lines-strategy-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// One trade a second from 14:31 UTC (09:31 New York): 6003.00 down a tick a second to
/// 5997.25 (through the demo map's S1 at 5997.50), then up to 6000.00, then flat for a minute.
fn write_market(path: &Path) {
    let start = ms("2026-11-05T14:31:00Z");
    let header = Header {
        start_wall_ms: start,
        provider: native::PROVIDER.into(),
        protocol: native::PROTOCOL.into(),
        product: "ES".into(),
        contract: Some(es()),
        version: FORMAT_VERSION,
    };
    let mut prices: Vec<i64> = Vec::new();
    let mut p = 600_300;
    while p > 599_725 {
        prices.push(p);
        p -= 25;
    }
    while p <= 600_000 {
        prices.push(p);
        p += 25;
    }
    prices.extend(std::iter::repeat_n(600_000, 60));
    let mut w = Writer::create(path, &header).unwrap();
    let mut buf = Vec::new();
    let mut last = prices[0];
    for (i, &price) in prices.iter().enumerate() {
        let t = start + i as i64 * 1000;
        buf.clear();
        native::encode(
            &NativeEvent::Trade {
                time_us: t * 1000,
                price: Price(price),
                qty: Qty(2),
                aggressor: if price < last {
                    Aggressor::Sell
                } else {
                    Aggressor::Buy
                },
            },
            &mut buf,
        );
        last = price;
        w.write(
            Direction::FromServer,
            Duration::from_millis((t - start) as u64),
            &Frame::binary(buf.clone()),
        )
        .unwrap();
    }
    w.finish().unwrap();
}

/// The demo source's one batch, recorded at 14:30 UTC as the levels runtime records it.
fn write_levels(dir: &Path) {
    let path = recording_path(dir, DEMO, "ES");
    let header = Header {
        start_wall_ms: ms("2026-11-05T14:30:00Z"),
        provider: PROVIDER.into(),
        protocol: format!("{PROTOCOL}/{DEMO}"),
        product: "ES".into(),
        contract: Some(es()),
        version: FORMAT_VERSION,
    };
    let mut w = Writer::create(&path, &header).unwrap();
    let mut buf = Vec::new();
    encode_batch(&[LevelFrame::new("demo/levels", BODY)], &mut buf);
    w.write(Direction::FromServer, Duration::ZERO, &Frame::binary(buf))
        .unwrap();
    w.finish().unwrap();
}

fn limits(window: (&str, &str)) -> StrategyLimits {
    StrategyLimits {
        max_position: 2,
        max_daily_loss: 500,
        max_orders_per_min: 10,
        hours: TradingHours {
            tz: "America/New_York".into(),
            windows: vec![HoursWindow::new(window.0, window.1)],
        },
    }
}

fn backtest(dir: &Path, levels: bool, window: (&str, &str)) -> Backtest {
    lines_feed::levels::register(Arc::new(DemoLevels)).unwrap();
    let market = dir.join("market.lfr");
    if !market.exists() {
        write_market(&market);
    }
    if levels {
        write_levels(dir);
    }
    let mut paper = PaperConfig {
        latency_ms: 25,
        ..Default::default()
    };
    paper.fees.insert(catalog::ES.0, Money::cents(200));
    Backtest {
        strategy: Arc::new(LevelTouchExt::new(DEMO)),
        params: serde_json::json!({
            "kind": "support",
            "qty": 1,
            "stop_ticks": 8,
            "target_ticks": 8,
            "max_distance_ticks": 8
        }),
        market,
        levels_dir: levels.then(|| dir.to_path_buf()),
        paper,
        account: AccountConfig::paper("paper-1", 50_000),
        limits: limits(window),
        timer_ms: 1000,
    }
}

#[test]
fn a_backtest_trades_and_is_deterministic() {
    let dir = scratch("deterministic");
    let bt = backtest(&dir, true, ("09:30", "16:00"));
    let a: Report = backtest::run(&bt).unwrap();
    let b: Report = backtest::run(&bt).unwrap();
    assert_eq!(a.to_json(), b.to_json(), "same inputs, same bytes");

    assert_eq!(a.strategy, "level-touch");
    assert_eq!(a.contract, "ESZ26");
    assert_eq!(a.market.len(), 1);
    assert_eq!(a.levels.len(), 1);
    assert!(a.warnings.is_empty(), "{:?}", a.warnings);
    assert_eq!(a.states[0].state, StrategyState::Disarmed);
    assert_eq!(a.states[1].state, StrategyState::Armed);

    // One decision: buy 1 at S1 5997.50 with a 2-point bracket; accepted.
    assert_eq!(a.decisions.len(), 1, "{:#?}", a.decisions);
    let d = &a.decisions[0];
    assert_eq!(d.decision.rule, "touch-long");
    assert!(
        d.decision.reason.contains("S1 5997.50"),
        "{}",
        d.decision.reason
    );
    assert_eq!(d.decision.levels[0].price, Price(599_750));
    assert!(matches!(&d.result, Some(Ok(ids)) if ids.len() == 3));

    // Filled when trades went through 5997.50; the target at 5999.50 paid on the way up.
    assert_eq!(a.trades.len(), 1);
    let t = &a.trades[0];
    assert_eq!((t.entry_qty, t.exit_qty), (1, 1));
    // 2 points × $50 = $100, less $2 a side.
    assert_eq!(t.realized, Money::whole(100));
    assert_eq!(a.summary.net, Money::whole(96));
    assert_eq!((a.summary.wins, a.summary.losses), (1, 0));
    assert!(t.mae <= Money::ZERO && t.mfe >= Money::whole(100));
    assert!(a.open_at_end.is_none());

    let json = a.to_json();
    assert!(json.contains("\"fnv1a64\""));
    assert!(
        !json.contains(&*dir.to_string_lossy()),
        "no local paths in reports"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn missing_levels_never_arm() {
    let dir = scratch("missing");
    let r = backtest::run(&backtest(&dir, false, ("09:30", "16:00"))).unwrap();
    assert!(
        r.warnings.iter().any(|w| w.contains("not recorded")),
        "{:?}",
        r.warnings
    );
    assert!(r.states.iter().all(|s| s.state != StrategyState::Armed));
    assert!(r.decisions.is_empty() && r.trades.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_core_gate_applies_in_backtests() {
    let dir = scratch("gate");
    // Allowed hours that the data never reaches: the strategy's order is denied by the core.
    let r = backtest::run(&backtest(&dir, true, ("03:00", "04:00"))).unwrap();
    let d = &r.decisions[0];
    match &d.result {
        Some(Err(e)) => {
            assert_eq!(e.code, RejectCode::Strategy);
            assert!(
                e.message.contains("outside its allowed hours"),
                "{}",
                e.message
            );
        }
        other => panic!("expected a denial, got {other:?}"),
    }
    assert!(r.trades.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn bad_parameters_refuse_the_run() {
    let dir = scratch("params");
    let mut bt = backtest(&dir, true, ("09:30", "16:00"));
    bt.params = serde_json::json!({"kind": "support", "qty": 0, "stop_ticks": 8,
        "target_ticks": 8, "max_distance_ticks": 8});
    let e = backtest::run(&bt).unwrap_err().to_string();
    assert!(e.contains("qty must be above 0"), "{e}");
    bt.params = serde_json::json!({"kind": "support"});
    assert!(backtest::run(&bt).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}
