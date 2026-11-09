//! Reverse as one step: close the position and open the same size on the other side.
//!
//! - Every order on the contract is cancelled (the position's brackets and exits too). Where the
//!   broker honors reduce-only (`BrokerCaps::oco`), the position's protective exits the core knows
//!   are reduce-only stay until it is flat (`core/flatten.rs` keeps them by role; a Reverse asks
//!   for the flag too, since one kept through the flip must never add to the new side): if the
//!   close can't happen they still protect it. A leg the broker never reported reduce-only is
//!   cancelled at once, so a close that then fails leaves the position without it.
//! - Two market orders, never one of twice the size a stale exit could overfill: the close is a
//!   Flatten's own (reduce-only, `Role::Flatten`), the opening a separate entry for the position's
//!   size (with its bracket in per-entry mode). Neither can leave a doubled position: the close
//!   stops at flat, the opening is only ever the old size.
//! - A broker that doesn't honor reduce-only gets nothing new until the cancels are answered (at
//!   most [`FLATTEN_ANSWER_MS`]). Past that wait an unanswered order could still fill beside the
//!   close: the close goes, as a Flatten's would, and the opening is refused, saying so.
//! - The opening is an entry: guardrails, lockouts, prop rules and risk limits check it before
//!   anything is sent (a refusal sends nothing), and again when it goes after a wait (a refusal
//!   then is a refused order, the close going on).
//! - One at a time: a second Reverse, or one while a Flatten is closing the position, is refused.
//!   A Flatten (anyone's) or a person's Cancel All takes back a Reverse still waiting, which says
//!   so as a refused order.

use lines_core::{ContractKey, Price, Qty, UtcMillis};

use super::{FLATTEN_ANSWER_MS, TradingCore};
use crate::broker::BrokerRequest;
use crate::command::{BracketSpec, Command, Envelope, OrderSpec, Reject, RejectCode};
use crate::order::{OrderState, Role};
use crate::types::{AccountId, OrderId, OrderKind, Side};

/// A Reverse from its press until both its orders are done.
pub(super) struct ReverseRun {
    account: AccountId,
    contract: ContractKey,
    env: Envelope,
    since: UtcMillis,
    /// The new position: its side and size.
    open: OrderSpec,
    /// The new position's bracket (per-entry mode).
    bracket: Option<(Option<Price>, Option<Price>)>,
    /// Waiting for the contract's cancels (a broker that doesn't honor reduce-only).
    waiting: bool,
    /// It didn't go at once: anything placed on the contract since may need cancelling too.
    deferred: bool,
    /// Its orders once sent.
    close: Option<OrderId>,
    opening: Option<OrderId>,
}

/// What a refused opening says.
fn opening_refused(why: &str) -> String {
    format!(
        "can't open the new position: {why}; nothing was sent, the position and its orders stand"
    )
}

impl TradingCore {
    /// `Command::Reverse`.
    pub(super) fn reverse(
        &mut self,
        env: &Envelope,
        now: UtcMillis,
        account: &AccountId,
        contract: ContractKey,
        take_profit: Option<Price>,
        stop_loss: Option<Price>,
    ) -> Result<Vec<OrderId>, Reject> {
        // A person's command: a strategy or rule closes and opens as two decisions of its own.
        if crate::broker::Origin::of(env.source) != crate::broker::Origin::Manual {
            return Err(Reject::new(
                RejectCode::Invalid,
                "Reverse is a person's command: close and open as two orders",
            ));
        }
        let pos = self.position(account, contract);
        if pos == 0 {
            return Err(Reject::new(RejectCode::Invalid, "no position to reverse"));
        }
        if !self.broker.connected() {
            return Err(Reject::new(
                RejectCode::Broker,
                "the broker is disconnected: nothing was sent, your orders stand; reverse when it is back",
            ));
        }
        if self.reverse_under_way(account, contract, now) {
            return Err(Reject::new(
                RejectCode::Invalid,
                "a Reverse is already on its way for this position",
            ));
        }
        if self.flatten_under_way(account, contract, now) {
            return Err(Reject::new(
                RejectCode::Invalid,
                "a Flatten is closing this position: reverse once it is done",
            ));
        }
        // An order with no known outcome can't be cancelled and could fill beside the new
        // position: refused now, while nothing has gone, rather than closing and not opening.
        let positions = [(contract, pos)];
        if self.orders.values().any(|o| {
            o.state == OrderState::Unresolved
                && &o.account == account
                && o.contract == contract
                && !self.reverse_kept(o, &positions)
        }) {
            return Err(Reject::new(
                RejectCode::Invalid,
                "an order on this contract has no known outcome: nothing was sent; reverse once it's settled",
            ));
        }
        let open = OrderSpec {
            account: account.clone(),
            contract,
            side: if pos > 0 { Side::Sell } else { Side::Buy },
            qty: Qty(pos.abs()),
            kind: OrderKind::Market,
        };
        // Per-entry mode: the new position gets its bracket as a fresh entry would. In position
        // mode its pair follows the flip (`core/position_bracket.rs`).
        let bracket = (take_profit.is_some() || stop_loss.is_some())
            && !self
                .position_brackets
                .contains_key(&(account.clone(), contract));
        let bracket = bracket.then_some((take_profit, stop_loss));
        if bracket.is_some() {
            self.bracket_supported(&open)?;
        }
        self.reverse_check(now, &open, pos)
            .map_err(|r| Reject::new(r.code, opening_refused(&r.message)))?;
        if let Some((tp, sl)) = bracket {
            self.bracket_exits_valid(&BracketSpec {
                entry: open.clone(),
                take_profit: tp,
                stop_loss: sl,
            })?;
        }
        // Every order on the contract goes, whoever placed it; where reduce-only holds, the
        // position's reduce-only exits stay until it is flat.
        let cancel = self
            .orders
            .values()
            .filter(|o| &o.account == account && o.contract == contract)
            .filter(|o| !self.reverse_kept(o, &positions))
            .map(|o| o.id)
            .collect();
        let mut ids = self.cancel_scoped(env, now, account, Some(contract), Some(cancel));
        let mut run = ReverseRun {
            account: account.clone(),
            contract,
            env: env.clone(),
            since: now,
            open,
            bracket,
            waiting: true,
            deferred: false,
            close: None,
            opening: None,
        };
        if self.broker.capabilities().oco || !self.flatten_waits(account, contract) {
            ids.extend(self.reverse_send(now, &mut run, false, false));
        } else {
            run.deferred = true;
        }
        self.reverses.push(run);
        Ok(ids)
    }

