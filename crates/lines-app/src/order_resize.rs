//! Resizing a working order (the DOM's marks, the chart's tags), without GPUI: what a − / + / a
//! typed size asks for, and the commands that do it. The size shown and set is what is **left**
//! on the order, so a part-filled order can't be set below its fills. The core has the last word
//! (`ModifyOrder`): an increase runs the new-order checks, and an exit stays within the position.

use lines_core::{Price, Qty};
use lines_trading::order::{Order, Role};
use lines_trading::{Command as TradeCommand, OrderId};

/// What the trader asked for.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SizeStep {
    Down,
    Up,
    /// A typed size: what should be left.
    To(i64),
}

/// A piece of the − / size / + control.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ResizeHit {
    Down,
    Value,
    Up,
}

/// One order's part of a resize.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Leg {
    pub order: OrderId,
    /// The new total (what is left plus what filled).
    pub qty: Qty,
    /// Where it rests, for the refusal's text.
    pub price: Price,
    pub command: TradeCommand,
}

/// A planned resize: its commands (a linked pair has two; none when nothing changes), with the
/// new total each order reports once acked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub commands: Vec<Leg>,
    /// Any leg grows: more quantity, so it needs the trading switch like a new order.
    pub grows: bool,
}

fn is_exit(o: &Order) -> bool {
    o.reduce_only || matches!(o.role, Role::TakeProfit | Role::StopLoss)
}

/// Why this order's size can't be changed at all, if so.
fn fixed(o: &Order, entry_working: bool) -> Option<String> {
    if !o.is_open() {
        return Some("the order is gone".to_owned());
    }
    if o.role == Role::Flatten {
        return Some("a Flatten order can't be resized".to_owned());
    }
    // Its size follows the entry's fills (one-updates-other).
    if entry_working && matches!(o.role, Role::TakeProfit | Role::StopLoss) {
        return Some("this exit's size follows its entry until the entry is filled".to_owned());
    }
    None
}

/// Whether a larger size could be accepted (the + control, before a click).
pub fn can_grow(o: &Order, entry_working: bool, position: i64) -> bool {
    fixed(o, entry_working).is_none() && (!is_exit(o) || o.remaining().0 < position.abs())
}

