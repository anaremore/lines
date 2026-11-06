use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lines_core::UtcMillis;
use lines_core::{ContractKey, Month};
use lines_feed::catalog::{self, Target};
use lines_feed::cqg::mock::MockConfig;
use lines_feed::store::{ChartStore, FeedStatus};
use lines_feed::{FeedConfig, Source, new_store, run_feed};
use tokio::sync::watch;

fn temp_path(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("lines-feed-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

/// Run the feed for `dur` and return its store.
async fn run_for(config: FeedConfig, dur: Duration) -> Arc<ChartStore> {
    let store = new_store(&config).unwrap();
    let (tx, rx) = watch::channel(false);
    let task = tokio::spawn(run_feed(config, store.clone(), rx));
    tokio::time::sleep(dur).await;
    tx.send(true).unwrap();
    task.await.unwrap();
    store
}

/// Run the feed until it stops by itself (replay end / fatal error).
async fn run_to_end(config: FeedConfig) -> Arc<ChartStore> {
    let store = new_store(&config).unwrap();
    let (_tx, rx) = watch::channel(false);
    tokio::time::timeout(Duration::from_secs(60), run_feed(config, store.clone(), rx))
        .await
        .expect("feed did not finish");
    store
}

fn mock(cfg: MockConfig) -> FeedConfig {
    FeedConfig::new(Source::CqgMock(cfg))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cqg_mock_end_to_end_at_5k() {
    let cfg = MockConfig {
        history_bars: 600,
        ..Default::default()
    };
    let store = run_for(mock(cfg), Duration::from_secs(3)).await;
    let d = store.read();
    assert_eq!(
        d.product.symbol, "ES",
        "catalog product, not the CQG symbol"
    );
    // Following the product: the store is keyed by its front-month contract.
    assert_eq!(d.contract, lines_feed::front_month(&d.product, None));
    assert_eq!(d.provider, "cqg");
    assert!(
        d.series.len() >= 600,
        "history back-filled: {} bars",
        d.series.len()
    );
    // ~5k updates/s for 3 s minus logon/history: demand at least 60% of nominal.
    assert!(
        d.stats.updates_in >= 9_000,
        "updates_in = {}",
        d.stats.updates_in
    );
    assert!(d.stats.trades_in > 0);
    assert_eq!((d.stats.decode_errors, d.stats.rejected), (0, 0));
    assert!(d.bid.is_some() && d.ask.is_some() && d.last_trade.is_some());
    let tick = d.product.price.tick;
    for b in d.series.bars() {
        for p in [b.open, b.high, b.low, b.close] {
            assert_eq!(p.0 % tick, 0, "off-tick price in {b:?}");
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

    let started = Instant::now();
    let replay_store = run_to_end(FeedConfig::new(Source::Replay {
        path: path.clone(),
        speed: 0.0,
        stop_at: None,
    }))
    .await;
    let elapsed = started.elapsed();

    let (a, b) = (live_store.read(), replay_store.read());
    assert_eq!(b.status, FeedStatus::ReplayFinished);
    assert_eq!(
        (&b.product, b.contract),
        (&a.product, a.contract),
        "product and contract restored from the recording header"
    );
    assert!(
        a.stats.updates_in > 5_000,
        "live updates_in = {}",
        a.stats.updates_in
    );
    assert_eq!(a.stats.updates_in, b.stats.updates_in);
    assert_eq!(a.stats.frames_in, b.stats.frames_in);
    assert_eq!(
        a.series.bars(),
        b.series.bars(),
        "replayed bars differ from live bars"
    );
    let ups = b.stats.updates_in as f64 / elapsed.as_secs_f64();
    println!(
        "replay (speed 0): {} updates in {elapsed:?} = {ups:.0}/s",
        b.stats.updates_in
    );
    let _ = std::fs::remove_file(&path);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replay_stops_at_timestamp() {
    let path = temp_path("stop_at.lfr");
    let mut live = mock(MockConfig {
        history_bars: 30,
        ..Default::default()
    });
    live.record = Some(path.clone());
    run_for(live, Duration::from_millis(1_500)).await;

    let full = run_to_end(FeedConfig::new(Source::Replay {
        path: path.clone(),
        speed: 0.0,
        stop_at: None,
    }))
    .await;
    let header = lines_feed::transport::read_header(&path).unwrap();
    let cut = UtcMillis(header.start_wall_ms + 700);
    let part = run_to_end(FeedConfig::new(Source::Replay {
        path: path.clone(),
        speed: 0.0,
        stop_at: Some(cut),
    }))
    .await;
    let again = run_to_end(FeedConfig::new(Source::Replay {
        path: path.clone(),
        speed: 0.0,
        stop_at: Some(cut),
    }))
    .await;
    let (f, p, a) = (full.read(), part.read(), again.read());
    assert!(
        p.stats.frames_in < f.stats.frames_in,
        "{} vs {}",
        p.stats.frames_in,
        f.stats.frames_in
    );
    assert!(p.stats.frames_in > 0);
    assert_eq!(p.series.bars(), a.series.bars(), "same cut, same result");
    assert_eq!(p.last_trade, a.last_trade);
    let _ = std::fs::remove_file(&path);
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
async fn unmapped_or_unknown_instruments_fail_cleanly() {
    // Coinbase does not carry ES: rejected before connecting.
    let mut c = FeedConfig::new(Source::Coinbase {
        url: "wss://invalid.example".into(),
    });
    c.target = Some(Target::Product(catalog::ES));
    assert!(new_store(&c).is_err());
    // CQG maps NQ, but the mock only knows EP: server says not found -> Failed, no retry loop.
    let mut c = mock(MockConfig {
        history_bars: 10,
        ..Default::default()
    });
    c.target = Some(Target::Product(catalog::NQ));
    let store = run_to_end(c).await;
    assert!(
        matches!(&store.read().status, FeedStatus::Failed(m) if m.contains("not found") || m.contains("knows only"))
    );
}

#[tokio::test]
async fn replay_of_missing_file_fails_cleanly() {
    let cfg = FeedConfig::new(Source::Replay {
        path: temp_path("does-not-exist.lfr"),
        speed: 1.0,
        stop_at: None,
    });
    assert!(new_store(&cfg).is_err());
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_contract_is_requested_and_recorded() {
    // ES March 2027: CQG symbol EPH7 by rule; the mock resolves it as asked.
    let h27 = ContractKey::monthly(catalog::ES, 2027, Month::new(3).unwrap());
    let path = temp_path("es-h27.lfr");
    let mut c = mock(MockConfig {
        history_bars: 30,
        ..Default::default()
    });
    c.target = Some(Target::Contract(h27));
    c.record = Some(path.clone());
    let store = run_for(c, Duration::from_millis(1_200)).await;
    {
        let d = store.read();
        assert_eq!(d.contract, Some(h27));
        assert!(d.stats.trades_in > 0, "{:?}", d.status);
    }
    let h = lines_feed::transport::read_header(&path).unwrap();
    assert_eq!(
        (h.version, h.product.as_str(), h.contract),
        (3, "ES", Some(h27))
    );

    // Replaying asks for the recorded contract only.
    let replay = |target| {
        let mut c = FeedConfig::new(Source::Replay {
            path: path.clone(),
            speed: 0.0,
            stop_at: None,
        });
        c.target = target;
        c
    };
    assert!(new_store(&replay(Some(Target::Contract(h27)))).is_ok());
    assert!(new_store(&replay(Some(Target::Product(catalog::ES)))).is_ok());
    let z26 = ContractKey::monthly(catalog::ES, 2026, Month::new(12).unwrap());
    let err = new_store(&replay(Some(Target::Contract(z26))))
        .err()
        .unwrap()
        .to_string();
    assert!(err.contains("ESH27") && err.contains("ESZ26"), "{err}");
    assert!(new_store(&replay(Some(Target::Product(catalog::NQ)))).is_err());
    let _ = std::fs::remove_file(&path);
}
