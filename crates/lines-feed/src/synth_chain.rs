//! A synthetic SPY session with its option chain, as a `lines-events-1` replay (decision 0010:
//! synthetic data only; decision 0028), for the chain pane's screenshots and tests.
//! Deterministic: a fixed-seed generator makes the file identical on every run.
//!
//! Trade date 2026-11-09 (Monday, EST): SPY trades every 2 s of the regular session,
//! 14:30-21:00 UTC (09:30-16:00 ET), a random walk around 684.00. Every series of six expiries
//! (the same day, the next two days, Friday's weekly, the November monthly and the December
//! quarterly), strikes 664 to 704 every dollar, calls and puts, is quoted from the open on: the
//! same day every 8 s and the rest every 40 s, inside the age limits by expiry
//! (`lines_trading::risk`), so the session can trade all day.
//!
//! Option prices are Black-Scholes values (r = 4%) on a volatility smile, rounded to the
//! penny: bid and ask a few cents around the value, a series worth under a cent quoted 0.00 /
//! 0.01 with no bid. These numbers are plausible, not market data, and only the generator
//! uses floating point: what it writes is exact (prices in cents). The file is identical on
//! every run on one platform; `exp`/`ln` may differ in the last bit between platforms, so a
//! value on a rounding boundary could differ across them.

use std::io;
use std::path::Path;
use std::time::Duration;

use lines_core::{CivilDate, ContractKey, OptionKey, OptionQuote, Price, Qty, Right, UtcMillis};

use crate::catalog;
use crate::native::{self, Aggressor, NativeEvent};
use crate::provider::Frame;
use crate::recording::{Direction, Header, Writer};

/// The session's trade date and regular hours (UTC).
pub const DATE: &str = "2026-11-09";
pub const OPEN: &str = "2026-11-09T14:30:00Z";
pub const CLOSE: &str = "2026-11-09T21:00:00Z";
/// Expiries in the file, soonest first.
pub const EXPIRIES: [(u16, u8, u8); 6] = [
    (2026, 11, 9),
    (2026, 11, 10),
    (2026, 11, 11),
    (2026, 11, 13),
    (2026, 11, 20),
    (2026, 12, 18),
];
/// Strikes, whole dollars.
pub const STRIKES: std::ops::RangeInclusive<i64> = 664..=704;
/// The same day's series are quoted this often (ms).
pub const SAME_DAY_EVERY_MS: i64 = 8_000;
/// Later expiries' series are quoted this often (ms).
pub const LATER_EVERY_MS: i64 = 40_000;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Standard normal CDF (Abramowitz-Stegun 7.1.26, |error| < 1.5e-7).
fn norm_cdf(x: f64) -> f64 {
    let t = 1.0 / (1.0 + 0.327_591_1 * x.abs() / std::f64::consts::SQRT_2);
    let poly = t
        * (0.254_829_592
            + t * (-0.284_496_736
                + t * (1.421_413_741 + t * (-1.453_152_027 + t * 1.061_405_429))));
    let erf = 1.0 - poly * (-(x * x) / 2.0).exp();
    if x >= 0.0 {
        0.5 * (1.0 + erf)
    } else {
        0.5 * (1.0 - erf)
    }
}

/// Black-Scholes value and delta of one option (spot and strike in dollars, `t` in years).
fn black_scholes(spot: f64, strike: f64, t: f64, iv: f64, right: Right) -> (f64, f64) {
    let r = 0.04;
    let sd = iv * t.sqrt();
    let d1 = ((spot / strike).ln() + (r + iv * iv / 2.0) * t) / sd;
    let d2 = d1 - sd;
    let disc = (-r * t).exp();
    match right {
        Right::Call => (
            spot * norm_cdf(d1) - strike * disc * norm_cdf(d2),
            norm_cdf(d1),
        ),
        Right::Put => (
            strike * disc * norm_cdf(-d2) - spot * norm_cdf(-d1),
            norm_cdf(d1) - 1.0,
        ),
    }
}

/// A smile: higher for lower strikes (put skew), rising with distance from the money.
fn smile(spot: f64, strike: f64) -> f64 {
    let m = (strike / spot).ln();
    (0.115 - 0.9 * m + 6.0 * m * m).clamp(0.05, 0.6)
}