/// The commands for `step` on order `o` (signed `position` on its contract; `entry_working`: its
/// bracket's entry is still open). `sibling`: its linked stop or target, resized with it to the
/// same size unless `one_leg` (Alt).
pub fn plan(
    o: &Order,
    sibling: Option<&Order>,
    entry_working: bool,
    position: i64,
    step: SizeStep,
    one_leg: bool,
) -> Result<Plan, String> {
    if let Some(why) = fixed(o, entry_working) {
        return Err(why);
    }
    let left = o.remaining().0;
    let target = match step {
        SizeStep::Down => left - 1,
        SizeStep::Up => left + 1,
        SizeStep::To(q) => q,
    };
    if target < 1 {
        return Err(format!("{left} left: cancel the order to remove it"));
    }
    // A typed size is for the pair. A step moves the other leg only when the two are equal: one
    // click never takes a larger stop down to a smaller target's size.
    let together = |s: &&Order| match step {
        SizeStep::To(_) => true,
        SizeStep::Down | SizeStep::Up => s.remaining() == o.remaining(),
    };
    let legs =
        std::iter::once(o).chain(sibling.filter(|s| !one_leg && s.is_open()).filter(together));
    let mut commands = Vec::with_capacity(2);
    let mut grows = false;
    for leg in legs {
        if target == leg.remaining().0 {
            continue;
        }
        if target > leg.remaining().0 {
            if is_exit(leg) && target > position.abs() {
                return Err(format!(
                    "exits can't exceed the position ({})",
                    position.abs()
                ));
            }
            grows = true;
        }
        let Some(price) = crate::dom_trade::order_price(leg) else {
            return Err("a market order can't be resized".to_owned());
        };
        let qty = Qty(leg.filled.0 + target);
        commands.push(Leg {
            order: leg.id,
            qty,
            price,
            command: TradeCommand::ModifyOrder {
                account: leg.account.clone(),
                order: leg.id,
                qty: Some(qty),
                price: None,
            },
        });
    }
    Ok(Plan { commands, grows })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lines_core::{ContractKey, Month, UtcMillis};
    use lines_trading::{AccountId, OrderKind, OrderState, Side};

    fn order(id: u64, role: Role, qty: i64, filled: i64) -> Order {
        Order {
            id: OrderId(id),
            account: AccountId::new("paper-1"),
            contract: ContractKey::monthly(lines_feed::catalog::ES, 2026, Month::new(12).unwrap()),
            side: Side::Sell,
            qty: Qty(qty),
            kind: OrderKind::Limit {
                price: Price(601_000),
            },
            state: OrderState::Working,
            filled: Qty(filled),
            fill_value: 0,
            role,
            parent: None,
            oco: None,
            triggered: false,
            created: UtcMillis(0),
            updated: UtcMillis(0),
            reduce_only: false,
            overfill: Qty(0),
        }
    }

    fn qtys(p: &Plan) -> Vec<(u64, i64)> {
        p.commands.iter().map(|l| (l.order.0, l.qty.0)).collect()
    }

    #[test]
    fn a_step_changes_what_is_left_and_only_more_needs_the_switch() {
        let o = order(1, Role::Single, 5, 0);
        let up = plan(&o, None, false, 0, SizeStep::Up, false).unwrap();
        assert_eq!((qtys(&up), up.grows), (vec![(1, 6)], true));
        let down = plan(&o, None, false, 0, SizeStep::Down, false).unwrap();
        assert_eq!((qtys(&down), down.grows), (vec![(1, 4)], false));
        let same = plan(&o, None, false, 0, SizeStep::To(5), false).unwrap();
        assert!(same.commands.is_empty(), "nothing to send");
    }

    /// A linked stop and target resize together, to one size; Alt takes one leg.
    #[test]
    fn a_pair_resizes_together_and_alt_takes_one_leg() {
        let stop = order(1, Role::StopLoss, 4, 0);
        let target = order(2, Role::TakeProfit, 4, 0);
        let both = plan(&stop, Some(&target), false, 4, SizeStep::To(2), false).unwrap();
        assert_eq!(qtys(&both), vec![(1, 2), (2, 2)]);
        let one = plan(&stop, Some(&target), false, 4, SizeStep::To(2), true).unwrap();
        assert_eq!(qtys(&one), vec![(1, 2)]);
        // Unequal legs meet at the size asked for; a leg already there sends nothing.
        let small = order(2, Role::TakeProfit, 2, 0);
        let meet = plan(&stop, Some(&small), false, 4, SizeStep::To(2), false).unwrap();
        assert_eq!((qtys(&meet), meet.grows), (vec![(1, 2)], false));
        // Equal legs step together; unequal, a step moves only the leg it is on, either way.
        let together = plan(&stop, Some(&target), false, 4, SizeStep::Down, false).unwrap();
        assert_eq!(qtys(&together), vec![(1, 3), (2, 3)]);
        for (leg, other, step, want) in [
            (&stop, &small, SizeStep::Down, (1, 3)),
            (&small, &stop, SizeStep::Down, (2, 1)),
            (&small, &stop, SizeStep::Up, (2, 3)),
        ] {
            let p = plan(leg, Some(other), false, 5, step, false).unwrap();
            assert_eq!(qtys(&p), vec![want], "{step:?}");
        }
        let larger_up = plan(&stop, Some(&small), false, 5, SizeStep::Up, false).unwrap();
        assert_eq!(qtys(&larger_up), vec![(1, 5)]);
        // A sibling that finished isn't touched.
        let mut done = order(2, Role::TakeProfit, 4, 4);
        done.state = OrderState::Filled;
        let alone = plan(&stop, Some(&done), false, 4, SizeStep::To(2), false).unwrap();
        assert_eq!(qtys(&alone), vec![(1, 2)]);
    }

    #[test]
    fn an_exit_stays_within_the_position_either_leg() {
        let mut stop = order(1, Role::Single, 2, 0);
        stop.reduce_only = true;
        let err = plan(&stop, None, false, 2, SizeStep::Up, false).unwrap_err();
        assert_eq!(err, "exits can't exceed the position (2)");
        assert!(!can_grow(&stop, false, 2));
        assert!(can_grow(&stop, false, -3), "either side of flat");
        // Shrunk by mistake: it can come back, up to the position.
        let up = plan(&stop, None, false, 3, SizeStep::Up, false).unwrap();
        assert_eq!((qtys(&up), up.grows), (vec![(1, 3)], true));
        // A plain order isn't an exit: the core's checks decide.
        let entry = order(1, Role::Single, 2, 0);
        assert!(can_grow(&entry, false, 2));
    }

    /// The size set is what's left, so a part-filled order keeps its fills and can't go below.
    #[test]
    fn a_part_filled_order_keeps_its_fills() {
        let o = order(1, Role::Single, 5, 3);
        let down = plan(&o, None, false, 0, SizeStep::Down, false).unwrap();
        assert_eq!(qtys(&down), vec![(1, 4)], "1 left: 3 filled + 1");
        let one = order(1, Role::Single, 4, 3);
        let err = plan(&one, None, false, 0, SizeStep::Down, false).unwrap_err();
        assert!(err.contains("cancel"), "{err}");
        assert!(plan(&one, None, false, 0, SizeStep::To(0), false).is_err());
    }

    #[test]
    fn an_exit_of_a_working_entry_and_a_flatten_are_fixed() {
        let stop = order(1, Role::StopLoss, 2, 0);
        let err = plan(&stop, None, true, 0, SizeStep::Down, false).unwrap_err();
        assert!(err.contains("follows its entry"), "{err}");
        assert!(!can_grow(&stop, true, 2));
        let flat = order(1, Role::Flatten, 2, 0);
        assert!(plan(&flat, None, false, 2, SizeStep::Down, false).is_err());
    }
}
