//! Kraken end to end through the real feed path (recording -> replay transport -> session ->
//! store), offline. Fixtures are synthetic (decision 0010), built here.
//!
//! The live check is opt-in: `cargo test -p lines-feed --test kraken -- --ignored --nocapture`
//! (internet, ~75 s): 60 s of live BTC/USD.KR, zero checksum mismatches, then replay
//! determinism.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use lines_core::{ContractKey, Price, Qty, Side};
use lines_feed::catalog::{self, Target};
use lines_feed::kraken::num::book_checksum;
use lines_feed::provider::{Frame, Provider};
use lines_feed::recording::{Direction, FORMAT_VERSION, Header, Writer};
use lines_feed::store::{ChartStore, FeedStatus};
use lines_feed::{FeedConfig, Source, new_store, run_feed};
use tokio::sync::watch;

fn temp_path(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("lines-kraken-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

fn record(name: &str, frames: &[Frame]) -> PathBuf {
    let path = temp_path(name);
    let header = Header {
        start_wall_ms: 1_793_988_000_000,
        provider: "kraken".into(),
        protocol: lines_feed::kraken::KrakenProvider::default().protocol(),
        product: "BTC/USD.KR".into(),
        contract: Some(ContractKey::spot(catalog::BTC_USD_KR)),
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
    tokio::time::timeout(Duration::from_secs(60), run_feed(config, store.clone(), rx))
        .await
        .expect("replay did not finish");
    store
}

/// (price, qty) in units, best first.
type Levels<'a> = &'a [(i64, i64)];

/// A v2 book message. `book` = the full book after applying it (asks ascending, bids
/// descending, in units) for the checksum; `corrupt` sends a wrong one instead.
fn book(kind: &str, bids: &str, asks: &str, after: (Levels, Levels), corrupt: bool) -> Frame {
    let mut check = book_checksum(after.0.iter().copied(), after.1.iter().copied());
    if corrupt {
        check ^= 0xdead_beef;
    }
    Frame::text(format!(
        r#"{{"channel":"book","type":"{kind}","data":[{{"symbol":"BTC/USD","bids":[{bids}],"asks":[{asks}],"checksum":{check},"timestamp":"2026-11-06T18:00:00.000000Z"}}]}}"#
    ))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn corrupted_book_update_is_detected_and_resynced() {
    let pairs = Frame::http(
        "https://api.kraken.com/0/public/AssetPairs?pair=XBTUSD\n\
         {\"error\":[],\"result\":{\"XXBTZUSD\":{\"pair_decimals\":1,\"lot_decimals\":8,\"tick_size\":\"0.1\"}}}"
            .as_bytes()
            .to_vec(),
    );
    let ack = Frame::text(
        r#"{"method":"subscribe","result":{"channel":"book","depth":100,"snapshot":true,"symbol":"BTC/USD"},"success":true}"#,
    );
    let asks = [(1_001, 100_000_000)];
    let frames = [
        pairs,
        ack,
        // 1. snapshot: bid 100.0 x 1, ask 100.1 x 1
        book(
            "snapshot",
            r#"{"price":100.0,"qty":1.00000000}"#,
            r#"{"price":100.1,"qty":1.00000000}"#,
            (&asks, &[(1_000, 100_000_000)]),
            false,
        ),
        // 2. good update: bid size 2
        book(
            "update",
            r#"{"price":100.0,"qty":2.00000000}"#,
            "",
            (&asks, &[(1_000, 200_000_000)]),
            false,
        ),
        // 3. corrupted: the exchange's checksum doesn't match what we rebuilt
        book(
            "update",
            r#"{"price":99.9,"qty":0.50000000}"#,
            "",
            (&asks, &[(1_000, 200_000_000), (999, 50_000_000)]),
            true,
        ),
        // 4. still in flight from the old subscription: must be ignored
        book(
            "update",
            r#"{"price":99.8,"qty":7.00000000}"#,
            "",
            (&asks, &[(1_000, 200_000_000), (998, 700_000_000)]),
            false,
        ),
        // 5. the fresh snapshot after resubscribing
        book(
            "snapshot",
            r#"{"price":100.0,"qty":3.00000000},{"price":99.9,"qty":0.50000000}"#,
            r#"{"price":100.1,"qty":1.00000000}"#,
            (&asks, &[(1_000, 300_000_000), (999, 50_000_000)]),
            false,
        ),
        // 6. good update on the new book
        book(
            "update",
            "",
            r#"{"price":100.2,"qty":4.00000000}"#,
            (
                &[(1_001, 100_000_000), (1_002, 400_000_000)],
                &[(1_000, 300_000_000), (999, 50_000_000)],
            ),
            false,
        ),
    ];
    let store = replay(record("corrupt.lfr", &frames)).await;
    let d = store.read();
    assert_eq!(d.status, FeedStatus::ReplayFinished);
    assert_eq!(
        d.stats.book_checksum_mismatches, 1,
        "the corrupted update is detected"
    );
    assert_eq!(d.stats.book_resyncs, 1, "and resynced");
    assert_eq!(
        d.stats.book_checksums_ok, 4,
        "1, 2, 5 and 6 verified; 4 ignored"
    );
    let levels = |side| {
        d.book
            .levels(side)
            .map(|(p, l)| (p, l.qty))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        levels(Side::Bid),
        [
            (Price(1_000), Qty(300_000_000)),
            (Price(999), Qty(50_000_000))
        ],
        "the book is the fresh snapshot's, not the stale update's"
    );
    assert_eq!(
        levels(Side::Ask),
        [
            (Price(1_001), Qty(100_000_000)),
            (Price(1_002), Qty(400_000_000))
        ]
    );
}

#[test]
fn kraken_products_are_offered_only_on_the_kraken_source() {
    let kraken = lines_feed::availability::availability(&Source::Kraken {
        spot_url: String::new(),
        futures_url: String::new(),
    })
    .unwrap();
    let coinbase =
        lines_feed::availability::availability(&Source::Coinbase { url: String::new() }).unwrap();
    for id in [
        catalog::BTC_USD_KR,
        catalog::ETH_USD_KR,
        catalog::BTC_PERP_KR,
        catalog::ETH_PERP_KR,
    ] {
        assert!(kraken.check(&Target::Product(id)).is_ok());
        assert_eq!(
            coinbase.check(&Target::Product(id)),
            Err("Kraken market: use --source kraken".into())
        );
    }
}

/// Live: 60 s of Kraken BTC/USD spot, every book message checked against Kraken's checksum.
/// Then the recording is replayed twice at full speed: both replays must agree exactly with
/// each other and with the live run's checksum count.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs internet (~75 s)"]
async fn live_spot_book_matches_kraken_checksums_and_replays_deterministically() {
    let path = temp_path("live-btc.lfr");
    let mut config = FeedConfig::new(Source::Kraken {
        spot_url: lines_feed::kraken::SPOT_WS_URL.into(),
        futures_url: lines_feed::kraken::FUTURES_WS_URL.into(),
    });
    config.target = Some(Target::Product(catalog::BTC_USD_KR));
    config.record = Some(path.clone());
    let store = new_store(&config).unwrap();
    let (tx, rx) = watch::channel(false);
    let task = tokio::spawn(run_feed(config, store.clone(), rx));
    tokio::time::sleep(Duration::from_secs(60)).await;
    tx.send(true).unwrap();
    task.await.unwrap();

    let live = {
        let d = store.read();
        println!(
            "live: {} book messages verified, {} mismatches, {} resyncs, {} trades, {} bars, history {:?}, rejected {}, gaps {}",
            d.stats.book_checksums_ok,
            d.stats.book_checksum_mismatches,
            d.stats.book_resyncs,
            d.stats.trades_in,
            d.series.len(),
            d.history,
            d.stats.rejected,
            d.stats.gaps
        );
        assert!(
            d.stats.book_checksums_ok > 100,
            "the book was live and checked"
        );
        assert_eq!(
            d.stats.book_checksum_mismatches, 0,
            "every update matched Kraken's checksum"
        );
        assert_eq!(
            d.stats.rejected, 0,
            "every value was exact on Kraken's grid"
        );
        d.stats.book_checksums_ok
    };

    let snapshot = |s: &ChartStore| {
        let d = s.read();
        (
            d.series.bars().to_vec(),
            d.book
                .levels(Side::Bid)
                .map(|(p, l)| (p, l.qty))
                .collect::<Vec<_>>(),
            d.book
                .levels(Side::Ask)
                .map(|(p, l)| (p, l.qty))
                .collect::<Vec<_>>(),
            d.stats.book_checksums_ok,
            d.stats.book_checksum_mismatches,
            d.stats.trades_in,
        )
    };
    let a = snapshot(&*replay(path.clone()).await);
    let b = snapshot(&*replay(path.clone()).await);
    assert_eq!(a, b, "two replays of one recording are identical");
    assert_eq!(
        a.3, live,
        "replay verifies exactly the checksums the live run did"
    );
    assert_eq!(a.4, 0);
    println!(
        "replay: {} bars, {} bids, {} asks, {} checksums verified, deterministic",
        a.0.len(),
        a.1.len(),
        a.2.len(),
        a.3
    );
    let _ = std::fs::remove_file(&path);
}
