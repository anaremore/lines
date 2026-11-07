use lines_core::{CivilDate, ContractKey, Month};

use super::*;

fn nq() -> ContractKey {
    ContractKey::monthly(lines_feed::catalog::NQ, 2026, Month::new(12).unwrap())
}

fn es() -> ContractKey {
    ContractKey::monthly(lines_feed::catalog::ES, 2026, Month::new(12).unwrap())
}

const MIN: i64 = 60_000;
const T0: UtcMillis = UtcMillis(1_800_000_000_000);
const END: UtcMillis = UtcMillis(1_800_000_000_000 + 8 * 60 * MIN);

fn day(d: u8) -> CivilDate {
    CivilDate::new(2026, 11, d).unwrap()
}

fn tracker(g: Guardrails) -> GuardrailTracker {
    let mut t = GuardrailTracker::default();
    t.request(g);
    t.roll_day(day(9));
    t
}

/// An entry of `qty` on `side` with `position` held and no other orders.
fn entry(side: Side, qty: i64, position: i64) -> Entry {
    Entry {
        side,
        qty,
        position,
        open_buy: 0,
        open_sell: 0,
        session_close: None,
    }
}

/// A round trip in `k` of one lot with P&L `pnl` (whole units), at `at`.
fn round_trip(t: &mut GuardrailTracker, k: ContractKey, pnl: i64, at: UtcMillis) {
    t.on_fill(k, 0, 1, -Money::whole(1), at, END);
    t.on_fill(k, 1, 0, Money::whole(pnl + 1), at, END);
}

#[test]
fn trades_are_round_trips_with_partial_fills_and_reversals() {
    let mut t = tracker(Guardrails::default());
    let k = nq();
    // Scale in 1 + 2, out 2 + 1: one trade.
    t.on_fill(k, 0, 1, Money::ZERO, T0, END);
    t.on_fill(k, 1, 3, Money::ZERO, T0, END);
    t.on_fill(k, 3, 1, Money::whole(-10), T0, END);
    assert_eq!(t.trades, 0, "still open");
    t.on_fill(k, 1, 0, Money::whole(4), T0, END);
    assert_eq!(t.trades, 1);
    assert_eq!(t.streak, 1, "-10 + 4: a loss");
    assert!(t.open.is_empty());
    // A reversal: long 2, one sell of 5 → short 3. Closes trade 2, opens trade 3.
    t.on_fill(k, 0, 2, Money::ZERO, T0, END);
    t.on_fill(k, 2, -3, Money::whole(50), T0, END);
    assert_eq!(t.trades, 2);
    assert_eq!(t.streak, 0, "a winner resets the streak");
    assert_eq!(t.open.len(), 1);
    t.on_fill(k, -3, 0, Money::whole(-5), T0, END);
    assert_eq!(t.trades, 3);
    assert_eq!(t.streak, 1);
    // Two contracts at once: separate round trips.
    t.on_fill(k, 0, 1, Money::ZERO, T0, END);
    t.on_fill(es(), 0, -1, Money::ZERO, T0, END);
    t.on_fill(k, 1, 0, Money::whole(1), T0, END);
    assert_eq!(t.trades, 4);
    assert_eq!(t.open.len(), 1, "ES still open");
    // Fees count against the trade: a scratch trade that paid fees is a loss.
    let mut f = tracker(Guardrails::default());
    f.on_fill(k, 0, 1, -Money::whole(2), T0, END);
    f.on_fill(k, 1, 0, Money::whole(2) - Money::whole(2), T0, END);
    assert_eq!(f.streak, 1);
}

#[test]
fn max_trades_locks_until_the_next_trade_date() {
    let mut t = tracker(Guardrails {
        max_trades_per_day: Some(2),
        ..Default::default()
    });
    round_trip(&mut t, nq(), 10, T0);
    assert!(t.check(T0, &entry(Side::Buy, 1, 0)).is_ok());
    round_trip(&mut t, nq(), 10, T0);
    assert_eq!(t.state(), GuardState::Locked);
    let lock = t.lock.clone().unwrap();
    assert_eq!(lock.until, END);
    let e = t.check(T0, &entry(Side::Buy, 1, 0)).unwrap_err();
    assert_eq!(e, "Locked: max 2 trades reached, until the next trade date");
    // A new trade date ends it.
    t.roll_day(day(10));
    assert_eq!(t.state(), GuardState::Active);
    assert_eq!(t.trades, 0);
}