    /// The opening's checks, as an entry: the whole reversal (the close and the opening, one
    /// order of their total) for guardrails and prop rules, which know a reversal from an entry;
    /// the opening alone, from flat, for risk limits (order size, value, position). The orders
    /// on the contract, about to be cancelled, are left out.
    fn reverse_check(&mut self, now: UtcMillis, open: &OrderSpec, pos: i64) -> Result<(), Reject> {
        let whole = OrderSpec {
            qty: Qty(open.qty.0 + pos.abs()),
            ..open.clone()
        };
        self.reverse_checking = Some((open.account.clone(), open.contract, open.qty));
        let r = self.check_new(now, &whole);
        self.reverse_checking = None;
        r
    }

    /// Send a Reverse's orders: the close (what the position is now: an exit may have filled
    /// while it waited) and the opening. `late`: cancels went unanswered, so the opening is
    /// refused rather than risk doubling. After a wait the opening is checked again.
    fn reverse_send(
        &mut self,
        now: UtcMillis,
        run: &mut ReverseRun,
        mut late: bool,
        cap: bool,
    ) -> Vec<OrderId> {
        run.waiting = false;
        let (account, contract) = (run.account.clone(), run.contract);
        let mut ids = Vec::new();
        let mut env = run.env.clone();
        let waited = run.deferred;
        if waited {
            // Anything placed while it waited (no cancel asked yet) goes too, and it waits for
            // those answers as well, under the same cap: an order filling beside the opening
            // would double it. Orders whose cancel was already asked aren't asked again.
            let new: std::collections::BTreeSet<OrderId> = self
                .orders
                .values()
                .filter(|o| o.is_open() && o.account == account && o.contract == contract)
                .filter(|o| !self.cancelling.contains_key(&o.id))
                .map(|o| o.id)
                .collect();
            if !new.is_empty() {
                self.cancel_scoped(&env, now, &account, Some(contract), Some(new));
                if !cap {
                    run.waiting = true;
                    return ids;
                }
                late = true;
            }
            // Click to sent counts from here.
            env.issued = std::time::Instant::now();
        }
        // An order whose cancel the broker refused, or that ran out of sends, still works (or
        // may: one a rebuild left with no known outcome): it could fill beside the opening. Not
        // an exit kept to protect the position: reduce-only, it can't add to the new side.
        let kept = [(contract, self.position(&account, contract))];
        let stuck = self.orders.values().any(|o| {
            !o.state.is_terminal()
                && o.account == account
                && o.contract == contract
                && (self.cancel_gave_up(o.id) || o.state == OrderState::Unresolved)
                && !self.reverse_kept(o, &kept)
        });
        let q = self.position(&account, contract);
        // The old side only: an exit that filled meanwhile may have left it flat.
        let held = q != 0 && q.signum() == -run.open.side.sign();
        if held {
            let id = self.flatten_send(&env, now, &account, contract, q);
            run.close = Some(id);
            ids.push(id);
        }
        let refused = if !held {
            // Closed while it waited (an exit filled): the reversal was asked of a position
            // that's gone, so nothing new opens.
            Some("the position closed while it waited".to_owned())
        } else if stuck {
            Some(
                "an order on the contract refused its cancel or has no known outcome, and could fill beside it"
                    .to_owned(),
            )
        } else if late {
            Some(
                "an order on the contract never answered its cancel and could have filled beside it"
                    .to_owned(),
            )
        } else if waited {
            self.reverse_check(now, &run.open, q.abs())
                .err()
                .map(|r| r.message)
        } else {
            None
        };
        if let Some(why) = refused {
            // Said as a refused order: the close (if any) is on its way regardless.
            let id = self.new_order(now, &run.open, Role::Single, None).id;
            let done = if held {
                "the Reverse closed the position but didn't open the new one"
            } else {
                "the Reverse didn't open the new position"
            };
            let reason = format!("{done}: {why}");
            self.transition(id, OrderState::Rejected { reason }, now);
            return ids;
        }
        let opening = match run.bracket {
            Some((take_profit, stop_loss)) => {
                let b = BracketSpec {
                    entry: run.open.clone(),
                    take_profit,
                    stop_loss,
                };
                let sent = self.bracket_send(&env, now, &b);
                ids.extend(sent.iter().skip(1));
                sent[0]
            }
            None => {
                let o = self.new_order(now, &run.open, Role::Single, None);
                let id = o.id;
                self.send(&env, now, BrokerRequest::Place(o));
                id
            }
        };
        run.opening = Some(opening);
        ids.push(opening);
        ids
    }

