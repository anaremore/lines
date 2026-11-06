//! Prop-firm rules as data (decision 0006): presets are user-editable config, labelled as
//! approximations, never presented as a firm's official rules.
//!
//! A [`RulePreset`] describes the rules; a [`RuleTracker`] follows one account through them
//! and reports, after every fill and price change, how far the account is from each limit
//! ([`RuleStatus`], part of the account event).
//!
//! **Trailing drawdown.** The account fails when equity reaches `high-water − amount`.
//! - *Intraday*: the high-water mark is the highest **equity** ever reached, unrealised P&L
//!   included, updated on every mark.
//! - *End of day*: the high-water mark is the highest **closing balance**, updated once per
//!   trade date (intraday swings don't move it).
//! - *Lock at break-even* (optional): once `high-water − amount` reaches the starting balance
//!   (plus an offset), the threshold stops trailing and stays there.
//!
//! **Daily loss**: realised + unrealised P&L of the current trade date (fees included) must stay
//! above `−limit`; a breach blocks new orders until the next trade date.
//! **Contract cap**: open position plus open orders, in micro-equivalent contracts (the
//! product's equivalence ratio: 1 NQ = 10 MNQ).
//! **Profit target** and **consistency** (the best day's share of total profit) are tracked and
//! reported; they don't block orders.

use lines_core::CivilDate;
use serde::{Deserialize, Serialize};

use crate::types::Money;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DrawdownMode {
    Intraday,
    EndOfDay,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrailingDrawdown {
    /// Whole currency units.
    pub amount: i64,
    pub mode: DrawdownMode,
    /// Stop trailing once the threshold reaches the starting balance plus this many whole
    /// currency units (the "locks at break-even" variant). `None`: trails forever.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock_at_start_plus: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Consistency {
    /// The best day may be at most this percentage of total profit.
    pub max_best_day_pct: u8,
}

/// A rule set. Saved and edited as data (JSON).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RulePreset {
    pub name: String,
    /// Always shown with the preset: these are approximations, not a firm's official rules.
    pub label: String,
    /// Whole currency units.
    pub account_size: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trailing_drawdown: Option<TrailingDrawdown>,
    /// Whole currency units.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daily_loss_limit: Option<i64>,
    /// Open position plus open orders, in micro-equivalent contracts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_micro_contracts: Option<u32>,
    /// Whole currency units of profit to pass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profit_target: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consistency: Option<Consistency>,
    /// Flatten the account (as a rule command) when the drawdown or daily loss is breached.
    #[serde(default)]
    pub liquidate_on_breach: bool,
}

pub const APPROXIMATION: &str =
    "User-editable approximation. Not a prop firm's official rules: check your firm's terms.";

/// Example presets. Shapes common in prop evaluations, with no firm's name or official numbers.
pub fn presets() -> Vec<RulePreset> {
    let base = |name: &str, size: i64, dd: i64, mode, lock, daily, micros, target| RulePreset {
        name: name.into(),
        label: APPROXIMATION.into(),
        account_size: size,
        trailing_drawdown: Some(TrailingDrawdown {
            amount: dd,
            mode,
            lock_at_start_plus: lock,
        }),
        daily_loss_limit: daily,
        max_micro_contracts: Some(micros),
        profit_target: Some(target),
        consistency: None,
        liquidate_on_breach: true,
    };
    vec![
        base(
            "50K evaluation, intraday trailing drawdown",
            50_000,
            2_500,
            DrawdownMode::Intraday,
            Some(100),
            None,
            100,
            3_000,
        ),
        base(
            "50K evaluation, end-of-day trailing drawdown, daily loss limit",
            50_000,
            2_000,
            DrawdownMode::EndOfDay,
            Some(0),
            Some(1_000),
            50,
            3_000,
        ),
        RulePreset {
            consistency: Some(Consistency {
                max_best_day_pct: 30,
            }),
            ..base(
                "100K funded, end-of-day trailing drawdown, consistency 30%",
                100_000,
                3_000,
                DrawdownMode::EndOfDay,
                Some(100),
                Some(2_000),
                120,
                6_000,
            )
        },
    ]
}