/// One series' quote at `now` with the underlying at `spot` (cents).
fn quote(key: OptionKey, spot: i64, now: i64, close_ms: i64) -> OptionQuote {
    let expiry_close = UtcMillis::parse_iso(&format!(
        "{:04}-{:02}-{:02}T21:00:00Z",
        key.expiry.year, key.expiry.month, key.expiry.day
    ))
    .expect("time")
    .0;
    // At least 10 minutes left, so a 0DTE quote near the close stays finite.
    let t = ((expiry_close - now).max(600_000) as f64) / (365.0 * 86_400_000.0);
    let (s, k) = (spot as f64 / 100.0, key.strike.0 as f64 / 100.0);
    let iv = smile(s, k);
    let (value, delta) = black_scholes(s, k, t, iv, key.right);
    let cents = (value * 100.0).round() as i64;
    let half_spread = 1 + cents / 150;
    // No bid when the bid would be zero: one form for "no bid".
    let bid = (cents - half_spread >= 1).then(|| Price(cents - half_spread));
    let ask = Some(Price((cents + half_spread).max(1)));
    // Volume grows through the day, most near the open's money (so it never falls); open
    // interest is fixed per series.
    let dist = ((k - 684.0).abs() as i64).max(1);
    let elapsed = ((now - (close_ms - 390 * 60_000)) / 60_000).max(1);
    // Puts trade a little less and carry other open interest, so the two sides differ.
    let (vol_k, oi_k) = match key.right {
        Right::Call => (200, 40_000),
        Right::Put => (150, 31_000),
    };
    let volume = elapsed * vol_k / dist;
    let open_interest = oi_k / dist + (key.strike.0 / 100 % 7) * 900;
    OptionQuote {
        bid,
        ask,
        last: (cents >= 1).then_some(Price(cents)),
        volume: Qty(volume),
        open_interest,
        iv_bp: Some((iv * 10_000.0).round() as i32),
        delta_e4: Some((delta * 10_000.0).round() as i32),
        time: UtcMillis(now),
    }
}

