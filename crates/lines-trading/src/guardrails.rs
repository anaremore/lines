//! Discipline guardrails (roadmap 8b; decision 0006: risk in the OMS, rules as data): the
//! trader's own limits, tighter than any prop rule, which they can't switch off mid-tilt.
//!
//! - **Max trades per trade date.** A trade is a round trip: flat → position → flat. Partial
//!   fills don't count separately; a reversal (long → short in one fill) closes one trade and
//!   opens the next. Reaching the maximum locks entries until the next trade date.
//! - **Losing streak → cooldown.** After N losing trades in a row (a trade's P&L: realised
//!   P&L minus every fee of its fills; a loss is below zero, anything else resets the streak),
//!   entries wait M minutes. The streak starts again after the cooldown and on a new trade date.
//! - **Max daily loss** (money): the trade date's P&L (realised + unrealised, fees included)
//!   reaching `−limit` locks entries until the next trade date. Separate from a prop preset's
//!   daily loss: this is the trader's own, tighter limit.
//! - **Max position** (quantity units, worst case with open orders, as `RiskLimits`): denied
//!   above it; no lockout.
//! - **No new entries in the last N minutes before the session closes.**
//! - **News blackout** ([`crate::news`]): around scheduled releases, warn, refuse entries, or
//!   refuse entries and flatten. Checked by the core with the economic calendar; a preset's
//!   rule and this one combine part by part.
//!
//! **Exits are never blocked**: Flatten, Cancel All and cancels never reach the guardrails,
//! and an order that only shrinks the position (no more than is left to close) always passes.
//!
//! **Tighten now, loosen tomorrow** ([`GuardrailTracker::request`]): each field of a new
//! request that is tighter than what is in force applies at once; each looser one waits for
//! the next trade date. A lockout can't be cleared early: it ends at its time (cooldown) or
//! with the trade date (locked), never by a settings change.
//!
//! Everything here is pure (time is passed in). The tracker is saved with the account in the
//! trading file, so a restart doesn't reset a lockout.

use lines_core::{CivilDate, ContractKey, UtcMillis};
use serde::{Deserialize, Serialize};

use crate::news::NewsBlackout;
use crate::types::{Money, Side, contract_serde};

/// A trader's guardrails. Every rule is optional (`None`: off).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Guardrails {
    /// Round trips per trade date.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_trades_per_day: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub losing_streak: Option<LosingStreak>,
    /// Whole currency units of loss per trade date (realised + unrealised, fees included).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_daily_loss: Option<i64>,
    /// Quantity units per contract, worst case (position plus open orders on one side).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_position: Option<i64>,
    /// No new entries this many minutes before the session closes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_entries_before_close_min: Option<u32>,
    /// Around scheduled economic releases (roadmap 8b).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub news: Option<NewsBlackout>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LosingStreak {
    /// Losing trades in a row that start a cooldown.
    pub losses: u32,
    /// How long entries wait, minutes.
    pub cooldown_min: u32,
}

/// The tighter of two optional maxima (a limit is tighter than none).
fn tighter_max<T: Ord + Copy>(a: Option<T>, b: Option<T>) -> Option<T> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (x, y) => x.or(y),
    }
}

impl Guardrails {
    pub fn is_default(&self) -> bool {
        *self == Guardrails::default()
    }

    /// Field by field, the tighter of `self` and `other`. A losing streak is tighter with
    /// fewer losses or a longer cooldown (each part on its own).
    pub fn tightest(&self, other: &Guardrails) -> Guardrails {
        Guardrails {
            max_trades_per_day: tighter_max(self.max_trades_per_day, other.max_trades_per_day),
            losing_streak: match (self.losing_streak, other.losing_streak) {
                (Some(a), Some(b)) => Some(LosingStreak {
                    losses: a.losses.min(b.losses),
                    cooldown_min: a.cooldown_min.max(b.cooldown_min),
                }),
                (a, b) => a.or(b),
            },
            max_daily_loss: tighter_max(self.max_daily_loss, other.max_daily_loss),
            max_position: tighter_max(self.max_position, other.max_position),
            // More minutes before the close is tighter.
            no_entries_before_close_min: match (
                self.no_entries_before_close_min,
                other.no_entries_before_close_min,
            ) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, b) => a.or(b),
            },
            news: NewsBlackout::tighter(self.news, other.news),
        }
    }
}

