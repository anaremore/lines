//! Reset a paper account: back to its starting balance with nothing open, the way a new one
//! starts.
//!
//! Never under a live core by deleting its file: the reset goes through the core like any other
//! command. It flattens and cancels everything on the account first (the same Flatten a person
//! sends, with its protections), and only once the account is flat, with no order open or
//! unresolved, does the reset happen: the broker's balance and positions, then the core's
//! balance, positions and prop-rule progress (high-water mark, today's P&L, breaches). A
//! flatten still working leaves the reset waiting, and the account says so. A waiting reset never
//! goes off later on its own: it is dropped (`Event::ResetDropped`) when its flatten ends without
//! closing the position (refused, or ended with the session) or when a new order is placed on
//! the account, and the trader asks again (with its confirmation) if they still want it.
//!
//! What a reset keeps: the account's settings (starting balance, limits, prop rules) and the
//! trader's guardrails with their counts, any lockout, and the day's P&L so far as the daily
//! loss guardrail counts it (`GuardrailTracker::carried`, until the next trade date). A reset
//! is not a way out of a limit the trader set, as a looser guardrail never is
//! (`Command::SetGuardrails`). The P&L shown and prop progress do start over.
//!
//! Paper only: a broker account (any broker that isn't simulated) is refused, and says why.

use lines_core::UtcMillis;

use super::TradingCore;
use crate::command::{Command, Envelope, Reject, RejectCode};
use crate::event::Event;
use crate::rules::RuleTracker;
use crate::types::{AccountId, Money, OrderId};

impl TradingCore {
    /// `Command::ResetPaper`: flatten and cancel now, reset once the account is quiet.
    pub(super) fn reset_start(
        &mut self,
        now: UtcMillis,
        env: &Envelope,
        account: &AccountId,
        clear_journal: bool,
    ) -> Result<Vec<OrderId>, Reject> {
        if !self.broker.simulated() {
            let name = self
                .accounts
                .get(account)
                .map_or_else(|| account.to_string(), |a| a.cfg.name.clone());
            return Err(Reject::new(
                RejectCode::Broker,
                format!("{name} is a broker account: only a paper account can be reset"),
            ));
        }
        // Everything open goes first, through the same Flatten a person sends.
        let flatten = Envelope {
            command: Command::Flatten {
                account: account.clone(),
                contract: None,
            },
            ..env.clone()
        };
        let ids = self.run(now, &flatten)?;
        self.trigger_person_acted(&flatten);
        // The reset asked last decides whether the journal goes.
        self.resets.insert(account.clone(), clear_journal);
        self.events.push(Event::ResetWaiting {
            account: account.clone(),
        });
        self.reset_pending(now);
        Ok(ids)
    }

    /// Whether the account holds nothing and has nothing open or unresolved at the broker.
    fn reset_quiet(&self, account: &AccountId) -> bool {
        self.accounts
            .get(account)
            .is_some_and(|a| a.positions.values().all(|p| p.qty() == 0))
            && !self
                .orders
                .values()
                .any(|o| &o.account == account && !o.state.is_terminal())
    }

    /// Whether something may still close the account's position: an order open or unresolved
    /// on it, or a Flatten waiting to send its market order.
    fn reset_closing(&self, account: &AccountId, now: UtcMillis) -> bool {
        self.orders
            .values()
            .any(|o| &o.account == account && !o.state.is_terminal())
            || self.accounts.get(account).is_some_and(|a| {
                a.positions
                    .iter()
                    .filter(|(_, p)| p.qty() != 0)
                    .any(|(k, _)| self.flatten_under_way(account, *k, now))
            })
    }

    /// Reset every account waiting for it whose flatten is done; drop the ones whose flatten
    /// ended with the position still open.
    pub(super) fn reset_pending(&mut self, now: UtcMillis) {
        if self.resets.is_empty() {
            return;
        }
        let waiting: Vec<AccountId> = self.resets.keys().cloned().collect();
        for account in waiting {
            if self.reset_quiet(&account) {
                let clear_journal = self.resets.remove(&account).unwrap_or(false);
                self.reset_now(now, &account, clear_journal);
            } else if !self.reset_closing(&account, now) {
                self.reset_drop(
                    &account,
                    "Reset not done: its flatten ended with the position still open",
                );
            }
        }
    }

    /// A new order on an account waiting for its reset drops the reset (it never resets the
    /// trading done since).
    pub(super) fn reset_on_new_order(&mut self, account: &AccountId) {
        if self.resets.contains_key(account) {
            self.reset_drop(
                account,
                "Reset not done: a new order was placed on the account",
            );
        }
    }

    fn reset_drop(&mut self, account: &AccountId, reason: &str) {
        self.resets.remove(account);
        self.events.push(Event::ResetDropped {
            account: account.clone(),
            reason: reason.to_owned(),
        });
    }

    fn reset_now(&mut self, now: UtcMillis, account: &AccountId, clear_journal: bool) {
        let Some(a) = self.accounts.get(account) else {
            return;
        };
        let start = Money::whole(a.cfg.starting_balance);
        if !self.broker.reset_account(now, account, start) {
            self.reset_drop(
                account,
                "Reset not done: this broker can't reset an account",
            );
            return;
        }
        let contracts: Vec<_> = a.positions.keys().copied().collect();
        // The day so far (the account is flat: all realised), for the daily-loss guardrail.
        let day_so_far = a.balance - a.tracker.day_open_balance;
        let a = self.accounts.get_mut(account).expect("checked");
        a.guard.carried = a.guard.carried + day_so_far;
        a.balance = start;
        a.positions.clear();
        // Prop-rule progress starts over: high-water mark, today's opening balance (so today's
        // P&L), finished days and breaches. The day itself is set again from `now`.
        a.tracker = RuleTracker::new(start);
        a.restore_day = None;
        for c in contracts {
            self.position_event(account, c);
        }
        self.events.push(Event::AccountReset {
            account: account.clone(),
            balance: start,
            at: now,
            clear_journal,
        });
        self.account_changed(now, account);
    }

    /// Accounts whose reset waits for their flatten (the UI says so).
    pub fn resets_waiting(&self) -> impl Iterator<Item = &AccountId> {
        self.resets.keys()
    }
}
