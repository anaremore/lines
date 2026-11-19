//! Adding and removing paper accounts (`Command::AddPaperAccount`, `RemovePaperAccount`).
//!
//! A new account is the paper broker's (opened at its starting balance) and the core's (its
//! config, a fresh rule tracker, the guardrails a new account joins with), saved with the rest.
//! Its id is made from its name (`paper-` and the name's letters and digits, made unique), so a
//! later rename of the display name never orphans what is saved under the id. An id is never
//! given twice: a removed account's id is retired (saved in the trading file), since its
//! journal trades keep it. A new account starts with the limits set for new paper accounts this
//! run ([`TradingCore::set_new_paper_limits`]) and the guardrails asked for its name
//! ([`TradingCore::set_join_guardrails`]).
//!
//! Removing one is the reverse, and only of an account with nothing left to lose: flat, no
//! order open or unresolved, no trigger armed, no strategy instance on it and no reset waiting.
//! Anything else is refused and says what to close first; a remove never flattens on its own.
//! It leaves its groups first (`Event::Groups`), then `Event::AccountRemoved`. The last paper
//! account can't be removed: a pane always has one to trade.

use lines_core::UtcMillis;

use super::{AccountConfig, TradingCore};
use crate::command::{Command, Envelope, Reject, RejectCode, Source};
use crate::event::Event;
use crate::rules::RulePreset;
use crate::types::{AccountId, Money, OrderId};

/// The largest starting balance a new paper account takes (whole units).
pub const MAX_START_BALANCE: i64 = 100_000_000;

/// A new account's id from its name: `paper-` and its ASCII letters and digits, lowercased,
/// runs of anything else as one `-` (`Paper 50K eval 2` → `paper-paper-50k-eval-2`); a name
/// with none gives `paper-account`. `taken` decides whether an id is in use: `-2`, `-3`, … are
/// tried after it.
pub fn account_id_for(name: &str, taken: impl Fn(&str) -> bool) -> AccountId {
    let mut slug = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_end_matches('-');
    let base = if slug.is_empty() {
        "paper-account".to_owned()
    } else {
        format!("paper-{slug}")
    };
    if !taken(&base) {
        return AccountId::new(&base);
    }
    let mut n = 2u32;
    loop {
        let id = format!("{base}-{n}");
        if !taken(&id) {
            return AccountId::new(&id);
        }
        n += 1;
    }
}

impl TradingCore {
    /// The limits a paper account added by command starts with (the app: `--max-order-qty`).
    pub fn set_new_paper_limits(&mut self, limits: crate::risk::RiskLimits) {
        self.new_paper_limits = limits;
    }

    /// Restore the ids of removed accounts (the trading file's): never given again.
    pub fn restore_retired(&mut self, ids: impl IntoIterator<Item = AccountId>) {
        self.retired.extend(ids);
    }

    /// The ids of removed accounts, never given again.
    pub fn retired(&self) -> impl Iterator<Item = &AccountId> {
        self.retired.iter()
    }

    /// An account command's result (`None`: not one). Its `Event::Command` is the caller's to
    /// push.
    pub(super) fn account_command(
        &mut self,
        now: UtcMillis,
        env: &Envelope,
    ) -> Option<Result<Vec<OrderId>, Reject>> {
        if !matches!(
            env.command,
            Command::AddPaperAccount { .. } | Command::RemovePaperAccount { .. }
        ) {
            return None;
        }
        if matches!(env.source, Source::Strategy(_)) {
            return Some(Err(Reject::new(
                RejectCode::Strategy,
                "a strategy can't add or remove accounts",
            )));
        }
        if !self.broker.simulated() {
            return Some(Err(Reject::new(
                RejectCode::Broker,
                "accounts are added and removed here on paper only",
            )));
        }
        Some(match env.command.clone() {
            Command::AddPaperAccount {
                name,
                starting_balance,
                rules,
            } => self.add_paper_account(now, &name, starting_balance, rules),
            Command::RemovePaperAccount { account } => self.remove_paper_account(&account),
            _ => unreachable!("matched above"),
        })
    }

