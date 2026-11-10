//! Account groups (decision 0006: the OMS supports them from day one; the copier, roadmap 14,
//! builds on them). A group is a name and its member accounts, in the order they were given.
//! An account may be in several groups. Groups carry no rules of their own: prop rules, risk
//! limits and guardrails stay per account, and a group command is its member accounts' own
//! commands, one per member, each checked (or not) exactly as it would be alone.

use serde::{Deserialize, Serialize};

use crate::command::{Reject, RejectCode};
use crate::types::{AccountId, CommandId, OrderId};

/// The longest group name, in characters.
pub const MAX_NAME_CHARS: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountGroup {
    pub name: String,
    /// In the order given; never twice the same account.
    #[serde(default)]
    pub members: Vec<AccountId>,
}

/// One member's part of a group command ([`crate::Event::GroupCommand`]): the member's own
/// command (`command`, an id from the internal range) and its result, as that command alone
/// would have answered.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberResult {
    pub account: AccountId,
    pub command: CommandId,
    pub result: Result<Vec<OrderId>, Reject>,
}

/// A group name as it is kept: trimmed; refused when empty, longer than [`MAX_NAME_CHARS`] or
/// holding a control character.
pub fn clean_name(name: &str) -> Result<String, Reject> {
    let n = name.trim();
    if n.is_empty() {
        return Err(Reject::new(RejectCode::Invalid, "a group needs a name"));
    }
    if n.chars().count() > MAX_NAME_CHARS {
        return Err(Reject::new(
            RejectCode::Invalid,
            format!("a group name is at most {MAX_NAME_CHARS} characters"),
        ));
    }
    if n.chars().any(char::is_control) {
        return Err(Reject::new(
            RejectCode::Invalid,
            "a group name can't hold control characters",
        ));
    }
    Ok(n.to_owned())
}

/// Whether two names are the same group's: names are unique ignoring case, so "Evals" and
/// "evals" can't be two groups a person would mistake for each other.
pub fn same_name(a: &str, b: &str) -> bool {
    a.trim().to_lowercase() == b.trim().to_lowercase()
}

/// Members as kept: duplicates refused (a list naming an account twice is a mistake, not a
/// request to flatten it twice).
pub fn check_members(members: &[AccountId]) -> Result<(), Reject> {
    for (i, m) in members.iter().enumerate() {
        if members[..i].contains(m) {
            return Err(Reject::new(
                RejectCode::Invalid,
                format!("{m} is listed twice"),
            ));
        }
    }
    Ok(())
}

/// Saved groups as restored: names cleaned, a later group with a name already taken dropped,
/// members deduplicated (first kept). Members are kept even if no such account is known yet:
/// a broker venue lists its accounts after it connects.
pub fn restored(saved: Vec<AccountGroup>) -> Vec<AccountGroup> {
    let mut out: Vec<AccountGroup> = Vec::with_capacity(saved.len());
    for g in saved {
        let Ok(name) = clean_name(&g.name) else {
            continue;
        };
        if out.iter().any(|o| same_name(&o.name, &name)) {
            continue;
        }
        let mut members: Vec<AccountId> = Vec::with_capacity(g.members.len());
        for m in g.members {
            if !members.contains(&m) {
                members.push(m);
            }
        }
        out.push(AccountGroup { name, members });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_trimmed_bounded_and_unique_ignoring_case() {
        assert_eq!(clean_name("  Evals ").unwrap(), "Evals");
        assert!(clean_name("   ").is_err());
        assert!(clean_name(&"x".repeat(MAX_NAME_CHARS)).is_ok());
        assert!(clean_name(&"x".repeat(MAX_NAME_CHARS + 1)).is_err());
        assert!(clean_name("a\nb").is_err());
        assert!(same_name("Evals", " evals"));
        assert!(!same_name("Evals", "Evals 2"));
    }

    #[test]
    fn a_member_listed_twice_is_refused() {
        let a = AccountId::new("a");
        assert!(check_members(&[a.clone(), AccountId::new("b")]).is_ok());
        assert_eq!(
            check_members(&[a.clone(), a]).unwrap_err().code,
            RejectCode::Invalid
        );
    }

    #[test]
    fn restoring_drops_bad_names_and_repeats() {
        let g = |n: &str, m: &[&str]| AccountGroup {
            name: n.into(),
            members: m.iter().map(|s| AccountId::new(s)).collect(),
        };
        let out = restored(vec![
            g(" Evals ", &["a", "b", "a"]),
            g("evals", &["c"]),
            g("", &["d"]),
            g("Funded", &["unknown-yet"]),
        ]);
        assert_eq!(
            out,
            vec![g("Evals", &["a", "b"]), g("Funded", &["unknown-yet"])]
        );
    }
}