    /// Reverses waiting for their cancels go once they're answered or the venue has had long
    /// enough; disconnected, they end as a refused order saying so. Done ones are forgotten.
    pub(super) fn reverse_pending(&mut self, now: UtcMillis) {
        let up = self.broker.connected();
        let mut runs = std::mem::take(&mut self.reverses);
        for run in runs.iter_mut().filter(|r| r.waiting) {
            if !up {
                run.waiting = false;
                let id = self.new_order(now, &run.open, Role::Single, None).id;
                let reason = "the broker disconnected before the Reverse could go: the position is still open and its orders may be canceled".to_owned();
                self.transition(id, OrderState::Rejected { reason }, now);
                continue;
            }
            let cap = now.0.saturating_sub(run.since.0) >= FLATTEN_ANSWER_MS;
            let waits = self.flatten_waits(&run.account, run.contract);
            if waits && !cap {
                continue;
            }
            self.reverse_send(now, run, waits, cap);
        }
        runs.retain(|r| self.run_under_way(r, now));
        // Started while this ran: none (commands don't come in here), but keep any.
        runs.append(&mut self.reverses);
        self.reverses = runs;
    }

    /// An exit kept to protect the position that can't add to the new side: reduce-only. A bracket
    /// leg a broker keeps but doesn't mark reduce-only could fill after the flip.
    fn reverse_kept(&self, o: &crate::order::Order, positions: &[(ContractKey, i64)]) -> bool {
        o.reduce_only && self.flatten_keeps(o, positions)
    }

    /// A Flatten (anyone's: it closes) or a person's Cancel All takes back a Reverse still
    /// waiting, which then says so as a refused order. A rule's or strategy's Cancel All leaves it:
    /// the person asked for the Reverse.
    pub(super) fn reverse_take_back(&mut self, env: &Envelope, now: UtcMillis) {
        let person = crate::broker::Origin::of(env.source) == crate::broker::Origin::Manual;
        let (account, contract, word) = match &env.command {
            Command::Flatten { account, contract } => (account.clone(), *contract, "a Flatten"),
            Command::CancelAll { account, contract } if person => {
                (account.clone(), *contract, "your Cancel All")
            }
            _ => return,
        };
        let (gone, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.reverses)
            .into_iter()
            .partition(|r| {
                r.waiting && r.account == account && contract.is_none_or(|c| c == r.contract)
            });
        self.reverses = kept;
        for r in gone {
            let id = self.new_order(now, &r.open, Role::Single, None).id;
            let reason = format!("the Reverse was taken back by {word} before it went");
            self.transition(id, OrderState::Rejected { reason }, now);
        }
    }

    fn run_under_way(&self, r: &ReverseRun, now: UtcMillis) -> bool {
        let live = |id: Option<OrderId>| {
            id.and_then(|i| self.orders.get(&i)).is_some_and(|o| {
                o.is_open()
                    || (o.state == OrderState::Unresolved
                        && now.0.saturating_sub(o.updated.0) < FLATTEN_ANSWER_MS)
            })
        };
        r.waiting || live(r.close) || live(r.opening)
    }

    /// A Reverse on this contract is waiting or has an order still working.
    pub(super) fn reverse_under_way(
        &self,
        account: &AccountId,
        contract: ContractKey,
        now: UtcMillis,
    ) -> bool {
        self.reverses
            .iter()
            .any(|r| &r.account == account && r.contract == contract && self.run_under_way(r, now))
    }

    /// When a waiting Reverse stops waiting.
    pub(super) fn reverse_wake(&self) -> Option<UtcMillis> {
        self.reverses
            .iter()
            .filter(|r| r.waiting)
            .map(|r| UtcMillis(r.since.0 + FLATTEN_ANSWER_MS))
            .min()
    }
}
