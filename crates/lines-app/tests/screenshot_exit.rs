//! Screenshot mode must exit non-zero, with no file, whenever it fails, including failures
//! inside the app (on macOS, quitting the app used to end the process with status 0).

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use lines_core::{ContractKey, UtcMillis};
use lines_feed::catalog;
use lines_feed::provider::Frame;
use lines_feed::recording::{Direction, FORMAT_VERSION, Header, Writer};

fn dir() -> PathBuf {
    let d = std::env::temp_dir().join(format!("lines-screenshot-exit-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A tiny synthetic Coinbase recording (decision 0010: no real data in tests).
fn recording() -> PathBuf {
    let path = dir().join("tiny.lfr");
    let header = Header {
        start_wall_ms: UtcMillis::parse_iso("2026-11-06T14:05:30Z").unwrap().0,
        provider: "coinbase".into(),
        protocol: "coinbase-exchange-ws-1".into(),
        product: "BTC/USD".into(),
        contract: Some(ContractKey::spot(catalog::BTC_USD)),
        version: FORMAT_VERSION,
    };
    let mut w = Writer::create(&path, &header).unwrap();
    for (i, json) in [
        r#"{"type":"subscriptions","channels":[]}"#,
        r#"{"type":"match","trade_id":1,"size":"0.5","price":"75000.00","time":"2026-11-06T14:05:31Z"}"#,
    ]
    .into_iter()
    .enumerate()
    {
        w.write(
            Direction::FromServer,
            Duration::from_millis(i as u64),
            &Frame::text(json),
        )
        .unwrap();
    }
    w.finish().unwrap();
    path
}

fn shoot(replay: &PathBuf, out: &PathBuf, extra: &[&str]) -> std::process::Output {
    let _ = std::fs::remove_file(out);
    Command::new(env!("CARGO_BIN_EXE_lines"))
        .arg("--replay")
        .arg(replay)
        .arg("--screenshot")
        .arg(out)
        .args(["--size", "320x200"])
        .args(extra)
        .output()
        .unwrap()
}

#[test]
fn screenshot_failures_exit_non_zero_without_a_file() {
    let rec = recording();
    // Fails inside the app: no display runs at 7× (or, headless, there is no display at all).
    let out = dir().join("scale.png");
    let r = shoot(&rec, &out, &["--scale", "7"]);
    let stderr = String::from_utf8_lossy(&r.stderr);
    assert!(!r.status.success(), "--scale 7 exited 0: {stderr}");
    assert!(!out.exists(), "no file on failure");

    // Fails before the app: missing replay.
    let out = dir().join("missing.png");
    let r = shoot(&dir().join("does-not-exist.lfr"), &out, &[]);
    assert!(!r.status.success());
    assert!(!out.exists());
}
