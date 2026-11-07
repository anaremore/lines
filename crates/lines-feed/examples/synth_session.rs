//! Write two synthetic ES sessions as a `lines-events-1` replay (decision 0010: synthetic data
//! only), for screenshots of session-based studies such as the volume profile.
//!
//! `cargo run -p lines-feed --example synth_session -- recordings/synth-es-2026-11-05.lfr`
//!
//! Trade dates 2026-11-04 and 2026-11-05 (CST): each ETH session runs 23:00 UTC the evening
//! before to 22:00 UTC, RTH 14:30-21:00 UTC; nothing trades in the 22:00-23:00 break.
//! Overnight trades range around an overnight level; each RTH open sells off 30 points on
//! heavier volume and recovers half of it, so the ETH and RTH profiles of a day differ
//! visibly, and the two days differ for composite profiles. A fixed-seed generator makes the
//! file identical on every run.
//!
//! `-- <out> --transit`: each frame is received 2-40 ms after its trade's exchange time (now
//! and then 150-400 ms), from a second fixed-seed generator, for latency HUD screenshots
//! (roadmap 6b). The trades are the same as without it.

use std::time::Duration;

use lines_core::{ContractKey, Month, Price, Qty, UtcMillis};
use lines_feed::catalog;
use lines_feed::native::{self, Aggressor, NativeEvent};
use lines_feed::provider::Frame;
use lines_feed::recording::{Direction, FORMAT_VERSION, Header, Writer};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    /// Uniform in `0..n`.
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// One trade date: UTC times of the ETH open, RTH open/close, ETH close; overnight level.
struct Day {
    open: &'static str,
    rth_open: &'static str,
    rth_close: &'static str,
    close: &'static str,
    overnight: f64,
}

const DAYS: [Day; 2] = [
    Day {
        open: "2026-11-03T23:00:00",
        rth_open: "2026-11-04T14:30:00",
        rth_close: "2026-11-04T21:00:00",
        close: "2026-11-04T22:00:00",
        overnight: 6035.0,
    },
    Day {
        open: "2026-11-04T23:00:00",
        rth_open: "2026-11-05T14:30:00",
        rth_close: "2026-11-05T21:00:00",
        close: "2026-11-05T22:00:00",
        overnight: 6020.0,
    },
];

fn main() -> anyhow::Result<()> {
    let out = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "recordings/synth-es-2026-11-05.lfr".into());
    let transit = std::env::args().any(|a| a == "--transit");
    let mut jitter = Rng(0x7a17_5eed_0000_0042);
    let es = catalog::get(catalog::ES).expect("ES");
    let contract = ContractKey::monthly(es.id, 2026, Month::new(12).expect("month"));
    let ms = |s: &str| UtcMillis::parse_iso(s).expect("time").0;
    let tick = es.price.tick; // 25 units = 0.25
    let pts = |p: f64| (p * 100.0).round() as i64;
    let start = ms(DAYS[0].open);

    let header = Header {
        start_wall_ms: start,
        provider: native::PROVIDER.into(),
        protocol: native::PROTOCOL.into(),
        product: es.symbol.into(),
        contract: Some(contract),
        version: FORMAT_VERSION,
    };
    let mut w = Writer::create(std::path::Path::new(&out), &header)?;
    let mut rng = Rng(0x5eed_1234_abcd_0001);
    let mut price = pts(DAYS[0].overnight);
    let mut buf = Vec::new();
    let mut trades = 0u64;
    for day in &DAYS {
        let (open, rth_open, rth_close, close) = (
            ms(day.open),
            ms(day.rth_open),
            ms(day.rth_close),
            ms(day.close),
        );
        let mut t = open;
        while t < close {
            let rth = (rth_open..rth_close).contains(&t);
            // Level the walk is pulled towards: the overnight level; in RTH, down 30 points
            // over the first 90 minutes, then back up 15 over two hours.
            let target = if !rth {
                pts(day.overnight)
            } else {
                let m = (t - rth_open) as f64 / 60_000.0;
                let drop = if m < 90.0 {
                    30.0 * m / 90.0
                } else {
                    30.0 - 15.0 * ((m - 90.0) / 120.0).min(1.0)
                };
                pts(day.overnight - drop)
            };
            // Mean-reverting one-tick steps; weaker pull overnight (a wider range).
            let pull = ((target - price) / tick / if rth { 1 } else { 6 }).clamp(-8, 8);
            let r = rng.below(16) as i64 - 8; // -8..=7
            let step = if r + pull > 0 {
                tick
            } else if r + pull < -1 {
                -tick
            } else {
                0
            };
            price += step;
            let aggressor = match step.signum() {
                1 => Aggressor::Buy,
                -1 => Aggressor::Sell,
                _ if rng.below(2) == 0 => Aggressor::Buy,
                _ => Aggressor::Sell,
            };
            let qty = (rng.below(5) + 1) as i64 * if rth { 3 } else { 2 };
            buf.clear();
            native::encode(
                &NativeEvent::Trade {
                    time_us: t * 1000,
                    price: Price(price),
                    qty: Qty(qty),
                    aggressor,
                },
                &mut buf,
            );
            // Receive time: the exchange time plus the link's transit (`--transit`). Trades are
            // at least 500 ms apart, so frames stay in order.
            let late = if !transit {
                0
            } else if jitter.below(50) == 0 {
                150 + jitter.below(250) as i64
            } else {
                2 + jitter.below(39) as i64
            };
            w.write(
                Direction::FromServer,
                Duration::from_millis((t + late - start) as u64),
                &Frame::binary(buf.clone()),
            )?;
            trades += 1;
            // Overnight: a trade every 1-3 s; RTH: every 0.5-2 s.
            t += if rth {
                500 + rng.below(1_500) as i64
            } else {
                1_000 + rng.below(2_000) as i64
            };
        }
    }
    w.finish()?;
    println!(
        "wrote {out}: {trades} trades, {} .. {}",
        UtcMillis(start),
        DAYS[1].close
    );
    Ok(())
}
