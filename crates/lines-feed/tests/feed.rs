use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lines_core::UtcMillis;
use lines_feed::mock::MockConfig;
use lines_feed::store::{ChartStore, FeedStatus};
use lines_feed::{FeedConfig, Source, run_feed};
use tokio::sync::watch;

fn temp_path(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("lines-feed-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

/// Run the feed for `dur` and return its store.
async fn run_for(config: FeedConfig, dur: Duration) -> Arc<ChartStore> {
    let store = Arc::new(ChartStore::new(UtcMillis::MINUTE, config.max_bars));
    let (tx, rx) = watch::channel(false);
    let task = tokio::spawn(run_feed(config, store.clone(), rx));
    tokio::time::sleep(dur).await;
    tx.send(true).unwrap();
    task.await.unwrap();
    store
}

fn mock(cfg: MockConfig) -> FeedConfig {
    let mut c = FeedConfig::new(Source::Mock(cfg));
    c.history_ms = 600 * UtcMillis::MINUTE;
    c
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mock_feed_end_to_end_at_5k() {
    let cfg = MockConfig {
        history_bars: 600,
        ..Default::default()
    };
    let store = run_for(mock(cfg), Duration::from_secs(3)).await;
    let d = store.read();
    let inst = d.instrument.as_ref().expect("symbol resolved");
    assert_eq!(inst.contract_symbol, "EPZ6");
    assert_eq!(inst.format.tick, 25);
    assert!(
        d.series.len() >= 600,
        "history back-filled: {} bars",
        d.series.len()
    );
    // ~5k quotes/s for 3 s minus logon/history: demand at least 60% of nominal.
    assert!(
        d.stats.quotes_in >= 9_000,
        "quotes_in = {}",
        d.stats.quotes_in
    );
    assert!(d.stats.trades_in > 0);
    assert_eq!(d.stats.decode_errors, 0);
    assert!(d.bid.is_some() && d.ask.is_some() && d.last_trade.is_some());
    // Every price is on the tick grid (exact integers end to end).
    for b in d.series.bars() {
        for p in [b.open, b.high, b.low, b.close] {
            assert_eq!(p.0 % 25, 0, "off-tick price in {b:?}");
        }
        assert!(b.low <= b.open && b.low <= b.close && b.high >= b.open && b.high >= b.close);
    }
    assert!(d.series.bars().windows(2).all(|w| w[0].time < w[1].time));
    println!("mock 3s: {:?}", d.stats);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn record_then_replay_reproduces_bars_exactly() {
    let path = temp_path("record_replay.lfr");
    let mut live = mock(MockConfig {
        history_bars: 300,
        ..Default::default()
    });
    live.record = Some(path.clone());
    let live_store = run_for(live, Duration::from_millis(2_000)).await;

    let replay = FeedConfig {
        history_ms: 600 * UtcMillis::MINUTE,
        ..FeedConfig::new(Source::Replay {
            path: path.clone(),
            speed: 0.0,
        })
    };
    let started = Instant::now();
    let replay_store = run_for_until_done(replay).await;
    let elapsed = started.elapsed();

    let (a, b) = (live_store.read(), replay_store.read());
    assert_eq!(b.status, FeedStatus::ReplayFinished);
    assert!(
        a.stats.quotes_in > 5_000,
        "live quotes_in = {}",
        a.stats.quotes_in
    );
    assert_eq!(a.stats.quotes_in, b.stats.quotes_in);
    assert_eq!(a.stats.frames_in, b.stats.frames_in);
    assert_eq!(
        a.series.bars(),
        b.series.bars(),
        "replayed bars differ from live bars"
    );
    assert_eq!(a.instrument, b.instrument);
    let qps = b.stats.quotes_in as f64 / elapsed.as_secs_f64();
    println!(
        "replay (speed 0): {} quotes in {elapsed:?} = {qps:.0} quotes/s",
        b.stats.quotes_in
    );
    let _ = std::fs::remove_file(&path);
}

async fn run_for_until_done(config: FeedConfig) -> Arc<ChartStore> {
    let store = Arc::new(ChartStore::new(UtcMillis::MINUTE, config.max_bars));
    let (_tx, rx) = watch::channel(false);
    tokio::time::timeout(Duration::from_secs(60), run_feed(config, store.clone(), rx))
        .await
        .expect("replay did not finish");
    store
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconnects_and_keeps_bars() {
    let cfg = MockConfig {
        history_bars: 120,
        disconnect_after: Some(Duration::from_millis(600)),
        ..Default::default()
    };
    let store = run_for(mock(cfg), Duration::from_millis(3_500)).await;
    let d = store.read();
    assert!(
        d.stats.reconnects >= 2,
        "reconnects = {}",
        d.stats.reconnects
    );
    assert!(d.series.len() >= 120);
    assert!(
        d.series.bars().windows(2).all(|w| w[0].time < w[1].time),
        "bars sorted & unique"
    );
    assert!(d.stats.trades_in > 0);
}

#[tokio::test]
async fn replay_of_missing_file_fails_cleanly() {
    let cfg = FeedConfig::new(Source::Replay {
        path: temp_path("does-not-exist.lfr"),
        speed: 1.0,
    });
    let store = run_for_until_done(cfg).await;
    assert!(matches!(store.read().status, FeedStatus::Failed(_)));
}

#[tokio::test]
async fn ws_transport_requires_wss() {
    let err =
        lines_feed::transport::WsTransport::connect("ws://example.invalid", Duration::from_secs(1))
            .await
            .err()
            .unwrap();
    assert!(err.to_string().contains("wss://"));
}

#[test]
fn tls_config_builds_with_ring_and_os_roots() {
    lines_feed::transport::tls_config().expect("rustls config");
}
