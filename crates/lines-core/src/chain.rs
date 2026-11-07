//! Option chains (decision 0028): the quotes of an underlying's listed options, by expiry and
//! strike, as a provider streams them.
//!
//! Prices are exact ([`Price`] in the underlying's price units, like the strike). Implied
//! volatility and delta are display values (decision 0028: never used for order prices or
//! P&L), kept as fixed-point integers so quotes compare exactly and replays are deterministic:
//! IV in hundredths of a percent (`1480` = 14.80%), delta in ten-thousandths (`-1400` =
//! -0.14).
//!
//! An [`OptionChains`] holds one [`OptionChain`] per expiry, each a strike-sorted list of
//! [`ChainRow`]s with the call's and the put's quote. Updates replace a series' quote whole.

use std::collections::BTreeMap;

use crate::calendar::CivilDate;
use crate::instrument::{OptionKey, Right};
use crate::price::Price;
use crate::qty::Qty;
use crate::time::UtcMillis;

/// One option series' quote. `None`: the provider has no value (shown as missing).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct OptionQuote {
    pub bid: Option<Price>,
    pub ask: Option<Price>,
    pub last: Option<Price>,
    /// Contracts traded today.
    pub volume: Qty,
    /// Open interest, contracts.
    pub open_interest: i64,
    /// Implied volatility, hundredths of a percent (display only).
    pub iv_bp: Option<i32>,
    /// Delta, ten-thousandths (display only).
    pub delta_e4: Option<i32>,
    /// When the provider stamped it.
    pub time: UtcMillis,
}

/// One strike of a chain: the call's and the put's quote, if any.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ChainRow {
    pub strike: Price,
    pub call: Option<OptionQuote>,
    pub put: Option<OptionQuote>,
}

impl ChainRow {
    pub fn side(&self, right: Right) -> Option<&OptionQuote> {
        match right {
            Right::Call => self.call.as_ref(),
            Right::Put => self.put.as_ref(),
        }
    }
}

/// One expiry's strikes, ascending.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OptionChain {
    pub rows: Vec<ChainRow>,
}

impl OptionChain {
    /// The row index for a strike: `Ok` if present, else where it would go.
    pub fn find(&self, strike: Price) -> Result<usize, usize> {
        self.rows.binary_search_by_key(&strike, |r| r.strike)
    }

    /// Where the underlying's price `last` falls: the index of the first strike above it
    /// (`0..=rows.len()`), so the "price line" is drawn between rows `i - 1` and `i`.
    pub fn split_at(&self, last: Price) -> usize {
        self.rows.partition_point(|r| r.strike <= last)
    }

    /// The at-the-money row: the strike nearest `last` (the lower one on a tie).
    pub fn atm(&self, last: Price) -> Option<usize> {
        if self.rows.is_empty() {
            return None;
        }
        let i = self.split_at(last);
        Some(match i {
            0 => 0,
            i if i == self.rows.len() => i - 1,
            i => {
                let (lo, hi) = (self.rows[i - 1].strike.0, self.rows[i].strike.0);
                if last.0 - lo <= hi - last.0 { i - 1 } else { i }
            }
        })
    }
}

/// Every expiry's chain for one underlying.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OptionChains {
    by_expiry: BTreeMap<CivilDate, OptionChain>,
    /// Moves on every applied quote (panes repaint when it moved).
    revision: u64,
}

impl OptionChains {
    /// Set one series' quote (replacing what was there).
    pub fn apply(&mut self, key: OptionKey, quote: OptionQuote) {
        let chain = self.by_expiry.entry(key.expiry).or_default();
        let i = match chain.find(key.strike) {
            Ok(i) => i,
            Err(i) => {
                chain.rows.insert(
                    i,
                    ChainRow {
                        strike: key.strike,
                        call: None,
                        put: None,
                    },
                );
                i
            }
        };
        let row = &mut chain.rows[i];
        match key.right {
            Right::Call => row.call = Some(quote),
            Right::Put => row.put = Some(quote),
        }
        self.revision += 1;
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn is_empty(&self) -> bool {
        self.by_expiry.is_empty()
    }

    /// Expiries with quotes, soonest first.
    pub fn expiries(&self) -> impl Iterator<Item = CivilDate> + '_ {
        self.by_expiry.keys().copied()
    }

    pub fn chain(&self, expiry: CivilDate) -> Option<&OptionChain> {
        self.by_expiry.get(&expiry)
    }

    /// Forget expiries before `today` (they no longer trade).
    pub fn drop_expired(&mut self, today: CivilDate) {
        let before = self.by_expiry.len();
        self.by_expiry.retain(|d, _| *d >= today);
        if self.by_expiry.len() != before {
            self.revision += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(day: u8, strike: i64, right: Right) -> OptionKey {
        OptionKey {
            expiry: CivilDate::new(2026, 11, day).unwrap(),
            strike: Price(strike),
            right,
        }
    }

    fn q(bid: i64) -> OptionQuote {
        OptionQuote {
            bid: Some(Price(bid)),
            ask: Some(Price(bid + 2)),
            ..Default::default()
        }
    }

    #[test]
    fn quotes_land_in_strike_order_per_expiry() {
        let mut c = OptionChains::default();
        c.apply(key(9, 68_500, Right::Call), q(50));
        c.apply(key(9, 68_300, Right::Put), q(40));
        c.apply(key(9, 68_400, Right::Call), q(90));
        c.apply(key(13, 68_400, Right::Put), q(200));
        c.apply(key(9, 68_400, Right::Put), q(60));
        c.apply(key(9, 68_400, Right::Call), q(95)); // replaces
        assert_eq!(c.revision(), 6);
        let exp: Vec<_> = c.expiries().map(|d| d.day).collect();
        assert_eq!(exp, [9, 13]);
        let ch = c.chain(CivilDate::new(2026, 11, 9).unwrap()).unwrap();
        let strikes: Vec<_> = ch.rows.iter().map(|r| r.strike.0).collect();
        assert_eq!(strikes, [68_300, 68_400, 68_500]);
        assert_eq!(ch.rows[1].call.unwrap().bid, Some(Price(95)));
        assert_eq!(ch.rows[1].side(Right::Put).unwrap().bid, Some(Price(60)));
        assert!(ch.rows[0].call.is_none());
        c.drop_expired(CivilDate::new(2026, 11, 10).unwrap());
        assert_eq!(c.expiries().count(), 1);
    }

    #[test]
    fn the_underlying_splits_the_strikes() {
        let mut c = OptionChains::default();
        for s in [68_200, 68_300, 68_400, 68_500] {
            c.apply(key(9, s, Right::Call), q(1));
        }
        let ch = c.chain(CivilDate::new(2026, 11, 9).unwrap()).unwrap();
        // 684.12 sits between 684 and 685: the line goes above row 3.
        assert_eq!(ch.split_at(Price(68_412)), 3);
        assert_eq!(ch.atm(Price(68_412)), Some(2), "684 is nearer");
        assert_eq!(ch.atm(Price(68_460)), Some(3), "685 is nearer");
        assert_eq!(ch.atm(Price(68_450)), Some(2), "a tie: the lower");
        assert_eq!(ch.atm(Price(60_000)), Some(0));
        assert_eq!(ch.atm(Price(90_000)), Some(3));
        assert_eq!(ch.split_at(Price(68_400)), 3, "on a strike: below it");
        assert_eq!(OptionChain::default().atm(Price(1)), None);
    }
}
