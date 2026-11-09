//! What an option might be worth at another price of its underlying (decision 0029): a display
//! value for a line's tag, always shown marked ≈.
//!
//! **Display only.** It is never used for an order's price, a risk check or P&L, and the type
//! keeps it that way: an [`Estimate`] turns into text and nothing else. There is no way from it
//! to a `Price` or a `Money`.
//!
//! The model is Black-Scholes with no rates or dividends, at the quote's own implied volatility,
//! and moved so that at today's underlying it reads the quote's mid: what the tag shows is the
//! model's *change* from what the market says now.

use std::fmt::{self, Write};

use lines_core::{ContractKey, OptionQuote, Price, Product, Right, UtcMillis};

const MS_PER_YEAR: f64 = 365.25 * 24.0 * 3600.0 * 1000.0;

/// An estimated option value, in the product's price units. Text only: see the module's note.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Estimate(f64);

impl Estimate {
    /// `1.25`, in the product's own decimals.
    pub fn write(self, out: &mut impl Write, product: &Product) -> fmt::Result {
        let scale = product.price.scale;
        let whole = self.0 * scale.num() as f64 / scale.den() as f64;
        write!(out, "{whole:.2}")
    }

    /// What holding `position` from `avg` (in price units) would show at this value, in whole
    /// currency, with `per_unit` the currency one price unit is worth per contract: `+$40`,
    /// `-$15`. An estimate too, and text for the same reason.
    pub fn write_pnl(
        self,
        out: &mut impl Write,
        position: i64,
        avg: f64,
        per_unit: f64,
    ) -> fmt::Result {
        let pnl = (self.0 - avg) * position as f64 * per_unit;
        let rounded = pnl.round();
        let sign = if rounded < 0.0 { '-' } else { '+' };
        write!(out, "{sign}${}", rounded.abs() as i64)
    }
}

/// The standard normal's cumulative distribution (Abramowitz and Stegun 26.2.17).
fn normal_cdf(x: f64) -> f64 {
    let t = 1.0 / (1.0 + 0.231_641_9 * x.abs());
    let d = 0.398_942_3 * (-x * x / 2.0).exp();
    let tail = d
        * t
        * (0.319_381_5 + t * (-0.356_563_8 + t * (1.781_478 + t * (-1.821_256 + t * 1.330_274))));
    if x > 0.0 { 1.0 - tail } else { tail }
}

/// Black-Scholes with no rates or dividends. At or past expiry, or with no volatility, what it
/// would be worth exercised.
fn model(under: f64, strike: f64, right: Right, years: f64, sigma: f64) -> f64 {
    let intrinsic = match right {
        Right::Call => (under - strike).max(0.0),
        Right::Put => (strike - under).max(0.0),
    };
    if years <= 0.0 || sigma <= 0.0 || under <= 0.0 || strike <= 0.0 {
        return intrinsic;
    }
    let root = sigma * years.sqrt();
    let d1 = ((under / strike).ln() + sigma * sigma * years / 2.0) / root;
    let d2 = d1 - root;
    match right {
        Right::Call => under * normal_cdf(d1) - strike * normal_cdf(d2),
        Right::Put => strike * normal_cdf(-d2) - under * normal_cdf(-d1),
    }
}

