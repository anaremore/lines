//! Roadmap 4: history back-fill + live stitch. Synthetic recorded sessions (decision 0010)
//! replayed through the real provider sessions, compared against the "exchange truth": every
//! trade aggregated once. No gap, no double-counted volume.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use lines_core::{Bar, BarSeries, ContractKey, FixedStr, Price, Qty, UtcMillis};
use lines_feed::catalog;
use lines_feed::connection::{ConnectionOptions, run_connection};
use lines_feed::cqg::mock::{MockConfig, SharedMockMarket};
use lines_feed::provider::{Frame, Provider};
use lines_feed::recording::{Direction, FORMAT_VERSION, Header, Writer};
use lines_feed::store::{ChartStore, HistoryState};
use lines_feed::transport::ReplayTransport;
use lines_feed::{FeedConfig, Source, new_store, run_feed};
use tokio::sync::watch;

const REST: &str = "https://api.exchange.coinbase.com/products/BTC-USD";

fn temp_path(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("lines-history-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

fn at(hms: &str) -> UtcMillis {
    UtcMillis::parse_iso(&format!("2026-11-06T{hms}Z")).unwrap()
}

/// One exchange trade: id, time, price (cents), size (satoshi).
#[derive(Copy, Clone, Debug)]
struct T {
    id: u64,
    time: UtcMillis,
    price: i64,
    qty: i64,
}

fn btc() -> lines_core::Product {
    catalog::get(catalog::BTC_USD).unwrap()
}

fn price_text(p: i64) -> String {
    btc().price.format::<32>(Price(p)).as_str().to_owned()
}

fn qty_text(q: i64) -> String {
    let mut s = FixedStr::<40>::new();
    btc().qty.write(Qty(q), &mut s).unwrap();
    s.as_str().to_owned()
}

/// The exchange's tape: a trade every ~2.5 s from 14:00 to 14:07, plus edge cases: a trade at
/// 14:04:59.999, one exactly on the 14:05:00.000 bar boundary, and two in the same millisecond.
fn tape() -> Vec<T> {
    let mut v = Vec::new();
    let mut id = 1000;
    let mut price = 7_500_000;
    let mut t = at("14:00:00.250").0;
    while t < at("14:07:00").0 {
        id += 1;
        price += [25, -13, 7, -30, 41, -2][(id % 6) as usize];
        v.push(T {
            id,
            time: UtcMillis(t),
            price,
            qty: 1_000 + (id as i64 % 7) * 333,
        });
        t += 2_500 + (id as i64 % 5) * 97;
    }
    for (time, p) in [
        (at("14:04:59.999"), 7_500_111),
        (at("14:05:00"), 7_500_222),
        (at("14:05:00"), 7_500_223),
    ] {
        id += 1;
        v.push(T {
            id,
            time,
            price: p,
            qty: 5_000,
        });
    }
    // Exchange order: by time, then id.
    v.sort_by_key(|t| (t.time, t.id));
    for (i, t) in v.iter_mut().enumerate() {
        t.id = 1000 + i as u64;
    }
    v
}

/// Ground truth: every trade aggregated exactly once.
fn truth(trades: &[T]) -> Vec<Bar> {
    let mut s = BarSeries::new(60_000, 10_000);
    for t in trades {
        s.apply_trade(t.time, Price(t.price), Qty(t.qty));
    }
    s.bars().to_vec()
}

fn ws_match(t: &T) -> Frame {
    Frame::text(format!(
        r#"{{"type":"match","trade_id":{},"size":"{}","price":"{}","time":"{}"}}"#,
        t.id,
        qty_text(t.qty),
        price_text(t.price),
        t.time
    ))
}

/// Candles response for buckets `[start, end_excl)`, built from the tape like Coinbase does.
fn candles(tape: &[T], start: UtcMillis, end_excl: UtcMillis) -> Frame {
    let inside: Vec<T> = tape
        .iter()
        .copied()
        .filter(|t| t.time >= start && t.time < end_excl)
        .collect();
    let mut rows: Vec<String> = truth(&inside)
        .iter()
        .map(|b| {
            format!(
                "[{},{},{},{},{},{}]",
                b.time.0 / 1000,
                price_text(b.low.0),
                price_text(b.high.0),
                price_text(b.open.0),
                price_text(b.close.0),
                qty_text(b.volume.0)
            )
        })
        .collect();
    rows.reverse(); // newest first
    let url = format!(
        "{REST}/candles?granularity=60&start={start}&end={}",
        UtcMillis(end_excl.0 - 60_000)
    );
    Frame::http(format!("{url}\n[{}]", rows.join(",")))
}

/// REST trades response: the newest `n` trades up to and including `newest_id`, newest first.
fn rest_trades(tape: &[T], newest_id: u64, n: usize) -> Frame {
    let rows: Vec<String> = tape
        .iter()
        .rev()
        .filter(|t| t.id <= newest_id)
        .take(n)
        .map(|t| {
            format!(
                r#"{{"trade_id":{},"side":"buy","size":"{}","price":"{}","time":"{}"}}"#,
                t.id,
                qty_text(t.qty),
                price_text(t.price),
                t.time
            )
        })
        .collect();
    Frame::http(format!("{REST}/trades?limit=1000\n[{}]", rows.join(",")))
}

/// Write a synthetic Coinbase recording whose clock starts at `start`.
fn record(name: &str, start: UtcMillis, frames: &[Frame]) -> PathBuf {
    let path = temp_path(name);
    let header = Header {
        start_wall_ms: start.0,
        provider: "coinbase".into(),
        protocol: "coinbase-exchange-ws-1".into(),
        product: "BTC/USD".into(),
        contract: Some(ContractKey::spot(catalog::BTC_USD)),
        version: FORMAT_VERSION,
    };
    let mut w = Writer::create(&path, &header).unwrap();
    for (i, f) in frames.iter().enumerate() {
        w.write(Direction::FromServer, Duration::from_millis(i as u64), f)
            .unwrap();
    }
    w.finish().unwrap();
    path
}

async fn replay(path: PathBuf) -> Arc<ChartStore> {
    let config = FeedConfig::new(Source::Replay {
        path,
        speed: 0.0,
        stop_at: None,
    });
    let store = new_store(&config).unwrap();
    let (_tx, rx) = watch::channel(false);
    tokio::time::timeout(Duration::from_secs(30), run_feed(config, store.clone(), rx))
        .await
        .unwrap();
    store
}

fn subscribed() -> Frame {
    Frame::text(r#"{"type":"subscriptions","channels":[]}"#)
}

/// Trades with ids in `[from, to]`.
fn ids(tape: &[T], from: u64, to: u64) -> Vec<T> {
    tape.iter()
        .copied()
        .filter(|t| t.id >= from && t.id <= to)
        .collect()
}

fn id_at(tape: &[T], time: UtcMillis) -> u64 {
    tape.iter()
        .filter(|t| t.time <= time)
        .map(|t| t.id)
        .max()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn coinbase_clean_backfill_then_live() {
    let tape = tape();
    let connect = at("14:05:30");
    let rest_newest = id_at(&tape, connect);
    let mut frames = vec![
        subscribed(),
        candles(&tape, at("14:00:00"), at("14:05:00")),
        rest_trades(&tape, rest_newest, 1000),
    ];
    // Websocket overlaps the REST snapshot by three trades, then continues.
    frames.extend(ids(&tape, rest_newest - 2, u64::MAX).iter().map(ws_match));
    let store = replay(record("clean.lfr", connect, &frames)).await;
    let d = store.read();
    assert_eq!(d.history, HistoryState::Complete);
    assert_eq!(
        d.series.bars(),
        truth(&tape).as_slice(),
        "no gap, no double count"
    );
    assert_eq!(d.stats.gaps, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn coinbase_boundary_trade_counted_once() {
    let tape = tape();
    let boundary: Vec<_> = tape
        .iter()
        .filter(|t| t.time == at("14:05:00"))
        .map(|t| t.id)
        .collect();
    assert_eq!(
        boundary.len(),
        2,
        "two trades exactly on the bar boundary, same millisecond"
    );
    let connect = at("14:05:30");
    let rest_newest = id_at(&tape, connect);
    let mut frames = vec![
        subscribed(),
        candles(&tape, at("14:00:00"), at("14:05:00")),
        rest_trades(&tape, rest_newest, 1000),
    ];
    // The websocket also replays the boundary trades (overlap reaching back to 14:04:59.999).
    frames.extend(ids(&tape, boundary[0] - 1, u64::MAX).iter().map(ws_match));
    let store = replay(record("boundary.lfr", connect, &frames)).await;
    let d = store.read();
    let bars = d.series.bars();
    let want = truth(&tape);
    let b5 = bars.iter().find(|b| b.time == at("14:05:00")).unwrap();
    let w5 = want.iter().find(|b| b.time == at("14:05:00")).unwrap();
    assert_eq!(
        b5, w5,
        "forming bar opens with the boundary trade, counted once"
    );
    assert_eq!(b5.open, Price(7_500_222));
    let b4 = bars.iter().find(|b| b.time == at("14:04:00")).unwrap();
    assert_eq!(
        b4.close,
        Price(7_500_111),
        "14:04:59.999 stays in the closed candle"
    );
    assert_eq!(bars, want.as_slice());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn coinbase_history_arriving_after_live_trades() {
    let tape = tape();
    let connect = at("14:05:30");
    let rest_newest = id_at(&tape, at("14:05:45"));
    let ws_first = id_at(&tape, connect) + 1;
    // Websocket trades stream first; the candles and REST trades (snapshot at 14:05:45, so
    // overlapping the websocket) land later, then live continues.
    let mut frames = vec![subscribed()];
    frames.extend(ids(&tape, ws_first, rest_newest + 1).iter().map(ws_match));
    frames.push(candles(&tape, at("14:00:00"), at("14:05:00")));
    frames.push(rest_trades(&tape, rest_newest, 1000));
    frames.extend(ids(&tape, rest_newest + 2, u64::MAX).iter().map(ws_match));
    let store = replay(record("late.lfr", connect, &frames)).await;
    let d = store.read();
    assert_eq!(d.series.bars(), truth(&tape).as_slice());
    assert_eq!(d.history, HistoryState::Complete);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn coinbase_empty_history() {
    let tape = tape();
    let connect = at("14:05:30");
    let live: Vec<T> = tape.iter().copied().filter(|t| t.time > connect).collect();
    let mut frames = vec![
        subscribed(),
        Frame::http(format!(
            "{REST}/candles?granularity=60&start=2026-11-06T14:00:00.000Z&end=2026-11-06T14:04:00.000Z\n[]"
        )),
        Frame::http(format!("{REST}/trades?limit=1000\n[]")),
    ];
    frames.extend(live.iter().map(ws_match));
    let store = replay(record("empty.lfr", connect, &frames)).await;
    let d = store.read();
    assert_eq!(d.history, HistoryState::Complete);
    assert_eq!(
        d.series.bars(),
        truth(&live).as_slice(),
        "only what was live; nothing invented"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn coinbase_reconnect_mid_bar() {
    let tape = tape();
    let product = btc();
    let spot = ContractKey::spot(catalog::BTC_USD);
    let config = FeedConfig::new(Source::Replay {
        path: PathBuf::new(),
        speed: 0.0,
        stop_at: None,
    });
    let store = Arc::new(ChartStore::new(
        product.clone(),
        Some(spot),
        lines_feed::coinbase::CoinbaseProvider.capabilities(),
        "coinbase",
        config.bar_period_ms,
        config.max_bars,
    ));

    // Connection 1 at 14:05:30, drops at 14:05:40 (mid-bar).
    let c1 = at("14:05:30");
    let rest1 = id_at(&tape, c1);
    let mut f1 = vec![
        subscribed(),
        candles(&tape, at("14:00:00"), at("14:05:00")),
        rest_trades(&tape, rest1, 1000),
    ];
    f1.extend(
        ids(&tape, rest1 + 1, id_at(&tape, at("14:05:40")))
            .iter()
            .map(ws_match),
    );
    // Connection 2 at 14:06:10: history from the newest stored bar (14:05, partial) to the new
    // cutoff, then REST trades for the forming 14:06 bar, then live.
    let c2 = at("14:06:10");
    let rest2 = id_at(&tape, c2);
    let mut f2 = vec![
        subscribed(),
        candles(&tape, at("14:05:00"), at("14:06:00")),
        rest_trades(&tape, rest2, 1000),
    ];
    f2.extend(ids(&tape, rest2 + 1, u64::MAX).iter().map(ws_match));

    let opts = ConnectionOptions {
        idle_timeout: Duration::from_secs(60),
        ..Default::default()
    };
    let (_tx, mut rx) = watch::channel(false);
    for (name, start, frames) in [("rc1.lfr", c1, f1), ("rc2.lfr", c2, f2)] {
        let path = record(name, start, &frames);
        let mut t = ReplayTransport::open(&path, 0.0, None).unwrap();
        let mut session = lines_feed::coinbase::CoinbaseProvider
            .new_session(&product, Some(spot))
            .unwrap();
        run_connection(&mut t, session.as_mut(), &store, &opts, &mut rx)
            .await
            .unwrap();
        if name == "rc1.lfr" {
            let d = store.read();
            let missing = truth(&tape)
                .iter()
                .find(|b| b.time == at("14:05:00"))
                .copied()
                .unwrap();
            let have = *d.series.last().unwrap();
            assert!(
                have.volume < missing.volume,
                "after the drop the 14:05 bar is partial"
            );
        }
    }
    let d = store.read();
    assert_eq!(
        d.series.bars(),
        truth(&tape).as_slice(),
        "the reconnect back-fill repaired the partial bar"
    );
    assert_eq!(d.history, HistoryState::Complete);
}

/// Reconnect inside the same minute (backoff starts at 0.5 s): no candles are due, and REST
/// trades rebuild the forming bar. They must replace it, not add to the trades connection 1
/// already applied (regression: the bar's early volume was counted twice).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn coinbase_reconnect_within_the_same_minute_counts_once() {
    let tape = tape();
    let product = btc();
    let spot = ContractKey::spot(catalog::BTC_USD);
    let store = Arc::new(ChartStore::new(
        product.clone(),
        Some(spot),
        lines_feed::coinbase::CoinbaseProvider.capabilities(),
        "coinbase",
        60_000,
        10_000,
    ));
    let c1 = at("14:05:10");
    let rest1 = id_at(&tape, c1);
    let mut f1 = vec![
        subscribed(),
        candles(&tape, at("14:00:00"), at("14:05:00")),
        rest_trades(&tape, rest1, 1000),
    ];
    f1.extend(
        ids(&tape, rest1 + 1, id_at(&tape, at("14:05:30")))
            .iter()
            .map(ws_match),
    );
    // Back at 14:05:40, still in the 14:05 bar.
    let c2 = at("14:05:40");
    let rest2 = id_at(&tape, c2);
    let mut f2 = vec![subscribed(), rest_trades(&tape, rest2, 1000)];
    f2.extend(ids(&tape, rest2 + 1, u64::MAX).iter().map(ws_match));

    let opts = ConnectionOptions {
        idle_timeout: Duration::from_secs(60),
        ..Default::default()
    };
    let (_tx, mut rx) = watch::channel(false);
    for (name, start, frames) in [("same1.lfr", c1, f1), ("same2.lfr", c2, f2)] {
        let path = record(name, start, &frames);
        let mut t = ReplayTransport::open(&path, 0.0, None).unwrap();
        let mut session = lines_feed::coinbase::CoinbaseProvider
            .new_session(&product, Some(spot))
            .unwrap();
        run_connection(&mut t, session.as_mut(), &store, &opts, &mut rx)
            .await
            .unwrap();
    }
    let d = store.read();
    assert_eq!(d.series.bars(), truth(&tape).as_slice(), "counted once");
    assert_eq!(d.history, HistoryState::Complete);
}

fn last_match(t: &T) -> Frame {
    Frame::text(
        ws_match(t)
            .data
            .iter()
            .map(|&b| b as char)
            .collect::<String>()
            .replace(r#""type":"match""#, r#""type":"last_match""#),
    )
}

/// A hole-fill response: trades older than `after`, as the REST endpoint saw them when its
/// newest trade was `rest_newest`.
fn fill(tape: &[T], after: u64, rest_newest: u64) -> Frame {
    let f = rest_trades(tape, rest_newest.min(after - 1), 1000);
    let body = &f.data[f.data.iter().position(|&b| b == b'\n').unwrap() + 1..];
    let mut data = format!("{REST}/trades?limit=1000&after={after}\n").into_bytes();
    data.extend_from_slice(body);
    Frame::http(data)
}

/// Seen live: the REST trades snapshot lags the websocket by seconds, so trades between the
/// REST newest and `last_match` came from neither. They are fetched by id; the first attempt
/// still sees stale REST, the retry (keepalive tick) gets them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn coinbase_stale_rest_hole_is_filled() {
    let tape = tape();
    let connect = at("14:05:30");
    let boundary = id_at(&tape, connect);
    let stale = boundary - 4;
    let first_ws = ids(&tape, boundary + 1, boundary + 1);
    let mut frames = vec![
        subscribed(),
        last_match(&ids(&tape, boundary, boundary)[0]),
        candles(&tape, at("14:00:00"), at("14:05:00")),
        rest_trades(&tape, stale, 1000),
        ws_match(&first_ws[0]),
        fill(&tape, boundary + 1, stale), // REST still stale
        fill(&tape, boundary + 1, boundary + 3),
    ];
    frames.extend(ids(&tape, boundary + 2, u64::MAX).iter().map(ws_match));
    let store = replay(record("stale.lfr", connect, &frames)).await;
    let d = store.read();
    assert_eq!(d.history, HistoryState::Complete);
    assert_eq!(d.series.bars(), truth(&tape).as_slice(), "hole filled once");
}

/// A hole that REST never fills is reported as missing history, never invented.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn coinbase_unfillable_hole_is_reported() {
    let tape = tape();
    let connect = at("14:05:30");
    let boundary = id_at(&tape, connect);
    let stale = boundary - 4;
    let mut frames = vec![
        subscribed(),
        last_match(&ids(&tape, boundary, boundary)[0]),
        candles(&tape, at("14:00:00"), at("14:05:00")),
        rest_trades(&tape, stale, 1000),
    ];
    frames.extend((0..6).map(|_| fill(&tape, boundary + 1, stale)));
    frames.extend(ids(&tape, boundary + 1, u64::MAX).iter().map(ws_match));
    let store = replay(record("unfillable.lfr", connect, &frames)).await;
    let d = store.read();
    assert!(
        matches!(d.history, HistoryState::Incomplete { .. }),
        "{:?}",
        d.history
    );
    let lost: Vec<T> = ids(&tape, stale + 1, boundary);
    let kept: Vec<T> = tape
        .iter()
        .copied()
        .filter(|t| t.id <= stale || t.id > boundary)
        .collect();
    assert_eq!(lost.len(), 4);
    assert_eq!(d.series.bars(), truth(&kept).as_slice(), "nothing invented");
}

/// CQG protocol path end to end: the mock exchange's own bars are the truth. Forced
/// disconnects mid-bar lose in-flight frames; each reconnect requests history from the newest
/// stored bar and must repair it exactly.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cqg_mock_backfill_and_reconnects_match_the_exchange_exactly() {
    for (disconnect, label) in [
        (None, "no reconnect"),
        (Some(Duration::from_millis(900)), "reconnects"),
    ] {
        let cfg = MockConfig {
            history_bars: 30,
            quotes_per_sec: 3_000,
            disconnect_after: disconnect,
            stop_trading_after: Some(Duration::from_millis(2_500)),
            ..Default::default()
        };
        let market = SharedMockMarket::new(&cfg, lines_feed::now_utc());
        let cfg = MockConfig {
            market: Some(market.clone()),
            ..cfg
        };
        let config = FeedConfig::new(Source::CqgMock(cfg));
        let store = new_store(&config).unwrap();
        let (tx, rx) = watch::channel(false);
        let task = tokio::spawn(run_feed(config, store.clone(), rx));
        // Trading stops at 2.5 s; give bar updates / reconnect back-fills time to converge.
        tokio::time::sleep(Duration::from_millis(5_000)).await;
        tx.send(true).unwrap();
        task.await.unwrap();
        let d = store.read();
        assert!(d.stats.trades_in > 1_000, "{label}: {:?}", d.stats);
        if disconnect.is_some() {
            assert!(d.stats.reconnects >= 2, "{label}: {:?}", d.stats);
        }
        assert_eq!(
            d.series.bars(),
            market.bars().as_slice(),
            "{label}: client bars differ from the exchange's"
        );
    }
}

/// Live Coinbase: the bar forming at connect (stitched from candles, REST trades and the
/// websocket) and the next, fully live bar must equal Coinbase's own candles for those minutes.
/// Takes ~2.5 minutes. Run with `cargo test -p lines-feed --test history -- --ignored`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs internet; ~2.5 minutes"]
async fn coinbase_live_stitch_matches_exchange_candles() {
    let path = temp_path("coinbase-live.lfr");
    let mut config = FeedConfig::new(Source::Coinbase {
        url: lines_feed::coinbase::WS_URL.into(),
    });
    config.record = Some(path.clone());
    config.history_ms = 60 * 60_000;
    let start = lines_feed::now_utc();
    let m0 = start.floor_to(60_000);
    let m1 = UtcMillis(m0.0 + 60_000);
    let store = new_store(&config).unwrap();
    let (tx, rx) = watch::channel(false);
    let task = tokio::spawn(run_feed(config, store.clone(), rx));
    // Run until m1 has closed, plus a little for in-flight trades.
    let until = m1.0 + 60_000 + 3_000;
    tokio::time::sleep(Duration::from_millis(
        (until - lines_feed::now_utc().0).max(0) as u64,
    ))
    .await;
    tx.send(true).unwrap();
    task.await.unwrap();
    let (ours, history) = {
        let d = store.read();
        println!(
            "live: {} bars, {:?}, history {:?}",
            d.series.len(),
            d.stats,
            d.history
        );
        // Roadmap 5: public level2_batch depth and aggressor-tagged prints arrived too.
        assert!(
            d.book.len(lines_core::Side::Bid) > 100 && d.book.len(lines_core::Side::Ask) > 100,
            "level2_batch book"
        );
        assert_eq!(
            d.stats.book_crossed, 0,
            "the book is never crossed at a frame boundary"
        );
        assert!(d.tape.iter_newest().take(50).all(|p| p.aggressor.is_some()));
        (d.series.bars().to_vec(), d.history)
    };
    assert_eq!(history, HistoryState::Complete);
    assert!(ours.len() >= 60, "an hour of history arrived");

    // Coinbase's candles for m0 and m1 (allow time for them to settle).
    let product = btc();
    let mut theirs = Vec::new();
    for _ in 0..6 {
        tokio::time::sleep(Duration::from_secs(10)).await;
        let url = format!("{REST}/candles?granularity=60&start={m0}&end={m1}");
        let body = ureq::get(&url)
            .header("User-Agent", "lines-test")
            .call()
            .unwrap()
            .body_mut()
            .read_to_string()
            .unwrap();
        let rows: Vec<Vec<serde_json::value::Value>> = serde_json::from_str(&body).unwrap();
        theirs = rows
            .iter()
            .map(|r| {
                let p = |i: usize| {
                    Price(
                        product
                            .price
                            .scale
                            .parse_decimal(&r[i].to_string())
                            .unwrap(),
                    )
                };
                Bar {
                    time: UtcMillis(r[0].as_i64().unwrap() * 1000),
                    low: p(1),
                    high: p(2),
                    open: p(3),
                    close: p(4),
                    volume: product.qty.parse_decimal(&r[5].to_string()).unwrap(),
                }
            })
            .collect();
        if theirs.len() == 2 {
            break;
        }
    }
    for m in [m0, m1] {
        let a = ours.iter().find(|b| b.time == m);
        let b = theirs.iter().find(|b| b.time == m);
        assert_eq!(a, b, "bar {m}: ours vs Coinbase's candle");
    }
    // The recording replays to the same bars.
    let replayed = replay(path.clone()).await;
    assert_eq!(replayed.read().series.bars(), ours.as_slice());
    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------------------
// Roadmap 5a: the local market-data store (decision 0019).

mod store {
    use super::*;
    use lines_core::{CivilDate, Price, Qty};
    use lines_feed::connection::EventSink;
    use lines_feed::disk::retention;
    use lines_feed::disk::segment::{
        End, FlushPolicy, SegmentHeader, SegmentReader, SegmentWriter,
    };
    use lines_feed::disk::{ContractDir, ContractWriter, StoreConfig};
    use lines_feed::provider::MarketEvent;

    fn root(name: &str) -> PathBuf {
        let d = temp_path(&format!("store-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn store_cfg(root: &std::path::Path) -> StoreConfig {
        StoreConfig::new(root.to_path_buf(), "test")
    }

    /// Run a recording through the full feed (`run_feed`) with the store on, as the app
    /// would after a launch; returns the chart store.
    async fn run_with_store(path: PathBuf, root: &std::path::Path) -> Arc<ChartStore> {
        let mut config = FeedConfig::new(Source::Replay {
            path,
            speed: 0.0,
            stop_at: None,
        });
        config.store = Some(store_cfg(root));
        let store = new_store(&config).unwrap();
        let (_tx, rx) = watch::channel(false);
        tokio::time::timeout(Duration::from_secs(30), run_feed(config, store.clone(), rx))
            .await
            .unwrap();
        store
    }

    fn spot() -> Option<ContractKey> {
        Some(ContractKey::spot(catalog::BTC_USD))
    }

    fn btc_dir(root: &std::path::Path) -> ContractDir {
        ContractDir::new(root, "test", &btc(), spot())
    }

    fn btc_store() -> Arc<ChartStore> {
        Arc::new(ChartStore::new(
            btc(),
            spot(),
            lines_feed::coinbase::CoinbaseProvider.capabilities(),
            "coinbase",
            60_000,
            1000,
        ))
    }

    /// Two launches inside the same minute (the reconnect regression, now across a restart):
    /// the second launch shows the stored bars at once, then the feed fills from where they
    /// end and the stitch merges it: no double count. The stored trades replay to the same
    /// bars as live wherever the store claims them complete.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cached_startup_then_stitch_counts_once_and_stored_session_replays() {
        let tape = tape();
        let root = root("cached");
        let c1 = at("14:05:10");
        let rest1 = id_at(&tape, c1);
        let mut f1 = vec![
            subscribed(),
            candles(&tape, at("14:00:00"), at("14:05:00")),
            rest_trades(&tape, rest1, 1000),
        ];
        f1.extend(
            ids(&tape, rest1 + 1, id_at(&tape, at("14:05:30")))
                .iter()
                .map(ws_match),
        );
        let first = run_with_store(record("st1.lfr", c1, &f1), &root).await;
        let first_bars = first.read().series.bars().to_vec();
        assert!(!first_bars.is_empty());

        // Launch 2: the cache alone, before any connection.
        let (w, opened) =
            ContractWriter::open(&store_cfg(&root), &btc(), spot(), btc_store()).unwrap();
        assert_eq!(
            opened.bars, first_bars,
            "cached bars are what launch 1 showed"
        );
        assert!(!opened.rebuilt);
        drop(w);

        let c2 = at("14:05:40");
        let rest2 = id_at(&tape, c2);
        let mut f2 = vec![subscribed(), rest_trades(&tape, rest2, 1000)];
        f2.extend(ids(&tape, rest2 + 1, u64::MAX).iter().map(ws_match));
        let second = run_with_store(record("st2.lfr", c2, &f2), &root).await;
        {
            let d = second.read();
            assert_eq!(d.series.bars(), truth(&tape).as_slice(), "counted once");
            assert_eq!(d.history, HistoryState::Complete);
        }

        // Determinism: replay the stored contract; every minute the store claims complete
        // for trades has the same bar as live.
        let dir = btc_dir(&root);
        let covered = dir.trade_coverage();
        assert!(!covered.is_empty());
        let replayed = replay(dir.path.clone()).await;
        let replayed = replayed.read().series.bars().to_vec();
        let mut compared = 0;
        for b in truth(&tape) {
            // The store's claim: every trade inside the covered ranges is stored. So a minute
            // whose trades all fall inside them must replay to the live bar.
            let mut minute = tape
                .iter()
                .filter(|t| t.time.floor_to(60_000) == b.time)
                .map(|t| t.time.0);
            if minute.all(|t| covered.contains(t)) {
                let r = replayed.iter().find(|r| r.time == b.time);
                assert_eq!(r, Some(&b), "minute {}", b.time);
                compared += 1;
            }
        }
        assert!(
            compared >= 1,
            "at least one fully stored minute: {covered:?}"
        );
    }

    #[test]
    fn torn_tail_is_truncated_on_open_and_claims_stop_at_the_last_block() {
        let root = root("torn");
        let (mut w, _) =
            ContractWriter::open(&store_cfg(&root), &btc(), spot(), btc_store()).unwrap();
        let t0 = at("14:00:00");
        w.begin(None, t0);
        w.frame(&[MarketEvent::HistoryComplete], t0);
        for i in 0..100 {
            let t = UtcMillis(t0.0 + 1 + i * 100);
            w.frame(
                &[MarketEvent::Trade {
                    time: t,
                    price: Price(7_500_000),
                    qty: Qty(1),
                    aggressor: None,
                }],
                t,
            );
        }
        w.end(UtcMillis(t0.0 + 9_901));
        drop(w);
        let dir = btc_dir(&root);
        assert!(dir.trade_coverage().contains(t0.0 + 9_900));

        // A crash mid-append: half a block at the end.
        let seg = dir.segment_path(CivilDate::from_utc(t0));
        let mut f = std::fs::OpenOptions::new().append(true).open(&seg).unwrap();
        std::io::Write::write_all(&mut f, &[99, 0, 0, 0, 7, 7]).unwrap();
        drop(f);
        let (w, opened) =
            ContractWriter::open(&store_cfg(&root), &btc(), spot(), btc_store()).unwrap();
        assert_eq!(opened.truncated_bytes, 6);
        drop(w);
        let mut r = SegmentReader::open(&seg).unwrap();
        let mut n = 0;
        while r.next_block().unwrap().is_ok() {
            n += 1;
        }
        assert_eq!(n, 100, "every complete block survives");
        let cov = dir.trade_coverage();
        assert!(cov.contains(t0.0 + 9_901), "{cov:?}");
        assert!(
            !cov.contains(t0.0 + 9_902),
            "nothing claimed past the last stored trade"
        );
    }

    #[test]
    fn readers_run_alongside_the_writer() {
        let root = root("concurrent");
        let path = root.join("d.lseg");
        let header = SegmentHeader {
            created_wall_ms: 0,
            source: "test".into(),
            encoding: "lines-events-1".into(),
            product: "ES".into(),
            contract: None,
            date: CivilDate::new(2026, 11, 6).unwrap(),
        };
        let policy = FlushPolicy {
            max_buffer: 1000, // small: many partial writes for the reader to meet
            write_every: Duration::from_millis(1),
            sync_every: Duration::from_millis(5),
        };
        let (mut w, _) = SegmentWriter::open(&path, &header, policy).unwrap();
        const N: i64 = 20_000;
        let writer = std::thread::spawn(move || {
            for i in 0..N {
                w.append(i, &i.to_le_bytes().repeat(1 + (i % 7) as usize))
                    .unwrap();
                if i % 50 == 0 {
                    w.tick().unwrap();
                }
            }
            w.close().unwrap();
        });
        let mut r = SegmentReader::open(&path).unwrap();
        let mut next = 0i64;
        let start = std::time::Instant::now();
        while next < N {
            match r.next_block().unwrap() {
                Ok(b) => {
                    assert_eq!(b.time_us, next, "in order, none skipped, none torn");
                    assert_eq!(&b.events[..8], &next.to_le_bytes());
                    next += 1;
                }
                Err(End::Eof | End::Partial) => std::thread::yield_now(),
                Err(End::Corrupt) => panic!("a reader saw a corrupt block at {next}"),
            }
            assert!(start.elapsed() < Duration::from_secs(30), "stuck at {next}");
        }
        writer.join().unwrap();
        assert_eq!(r.next_block().unwrap(), Err(End::Eof));
    }

    #[test]
    fn gaps_are_tracked_across_restarts_and_refetched() {
        let root = root("gaps");
        let es = catalog::get(catalog::ES).unwrap();
        let k = ContractKey::monthly(es.id, 2026, lines_core::Month::new(12).unwrap());
        let open = || {
            let store = Arc::new(ChartStore::new(
                es.clone(),
                Some(k),
                lines_feed::cqg::CqgProvider::new(Default::default()).capabilities(),
                "cqg",
                60_000,
                1000,
            ));
            ContractWriter::open(&store_cfg(&root), &es, Some(k), store)
                .unwrap()
                .0
        };
        let (t0, t1, t2) = (at("10:00:00"), at("11:00:00"), at("12:00:00"));
        // Launch 1: connected 10:00..11:00, history complete from 10:00, and the provider
        // reports one minute at 10:30 it couldn't deliver.
        let mut w = open();
        assert_eq!(w.begin(None, t0), None);
        w.frame(
            &[
                MarketEvent::History {
                    bars: vec![],
                    as_of: Some(t0),
                },
                MarketEvent::HistoryGap {
                    from: at("10:30:00"),
                    to: at("10:30:59.999"),
                },
                MarketEvent::HistoryComplete,
            ],
            t0,
        );
        w.frame(&[], t1);
        w.end(t1);
        drop(w);

        // Launch 2 with bars up to 11:00 and a window from 10:00: history is requested from
        // the first hole (10:30), so the provider fills it if it can.
        let mut w = open();
        let cov = w.coverage().bars.clone();
        assert!(cov.contains(at("10:10:00").0) && cov.contains(at("10:59:00").0));
        assert!(
            !cov.contains(at("10:30:30").0),
            "the reported hole stays missing"
        );
        assert_eq!(w.begin(Some(t1), t0), Some(at("10:30:00")));
        // A window that starts after the hole: from the newest bar.
        assert_eq!(w.begin(Some(t1), at("10:45:00")), Some(t1));
        // A connection whose history never completes claims nothing.
        w.frame(&[], t2);
        w.end(t2);
        drop(w);
        let cov = open().coverage().bars.clone();
        assert!(!cov.contains(at("11:30:00").0), "{cov:?}");
    }

    #[test]
    fn retention_evicts_oldest_dates_first() {
        let root = root("retention");
        let base = root.join("store-v1").join("test");
        let write = |contract: &str, date: &str| {
            let p = base.join(contract).join(format!("{date}.lseg"));
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, vec![0u8; 100]).unwrap();
            p
        };
        let a1 = write("ES_2026-12", "2026-11-01");
        let b1 = write("NQ_2026-12", "2026-11-02");
        let a2 = write("ES_2026-12", "2026-11-03");
        let b2 = write("NQ_2026-12", "2026-11-04");
        let ev = retention::enforce(&root, 250);
        assert_eq!(
            ev.files,
            vec![a1, b1],
            "oldest dates first, across contracts"
        );
        assert_eq!((ev.bytes, ev.kept), (200, 200));
        assert!(a2.exists() && b2.exists());
        assert!(
            retention::enforce(&root, 250).files.is_empty(),
            "within budget"
        );
    }

    /// docs/design.md §7: a live feed that goes quiet is stale until data resumes; a finished
    /// replay is not.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_pausing_feed_shows_stale_until_data_resumes() {
        use lines_feed::native::{self, NativeEvent};
        let path = temp_path("pause.lfr");
        let nq = catalog::get(catalog::NQ).unwrap();
        let header = Header {
            start_wall_ms: at("14:00:00").0,
            provider: native::PROVIDER.into(),
            protocol: native::PROTOCOL.into(),
            product: "NQ".into(),
            contract: Some(ContractKey::monthly(
                nq.id,
                2026,
                lines_core::Month::new(12).unwrap(),
            )),
            version: FORMAT_VERSION,
        };
        let mut w = Writer::create(&path, &header).unwrap();
        let times: Vec<u64> = (0..300)
            .step_by(20)
            .chain((1500..1800).step_by(20))
            .collect();
        for ms in times {
            let mut buf = Vec::new();
            native::encode(
                &NativeEvent::Trade {
                    time_us: (at("14:00:00").0 + ms as i64) * 1000,
                    price: Price(2_500_000),
                    qty: Qty(1),
                    aggressor: native::Aggressor::Buy,
                },
                &mut buf,
            );
            w.write(
                Direction::FromServer,
                Duration::from_millis(ms),
                &Frame::binary(buf),
            )
            .unwrap();
        }
        w.finish().unwrap();

        let mut config = FeedConfig::new(Source::Replay {
            path,
            speed: 1.0,
            stop_at: None,
        });
        config.stale_after = Some(Duration::from_millis(400));
        let store = new_store(&config).unwrap();
        let (_tx, rx) = watch::channel(false);
        let task = tokio::spawn(run_feed(config, store.clone(), rx));
        let (mut seen_live, mut stale_in_pause, mut stale_late) = (false, false, false);
        let t0 = std::time::Instant::now();
        while !task.is_finished() {
            let ms = t0.elapsed().as_millis();
            let stale = store.stale_for().is_some();
            if ms < 250 {
                seen_live |= store.read().status == lines_feed::store::FeedStatus::Live;
                assert!(!stale, "ticking: not stale ({ms} ms)");
            }
            if (950..1350).contains(&ms) {
                stale_in_pause |= stale;
            }
            if (1650..1750).contains(&ms) {
                stale_late |= stale;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        task.await.unwrap();
        assert!(seen_live);
        assert!(stale_in_pause, "the pause shows as stale");
        assert!(!stale_late, "data resumed: not stale");
        assert_eq!(store.stale_for(), None, "a finished replay is not stale");
    }
}