#[test]
fn a_reversal_needs_room_for_two_trades() {
    let mut t = tracker(Guardrails {
        max_trades_per_day: Some(2),
        ..Default::default()
    });
    round_trip(&mut t, nq(), 10, T0);
    // Long 1 (trade 2 in progress): selling 2 would finish trade 2 and open trade 3.
    t.on_fill(nq(), 0, 1, Money::ZERO, T0, END);
    let e = t.check(T0, &entry(Side::Sell, 2, 1)).unwrap_err();
    assert!(e.starts_with("Max trades: 1 of 2"), "{e}");
    // Closing it is an exit; adding is part of the trade in progress.
    assert!(t.check(T0, &entry(Side::Sell, 1, 1)).is_ok());
    assert!(t.check(T0, &entry(Side::Buy, 1, 1)).is_ok());
}

#[test]
fn a_losing_streak_starts_a_cooldown() {
    let mut t = tracker(Guardrails {
        losing_streak: Some(LosingStreak {
            losses: 3,
            cooldown_min: 15,
        }),
        ..Default::default()
    });
    round_trip(&mut t, nq(), -10, T0);
    round_trip(&mut t, nq(), -10, T0);
    assert_eq!(t.state(), GuardState::Active);
    round_trip(&mut t, nq(), -10, T0);
    assert_eq!(t.state(), GuardState::Cooldown);
    assert_eq!(t.next_wake(), Some(UtcMillis(T0.0 + 15 * MIN)));
    // Three minutes later: 12 left.
    let e = t
        .check(UtcMillis(T0.0 + 3 * MIN), &entry(Side::Buy, 1, 0))
        .unwrap_err();
    assert_eq!(e, "Cooldown: 3 losses in a row, 12 min left");
    // Not over a second early; over at its end, and the streak starts again.
    t.expire(UtcMillis(T0.0 + 15 * MIN - 1));
    assert_eq!(t.state(), GuardState::Cooldown);
    t.expire(UtcMillis(T0.0 + 15 * MIN));
    assert_eq!(t.state(), GuardState::Active);
    assert_eq!(t.streak, 0);
    round_trip(&mut t, nq(), -10, T0);
    assert_eq!(t.state(), GuardState::Active, "one loss after the cooldown");
}

#[test]
fn daily_loss_locks_for_the_trade_date() {
    let mut t = tracker(Guardrails {
        max_daily_loss: Some(500),
        ..Default::default()
    });
    t.on_day_pnl(Money::whole(-499), END);
    assert_eq!(t.state(), GuardState::Active);
    t.on_day_pnl(Money::whole(-500), END);
    assert_eq!(t.state(), GuardState::Locked);
    // Recovering doesn't unlock.
    t.on_day_pnl(Money::whole(100), END);
    let e = t.check(T0, &entry(Side::Sell, 1, 0)).unwrap_err();
    assert!(e.contains("daily loss limit of 500 reached"), "{e}");
    t.roll_day(day(10));
    assert_eq!(t.state(), GuardState::Active);
}

#[test]
fn max_position_counts_open_orders() {
    let t = tracker(Guardrails {
        max_position: Some(3),
        ..Default::default()
    });
    assert!(t.check(T0, &entry(Side::Buy, 3, 0)).is_ok());
    assert!(t.check(T0, &entry(Side::Buy, 1, 2)).is_ok());
    let mut e = entry(Side::Buy, 1, 2);
    e.open_buy = 1;
    assert_eq!(
        t.check(T0, &e).unwrap_err(),
        "Max position: would be 4, your limit is 3"
    );
    // Short side too; a reversal past the limit is denied.
    assert!(t.check(T0, &entry(Side::Sell, 4, 0)).is_err());
    assert!(t.check(T0, &entry(Side::Sell, 6, 2)).is_err());
    assert!(t.check(T0, &entry(Side::Sell, 5, 2)).is_ok());
    // Not a lockout.
    assert_eq!(t.state(), GuardState::Active);
}