/// A limit the account has crossed.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Breach {
    TrailingDrawdown,
    DailyLoss,
}

/// Where the account stands against its rules (in every account event).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleStatus {
    /// The equity at which the trailing drawdown fails the account.
    pub drawdown_threshold: Option<Money>,
    /// Equity minus the threshold (≤ 0 = breached).
    pub to_drawdown: Option<Money>,
    /// How much more the account may lose today.
    pub daily_loss_left: Option<Money>,
    pub micro_contracts_used: u32,
    pub micro_contracts_max: Option<u32>,
    /// Profit still needed (≤ 0 = reached).
    pub to_profit_target: Option<Money>,
    /// The best day's share of total profit, percent (if there is a profit).
    pub best_day_pct: Option<u8>,
    pub consistency_ok: Option<bool>,
    /// Breaches so far (drawdown is final; daily loss lasts the trade date).
    pub breaches: Vec<Breach>,
}

/// One account's progress through its preset. Saved with the account.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleTracker {
    pub start_balance: Money,
    /// Highest equity (intraday) or highest closing balance (end of day).
    pub high_water: Money,
    /// Once the lock level is reached the threshold stays there.
    pub locked: bool,
    /// The current trade date and the balance it opened with.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "opt_date")]
    pub day: Option<CivilDate>,
    pub day_open_balance: Money,
    /// Profit per finished trade date (for consistency).
    pub day_profits: Vec<Money>,
    pub drawdown_breached: bool,
    pub daily_breached: bool,
}

mod opt_date {
    use lines_core::CivilDate;
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(d: &Option<CivilDate>, s: S) -> Result<S::Ok, S::Error> {
        match d {
            Some(d) => s.serialize_some(&d.to_string()),
            None => s.serialize_none(),
        }
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<CivilDate>, D::Error> {
        let s = Option::<String>::deserialize(d)?;
        Ok(s.and_then(|s| {
            let mut it = s.split('-').map(|x| x.parse::<u32>().ok());
            let (y, m, d) = (it.next()??, it.next()??, it.next()??);
            CivilDate::new(y as u16, m as u8, d as u8)
        }))
    }
}

impl RuleTracker {
    pub fn new(start_balance: Money) -> RuleTracker {
        RuleTracker {
            start_balance,
            high_water: start_balance,
            locked: false,
            day: None,
            day_open_balance: start_balance,
            day_profits: Vec::new(),
            drawdown_breached: false,
            daily_breached: false,
        }
    }

    /// The trade date changed: close the previous day (end-of-day high-water, day profit).
    pub fn roll_day(&mut self, preset: &RulePreset, date: CivilDate, balance: Money) {
        if self.day == Some(date) {
            return;
        }
        if self.day.is_some() {
            self.day_profits.push(balance - self.day_open_balance);
            if let Some(dd) = &preset.trailing_drawdown
                && dd.mode == DrawdownMode::EndOfDay
            {
                self.raise_high_water(dd, balance);
            }
        }
        self.day = Some(date);
        self.day_open_balance = balance;
        self.daily_breached = false;
    }

    fn raise_high_water(&mut self, dd: &TrailingDrawdown, level: Money) {
        if level > self.high_water {
            self.high_water = level;
        }
        if let Some(plus) = dd.lock_at_start_plus
            && self.high_water - Money::whole(dd.amount) >= self.start_balance + Money::whole(plus)
        {
            self.locked = true;
        }
    }

    pub fn threshold(&self, preset: &RulePreset) -> Option<Money> {
        let dd = preset.trailing_drawdown.as_ref()?;
        let trailing = self.high_water - Money::whole(dd.amount);
        Some(match dd.lock_at_start_plus {
            Some(plus) if self.locked => self.start_balance + Money::whole(plus),
            _ => trailing,
        })
    }

