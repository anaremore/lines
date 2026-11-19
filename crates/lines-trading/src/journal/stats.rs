//! Journal statistics over closed trades: count, win rate, average win and loss,
//! expectancy, profit factor, max drawdown on closed trades, streaks; grouped by trade date,
//! product, hour of day (exchange time), account or tag.
//!
//! A **win** nets more than zero after fees, a **loss** less; a scratch (exactly zero) is
//! neither but counts as a trade and breaks streaks. Money stays exact (`Money`); ratios are
//! `f64` for display. Trades are taken in the order they closed (exit time, then id), so the
//! drawdown and streaks don't depend on the order they were handed in.

use std::collections::BTreeMap;

use lines_core::UtcMillis;

use super::Trade;
use crate::types::Money;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub count: u32,
    pub wins: u32,
    pub losses: u32,
    /// Σ net (after fees).
    pub net: Money,
    pub fees: Money,
    /// Σ net of winners (≥ 0) and of losers (≤ 0).
    pub gross_win: Money,
    pub gross_loss: Money,
    /// Largest fall of the running net from a previous high, closed trades only (≥ 0).
    pub max_drawdown: Money,
    pub longest_win_streak: u32,
    pub longest_loss_streak: u32,
    /// The streak at the end: positive wins, negative losses, 0 after a scratch.
    pub streak: i32,
}

impl Stats {
    pub fn of<'a>(trades: impl IntoIterator<Item = &'a Trade>) -> Stats {
        let mut v: Vec<&Trade> = trades.into_iter().collect();
        v.sort_by(|a, b| (a.exit_time, &a.id).cmp(&(b.exit_time, &b.id)));
        let mut s = Stats::default();
        let (mut run, mut peak) = (Money::ZERO, Money::ZERO);
        for t in v {
            let n = t.net();
            s.count += 1;
            s.net += n;
            s.fees += t.fees;
            if n > Money::ZERO {
                s.wins += 1;
                s.gross_win += n;
                s.streak = s.streak.max(0) + 1;
            } else if n < Money::ZERO {
                s.losses += 1;
                s.gross_loss += n;
                s.streak = s.streak.min(0) - 1;
            } else {
                s.streak = 0;
            }
            s.longest_win_streak = s.longest_win_streak.max(s.streak.max(0) as u32);
            s.longest_loss_streak = s.longest_loss_streak.max((-s.streak).max(0) as u32);
            run += n;
            if run > peak {
                peak = run;
            }
            if peak - run > s.max_drawdown {
                s.max_drawdown = peak - run;
            }
        }
        s
    }

    /// Wins ÷ trades.
    pub fn win_rate(&self) -> Option<f64> {
        (self.count > 0).then(|| f64::from(self.wins) / f64::from(self.count))
    }

    pub fn avg_win(&self) -> Option<Money> {
        (self.wins > 0).then(|| Money(self.gross_win.0 / i128::from(self.wins)))
    }

    /// Negative (or zero).
    pub fn avg_loss(&self) -> Option<Money> {
        (self.losses > 0).then(|| Money(self.gross_loss.0 / i128::from(self.losses)))
    }

    /// Net per trade.
    pub fn expectancy(&self) -> Option<Money> {
        (self.count > 0).then(|| Money(self.net.0 / i128::from(self.count)))
    }

    /// Gross wins ÷ gross losses; `None` without losses (infinite) or without trades.
    pub fn profit_factor(&self) -> Option<f64> {
        (self.gross_loss < Money::ZERO).then(|| self.gross_win.0 as f64 / -self.gross_loss.0 as f64)
    }
}

/// How to group trades.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum GroupBy {
    TradeDate,
    /// The catalog product (`ES`, `NQ`, …), across contracts.
    Product,
    /// The hour of the entry in the exchange's time zone (UTC without a session calendar),
    /// `"09"`.
    Hour,
    /// The account id.
    Account,
    /// Each tag; a trade with several counts in each, untagged trades under [`UNTAGGED`].
    Tag,
}

/// The group key of trades without tags: empty, which no tag can be (tags are trimmed and
/// empty ones dropped), so it never merges with a user's tag.
pub const UNTAGGED: &str = "";

