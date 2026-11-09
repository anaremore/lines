//! Reset paper account: the second-press confirmation every way in shares (the palette, the
//! DOM's account menu, Settings › Accounts), and its words. GPUI-free.
//!
//! The first press arms it and names what it will do ("Reset Paper (mock): flattens and clears
//! balance, positions, orders and today's P&L"); a second press of the same reset within
//! [`CONFIRM`] sends it (`Command::ResetPaper`, which flattens through the core first). Anything
//! else, or the time running out, disarms it. A broker account is refused before anything arms.

use std::time::{Duration, Instant};

use lines_trading::AccountId;

/// How long a first press waits for the second.
pub const CONFIRM: Duration = Duration::from_secs(5);

/// A first press, waiting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Arm {
    pub account: AccountId,
    /// The reset clears the account's journal too.
    pub journal: bool,
    pub at: Instant,
}

/// What a press does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Press {
    /// The first press: show this and wait for the second.
    Confirm(String),
    /// The second: send the reset.
    Go,
}

/// The account as a reset names it: "Paper (mock)" (the name alone without a source).
pub fn account_words(name: &str, source: &str) -> String {
    if source.is_empty() {
        name.to_owned()
    } else {
        format!("{name} ({source})")
    }
}

/// What the reset will do, in the words its confirmation shows.
pub fn confirm_text(name: &str, source: &str, journal: bool) -> String {
    let who = account_words(name, source);
    if journal {
        format!(
            "Reset {who}: flattens and clears balance, positions, orders, today's P&L and its journal"
        )
    } else {
        format!("Reset {who}: flattens and clears balance, positions, orders and today's P&L")
    }
}

/// Why a broker account can't be reset.
pub fn broker_refusal(name: &str) -> String {
    format!("{name} is a broker account: only a paper account can be reset")
}

/// Whether this reset is armed now (its first press was within [`CONFIRM`]).
pub fn armed(slot: &Option<Arm>, account: &AccountId, journal: bool, now: Instant) -> bool {
    slot.as_ref().is_some_and(|a| {
        &a.account == account && a.journal == journal && now.duration_since(a.at) <= CONFIRM
    })
}

/// A press of the reset for `account` (`journal`: clearing its journal too).
pub fn press(
    slot: &mut Option<Arm>,
    account: &AccountId,
    journal: bool,
    now: Instant,
    text: impl FnOnce() -> String,
) -> Press {
    if armed(slot, account, journal, now) {
        *slot = None;
        return Press::Go;
    }
    *slot = Some(Arm {
        account: account.clone(),
        journal,
        at: now,
    });
    Press::Confirm(text())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paper() -> AccountId {
        AccountId::new("paper")
    }

    #[test]
    fn the_words_name_the_account_and_source() {
        assert_eq!(
            confirm_text("Paper", "databento", false),
            "Reset Paper (databento): flattens and clears balance, positions, orders and today's P&L"
        );
        assert_eq!(
            confirm_text("Paper", "mock", true),
            "Reset Paper (mock): flattens and clears balance, positions, orders, today's P&L and its journal"
        );
        assert!(confirm_text("Paper", "", false).starts_with("Reset Paper: "));
        assert_eq!(
            broker_refusal("Evaluation"),
            "Evaluation is a broker account: only a paper account can be reset"
        );
    }

    #[test]
    fn the_second_press_of_the_same_reset_sends_it() {
        let t0 = Instant::now();
        let mut slot = None;
        let first = press(&mut slot, &paper(), false, t0, || "confirm".into());
        assert_eq!(first, Press::Confirm("confirm".into()));
        assert!(armed(&slot, &paper(), false, t0));
        assert_eq!(
            press(
                &mut slot,
                &paper(),
                false,
                t0 + Duration::from_secs(2),
                String::new
            ),
            Press::Go
        );
        assert_eq!(slot, None, "sent: disarmed");
    }

    #[test]
    fn a_late_or_different_second_press_only_arms_again() {
        let t0 = Instant::now();
        let mut slot = None;
        press(&mut slot, &paper(), false, t0, String::new);
        // Too late.
        let late = t0 + CONFIRM + Duration::from_millis(1);
        assert!(matches!(
            press(&mut slot, &paper(), false, late, String::new),
            Press::Confirm(_)
        ));
        // The other reset (with the journal): its own first press.
        assert!(matches!(
            press(&mut slot, &paper(), true, late, String::new),
            Press::Confirm(_)
        ));
        // Another account.
        assert!(matches!(
            press(&mut slot, &AccountId::new("eval"), true, late, String::new),
            Press::Confirm(_)
        ));
        assert!(!armed(&slot, &paper(), true, late));
    }
}