    /// Follow a new equity (balance + open P&L): intraday high-water, breaches. Returns the
    /// breaches that happened just now.
    pub fn on_equity(&mut self, preset: &RulePreset, balance: Money, equity: Money) -> Vec<Breach> {
        let mut new = Vec::new();
        if let Some(dd) = &preset.trailing_drawdown {
            if dd.mode == DrawdownMode::Intraday {
                self.raise_high_water(dd, equity);
            }
            if !self.drawdown_breached
                && let Some(t) = self.threshold(preset)
                && equity <= t
            {
                self.drawdown_breached = true;
                new.push(Breach::TrailingDrawdown);
            }
        }
        if let Some(limit) = preset.daily_loss_limit
            && !self.daily_breached
            && equity - self.day_open_balance <= -Money::whole(limit)
        {
            self.daily_breached = true;
            new.push(Breach::DailyLoss);
        }
        let _ = balance;
        new
    }

    /// New orders are blocked (Flatten and Cancel All never are).
    pub fn blocked(&self) -> Option<Breach> {
        if self.drawdown_breached {
            Some(Breach::TrailingDrawdown)
        } else if self.daily_breached {
            Some(Breach::DailyLoss)
        } else {
            None
        }
    }

    pub fn status(
        &self,
        preset: &RulePreset,
        balance: Money,
        equity: Money,
        micros: u32,
    ) -> RuleStatus {
        let threshold = self.threshold(preset);
        let total = balance - self.start_balance;
        let today = balance - self.day_open_balance;
        let best = self.day_profits.iter().copied().chain([today]).max();
        let best_day_pct = (total > Money::ZERO)
            .then(|| best.map(|b| ((b.0.max(0) * 100) / total.0).clamp(0, 255) as u8))
            .flatten();
        let mut breaches = Vec::new();
        if self.drawdown_breached {
            breaches.push(Breach::TrailingDrawdown);
        }
        if self.daily_breached {
            breaches.push(Breach::DailyLoss);
        }
        RuleStatus {
            drawdown_threshold: threshold,
            to_drawdown: threshold.map(|t| equity - t),
            daily_loss_left: preset
                .daily_loss_limit
                .map(|l| Money::whole(l) + (equity - self.day_open_balance)),
            micro_contracts_used: micros,
            micro_contracts_max: preset.max_micro_contracts,
            to_profit_target: preset.profit_target.map(|t| Money::whole(t) - total),
            best_day_pct,
            consistency_ok: preset
                .consistency
                .as_ref()
                .and_then(|c| best_day_pct.map(|p| p <= c.max_best_day_pct)),
            breaches,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preset(mode: DrawdownMode, lock: Option<i64>) -> RulePreset {
        RulePreset {
            name: "t".into(),
            label: APPROXIMATION.into(),
            account_size: 50_000,
            trailing_drawdown: Some(TrailingDrawdown {
                amount: 2_000,
                mode,
                lock_at_start_plus: lock,
            }),
            daily_loss_limit: Some(1_000),
            max_micro_contracts: Some(50),
            profit_target: Some(3_000),
            consistency: Some(Consistency {
                max_best_day_pct: 50,
            }),
            liquidate_on_breach: false,
        }
    }

    fn d(day: u8) -> CivilDate {
        CivilDate::new(2026, 11, day).unwrap()
    }

    /// The same P&L path under both modes: intraday trails the unrealised peak, end of day
    /// only the closing balances.
    #[test]
    fn trailing_drawdown_intraday_vs_end_of_day() {
        let start = Money::whole(50_000);
        // Day 1: equity peaks at +1500 intraday, closes at +500. Day 2: falls to −1200.
        let path = [(1, 51_500), (1, 50_500), (2, 49_800), (2, 49_400)];
        let mut intra = RuleTracker::new(start);
        let mut eod = RuleTracker::new(start);
        let (mut pi, mut pe) = (
            preset(DrawdownMode::Intraday, None),
            preset(DrawdownMode::EndOfDay, None),
        );
        // Only the drawdown here (day 2's −1,100 would also trip a 1,000 daily loss limit).
        pi.daily_loss_limit = None;
        pe.daily_loss_limit = None;
        let mut balance = start;
        let mut breached = (None, None);
        for (i, &(day, eq)) in path.iter().enumerate() {
            let eq = Money::whole(eq);
            if i == 2 {
                balance = Money::whole(50_500); // day 1 closed flat at +500
            }
            intra.roll_day(&pi, d(day), balance);
            eod.roll_day(&pe, d(day), balance);
            if !intra.on_equity(&pi, balance, eq).is_empty() {
                breached.0.get_or_insert(i);
            }
            if !eod.on_equity(&pe, balance, eq).is_empty() {
                breached.1.get_or_insert(i);
            }
        }
        // Intraday: high-water 51,500 → threshold 49,500; 49,400 breaches it.
        assert_eq!(intra.high_water, Money::whole(51_500));
        assert_eq!(intra.threshold(&pi), Some(Money::whole(49_500)));
        assert_eq!(breached.0, Some(3));
        // End of day: high-water is day 1's close (50,500) → threshold 48,500; no breach.
        assert_eq!(eod.high_water, Money::whole(50_500));
        assert_eq!(eod.threshold(&pe), Some(Money::whole(48_500)));
        assert_eq!(breached.1, None);
    }

    #[test]
    fn lock_at_break_even_stops_trailing() {
        let p = preset(DrawdownMode::Intraday, Some(0));
        let mut t = RuleTracker::new(Money::whole(50_000));
        t.roll_day(&p, d(1), Money::whole(50_000));
        t.on_equity(&p, Money::whole(50_000), Money::whole(51_000));
        assert_eq!(
            t.threshold(&p),
            Some(Money::whole(49_000)),
            "still trailing"
        );
        t.on_equity(&p, Money::whole(50_000), Money::whole(52_500));
        assert!(t.locked);
        assert_eq!(
            t.threshold(&p),
            Some(Money::whole(50_000)),
            "locked at break-even"
        );
        t.on_equity(&p, Money::whole(50_000), Money::whole(55_000));
        assert_eq!(
            t.threshold(&p),
            Some(Money::whole(50_000)),
            "no longer trails"
        );
    }

    #[test]
    fn daily_loss_blocks_until_the_next_trade_date_and_status_reports_distances() {
        let p = preset(DrawdownMode::EndOfDay, None);
        let mut t = RuleTracker::new(Money::whole(50_000));
        t.roll_day(&p, d(1), Money::whole(50_000));
        assert_eq!(
            t.on_equity(&p, Money::whole(50_000), Money::whole(49_000)),
            vec![Breach::DailyLoss]
        );
        assert_eq!(t.blocked(), Some(Breach::DailyLoss));
        t.roll_day(&p, d(2), Money::whole(49_200));
        assert_eq!(t.blocked(), None, "a new trade date");
        let s = t.status(&p, Money::whole(49_200), Money::whole(49_000), 12);
        assert_eq!(s.daily_loss_left, Some(Money::whole(800)));
        assert_eq!(s.to_drawdown, Some(Money::whole(1_000)));
        assert_eq!(s.to_profit_target, Some(Money::whole(3_800)));
        assert_eq!(
            (s.micro_contracts_used, s.micro_contracts_max),
            (12, Some(50))
        );
    }

    #[test]
    fn presets_are_labelled_approximations_and_round_trip() {
        for p in presets() {
            assert_eq!(p.label, APPROXIMATION);
            let json = serde_json::to_string(&p).unwrap();
            assert_eq!(serde_json::from_str::<RulePreset>(&json).unwrap(), p);
        }
    }
}