/// The option's estimated value with the underlying at `level`; `None` without the quote's
/// volatility or an underlying price to start from, which the tag shows as no estimate.
pub fn estimate(
    contract: ContractKey,
    quote: Option<&OptionQuote>,
    under: Option<Price>,
    level: Price,
    now: Option<UtcMillis>,
) -> Option<Estimate> {
    let key = contract.option_key()?;
    let q = quote?;
    let sigma = f64::from(q.iv_bp?) / 10_000.0;
    let under = under?.0 as f64;
    let now = now?;
    let close = lines_feed::catalog::sessions(contract.product)
        .and_then(|c| c.session(key.expiry, lines_core::SessionKind::Rth))
        .map_or_else(|| key.expiry.add_days(1).start_utc(), |s| s.close);
    let years = (close.0 - now.0) as f64 / MS_PER_YEAR;
    let strike = key.strike.0 as f64;
    let at = |s: f64| model(s, strike, key.right, years, sigma);
    let value = match (q.bid, q.ask) {
        (Some(b), Some(a)) if a.0 >= b.0 => {
            let mid = (b.0 + a.0) as f64 / 2.0;
            mid + at(level.0 as f64) - at(under)
        }
        _ => at(level.0 as f64),
    };
    Some(Estimate(value.max(0.0)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lines_core::{CivilDate, Qty};
    use lines_feed::catalog;

    fn close_to(a: f64, b: f64, within: f64) {
        assert!((a - b).abs() <= within, "{a} vs {b}");
    }

    fn quote(bid: i64, ask: i64, iv_bp: Option<i32>) -> OptionQuote {
        OptionQuote {
            bid: Some(Price(bid)),
            ask: Some(Price(ask)),
            last: None,
            volume: Qty(0),
            open_interest: 0,
            iv_bp,
            delta_e4: None,
            time: UtcMillis(0),
        }
    }

    fn contract(strike: i64, right: Right) -> ContractKey {
        ContractKey::option(
            catalog::SPY,
            CivilDate::new(2026, 11, 13).unwrap(),
            Price(strike * 100),
            right,
        )
    }

    fn at_ten() -> Option<UtcMillis> {
        UtcMillis::parse_iso("2026-11-09T15:00:00Z")
    }

    #[test]
    fn the_cdf_and_the_model_read_as_published() {
        close_to(normal_cdf(0.0), 0.5, 1e-6);
        close_to(normal_cdf(1.96), 0.975, 1e-3);
        close_to(normal_cdf(-1.96), 0.025, 1e-3);
        // The textbook case: S = K = 100, a year, 20%, no rates.
        close_to(model(100.0, 100.0, Right::Call, 1.0, 0.2), 7.97, 0.01);
        close_to(model(100.0, 100.0, Right::Put, 1.0, 0.2), 7.97, 0.01);
    }

    /// With nothing left it's what exercising pays, and never below that for a long call.
    #[test]
    fn at_expiry_it_is_the_intrinsic_value() {
        assert_eq!(model(110.0, 100.0, Right::Call, 0.0, 0.2), 10.0);
        assert_eq!(model(90.0, 100.0, Right::Call, 0.0, 0.2), 0.0);
        assert_eq!(model(90.0, 100.0, Right::Put, -1.0, 0.2), 10.0);
        assert_eq!(model(90.0, 100.0, Right::Put, 1.0, 0.0), 10.0);
    }

    /// At today's price the tag agrees with the market's mid.
    #[test]
    fn at_the_underlying_it_reads_the_mid() {
        let q = quote(240, 244, Some(1_500));
        let e = estimate(
            contract(684, Right::Call),
            Some(&q),
            Some(Price(68_400)),
            Price(68_400),
            at_ten(),
        )
        .unwrap();
        assert_eq!(e, Estimate(242.0));
    }

    /// A call is worth more as the underlying rises and a put as it falls, and neither below 0.
    #[test]
    fn it_moves_the_way_the_right_does() {
        let q = quote(240, 244, Some(1_500));
        let value = |right, level| {
            let e = estimate(
                contract(684, right),
                Some(&q),
                Some(Price(68_400)),
                Price(level),
                at_ten(),
            );
            e.unwrap().0
        };
        assert!(value(Right::Call, 68_600) > 242.0);
        assert!(value(Right::Call, 68_200) < 242.0);
        assert!(value(Right::Put, 68_200) > 242.0);
        assert!(value(Right::Put, 68_600) < 242.0);
        assert!(value(Right::Call, 40_000) >= 0.0);
    }

    #[test]
    fn without_what_it_needs_there_is_no_estimate() {
        let c = contract(684, Right::Call);
        let at = Some(Price(68_400));
        let q = quote(240, 244, Some(1_500));
        let no_iv = quote(240, 244, None);
        assert!(estimate(c, None, at, Price(68_500), at_ten()).is_none());
        assert!(estimate(c, Some(&no_iv), at, Price(68_500), at_ten()).is_none());
        assert!(estimate(c, Some(&q), None, Price(68_500), at_ten()).is_none());
        assert!(estimate(c, Some(&q), at, Price(68_500), None).is_none());
        let shares = ContractKey::equity(catalog::SPY);
        assert!(estimate(shares, Some(&q), at, Price(68_500), at_ten()).is_none());
    }

    #[test]
    fn it_reads_as_text() {
        let p = catalog::get(catalog::SPY).unwrap();
        let mut s = String::new();
        Estimate(125.0).write(&mut s, &p).unwrap();
        assert_eq!(s, "1.25");
        let pnl = |value: f64, position: i64| {
            let mut s = String::new();
            Estimate(value)
                .write_pnl(&mut s, position, 242.0, 1.0)
                .unwrap();
            s
        };
        assert_eq!(pnl(282.0, 1), "+$40");
        assert_eq!(pnl(202.0, 1), "-$40");
        assert_eq!(pnl(282.0, -2), "-$80");
        assert_eq!(pnl(242.0, 1), "+$0");
    }
}