/// What an account may do under its guardrails.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuardState {
    #[default]
    Active,
    /// Entries wait until a time (a losing streak).
    Cooldown,
    /// No entries until the next trade date (max trades, daily loss).
    Locked,
}

/// A lockout in force: its kind, why, and when it ends.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lockout {
    pub state: GuardState,
    /// Short, US English, without the time left (see [`Lockout::message`]).
    pub reason: String,
    /// When it ends: the cooldown's end, or the end of the trade date.
    #[serde(with = "crate::types::time_serde")]
    pub until: UtcMillis,
    /// Locked: the trade date it lasts (it ends when the trade date changes, even if the
    /// session calendar moved `until`).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::rules::opt_date"
    )]
    pub date: Option<CivilDate>,
}

impl Lockout {
    /// The denial text: "Cooldown: 3 losses in a row, 12 min left",
    /// "Locked: max 5 trades reached, until the next trade date".
    pub fn message(&self, now: UtcMillis) -> String {
        match self.state {
            GuardState::Cooldown => {
                let min = ((self.until.0 - now.0).max(0) + 59_999) / 60_000;
                format!("Cooldown: {}, {min} min left", self.reason)
            }
            _ => format!("Locked: {}, until the next trade date", self.reason),
        }
    }
}

/// What a fill did to a round trip (flat → position → flat), from the signed position
/// before and after it. The one definition of a trade, shared by the guardrails' counts and
/// the journal (`crate::journal`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RoundTrip {
    /// Flat before and after (nothing to count).
    None,
    /// From flat: a trade starts.
    Opens,
    /// Same side before and after (an add or a partial exit).
    Continues,
    /// Back to flat: the trade ends.
    Closes,
    /// Through flat to the other side in one fill: one trade ends, the next starts.
    Reverses,
}

impl RoundTrip {
    pub fn of(before: i64, after: i64) -> RoundTrip {
        match (before, after) {
            (0, 0) => RoundTrip::None,
            (0, _) => RoundTrip::Opens,
            (_, 0) => RoundTrip::Closes,
            (b, a) if a.signum() != b.signum() => RoundTrip::Reverses,
            _ => RoundTrip::Continues,
        }
    }
}

/// A round trip in progress.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenTrade {
    #[serde(with = "contract_serde")]
    pub contract: ContractKey,
    /// Realised P&L minus fees so far.
    pub pnl: Money,
}

/// An order as the guardrails see it.
#[derive(Copy, Clone, Debug)]
pub struct Entry {
    pub side: Side,
    pub qty: i64,
    /// Signed position in the order's contract.
    pub position: i64,
    /// Open quantity of other orders in the contract, per side (bracket exits excluded).
    pub open_buy: i64,
    pub open_sell: i64,
    /// When the current session of the order's product closes (if it has a calendar).
    pub session_close: Option<UtcMillis>,
}

impl Entry {
    /// What may still be closed on this order's side: the position less what open orders
    /// already close.
    pub fn closable(&self) -> i64 {
        match self.side {
            Side::Sell if self.position > 0 => (self.position - self.open_sell).max(0),
            Side::Buy if self.position < 0 => (-self.position - self.open_buy).max(0),
            _ => 0,
        }
    }

    /// Only shrinks the position: always allowed.
    pub fn is_exit(&self) -> bool {
        self.qty <= self.closable()
    }

    /// Goes through flat to the other side (closes one trade and opens the next).
    fn reverses(&self) -> bool {
        self.position != 0 && !self.is_exit() && self.closable() > 0
    }
}

