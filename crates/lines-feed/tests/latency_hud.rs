//! The latency HUD's feed side (roadmap 6b) through the real feed driver: a replay records
//! nothing while the HUD is hidden (the default), and exchange → receive on the recording's
//! own clock while it is shown. One test (the HUD switch is process-wide).

use std::time::Duration;

use lines_core::{ContractKey, Month, Price, Qty, UtcMillis, latency};
use lines_feed::native::{self, NativeEvent};
use lines_feed::provider::Frame;
use lines_feed::recording::{Direction, FORMAT_VERSION, Header, Writer};
use lines_feed::{FeedConfig, Source, catalog, new_store, run_feed};
use tokio::sync::watch;

/// A minute of ES trades, one a second, each received 20 ms after its exchange time (every
/// tenth 120 ms).
fn recording() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("lines-feed-latency-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("es.lfr");
    let start = UtcMillis::parse_iso("2026-11-10T15:00:00Z").unwrap().0;
    let mut w = Writer::create(
        &path,
        &Header {
            start_wall_ms: start,
            provider: native::PROVIDER.into(),
            protocol: native::PROTOCOL.into(),
            product: "ES".into(),
            contract: Some(ContractKey::monthly(
                catalog::ES,
                2026,
                Month::new(12).unwrap(),
            )),
            version: FORMAT_VERSION,
        },
    )
    .unwrap();
    for s in 0..60i64 {
        let mut buf = Vec::new();
        native::encode(
            &NativeEvent::Trade {
                time_us: (start + s * 1000) * 1000,
                price: Price(600_000),
                qty: Qty(1),
                aggressor: native::Aggressor::Buy,
            },
            &mut buf,
        );
        let late = if s % 10 == 9 { 120 } else { 20 };
        w.write(
            Direction::FromServer,
            Duration::from_millis((s * 1000 + late) as u64),
            &Frame::binary(buf),
        )
        .unwrap();
    }
    w.finish().unwrap();
    path
}

async fn replay(path: &std::path::Path) -> std::sync::Arc<lines_feed::store::ChartStore> {
    let config = FeedConfig::new(Source::Replay {
        path: path.to_owned(),
        speed: 0.0,
        stop_at: None,
    });
    let store = new_store(&config).unwrap();
    let (_tx, srx) = watch::channel(false);
    tokio::time::timeout(
        Duration::from_secs(30),
        run_feed(config, store.clone(), srx),
    )
    .await
    .unwrap();
    store
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn feed_latency_is_recorded_only_while_the_hud_is_shown() {
    let path = recording();
    assert!(!latency::enabled(), "off by default");
    let hidden = replay(&path).await;
    assert!(
        hidden.read().latency.is_none(),
        "hidden: nothing recorded, nothing allocated"
    );

    latency::set_enabled(true);
    let shown = replay(&path).await;
    latency::set_enabled(false);
    let d = shown.read();
    let l = d.latency.as_ref().expect("recorded while shown");
    assert!(
        !l.wall_clock,
        "a replay's receive times are the recording's"
    );
    let s = l.exch_recv.summary(l.at_ms);
    assert_eq!(s.n, 60);
    let (p50, p99) = (s.p50_us.unwrap(), s.p99_us.unwrap());
    assert!((20_000..=25_000).contains(&p50), "{p50}");
    assert!((120_000..=150_000).contains(&p99), "{p99}");
    assert_eq!(l.offset.bound_ms(l.at_ms), Some(20));
}