#[test]
fn no_entries_just_before_the_close() {
    let t = tracker(Guardrails {
        no_entries_before_close_min: Some(10),
        ..Default::default()
    });
    let close = UtcMillis(T0.0 + 60 * MIN);
    let at = |m: i64| UtcMillis(close.0 - m * MIN);
    let mut e = entry(Side::Buy, 1, 0);
    e.session_close = Some(close);
    assert!(t.check(at(11), &e).is_ok());
    assert_eq!(
        t.check(at(10), &e).unwrap_err(),
        "No new entries in the last 10 min before the close"
    );
    assert!(t.check(at(1), &e).is_err());
    // An exit is fine.
    let mut x = entry(Side::Sell, 1, 1);
    x.session_close = Some(close);
    assert!(t.check(at(1), &x).is_ok());
    // No calendar: no rule.
    e.session_close = None;
    assert!(t.check(at(1), &e).is_ok());
}

#[test]
fn exits_are_never_blocked() {
    let mut t = tracker(Guardrails {
        max_trades_per_day: Some(1),
        max_position: Some(1),
        no_entries_before_close_min: Some(30),
        ..Default::default()
    });
    round_trip(&mut t, nq(), -10, T0);
    assert_eq!(t.state(), GuardState::Locked);
    // Long 5 (held from before): selling up to what is left to close always passes, even
    // over the max position and right before the close.
    let mut x = entry(Side::Sell, 5, 5);
    x.session_close = Some(UtcMillis(T0.0 + MIN));
    assert!(t.check(T0, &x).is_ok());
    x.qty = 2;
    x.open_sell = 3;
    assert!(t.check(T0, &x).is_ok(), "with other exits working");
    x.qty = 3;
    assert!(t.check(T0, &x).is_err(), "beyond what is left to close");
    assert!(t.check(T0, &entry(Side::Buy, 2, -2)).is_ok());
    assert!(t.check(T0, &entry(Side::Buy, 3, -2)).is_err(), "a reversal");
}

#[test]
fn tightening_applies_now_loosening_tomorrow() {
    let mut t = tracker(Guardrails {
        max_trades_per_day: Some(5),
        max_daily_loss: Some(500),
        max_position: Some(2),
        ..Default::default()
    });
    // Tighter trades, looser loss, a new rule, a removed one: the tighter parts now.
    let wanted = Guardrails {
        max_trades_per_day: Some(3),
        max_daily_loss: Some(1_000),
        losing_streak: Some(LosingStreak {
            losses: 2,
            cooldown_min: 10,
        }),
        max_position: None,
        ..Default::default()
    };
    t.request(wanted.clone());
    assert_eq!(t.active.max_trades_per_day, Some(3));
    assert_eq!(t.active.max_daily_loss, Some(500), "looser: tomorrow");
    assert_eq!(t.active.max_position, Some(2), "removed: tomorrow");
    assert_eq!(
        t.active.losing_streak,
        Some(LosingStreak {
            losses: 2,
            cooldown_min: 10
        })
    );
    assert_eq!(t.pending, Some(wanted.clone()));
    // Asked again (every start reads settings): nothing changes until the date does.
    t.request(wanted.clone());
    assert_eq!(t.active.max_daily_loss, Some(500));
    t.roll_day(day(9));
    assert_eq!(t.active.max_daily_loss, Some(500), "same trade date");
    t.roll_day(day(10));
    assert_eq!(t.active, wanted);
    assert_eq!(t.pending, None);
    // A streak is tighter with fewer losses or a longer cooldown, part by part.
    let a = Guardrails {
        losing_streak: Some(LosingStreak {
            losses: 3,
            cooldown_min: 30,
        }),
        ..Default::default()
    };
    let b = Guardrails {
        losing_streak: Some(LosingStreak {
            losses: 2,
            cooldown_min: 10,
        }),
        ..Default::default()
    };
    assert_eq!(
        a.tightest(&b).losing_streak,
        Some(LosingStreak {
            losses: 2,
            cooldown_min: 30
        })
    );
    // More minutes before the close is tighter.
    let c = |m| Guardrails {
        no_entries_before_close_min: Some(m),
        ..Default::default()
    };
    assert_eq!(c(5).tightest(&c(15)), c(15));
}