/// A change of guardrail state, for [`crate::Event::Guardrail`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuardChange {
    pub state: GuardState,
    pub reason: Option<String>,
    pub until: Option<UtcMillis>,
}

/// One account's guardrails in force and its progress through them. Saved with the account.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuardrailTracker {
    /// In force now.
    #[serde(default, skip_serializing_if = "Guardrails::is_default")]
    pub active: Guardrails,
    /// Asked for, looser than what is in force: takes effect with the next trade date.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<Guardrails>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::rules::opt_date"
    )]
    pub day: Option<CivilDate>,
    /// Round trips finished this trade date.
    #[serde(default)]
    pub trades: u32,
    /// Losing trades in a row.
    #[serde(default)]
    pub streak: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open: Vec<OpenTrade>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock: Option<Lockout>,
    /// The day's P&L before a paper reset this trade date (`Command::ResetPaper`): the daily
    /// loss guardrail still counts it, so a reset is no way around it. Gone with the next trade
    /// date. Optional: older files have none.
    #[serde(default, skip_serializing_if = "crate::types::Money::is_zero")]
    pub carried: crate::types::Money,
}

impl GuardrailTracker {
    pub fn is_default(&self) -> bool {
        *self == GuardrailTracker::default()
    }

    pub fn state(&self) -> GuardState {
        self.lock.as_ref().map_or(GuardState::Active, |l| l.state)
    }

    /// The change to report, as the state is now.
    pub fn change(&self) -> GuardChange {
        GuardChange {
            state: self.state(),
            reason: self.lock.as_ref().map(|l| l.reason.clone()),
            until: self.lock.as_ref().map(|l| l.until),
        }
    }

    /// New guardrails asked for (settings). Tighter fields apply at once, looser ones with the
    /// next trade date; a lockout in force is untouched either way.
    pub fn request(&mut self, wanted: Guardrails) {
        let now = self.active.tightest(&wanted);
        self.pending = (now != wanted).then_some(wanted);
        self.active = now;
    }

    /// The trade date is `date`: on a new one, pending (looser) guardrails take effect, the
    /// counts restart and a lock for the old date ends.
    pub fn roll_day(&mut self, date: CivilDate) {
        if self.day == Some(date) {
            return;
        }
        if self.day.is_some() {
            if let Some(p) = self.pending.take() {
                self.active = p;
            }
            self.trades = 0;
            self.streak = 0;
            self.carried = crate::types::Money::ZERO;
        }
        self.day = Some(date);
        if self
            .lock
            .as_ref()
            .is_some_and(|l| l.state == GuardState::Locked && l.date != Some(date))
        {
            self.lock = None;
        }
    }

    /// A cooldown whose time has come ends.
    pub fn expire(&mut self, now: UtcMillis) {
        if self
            .lock
            .as_ref()
            .is_some_and(|l| l.state == GuardState::Cooldown && now >= l.until)
        {
            self.lock = None;
        }
    }

    /// When the lockout ends: a cooldown's end, or the end of the locked trade date (the core
    /// wakes then, so the end is reported even if nothing else happens).
    pub fn next_wake(&self) -> Option<UtcMillis> {
        self.lock.as_ref().map(|l| l.until)
    }

    /// A day lock still in force at `now` past its `until` (the calendar put the date's end
    /// later than first thought): it lasts until `until` instead. Never shortens a lock.
    pub fn extend_lock(&mut self, now: UtcMillis, until: UtcMillis) {
        if let Some(l) = &mut self.lock
            && l.state == GuardState::Locked
            && l.until <= now
        {
            l.until = until.max(UtcMillis(now.0 + 60_000));
        }
    }

    /// Lock until the end of the trade date (never weakened by a cooldown).
    fn lock_day(&mut self, reason: String, day_end: UtcMillis) {
        if self.state() == GuardState::Locked {
            return;
        }
        self.lock = Some(Lockout {
            state: GuardState::Locked,
            reason,
            until: day_end,
            date: self.day,
        });
    }

