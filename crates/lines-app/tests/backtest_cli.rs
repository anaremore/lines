//! `lines --backtest` (decision 0025): the public example strategy over a synthetic recording
//! and a recorded synthetic level map, through the same entry point official builds use.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use lines_app::Extensions;
use lines_core::{ContractKey, Month, Price, Qty, UtcMillis};
use lines_feed::catalog;
use lines_feed::levels::demo::{BODY, DemoLevels, ID as DEMO};
use lines_feed::levels::runtime::{PROTOCOL, PROVIDER, recording_path};
use lines_feed::levels::{LevelFrame, encode_batch};
use lines_feed::native::{self, Aggressor, NativeEvent};
use lines_feed::provider::Frame;
use lines_feed::recording::{Direction, FORMAT_VERSION, Header, Writer};

fn es() -> ContractKey {
    ContractKey::monthly(catalog::ES, 2026, Month::new(12).unwrap())
}

fn ms(s: &str) -> i64 {
    UtcMillis::parse_iso(s).unwrap().0
}

fn header(start: i64, provider: &str, protocol: String) -> Header {
    Header {
        start_wall_ms: start,
        provider: provider.into(),
        protocol,
        product: "ES".into(),
        contract: Some(es()),
        version: FORMAT_VERSION,
    }
}

/// Down a tick a second from 6003.00 through the demo map's S1 (5997.50), then back up.
fn write_inputs(dir: &Path) -> PathBuf {
    let start = ms("2026-11-05T14:31:00Z");
    let market = dir.join("market.lfr");
    let mut w = Writer::create(
        &market,
        &header(start, native::PROVIDER, native::PROTOCOL.into()),
    )
    .unwrap();
    let mut prices: Vec<i64> = (0..24).map(|i| 600_300 - 25 * i).collect();
    prices.extend((0..12).map(|i| 599_725 + 25 * i));
    let mut buf = Vec::new();
    for (i, p) in prices.iter().enumerate() {
        let t = start + i as i64 * 1000;
        buf.clear();
        native::encode(
            &NativeEvent::Trade {
                time_us: t * 1000,
                price: Price(*p),
                qty: Qty(1),
                aggressor: Aggressor::Sell,
            },
            &mut buf,
        );
        w.write(
            Direction::FromServer,
            Duration::from_millis((t - start) as u64),
            &Frame::binary(buf.clone()),
        )
        .unwrap();
    }
    w.finish().unwrap();
    let mut w = Writer::create(
        &recording_path(dir, DEMO, "ES"),
        &header(
            ms("2026-11-05T14:30:00Z"),
            PROVIDER,
            format!("{PROTOCOL}/{DEMO}"),
        ),
    )
    .unwrap();
    let mut buf = Vec::new();
    encode_batch(&[LevelFrame::new("demo/levels", BODY)], &mut buf);
    w.write(Direction::FromServer, Duration::ZERO, &Frame::binary(buf))
        .unwrap();
    w.finish().unwrap();
    market
}

#[test]
fn backtest_flag_writes_a_report() {
    let dir = std::env::temp_dir().join(format!("lines-backtest-cli-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let market = write_inputs(&dir);
    let config = dir.join("backtest.json");
    std::fs::write(
        &config,
        r#"{
          "params": {"kind": "support", "qty": 1, "stop_ticks": 8, "target_ticks": 8,
                     "max_distance_ticks": 8},
          "limits": {"max_position": 1, "max_daily_loss": 500, "max_orders_per_min": 10,
                     "hours": {"tz": "America/New_York",
                               "windows": [{"start": "09:30", "end": "16:00"}]}}
        }"#,
    )
    .unwrap();
    let report = dir.join("report.json");
    let arg = |p: &Path| p.to_string_lossy().into_owned();
    lines_app::run_with_args(
        [
            "lines".to_owned(),
            "--backtest".into(),
            "level-touch".into(),
            "--replay".into(),
            arg(&market),
            "--replay-levels".into(),
            arg(&dir),
            "--backtest-config".into(),
            arg(&config),
            "--report".into(),
            arg(&report),
        ],
        Extensions::new().with_level_source(Arc::new(DemoLevels)),
    )
    .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
    assert_eq!(json["strategy"], "level-touch");
    assert_eq!(json["contract"], "ESZ26");
    assert_eq!(json["decisions"][0]["rule"], "touch-long");
    assert_eq!(json["summary"]["trades"], 1);

    // An unknown strategy names the ones there are.
    let e = lines_app::run_with_args(
        [
            "lines",
            "--backtest",
            "nope",
            "--replay",
            &arg(&market),
            "--backtest-config",
            &arg(&config),
        ],
        Extensions::new(),
    )
    .unwrap_err()
    .to_string();
    assert!(e.contains("available: level-touch"), "{e}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn run_strategy_flag_runs_it_through_the_host_on_a_replay() {
    let dir = std::env::temp_dir().join(format!("lines-run-strategy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let market = write_inputs(&dir);
    let config = dir.join("strategy.json");
    std::fs::write(
        &config,
        r#"{
          "params": {"kind": "support", "qty": 1, "stop_ticks": 8, "target_ticks": 8,
                     "max_distance_ticks": 8},
          "limits": {"max_position": 1, "max_daily_loss": 500, "max_orders_per_min": 10,
                     "hours": {"tz": "America/New_York",
                               "windows": [{"start": "09:30", "end": "16:00"}]}}
        }"#,
    )
    .unwrap();
    let arg = |p: &Path| p.to_string_lossy().into_owned();
    lines_app::run_with_args(
        [
            "lines".to_owned(),
            "--source".into(),
            "replay".into(),
            "--replay".into(),
            arg(&market),
            "--speed".into(),
            "20".into(),
            "--replay-levels".into(),
            arg(&dir),
            "--run-strategy".into(),
            "level-touch".into(),
            "--backtest-config".into(),
            arg(&config),
            "--headless".into(),
            "4".into(),
        ],
        Extensions::new().with_level_source(Arc::new(DemoLevels)),
    )
    .unwrap();
    // A settings file without --backtest or --run-strategy is a mistake, said plainly.
    let e = lines_app::run_with_args(
        [
            "lines",
            "--source",
            "replay",
            "--replay",
            &arg(&market),
            "--backtest-config",
            &arg(&config),
            "--headless",
            "1",
        ],
        Extensions::new(),
    )
    .unwrap_err()
    .to_string();
    assert!(e.contains("goes with --backtest or --run-strategy"), "{e}");
    let _ = std::fs::remove_dir_all(&dir);
}