#[test]
fn a_lockout_cannot_be_cleared_by_loosening() {
    let mut t = tracker(Guardrails {
        max_daily_loss: Some(200),
        ..Default::default()
    });
    t.on_day_pnl(Money::whole(-250), END);
    assert_eq!(t.state(), GuardState::Locked);
    t.request(Guardrails::default());
    assert_eq!(t.state(), GuardState::Locked, "settings can't clear it");
    assert_eq!(t.active.max_daily_loss, Some(200));
    assert!(t.check(T0, &entry(Side::Buy, 1, 0)).is_err());
    t.roll_day(day(10));
    assert_eq!(t.state(), GuardState::Active);
    assert!(
        t.active.is_default(),
        "the loosening took effect with the date"
    );
}

#[test]
fn a_day_lock_past_its_time_but_not_its_date_waits_never_loops() {
    let mut t = tracker(Guardrails {
        max_daily_loss: Some(10),
        ..Default::default()
    });
    t.on_day_pnl(Money::whole(-10), T0);
    let late = UtcMillis(T0.0 + MIN);
    t.roll_day(day(9)); // the same trade date
    t.extend_lock(late, late);
    assert_eq!(t.state(), GuardState::Locked);
    assert!(
        t.next_wake().unwrap() > late,
        "a future wake, not a busy loop"
    );
    t.extend_lock(late, UtcMillis(T0.0 + 60 * MIN));
    assert_eq!(
        t.next_wake(),
        Some(UtcMillis(T0.0 + 2 * MIN)),
        "never moved twice"
    );
}

#[test]
fn tracker_round_trips_through_json() {
    let mut t = tracker(Guardrails {
        losing_streak: Some(LosingStreak {
            losses: 1,
            cooldown_min: 20,
        }),
        max_trades_per_day: Some(9),
        ..Default::default()
    });
    t.request(Guardrails::default());
    t.on_fill(es(), 0, 2, Money::ZERO, T0, END);
    round_trip(&mut t, nq(), -1, T0);
    assert_eq!(t.state(), GuardState::Cooldown);
    let json = serde_json::to_string(&t).unwrap();
    let back: GuardrailTracker = serde_json::from_str(&json).unwrap();
    assert_eq!(back, t);
    // Nothing set: written as `{}`-ish and read back the same.
    let none = GuardrailTracker::default();
    assert!(none.is_default());
    let back: GuardrailTracker =
        serde_json::from_str(&serde_json::to_string(&none).unwrap()).unwrap();
    assert_eq!(back, none);
}

/// The one definition of a trade (shared with the journal): flat → position → flat; a
/// reversal ends one and starts the next.
#[test]
fn round_trip_steps() {
    use crate::guardrails::RoundTrip;
    assert_eq!(RoundTrip::of(0, 0), RoundTrip::None);
    assert_eq!(RoundTrip::of(0, 2), RoundTrip::Opens);
    assert_eq!(RoundTrip::of(0, -1), RoundTrip::Opens);
    assert_eq!(RoundTrip::of(2, 3), RoundTrip::Continues, "add");
    assert_eq!(RoundTrip::of(3, 1), RoundTrip::Continues, "scale out");
    assert_eq!(RoundTrip::of(-2, 0), RoundTrip::Closes);
    assert_eq!(RoundTrip::of(2, -3), RoundTrip::Reverses);
    assert_eq!(RoundTrip::of(-1, 1), RoundTrip::Reverses);
}
