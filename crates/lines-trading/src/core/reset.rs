//! Reset a paper account: back to its starting balance with nothing open, the way a new one
//! starts.
//!
//! Never under a live core by deleting its file: the reset goes through the core like any other
//! command. It flattens and cancels everything on the account first (the same Flatten a person
//! sends, with its protections), and only once the account is flat, with no order open or
//! unresolved, does the reset happen: the broker's balance and positions, then the core's
//! balance, positions and prop-rule progress (high-water mark, today's P&L, breaches). A
//! flatten that can't fill yet (no market) leaves the reset waiting, and the account says so.
//!
//! What a reset keeps: the account's settings (starting balance, limits, prop rules) and the
//! trader's guardrails with their counts and any lockout. A reset is not a way out of a lock
//! the trader set, as a looser guardrail never is (`Command::SetGuardrails`).
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
        self.resets.insert(account.clone());
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

    /// Reset every account waiting for it whose flatten is done.
    pub(super) fn reset_pending(&mut self, now: UtcMillis) {
        if self.resets.is_empty() {
            return;
        }
        let ready: Vec<AccountId> = self
            .resets
            .iter()
            .filter(|a| self.reset_quiet(a))
            .cloned()
            .collect();
        for account in ready {
            self.resets.remove(&account);
            self.reset_now(now, &account);
        }
    }

    fn reset_now(&mut self, now: UtcMillis, account: &AccountId) {
        let Some(a) = self.accounts.get(account) else {
            return;
        };
        let start = Money::whole(a.cfg.starting_balance);
        if !self.broker.reset_account(now, account, start) {
            return;
        }
        let contracts: Vec<_> = a.positions.keys().copied().collect();
        let a = self.accounts.get_mut(account).expect("checked");
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
        });
        self.account_changed(now, account);
    }

    /// Accounts whose reset waits for their flatten (the UI says so).
    pub fn resets_waiting(&self) -> impl Iterator<Item = &AccountId> {
        self.resets.iter()
    }
}
