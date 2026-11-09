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

    /// What holding `position` from `avg` (in price units) would show at this value, with
    /// `per_unit` the currency one price unit is worth per contract.
    pub fn pnl(self, position: i64, avg: f64, per_unit: f64) -> PnlEstimate {
        PnlEstimate((self.0 - avg) * position as f64 * per_unit)
    }

    /// [`Self::pnl`] as text: `+$40`, `-$15`.
    pub fn write_pnl(
        self,
        out: &mut impl Write,
        position: i64,
        avg: f64,
        per_unit: f64,
    ) -> fmt::Result {
        self.pnl(position, avg, per_unit).write(out)
    }
}

/// An estimated P&L, in whole currency. Display only, like [`Estimate`]: text, a shade and a
/// ratio for the screen, and no way to a `Money`.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct PnlEstimate(f64);

impl PnlEstimate {
    /// `+$40`, `-$15`.
    pub fn write(self, out: &mut impl Write) -> fmt::Result {
        let rounded = self.0.round();
        let sign = if rounded < 0.0 { '-' } else { '+' };
        write!(out, "{sign}${}", rounded.abs() as i64)
    }

    pub fn is_loss(self) -> bool {
        self.0 < 0.0
    }

    pub fn size(self) -> f64 {
        self.0.abs()
    }

    /// How strong a shade is against the largest in view: 0 to 1.
    pub fn shade(self, largest: f64) -> f32 {
        if largest <= 0.0 {
            return 0.0;
        }
        ((self.0.abs() / largest).clamp(0.0, 1.0)).sqrt() as f32
    }

    /// `1 : 4.3` as 4.3: reward for each unit of risk, when one is a loss and the other a gain.
    pub fn reward_to_risk(reward: PnlEstimate, risk: PnlEstimate) -> Option<f64> {
        (reward.0 > 0.0 && risk.0 < 0.0).then(|| reward.0 / -risk.0)
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

/// The model for one option as the market has it now. At today's underlying and time it reads the
/// mid; the difference from the plain model fades to nothing at the close, where only what
/// exercising pays is left. Copied by value, so a paint can use it freely.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Model {
    strike: f64,
    right: Right,
    sigma: f64,
    now: i64,
    close: i64,
    /// The mid less the plain model, now.
    offset: f64,
}

impl Model {
    /// `None` without the quote's volatility or an underlying price to start from, which the
    /// screen shows as no estimate.
    pub fn new(
        contract: ContractKey,
        quote: Option<&OptionQuote>,
        under: Option<Price>,
        now: Option<UtcMillis>,
    ) -> Option<Model> {
        let key = contract.option_key()?;
        let q = quote?;
        let sigma = f64::from(q.iv_bp?) / 10_000.0;
        let under = under?.0 as f64;
        let now = now?;
        let close = lines_feed::catalog::sessions(contract.product)
            .and_then(|c| c.session(key.expiry, lines_core::SessionKind::Rth))
            .map_or_else(|| key.expiry.add_days(1).start_utc(), |s| s.close);
        let strike = key.strike.0 as f64;
        let years = (close.0 - now.0) as f64 / MS_PER_YEAR;
        let offset = match (q.bid, q.ask) {
            (Some(b), Some(a)) if a.0 >= b.0 => {
                (b.0 + a.0) as f64 / 2.0 - model(under, strike, key.right, years, sigma)
            }
            _ => 0.0,
        };
        Some(Model {
            strike,
            right: key.right,
            sigma,
            now: now.0,
            close: close.0,
            offset,
        })
    }

    pub fn now(&self) -> i64 {
        self.now
    }

    pub fn close(&self) -> i64 {
        self.close
    }

