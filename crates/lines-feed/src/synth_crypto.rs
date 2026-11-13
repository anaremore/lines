//! A synthetic BTC/USD (Coinbase) session as a `lines-events-1` replay (decision 0010:
//! synthetic data only), for screenshots of a public run, whose first-run workspace is
//! BTC/USD. Deterministic: a fixed-seed generator makes the file identical on every run.
//!
//! 2026-11-09, 14:00-20:00 UTC: a trade every 2 s, a random walk around 97,250.00 in cent
//! steps, with a 10-level book on each side around each trade, a cent apart (sizes a function of price and
//! level), so the DOM has something to show. Plausible, not market data.

use std::io;
use std::path::Path;
use std::time::Duration;

use lines_core::{ContractKey, Price, Qty, Side, UtcMillis};

use crate::catalog;
use crate::native::{self, Aggressor, NativeEvent};
use crate::provider::Frame;
use crate::recording::{Direction, Header, Writer};

/// The session's first and last times (UTC).
pub const OPEN: &str = "2026-11-09T14:00:00Z";
pub const CLOSE: &str = "2026-11-09T20:00:00Z";

/// Levels per side in the synthetic book.
const BOOK_LEVELS: i64 = 10;

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

/// Write the session to `out`; returns the number of trades.
pub fn write_btc_session(out: &Path) -> io::Result<u64> {
    let btc = catalog::get(catalog::BTC_USD).expect("BTC/USD");
    let contract = ContractKey::spot(btc.id);
    let ms = |s: &str| UtcMillis::parse_iso(s).expect("time").0;
    let (open, close) = (ms(OPEN), ms(CLOSE));
    let header = Header {
        start_wall_ms: open,
        provider: native::PROVIDER.into(),
        protocol: native::PROTOCOL.into(),
        product: btc.symbol.into(),
        contract: Some(contract),
        version: crate::recording::version_for(Some(contract)),
    };
    let mut w = Writer::create(out, &header)?;
    let mut rng = Rng(0xb7c0_0030_5eed_0001);
    // Cents; the tick is one cent.
    let mut price: i64 = 9_725_000;
    let lot = 1_000_000; // 0.01 BTC in 1e-8 units
    let mut buf = Vec::new();
    let mut trades = 0u64;
    let mut t = open;
    while t < close {
        buf.clear();
        let step = rng.below(1_201) as i64 - 600; // -6.00..=6.00
        price += step;
        let time_us = t * 1000;
        native::encode(&NativeEvent::BookClear { time_us }, &mut buf);
        for k in 0..BOOK_LEVELS {
            let size = |p: i64| Qty(lot * (1 + (p + k * 7).rem_euclid(40)));
            for (side, p) in [(Side::Bid, price - (k + 1)), (Side::Ask, price + k)] {
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
        native::encode(
            &NativeEvent::Trade {
                time_us,
                price: Price(price),
                qty: Qty(lot * (1 + rng.below(25) as i64)),
                aggressor: if step >= 0 {
                    Aggressor::Buy
                } else {
                    Aggressor::Sell
                },
            },
            &mut buf,
        );
        trades += 1;
        w.write(
            Direction::FromServer,
            Duration::from_millis((t - open) as u64),
            &Frame::binary(buf.clone()),
        )?;
        t += 2_000;
    }
    w.finish()?;
    Ok(trades)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_btc_session_is_deterministic() {
        let dir = lines_core::scratch::Scratch::new("synth-btc");
        let (a, b) = (dir.join("a.lfr"), dir.join("b.lfr"));
        let n = write_btc_session(&a).unwrap();
        assert_eq!(n, 6 * 3600 / 2);
        write_btc_session(&b).unwrap();
        assert_eq!(std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap());
    }
}