    fn add_paper_account(
        &mut self,
        now: UtcMillis,
        name: &str,
        starting_balance: i64,
        rules: Option<RulePreset>,
    ) -> Result<Vec<OrderId>, Reject> {
        let name = crate::group::clean_name_of(name, "account")?;
        if self
            .accounts
            .values()
            .any(|a| crate::group::same_name(&a.cfg.name, &name))
        {
            return Err(Reject::new(
                RejectCode::Invalid,
                format!("there is already an account {name}"),
            ));
        }
        if !(1..=MAX_START_BALANCE).contains(&starting_balance) {
            return Err(Reject::new(
                RejectCode::Invalid,
                format!(
                    "a starting balance is $1 to ${}",
                    super::grouped(MAX_START_BALANCE)
                ),
            ));
        }
        if let Some(r) = &rules
            && r.account_size != starting_balance
        {
            return Err(Reject::new(
                RejectCode::Invalid,
                format!(
                    "those prop rules are for a ${} account",
                    super::grouped(r.account_size)
                ),
            ));
        }
        // Never an id in use, removed (its journal trades keep it) or held by the broker.
        let at_broker: std::collections::BTreeSet<AccountId> = self
            .broker
            .snapshot()
            .accounts
            .into_iter()
            .map(|a| a.id)
            .collect();
        let id = account_id_for(&name, |s| {
            let id = AccountId::new(s);
            self.accounts.contains_key(&id) || self.retired.contains(&id) || at_broker.contains(&id)
        });
        if !self
            .broker
            .open_account(&id, Money::whole(starting_balance))
        {
            return Err(Reject::new(
                RejectCode::Broker,
                "the paper broker couldn't open the account",
            ));
        }
        let mut cfg = AccountConfig::paper(&id.0, starting_balance);
        cfg.name = name;
        cfg.rules = rules;
        // The limits every paper account has this run (`--max-order-qty`).
        cfg.limits = self.new_paper_limits.clone();
        self.add_account(cfg.clone(), None);
        self.join_guardrails(&id);
        self.events.push(Event::AccountAdded { config: cfg });
        self.account_changed(now, &id);
        Ok(Vec::new())
    }

    fn remove_paper_account(&mut self, account: &AccountId) -> Result<Vec<OrderId>, Reject> {
        let Some(a) = self.accounts.get(account) else {
            return Err(Reject::new(
                RejectCode::UnknownAccount,
                format!("no account {account}"),
            ));
        };
        let name = a.cfg.name.clone();
        let refuse = |why: &str| {
            Err(Reject::new(
                RejectCode::Invalid,
                format!("{name} can't be removed: {why}"),
            ))
        };
        if self.accounts.len() < 2 {
            return refuse("it is the only account");
        }
        if a.positions.values().any(|p| p.qty() != 0) {
            return refuse("flatten it first");
        }
        if self
            .orders
            .values()
            .any(|o| &o.account == account && !o.state.is_terminal())
        {
            return refuse("cancel its orders first");
        }
        if self.triggers().any(|t| &t.account == account) {
            return refuse("cancel its triggers first");
        }
        if self.strategies.values().any(|i| &i.cfg.account == account) {
            return refuse("remove its strategies first");
        }
        if self.resets.contains_key(account) {
            return refuse("its reset is still waiting");
        }
        if !self.broker.close_account(account) {
            return refuse("the paper broker still holds something on it");
        }
        self.accounts.remove(account);
        self.retired.insert(account.clone());
        self.news_flattened.retain(|(a, _, _)| a != account);
        self.position_brackets.retain(|(a, _), _| a != account);
        let mut left = false;
        for g in &mut self.groups {
            let before = g.members.len();
            g.members.retain(|m| m != account);
            left |= g.members.len() != before;
        }
        if left {
            self.events.push(Event::Groups {
                groups: self.groups.clone(),
            });
        }
        self.events.push(Event::AccountRemoved {
            account: account.clone(),
        });
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_come_from_names_and_stay_unique() {
        let none = |_: &str| false;
        assert_eq!(
            account_id_for("Paper 50K eval 2", none).0,
            "paper-paper-50k-eval-2"
        );
        assert_eq!(account_id_for("  Évals!! ", none).0, "paper-vals");
        assert_eq!(account_id_for("★", none).0, "paper-account");
        let taken = |s: &str| s == "paper-evals" || s == "paper-evals-2";
        assert_eq!(account_id_for("Evals", taken).0, "paper-evals-3");
    }
}