    /// The value with the underlying at `under` (price units) at time `at`.
    pub fn value_at(&self, under: f64, at: i64) -> Estimate {
        let span = (self.close - self.now) as f64;
        let left = (self.close - at) as f64;
        let fade = if span > 0.0 {
            (left / span).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let years = left / MS_PER_YEAR;
        let plain = model(under, self.strike, self.right, years, self.sigma);
        Estimate((plain + self.offset * fade).max(0.0))
    }

    /// The underlying's price between `lo` and `hi` where the option is worth `value` at time
    /// `at`; `None`: it isn't, in that range.
    pub fn level_for(&self, value: f64, at: i64, lo: f64, hi: f64) -> Option<f64> {
        let above = |s: f64| self.value_at(s, at).0 > value;
        let (mut a, mut b) = (lo, hi);
        if above(a) == above(b) {
            return None;
        }
        let at_a = above(a);
        for _ in 0..24 {
            let mid = (a + b) / 2.0;
            if above(mid) == at_a {
                a = mid;
            } else {
                b = mid;
            }
        }
        Some((a + b) / 2.0)
    }
}

/// The option's estimated value with the underlying at `level`, now; `None` as [`Model::new`].
pub fn estimate(
    contract: ContractKey,
    quote: Option<&OptionQuote>,
    under: Option<Price>,
    level: Price,
    now: Option<UtcMillis>,
) -> Option<Estimate> {
    let m = Model::new(contract, quote, under, now)?;
    Some(m.value_at(level.0 as f64, m.now))
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
        close_to(e.0, 242.0, 1e-6);
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

    /// Time decays: at the close it's what exercising pays, whatever the quote said; before it
    /// the same price is worth more, and now it reads the mid.
    #[test]
    fn the_estimate_runs_to_the_close() {
        let q = quote(240, 244, Some(1_500));
        let c = contract(684, Right::Call);
        let m = Model::new(c, Some(&q), Some(Price(68_400)), at_ten()).unwrap();
        close_to(m.value_at(68_400.0, m.now()).0, 242.0, 1e-6);
        // At the money with nothing left: nothing.
        close_to(m.value_at(68_400.0, m.close()).0, 0.0, 1e-6);
        close_to(m.value_at(68_500.0, m.close()).0, 100.0, 1e-6);
        close_to(m.value_at(68_300.0, m.close()).0, 0.0, 1e-6);
        let half = (m.now() + m.close()) / 2;
        let (now, mid, end) = (
            m.value_at(68_400.0, m.now()).0,
            m.value_at(68_400.0, half).0,
            m.value_at(68_400.0, m.close()).0,
        );
        assert!(now > mid && mid > end, "{now} {mid} {end}");
        // Past the close it stays there.
        close_to(m.value_at(68_500.0, m.close() + 1_000).0, 100.0, 1e-6);
    }

    /// The breakeven: where it's worth what was paid, and none where it can't be.
    #[test]
    fn a_level_is_found_for_a_value_either_right() {
        let q = quote(240, 244, Some(1_500));
        let (lo, hi) = (60_000.0, 76_000.0);
        let call = contract(684, Right::Call);
        let m = Model::new(call, Some(&q), Some(Price(68_400)), at_ten()).unwrap();
        // At the close: the strike plus what was paid.
        close_to(
            m.level_for(242.0, m.close(), lo, hi).unwrap(),
            68_642.0,
            0.5,
        );
        assert_eq!(m.level_for(242.0, m.close(), 68_000.0, 68_100.0), None);
        let put = contract(684, Right::Put);
        let p = Model::new(put, Some(&q), Some(Price(68_400)), at_ten()).unwrap();
        close_to(
            p.level_for(242.0, p.close(), lo, hi).unwrap(),
            68_158.0,
            0.5,
        );
    }

    #[test]
    fn a_pnl_reads_signed_and_shades_by_size() {
        let text = |p: PnlEstimate| {
            let mut s = String::new();
            p.write(&mut s).unwrap();
            s
        };
        let gain = Estimate(282.0).pnl(1, 242.0, 1.0);
        let loss = Estimate(202.0).pnl(2, 242.0, 1.0);
        assert_eq!((text(gain), text(loss)), ("+$40".into(), "-$80".into()));
        assert!(loss.is_loss() && !gain.is_loss());
        assert_eq!(loss.size(), 80.0);
        assert_eq!(gain.shade(40.0), 1.0);
        assert_eq!(gain.shade(160.0), 0.5);
        assert_eq!(gain.shade(0.0), 0.0);
        assert_eq!(PnlEstimate::reward_to_risk(gain, loss), Some(0.5));
        assert_eq!(PnlEstimate::reward_to_risk(loss, gain), None, "backward");
        assert_eq!(PnlEstimate::reward_to_risk(gain, gain), None, "no risk");
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