    /// A fill moved the position in `contract` from `before` to `after`; `pnl` is what it
    /// realised minus its fee. Counts round trips, losing streaks, and locks.
    pub fn on_fill(
        &mut self,
        contract: ContractKey,
        before: i64,
        after: i64,
        pnl: Money,
        now: UtcMillis,
        day_end: UtcMillis,
    ) {
        let i = self.open.iter().position(|t| t.contract == contract);
        let step = RoundTrip::of(before, after);
        if before == 0 {
            if step == RoundTrip::Opens {
                self.open.push(OpenTrade { contract, pnl });
            }
            return;
        }
        let mut trade = match i {
            Some(i) => self.open.remove(i),
            // A position from before the guardrails knew it (a restart without the tracker):
            // its round trip starts now.
            None => OpenTrade {
                contract,
                pnl: Money::ZERO,
            },
        };
        trade.pnl += pnl;
        let reversed = step == RoundTrip::Reverses;
        if step == RoundTrip::Continues {
            self.open.push(trade);
            return;
        }
        // A round trip finished.
        self.trades += 1;
        if reversed {
            self.open.push(OpenTrade {
                contract,
                pnl: Money::ZERO,
            });
        }
        if trade.pnl < Money::ZERO {
            self.streak += 1;
        } else {
            self.streak = 0;
        }
        if let Some(max) = self.active.max_trades_per_day
            && self.trades >= max
        {
            let s = if max == 1 { "" } else { "s" };
            self.lock_day(format!("max {max} trade{s} reached"), day_end);
        }
        if let Some(ls) = self.active.losing_streak
            && ls.losses > 0
            && self.streak >= ls.losses
        {
            self.streak = 0;
            if self.state() == GuardState::Active {
                self.lock = Some(Lockout {
                    state: GuardState::Cooldown,
                    reason: match ls.losses {
                        1 => "1 loss in a row".to_owned(),
                        n => format!("{n} losses in a row"),
                    },
                    until: UtcMillis(now.0 + ls.cooldown_min as i64 * 60_000),
                    date: None,
                });
            }
        }
    }

    /// The trade date's P&L (realised + unrealised, fees included) moved.
    pub fn on_day_pnl(&mut self, day_pnl: Money, day_end: UtcMillis) {
        if let Some(limit) = self.active.max_daily_loss
            && day_pnl <= -Money::whole(limit)
        {
            self.lock_day(format!("daily loss limit of {limit} reached"), day_end);
        }
    }

    /// May this order go out? Exits always may; entries are checked against the lockout and
    /// every rule. The error is the denial's short reason.
    pub fn check(&self, now: UtcMillis, e: &Entry) -> Result<(), String> {
        if e.is_exit() {
            return Ok(());
        }
        if let Some(l) = &self.lock {
            return Err(l.message(now));
        }
        let g = &self.active;
        if let Some(max) = g.max_trades_per_day {
            // A reversal finishes a trade and starts another: it needs room for both.
            let starting = self.trades + u32::from(e.reverses());
            if e.position == 0 && self.trades >= max || e.reverses() && starting >= max {
                return Err(format!("Max trades: {} of {max} today", self.trades));
            }
        }
        if let Some(max) = g.max_position {
            let (buy, sell) = match e.side {
                Side::Buy => (e.open_buy + e.qty, e.open_sell),
                Side::Sell => (e.open_buy, e.open_sell + e.qty),
            };
            let worst = (e.position + buy).abs().max((e.position - sell).abs());
            if worst > max {
                return Err(format!(
                    "Max position: would be {worst}, your limit is {max}"
                ));
            }
        }
        if let (Some(min), Some(close)) = (g.no_entries_before_close_min, e.session_close)
            && now.0 >= close.0 - min as i64 * 60_000
            && now < close
        {
            return Err(format!(
                "No new entries in the last {min} min before the close"
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