/// Write the session to `out`; returns (trades, option quotes) written.
pub fn write_spy_chain(out: &Path) -> io::Result<(u64, u64)> {
    let spy = catalog::get(catalog::SPY).expect("SPY");
    let ms = |s: &str| UtcMillis::parse_iso(s).expect("time").0;
    let (open, close) = (ms(OPEN), ms(CLOSE));
    let header = Header {
        start_wall_ms: open,
        provider: native::PROVIDER.into(),
        protocol: native::PROTOCOL.into(),
        product: spy.symbol.into(),
        contract: Some(ContractKey::equity(spy.id)),
        version: crate::recording::version_for(Some(ContractKey::equity(spy.id))),
    };
    let mut w = Writer::create(out, &header)?;
    let mut rng = Rng(0x5e7d_0028_c4a1_0001);
    let mut spot: i64 = 68_400;
    let (mut trades, mut quotes) = (0u64, 0u64);
    let mut buf = Vec::new();
    let (mut next_same_day, mut next_later) = (open, open);
    let mut t = open;
    while t < close {
        buf.clear();
        // The underlying: a step of -3..=3 cents, 1-500 shares.
        spot += rng.below(7) as i64 - 3;
        let qty = 1 + rng.below(500) as i64;
        native::encode(
            &NativeEvent::Trade {
                time_us: t * 1000,
                price: Price(spot),
                qty: Qty(qty),
                aggressor: if rng.below(2) == 0 {
                    Aggressor::Buy
                } else {
                    Aggressor::Sell
                },
            },
            &mut buf,
        );
        trades += 1;
        let (same_day, later) = (t >= next_same_day, t >= next_later);
        if same_day {
            next_same_day += SAME_DAY_EVERY_MS;
        }
        if later {
            next_later += LATER_EVERY_MS;
        }
        for (i, &(y, m, d)) in EXPIRIES.iter().enumerate() {
            if if i == 0 { same_day } else { later } {
                let expiry = CivilDate::new(y, m, d).expect("date");
                for strike in STRIKES {
                    for right in [Right::Call, Right::Put] {
                        let key = OptionKey {
                            expiry,
                            strike: Price(strike * 100),
                            right,
                        };
                        native::encode(
                            &NativeEvent::OptionQuote {
                                time_us: t * 1000,
                                key,
                                quote: quote(key, spot, t, close),
                            },
                            &mut buf,
                        );
                        quotes += 1;
                    }
                }
            }
        }
        w.write(
            Direction::FromServer,
            Duration::from_millis((t - open) as u64),
            &Frame::binary(buf.clone()),
        )?;
        t += 2_000;
    }
    w.finish()?;
    Ok((trades, quotes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::MarketEvent;
    use crate::recorded::RecordedEvents;

    #[test]
    fn the_chain_is_plausible_and_exact() {
        let spot = 68_412;
        let now = UtcMillis::parse_iso("2026-11-09T15:30:00Z").unwrap().0;
        let close = UtcMillis::parse_iso(CLOSE).unwrap().0;
        let key = |d: u8, k: i64, right| OptionKey {
            expiry: CivilDate::new(2026, 11, d).unwrap(),
            strike: Price(k * 100),
            right,
        };
        let atm_call = quote(key(9, 684, Right::Call), spot, now, close);
        let atm_put = quote(key(9, 684, Right::Put), spot, now, close);
        // Near the money a 0DTE is worth well under a dollar here (low IV); delta near ±0.5.
        for q in [atm_call, atm_put] {
            let (b, a) = (q.bid.unwrap().0, q.ask.unwrap().0);
            assert!(b < a && (50..800).contains(&b), "{q:?}");
            assert!((3_000..7_000).contains(&q.delta_e4.unwrap().abs()), "{q:?}");
        }
        // Puts below the money carry skew; calls far out are worth (almost) nothing.
        let otm_put = quote(key(9, 670, Right::Put), spot, now, close);
        let otm_call = quote(key(9, 698, Right::Call), spot, now, close);
        assert!(otm_put.iv_bp.unwrap() > otm_call.iv_bp.unwrap());
        // Longer expiries are worth more.
        let dec = quote(
            OptionKey {
                expiry: CivilDate::new(2026, 12, 18).unwrap(),
                ..key(9, 684, Right::Call)
            },
            spot,
            now,
            close,
        );
        assert!(dec.bid.unwrap() > atm_call.ask.unwrap());
        // Call - put parity, roughly (r small over hours): C - P ≈ S - K.
        let parity = atm_call.last.unwrap().0 - atm_put.last.unwrap().0;
        assert!((parity - (spot - 68_400)).abs() <= 10, "{parity}");
    }

    #[test]
    fn the_file_replays_into_chains() {
        let dir = std::env::temp_dir().join(format!("lines-synth-chain-{}", std::process::id()));
        let path = dir.join("spy.lfr");
        let (trades, quotes) = write_spy_chain(&path).unwrap();
        assert_eq!(trades, 390 * 30);
        let per_expiry = 41 * 2;
        assert_eq!(
            quotes,
            2925 * per_expiry + 585 * 5 * per_expiry,
            "from the open: the same day every 8 s, the rest every 40 s"
        );
        let mut r = RecordedEvents::open(&path).unwrap();
        assert_eq!(r.product().symbol, "SPY");
        let (mut seen_quotes, mut seen_trades) = (0u64, 0u64);
        let mut chains = lines_core::OptionChains::default();
        while let Some((_, events)) = r.next_batch().unwrap() {
            for e in events {
                match e {
                    MarketEvent::OptionQuote { key, quote } => {
                        chains.apply(key, quote);
                        seen_quotes += 1;
                    }
                    MarketEvent::Trade { .. } => seen_trades += 1,
                    _ => {}
                }
            }
        }
        assert_eq!((seen_trades, seen_quotes), (trades, quotes));
        assert_eq!(chains.expiries().count(), 6);
        let c = chains.chain(CivilDate::new(2026, 11, 9).unwrap()).unwrap();
        assert_eq!(c.rows.len(), 41);
        assert!(c.rows.iter().all(|r| r.call.is_some() && r.put.is_some()));
        // Written twice: the same bytes.
        let again = dir.join("spy2.lfr");
        write_spy_chain(&again).unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            std::fs::read(&again).unwrap()
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
