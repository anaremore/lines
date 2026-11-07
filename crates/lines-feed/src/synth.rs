//! Two synthetic ES sessions as a `lines-events-1` replay (decision 0010: synthetic data only),
//! for screenshots, the UI gallery and session-based studies. Deterministic: a fixed-seed
//! generator makes the file identical on every run.
//!
//! Trade dates 2026-11-04 and 2026-11-05 (CST): each ETH session runs 23:00 UTC the evening
//! before to 22:00 UTC, RTH 14:30-21:00 UTC; nothing trades in the 22:00-23:00 break.
//! Overnight trades range around an overnight level; each RTH open sells off 30 points on
//! heavier volume and recovers half of it, so the ETH and RTH profiles of a day differ
//! visibly, and the two days differ for composite profiles.
//!
//! With `transit`, each frame is received 2-40 ms after its trade's exchange time (now and
//! then 150-400 ms), from a second fixed-seed generator, for latency HUD screenshots
//! (roadmap 6b). The trades are the same as without it.
//!
//! With `depth`, every trade's frame also carries a 10-level book on each side around the
//! trade (sizes a function of price and level, so nothing in the trade stream changes): the
//! DOM has something to show.

use std::io;
use std::path::Path;
use std::time::Duration;

use lines_core::{ContractKey, Month, Price, Qty, Side, UtcMillis};

use crate::catalog;
use crate::native::{self, Aggressor, NativeEvent};
use crate::provider::Frame;
use crate::recording::{Direction, FORMAT_VERSION, Header, Writer};

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

/// Levels per side in the synthetic book.
pub const BOOK_LEVELS: i64 = 10;

/// Write the sessions to `out`; returns the number of trades.
pub fn write_es_sessions(out: &Path, depth: bool, transit: bool) -> io::Result<u64> {
    write(out, depth, transit, None, None)
}

/// [`write_es_sessions`], recorded only from `from` on (the same trades after it): a replay
/// that starts mid-session, as a feed opened late does.
pub fn write_es_sessions_from(
    out: &Path,
    depth: bool,
    transit: bool,
    from: Option<UtcMillis>,
) -> io::Result<u64> {
    write(out, depth, transit, None, from)
}

/// Only trade date `day` (1 or 2) of the same trades, its first frame at that session's open
/// and a last quote at its close (as a live feed sends): day 1 into a local store, then day
/// 2 replayed over it, shows the stored order flow after a restart (`--store-dir`).
pub fn write_es_day(out: &Path, day: usize, depth: bool, transit: bool) -> io::Result<u64> {
    if !(1..=DAYS.len()).contains(&day) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("day {day}: 1..={}", DAYS.len()),
        ));
    }
    write(out, depth, transit, Some(day), None)
}

/// The trades of `only_day` (else every day), recorded from `from` on (else from the first
/// written day's open).
fn write(
    out: &Path,
    depth: bool,
    transit: bool,
    only_day: Option<usize>,
    from: Option<UtcMillis>,
) -> io::Result<u64> {
    let mut jitter = Rng(0x7a17_5eed_0000_0042);
    let es = catalog::get(catalog::ES).expect("ES");
    let contract = ContractKey::monthly(es.id, 2026, Month::new(12).expect("month"));
    let ms = |s: &str| UtcMillis::parse_iso(s).expect("time").0;
    let tick = es.price.tick; // 25 units = 0.25
    let pts = |p: f64| (p * 100.0).round() as i64;
    let day_open = ms(DAYS[only_day.map_or(0, |d| d - 1)].open);
    let start = from.map_or(day_open, |f| f.0.max(day_open));

    let header = Header {
        start_wall_ms: start,
        provider: native::PROVIDER.into(),
        protocol: native::PROTOCOL.into(),
        product: es.symbol.into(),
        contract: Some(contract),
        version: FORMAT_VERSION,
    };
    let mut w = Writer::create(out, &header)?;
    let mut rng = Rng(0x5eed_1234_abcd_0001);
    let mut price = pts(DAYS[0].overnight);
    let mut buf = Vec::new();
    let mut trades = 0u64;
    for (i, day) in DAYS.iter().enumerate() {
        // Every day is generated (the same random walk); only the chosen one is written.
        let write = only_day.is_none_or(|d| d == i + 1);
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
            let time_us = t * 1000;
            if depth {
                // Best ask at the trade, best bid one tick under; sizes from price and level.
                native::encode(&NativeEvent::BookClear { time_us }, &mut buf);
                for k in 0..BOOK_LEVELS {
                    let size = |p: i64| Qty(4 + ((p / tick + k * 7).rem_euclid(23)));
                    for (side, p) in [
                        (Side::Bid, price - (k + 1) * tick),
                        (Side::Ask, price + k * tick),
                    ] {
                        native::encode(
                            &NativeEvent::BookLevel {
                                time_us,
                                side,
                                price: Price(p),
                                qty: size(p),
                                orders: 1,
                            },
                            &mut buf,
                        );
                    }
                }
            }
            native::encode(
                &NativeEvent::Trade {
                    time_us,
                    price: Price(price),
                    qty: Qty(qty),
                    aggressor,
                },
                &mut buf,
            );
            // Receive time: the exchange time plus the link's transit. Trades are at least
            // 500 ms apart, so frames stay in order.
            let late = if !transit {
                0
            } else if jitter.below(50) == 0 {
                150 + jitter.below(250) as i64
            } else {
                2 + jitter.below(39) as i64
            };
            if write && t >= start {
                w.write(
                    Direction::FromServer,
                    Duration::from_millis((t + late - start) as u64),
                    &Frame::binary(buf.clone()),
                )?;
                trades += 1;
            }
            // Overnight: a trade every 1-3 s; RTH: every 0.5-2 s.
            t += if rth {
                500 + rng.below(1_500) as i64
            } else {
                1_000 + rng.below(2_000) as i64
            };
        }
        // One day: a last quote at the close, so a store built from it is complete to the
        // close (its last minute isn't cut at the last trade).
        if write && only_day.is_some() {
            buf.clear();
            native::encode(
                &NativeEvent::Bbo {
                    time_us: close * 1000,
                    bid: Some(Price(price - tick)),
                    ask: Some(Price(price)),
                },
                &mut buf,
            );
            w.write(
                Direction::FromServer,
                Duration::from_millis((close - start) as u64),
                &Frame::binary(buf.clone()),
            )?;
        }
    }
    w.finish()?;
    Ok(trades)
}

/// The last trade time of the sessions (an `--at` that shows everything).
pub const END: &str = "2026-11-05T21:59:00Z";
