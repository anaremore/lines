//! Account group commands (`crate::group`): creating, renaming and deleting groups and setting
//! their members, and the group Cancel All and Flatten.
//!
//! A group Cancel All or Flatten is its members' own commands: for each member, in the group's
//! order, a [`Command::CancelAll`] or [`Command::Flatten`] with an id from the internal range and
//! the group command's source and input time, handled as if it had come alone (the same checks,
//! which never include risk, prop rules or guardrails; a person's pauses the strategies trading
//! the member; the same resends and Flatten bookkeeping). Each gets its own `Event::Command`;
//! then `Event::GroupCommand` lists them, one result per member, and the group command answers
//! with every order id they name. A member refused (an account not known, a Flatten refused
//! while the broker is disconnected) doesn't stop the others; with every member refused the
//! group command is refused too (the first member's code and reason), so a way out never
//! reads as done when nothing was.

use lines_core::UtcMillis;

use super::{INTERNAL_COMMAND_IDS, TradingCore};
use crate::command::{Command, Envelope, Reject, RejectCode, Source};
use crate::event::Event;
use crate::group::{self, AccountGroup, MemberResult};
use crate::types::{AccountId, CommandId, OrderId};

impl TradingCore {
    /// Restore saved groups (the trading file's, at startup, after the accounts), replacing any.
    /// Members not known yet are kept: a broker venue lists its accounts after it connects.
    pub fn restore_groups(&mut self, saved: Vec<AccountGroup>) {
        self.groups = group::restored(saved);
        self.publish_groups();
    }

    fn publish_groups(&mut self) {
        self.events.push(Event::Groups {
            groups: self.groups.clone(),
        });
    }

    fn group_index(&self, name: &str) -> Result<usize, Reject> {
        self.groups
            .iter()
            .position(|g| group::same_name(&g.name, name))
            .ok_or_else(|| {
                Reject::new(
                    RejectCode::UnknownGroup,
                    format!("no group {}", name.trim()),
                )
            })
    }

    /// Members as a command gives them: each once, each a known account.
    fn group_members(&self, members: &[AccountId]) -> Result<(), Reject> {
        group::check_members(members)?;
        match members.iter().find(|m| !self.accounts.contains_key(*m)) {
            Some(m) => Err(Reject::new(
                RejectCode::UnknownAccount,
                format!("no account {m}"),
            )),
            None => Ok(()),
        }
    }

    /// A group command's result (`None`: not a group command). Its `Event::Command` is the
    /// caller's to push.
    pub(super) fn group_command(
        &mut self,
        now: UtcMillis,
        env: &Envelope,
    ) -> Option<Result<Vec<OrderId>, Reject>> {
        env.command.group()?;
        // Groups are a person's (or the API's) to manage and to act on: a strategy trades
        // its own account only (decision 0025).
        if matches!(env.source, Source::Strategy(_)) {
            return Some(Err(Reject::new(
                RejectCode::Strategy,
                "a strategy can't manage or act on account groups",
            )));
        }
        Some(match env.command.clone() {
            Command::CreateGroup { name, members } => self.create_group(&name, members),
            Command::RenameGroup { group, name } => self.rename_group(&group, &name),
            Command::DeleteGroup { group } => self.group_index(&group).map(|i| {
                self.groups.remove(i);
                self.publish_groups();
                Vec::new()
            }),
            Command::SetGroupMembers { group, members } => self.set_group_members(&group, members),
            Command::GroupCancelAll { group, contract } => {
                self.group_each(now, env, &group, |account| Command::CancelAll {
                    account,
                    contract,
                })
            }
            Command::GroupFlatten { group, contract } => {
                self.group_each(now, env, &group, |account| Command::Flatten {
                    account,
                    contract,
                })
            }
            _ => unreachable!("every command naming a group is handled"),
        })
    }

    fn create_group(
        &mut self,
        name: &str,
        members: Vec<AccountId>,
    ) -> Result<Vec<OrderId>, Reject> {
        let name = group::clean_name(name)?;
        if self.groups.iter().any(|g| group::same_name(&g.name, &name)) {
            return Err(Reject::new(
                RejectCode::Invalid,
                format!("there is already a group {name}"),
            ));
        }
        self.group_members(&members)?;
        self.groups.push(AccountGroup { name, members });
        self.publish_groups();
        Ok(Vec::new())
    }

    fn rename_group(&mut self, group: &str, name: &str) -> Result<Vec<OrderId>, Reject> {
        let i = self.group_index(group)?;
        let name = group::clean_name(name)?;
        // Its own name in other letter case is a rename; another group's is refused.
        if self
            .groups
            .iter()
            .enumerate()
            .any(|(j, g)| j != i && group::same_name(&g.name, &name))
        {
            return Err(Reject::new(
                RejectCode::Invalid,
                format!("there is already a group {name}"),
            ));
        }
        self.groups[i].name = name;
        self.publish_groups();
        Ok(Vec::new())
    }

    fn set_group_members(
        &mut self,
        group: &str,
        members: Vec<AccountId>,
    ) -> Result<Vec<OrderId>, Reject> {
        let i = self.group_index(group)?;
        self.group_members(&members)?;
        self.groups[i].members = members;
        self.publish_groups();
        Ok(Vec::new())
    }

    /// One member command per member, each handled as if alone (module docs).
    fn group_each(
        &mut self,
        now: UtcMillis,
        env: &Envelope,
        group: &str,
        member: impl Fn(AccountId) -> Command,
    ) -> Result<Vec<OrderId>, Reject> {
        let i = self.group_index(group)?;
        let name = self.groups[i].name.clone();
        let members = self.groups[i].members.clone();
        let mut results = Vec::with_capacity(members.len());
        let mut ids = Vec::new();
        for (n, account) in members.into_iter().enumerate() {
            let id = CommandId(self.next_internal);
            self.next_internal += 1;
            debug_assert!(id.0 >= INTERNAL_COMMAND_IDS);
            let result = self.handle_result(
                now,
                Envelope {
                    id,
                    source: env.source,
                    // The first member is timed from the input; each later one from when its
                    // own handling starts, so the latency samples don't add up the earlier
                    // members' work.
                    issued: if n == 0 {
                        env.issued
                    } else {
                        std::time::Instant::now()
                    },
                    command: member(account.clone()),
                    pane: env.pane.clone(),
                },
            );
            if let Ok(o) = &result {
                ids.extend(o.iter().copied());
            }
            results.push(MemberResult {
                account,
                command: id,
                result,
            });
        }
        // A way out must never read as done when nothing was: with every member refused the
        // group command is refused too (each member's reason is in the results).
        let refused = results
            .iter()
            .filter_map(|m| m.result.as_ref().err().map(|r| (&m.account, r)))
            .collect::<Vec<_>>();
        let all_refused = (!results.is_empty() && refused.len() == results.len()).then(|| {
            let (account, first) = refused[0];
            Reject::new(
                first.code,
                format!("no account in {name} acted ({account}: {})", first.message),
            )
        });
        self.events.push(Event::GroupCommand {
            id: env.id,
            group: name,
            results,
        });
        match all_refused {
            Some(r) => Err(r),
            None => Ok(ids),
        }
    }
}