/// The group keys of a trade (one, except by tag).
pub fn keys(t: &Trade, by: GroupBy) -> Vec<String> {
    match by {
        GroupBy::TradeDate => vec![t.trade_date.to_string()],
        GroupBy::Product => vec![lines_feed::catalog::get(t.contract.product).map_or_else(
            || format!("{:?}", t.contract.product),
            |p| p.symbol.to_owned(),
        )],
        GroupBy::Hour => vec![format!("{:02}", entry_hour(t))],
        GroupBy::Account => vec![t.account.0.clone()],
        GroupBy::Tag if t.tags.is_empty() => vec![UNTAGGED.to_owned()],
        GroupBy::Tag => t.tags.clone(),
    }
}

/// The entry's hour in the exchange's time zone.
pub fn entry_hour(t: &Trade) -> u8 {
    exchange_hour(t.contract.product, t.entry_time)
}

pub fn exchange_hour(product: lines_core::ProductId, at: UtcMillis) -> u8 {
    match lines_feed::catalog::sessions(product) {
        Some(cal) => cal.local(at).1.hour(),
        None => (at.0.rem_euclid(86_400_000) / 3_600_000) as u8,
    }
}

/// Stats per group, keys in order.
pub fn grouped<'a>(
    trades: impl IntoIterator<Item = &'a Trade>,
    by: GroupBy,
) -> BTreeMap<String, Stats> {
    let mut groups: BTreeMap<String, Vec<&Trade>> = BTreeMap::new();
    for t in trades {
        for k in keys(t, by) {
            groups.entry(k).or_default().push(t);
        }
    }
    groups.into_iter().map(|(k, v)| (k, Stats::of(v))).collect()
}

/// One account's progress through a consistency rule (a prop preset's: the best trade date may
/// hold at most `max_pct` percent of the total profit), from its closed trades in the journal:
/// net after fees per trade date. Open trades don't count, so it can differ from the rule
/// strip's figure while a position is open (that one counts today's open P&L).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConsistencyProgress {
    pub max_pct: u8,
    /// Σ net of every trade date.
    pub total: Money,
    /// The best trade date and its net (`None`: no trades).
    pub best_day: Option<(lines_core::CivilDate, Money)>,
    /// The best day's share of the total, whole percent rounded down (the rule strip's
    /// arithmetic); `None` without a total profit.
    pub best_pct: Option<u8>,
    /// Within the rule, exactly (`None` without a total profit: nothing to measure yet).
    pub ok: Option<bool>,
    /// More total profit needed for the best day to fit (zero when it does).
    pub needed: Money,
}

/// [`ConsistencyProgress`] over `trades` (one account's), for a rule of `max_pct` percent;
/// `None` for a rule of 0 % (nothing can meet it).
pub fn consistency<'a>(
    trades: impl IntoIterator<Item = &'a Trade>,
    max_pct: u8,
) -> Option<ConsistencyProgress> {
    if max_pct == 0 {
        return None;
    }
    let mut days: BTreeMap<lines_core::CivilDate, Money> = BTreeMap::new();
    for t in trades {
        *days.entry(t.trade_date).or_insert(Money::ZERO) += t.net();
    }
    let total = days.values().fold(Money::ZERO, |a, &b| a + b);
    // The first of equal best days (dates in order).
    let best_day = days.iter().fold(
        None,
        |best: Option<(lines_core::CivilDate, Money)>, (&d, &m)| match best {
            Some((_, b)) if b >= m => best,
            _ => Some((d, m)),
        },
    );
    let best = best_day.map_or(Money::ZERO, |(_, m)| m.max(Money::ZERO));
    let best_pct = (total > Money::ZERO).then(|| ((best.0 * 100) / total.0).clamp(0, 255) as u8);
    // best × 100 ≤ max × total  ⇔  total ≥ ⌈best × 100 / max⌉.
    let max = i128::from(max_pct);
    let required = Money((best.0 * 100 + max - 1) / max);
    Some(ConsistencyProgress {
        max_pct,
        total,
        best_day,
        best_pct,
        // Exact, not from the rounded percent: 30.4 % is over a 30 % rule.
        ok: (total > Money::ZERO).then(|| best.0 * 100 <= max * total.0),
        needed: (required - total).max(Money::ZERO),
    })
}
