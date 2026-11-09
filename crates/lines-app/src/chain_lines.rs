//! Entry, Stop and Target as lines on the underlying's price (decision 0029), without GPUI: which
//! lines there are, where a drag may put one, and the words on them. The core holds the triggers
//! (`lines_trading::trigger`) and decides; these ask the same questions first, so a drag refuses
//! before anything is sent. What a line's tag shows is `option_estimate`'s, display only.

use std::fmt::Write;

use lines_core::{ContractKey, Price, Right};
use lines_trading::trigger::{
    Direction, TriggerId, TriggerKind, TriggerSpec, TriggerState, TriggerView,
};
use lines_trading::{AccountId, Command};

/// Lines kept per pane (more than this are still in the armed rows).
pub const MAX_LINES: usize = 16;

/// Stated in the ticket and on every armed row (decision 0029).
pub const LIMIT: &str = "Fires only while Lines is running with a fresh feed";

/// Where a line comes from, which decides what a drag does.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum LineSource {
    /// Armed in the core: a drag moves it there.
    Armed(TriggerId),
    /// The ticket's entry, not armed yet: a drag moves the ticket's level.
    Ticket,
    /// A Stop or Target not armed yet: a drag moves it here.
    Draft,
}

/// A line on the underlying's price.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Line {
    pub kind: TriggerKind,
    pub contract: ContractKey,
    pub level: Price,
    pub source: LineSource,
    /// Which way an armed entry fires, from where the market was when armed.
    pub fires: Option<Direction>,
    /// An exit already closing: it can't be moved.
    pub working: bool,
    pub stale: bool,
}

impl Line {
    pub fn of(v: &TriggerView) -> Line {
        Line {
            kind: v.kind,
            contract: v.contract,
            level: v.level,
            source: LineSource::Armed(v.id),
            fires: Some(v.direction),
            working: v.state == TriggerState::Working,
            stale: v.stale,
        }
    }

    pub fn draggable(&self) -> bool {
        !self.working
    }

    /// The same line, whatever its level: drafts and the ticket have no id of their own.
    pub fn same(&self, other: &Line) -> bool {
        self.source == other.source && self.kind == other.kind && self.contract == other.contract
    }
}

/// What a line's place is checked against.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct Around {
    /// The underlying's last price, if known.
    pub last: Option<Price>,
    /// The account's position in the line's contract.
    pub position: i64,
    /// The series' other lines.
    pub stop: Option<Price>,
    pub target: Option<Price>,
    pub entry: Option<Price>,
}

/// Why a line can't go to `to`, in plain words; `None`: it can.
pub fn refusal(line: &Line, to: Price, around: &Around) -> Option<&'static str> {
    let right = line.contract.option_key()?.right;
    if !line.draggable() {
        return Some("it's already closing the position");
    }
    if to.0 <= 0 {
        return Some("that isn't a price");
    }
    match line.kind {
        TriggerKind::Entry { .. } => entry_refusal(line, to, right, around),
        kind => exit_refusal(kind, to, right, around),
    }
}

fn entry_refusal(line: &Line, to: Price, right: Right, a: &Around) -> Option<&'static str> {
    // The core won't arm an entry without a fresh print, and a move starts by canceling this one.
    if line.stale {
        return Some("the feed is stale: moving it would end it");
    }
    let Some(last) = a.last else {
        return Some("no underlying price: an entry can't tell which way it fires");
    };
    if to == last {
        return Some("the underlying is already at this level");
    }
    // A level typed halfway (68 for 684) would arm and never fire.
    if (to.0 - last.0).abs().saturating_mul(5) > last.0 {
        return Some("that's far from the market: check the level");
    }
    // An armed entry keeps the way it fires: across the market it would be another trade.
    if line.fires.is_some_and(|d| d.reached(to, last)) {
        return Some("the wrong side of the market: it would fire the other way");
    }
    // With exits armed, an entry at or past one would fill into its own exit.
    if a.position != 0 {
        let past = |kind, level: Option<Price>| {
            level.is_some_and(|l| Direction::of_exit(kind, right, a.position).reached(l, to))
        };
        if past(TriggerKind::Stop, a.stop) {
            return Some("the entry must be on this side of the Stop");
        }
        if past(TriggerKind::Target, a.target) {
            return Some("the entry must be on this side of the Target");
        }
    }
    None
}

fn exit_refusal(kind: TriggerKind, to: Price, right: Right, a: &Around) -> Option<&'static str> {
    if a.position == 0 {
        return Some("no open position for this exit to close");
    }
    let is_stop = kind == TriggerKind::Stop;
    let direction = Direction::of_exit(kind, right, a.position);
    if a.last.is_some_and(|l| direction.reached(to, l)) {
        return Some(if is_stop {
            "the wrong side of the market: the Stop would fire at once"
        } else {
            "the wrong side of the market: the Target would fire at once"
        });
    }
    if a.entry.is_some_and(|e| direction.reached(to, e)) {
        return Some(if is_stop {
            "the wrong side of the entry for a Stop"
        } else {
            "the wrong side of the entry for a Target"
        });
    }
    let (stop, target) = if is_stop {
        (Some(to), a.target)
    } else {
        (a.stop, Some(to))
    };
    if let (Some(s), Some(t)) = (stop, target) {
        let apart = match Direction::of_exit(TriggerKind::Stop, right, a.position) {
            Direction::AtOrBelow => s < t,
            Direction::AtOrAbove => s > t,
        };
        if !apart {
            return Some("the Stop and the Target must be apart, the Stop on the losing side");
        }
    }
    None
}

/// What a moved entry does once the core answers its cancel: arm at the new level, or stop. A
/// refused cancel means the entry isn't there to move (it fired, say), and arming anyway would
/// buy a second time.
pub fn arm_after_cancel<T, E>(answer: &Result<T, E>) -> bool {
    answer.is_ok()
}

/// Arm a line where it is.
pub fn arm(account: AccountId, line: &Line, level: Price) -> Command {
    Command::ArmTrigger(TriggerSpec {
        account,
        contract: line.contract,
        kind: line.kind,
        level,
    })
}

/// An armed line moved to `to`, in order. A Stop or Target is replaced by arming it again, and the
/// old one stays if that's refused. An entry has no such swap: it's canceled first, because with
/// both alive one print could fire two, and the second goes only on [`arm_after_cancel`]. A refusal
/// of that one ends the entry and says so.
pub fn move_commands(account: AccountId, line: &Line, id: TriggerId, to: Price) -> Vec<Command> {
    let again = arm(account.clone(), line, to);
    if line.kind.is_exit() {
        vec![again]
    } else {
        vec![Command::CancelTrigger { account, id }, again]
    }
}

/// Where a new Stop or Target starts: half a dollar's worth of ticks out on its own side.
pub fn default_exit_level(
    kind: TriggerKind,
    right: Right,
    position: i64,
    last: Price,
    tick: i64,
) -> Price {
    let away = 50 * tick.max(1);
    match Direction::of_exit(kind, right, position) {
        Direction::AtOrBelow => Price((last.0 - away).max(tick.max(1))),
        Direction::AtOrAbove => Price(last.0 + away),
    }
}

/// Where an entry starts: above the market for a call, below it for a put.
pub fn default_entry_level(right: Right, last: Price, tick: i64) -> Price {
    let away = 50 * tick.max(1);
    match right {
        Right::Call => Price(last.0 + away),
        Right::Put => Price((last.0 - away).max(tick.max(1))),
    }
}

/// The price nearest `price` on the underlying's grid.
pub fn snap(price: f64, tick: i64) -> Price {
    let tick = tick.max(1);
    Price((price / tick as f64).round() as i64 * tick)
}

/// What a person's Cancel All on these leaves unprotected: positions whose Stop it removes. The
/// owner's ruling: triggers count as orders and the position stays, so the screen says so.
pub fn stops_removed<'a>(
    triggers: impl Iterator<Item = &'a TriggerView>,
    account: &AccountId,
    covered: &[ContractKey],
    position: impl Fn(ContractKey) -> i64,
) -> Vec<ContractKey> {
    let mut out = Vec::new();
    for t in triggers {
        if &t.account == account
            && t.kind == TriggerKind::Stop
            && covered.contains(&t.contract)
            && position(t.contract) != 0
            && !out.contains(&t.contract)
        {
            out.push(t.contract);
        }
    }
    out
}

/// `684C`
pub fn write_series(out: &mut impl Write, contract: ContractKey) -> std::fmt::Result {
    let Some(k) = contract.option_key() else {
        return Ok(());
    };
    let whole = k.strike.0 as f64 / 100.0;
    let right = if k.right == Right::Call { 'C' } else { 'P' };
    if k.strike.0 % 100 == 0 {
        write!(out, "{}{right}", k.strike.0 / 100)
    } else {
        write!(out, "{whole}{right}")
    }
}

/// The message after Cancel All removes a position's Stop: never a position that looks protected
/// and isn't.
pub fn no_stop_message(removed: &[ContractKey]) -> Option<String> {
    let (first, rest) = removed.split_first()?;
    let mut series = String::new();
    let _ = write_series(&mut series, *first);
    Some(if rest.is_empty() {
        format!("Canceled {series}'s Stop too: the position is still open with no stop")
    } else {
        format!(
            "Canceled the Stops on {} positions too: they are still open with no stop",
            removed.len()
        )
    })
}

/// The loud part of an armed row, if anything is wrong or worth saying.
pub fn row_warning(v: &TriggerView) -> Option<(&'static str, String)> {
    if v.stale {
        return Some((
            "Stale feed",
            "no fresh underlying price: this can't fire".to_owned(),
        ));
    }
    if let Some(n) = &v.note {
        return Some(("Waiting", n.clone()));
    }
    if v.restored {
        return Some((
            "Restored",
            "back armed after a restart: check it's still what you want".to_owned(),
        ));
    }
    None
}

/// An ended trigger worth telling: what the trader did themselves isn't news.
pub fn ended_message(v: &TriggerView) -> Option<String> {
    let why = v.reason.as_deref()?;
    if why == "replaced" || why == "canceled" || why.starts_with("you ") {
        return None;
    }
    let mut series = String::new();
    let _ = write_series(&mut series, v.contract);
    Some(format!("{} on {series} ended: {why}", v.kind.word()))
}

/// Tag tops spread so none overlap, keeping order and staying as near their lines as they can.
/// `ys` are the tags' centers, top first; they come back moved.
pub fn spread_tags(ys: &mut [f32], tag_h: f32, top: f32, bottom: f32) {
    let half = tag_h / 2.0;
    let mut floor = top + half;
    for y in ys.iter_mut() {
        if *y < floor {
            *y = floor;
        }
        floor = *y + tag_h;
    }
    let mut ceiling = bottom - half;
    for y in ys.iter_mut().rev() {
        if *y > ceiling {
            *y = ceiling;
        }
        ceiling = *y - tag_h;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lines_core::{CivilDate, Qty};
    use lines_trading::trigger::Direction::{AtOrAbove, AtOrBelow};

    const ENTRY: TriggerKind = TriggerKind::Entry {
        qty: Qty(1),
        max_price: Price(300),
    };

    fn series(strike: i64, right: Right) -> ContractKey {
        ContractKey::option(
            lines_feed::catalog::SPY,
            CivilDate::new(2026, 11, 9).unwrap(),
            Price(strike * 100),
            right,
        )
    }

    fn line(kind: TriggerKind, right: Right) -> Line {
        Line {
            kind,
            contract: series(684, right),
            level: Price(68_400),
            source: LineSource::Draft,
            fires: None,
            working: false,
            stale: false,
        }
    }

    fn around(position: i64) -> Around {
        Around {
            last: Some(Price(68_400)),
            position,
            ..Around::default()
        }
    }

    /// Long a call: a Stop below the market, a Target above. A long put is the mirror.
    #[test]
    fn a_stop_or_target_on_the_wrong_side_of_the_market_refuses() {
        let check = |kind, right, position, to| {
            refusal(&line(kind, right), Price(to), &around(position)).is_some()
        };
        use TriggerKind::{Stop, Target};
        assert!(!check(Stop, Right::Call, 1, 68_300), "below: fine");
        assert!(check(Stop, Right::Call, 1, 68_500), "above: fires at once");
        assert!(check(Stop, Right::Call, 1, 68_400), "at the market");
        assert!(!check(Target, Right::Call, 1, 68_500));
        assert!(check(Target, Right::Call, 1, 68_300));
        assert!(!check(Stop, Right::Put, 1, 68_500), "a put flips");
        assert!(check(Stop, Right::Put, 1, 68_300));
        assert!(!check(Target, Right::Put, 1, 68_300));
        assert!(check(Target, Right::Put, 1, 68_500));
        assert!(!check(Stop, Right::Call, -1, 68_500), "short flips again");
        assert!(check(Stop, Right::Call, -1, 68_300));
    }

    #[test]
    fn a_stop_or_target_on_the_wrong_side_of_the_entry_refuses() {
        let mut a = around(1);
        a.entry = Some(Price(68_350));
        let stop = line(TriggerKind::Stop, Right::Call);
        let target = line(TriggerKind::Target, Right::Call);
        assert!(refusal(&stop, Price(68_300), &a).is_none());
        assert!(
            refusal(&stop, Price(68_380), &a).is_some(),
            "above the entry"
        );
        assert!(refusal(&stop, Price(68_350), &a).is_some(), "on the entry");
        a.entry = Some(Price(68_450));
        assert!(refusal(&target, Price(68_500), &a).is_none());
        assert!(
            refusal(&target, Price(68_420), &a).is_some(),
            "below the entry"
        );
    }

    #[test]
    fn a_stop_and_a_target_stay_apart() {
        let mut a = Around {
            last: None,
            position: 1,
            target: Some(Price(68_500)),
            ..Around::default()
        };
        let stop = line(TriggerKind::Stop, Right::Call);
        assert!(refusal(&stop, Price(68_400), &a).is_none());
        assert!(refusal(&stop, Price(68_500), &a).is_some());
        assert!(refusal(&stop, Price(68_600), &a).is_some());
        a.target = None;
        a.stop = Some(Price(68_400));
        let target = line(TriggerKind::Target, Right::Call);
        assert!(refusal(&target, Price(68_500), &a).is_none());
        assert!(refusal(&target, Price(68_300), &a).is_some());
    }

    #[test]
    fn an_exit_with_no_position_refuses() {
        let stop = line(TriggerKind::Stop, Right::Call);
        assert!(refusal(&stop, Price(68_300), &around(0)).is_some());
    }

    /// A new entry may be either side: that's what decides how it fires. An armed one keeps it.
    #[test]
    fn an_armed_entry_keeps_the_way_it_fires() {
        let draft = line(ENTRY, Right::Call);
        assert!(refusal(&draft, Price(68_500), &around(0)).is_none());
        assert!(refusal(&draft, Price(68_300), &around(0)).is_none());
        assert!(
            refusal(&draft, Price(68_400), &around(0)).is_some(),
            "at it"
        );
        let mut armed = draft;
        armed.fires = Some(AtOrAbove);
        assert!(refusal(&armed, Price(68_500), &around(0)).is_none());
        assert!(
            refusal(&armed, Price(68_300), &around(0)).is_some(),
            "across"
        );
        armed.fires = Some(AtOrBelow);
        assert!(refusal(&armed, Price(68_300), &around(0)).is_none());
        assert!(
            refusal(&armed, Price(68_500), &around(0)).is_some(),
            "across"
        );
        let mut blind = around(0);
        blind.last = None;
        assert!(refusal(&draft, Price(68_500), &blind).is_some(), "no price");
    }

    #[test]
    fn a_half_typed_level_refuses() {
        let entry = line(ENTRY, Right::Call);
        assert!(refusal(&entry, Price(6_800), &around(0)).is_some());
        assert!(refusal(&entry, Price(600), &around(0)).is_some());
        assert!(refusal(&entry, Price(68_900), &around(0)).is_none());
    }

    #[test]
    fn an_entry_at_or_past_an_armed_exit_refuses() {
        let mut a = around(1);
        a.stop = Some(Price(68_300));
        a.target = Some(Price(68_500));
        let entry = line(ENTRY, Right::Call);
        assert!(refusal(&entry, Price(68_350), &a).is_none());
        assert!(refusal(&entry, Price(68_300), &a).is_some());
        assert!(refusal(&entry, Price(68_250), &a).is_some());
        assert!(refusal(&entry, Price(68_500), &a).is_some());
        assert!(refusal(&entry, Price(68_550), &a).is_some());
    }

    #[test]
    fn a_closing_exit_cant_be_moved() {
        let mut stop = line(TriggerKind::Stop, Right::Call);
        stop.working = true;
        assert!(refusal(&stop, Price(68_300), &around(1)).is_some());
    }

    /// One print must never be able to fire two entries: the old goes first. An exit's swap is
    /// the core's, and the old one stays if the new one is refused.
    #[test]
    fn moving_an_entry_cancels_first_and_an_exit_is_replaced() {
        let account = AccountId::new("paper");
        let id = TriggerId(7);
        let cmds = move_commands(
            account.clone(),
            &line(ENTRY, Right::Call),
            id,
            Price(68_500),
        );
        assert!(matches!(cmds[0], Command::CancelTrigger { id: i, .. } if i == id));
        assert!(matches!(&cmds[1], Command::ArmTrigger(s) if s.level == Price(68_500)));
        assert_eq!(cmds.len(), 2);
        let stop = line(TriggerKind::Stop, Right::Call);
        let cmds = move_commands(account, &stop, id, Price(68_300));
        assert_eq!(cmds.len(), 1);
        assert!(matches!(&cmds[0], Command::ArmTrigger(s) if s.level == Price(68_300)));
    }

    /// Two series' drafts of one kind are two lines: a drag moves the one it holds.
    #[test]
    fn a_line_is_itself_across_series() {
        let a = line(TriggerKind::Stop, Right::Call);
        let mut b = a;
        b.contract = series(690, Right::Call);
        assert!(a.same(&a));
        assert!(!a.same(&b));
        let mut other_kind = a;
        other_kind.kind = TriggerKind::Target;
        assert!(!a.same(&other_kind));
        let mut moved = a;
        moved.level = Price(1);
        assert!(a.same(&moved), "the level is what a drag changes");
    }

    #[test]
    fn a_refused_cancel_never_arms() {
        assert!(arm_after_cancel::<(), ()>(&Ok(())));
        assert!(!arm_after_cancel::<(), ()>(&Err(())));
    }

    #[test]
    fn a_stale_armed_entry_refuses_a_move_rather_than_lose_it() {
        let mut entry = line(ENTRY, Right::Call);
        entry.fires = Some(AtOrAbove);
        entry.stale = true;
        assert!(refusal(&entry, Price(68_500), &around(0)).is_some());
        // An exit can move: it needs no fresh print to arm.
        let mut stop = line(TriggerKind::Stop, Right::Call);
        stop.stale = true;
        assert!(refusal(&stop, Price(68_300), &around(1)).is_none());
    }

    #[test]
    fn new_lines_start_on_their_own_side() {
        use TriggerKind::{Stop, Target};
        let d =
            |kind, right, position| default_exit_level(kind, right, position, Price(68_400), 1).0;
        assert_eq!(d(Stop, Right::Call, 1), 68_350);
        assert_eq!(d(Target, Right::Call, 1), 68_450);
        assert_eq!(d(Stop, Right::Put, 1), 68_450);
        assert_eq!(d(Target, Right::Put, 1), 68_350);
        assert_eq!(default_entry_level(Right::Call, Price(68_400), 1).0, 68_450);
        assert_eq!(default_entry_level(Right::Put, Price(68_400), 1).0, 68_350);
        // And a new line is never refused for where it starts.
        for (kind, right) in [(Stop, Right::Call), (Target, Right::Put)] {
            let to = default_exit_level(kind, right, 1, Price(68_400), 1);
            assert_eq!(refusal(&line(kind, right), to, &around(1)), None);
        }
    }

    #[test]
    fn a_price_snaps_to_the_grid() {
        assert_eq!(snap(68_404.4, 5), Price(68_405));
        assert_eq!(snap(68_401.0, 5), Price(68_400));
        assert_eq!(snap(68_401.4, 1), Price(68_401));
    }

    fn view(id: u64, kind: TriggerKind, contract: ContractKey) -> TriggerView {
        TriggerView {
            id: TriggerId(id),
            account: AccountId::new("paper"),
            contract,
            kind,
            level: Price(68_300),
            direction: AtOrBelow,
            state: TriggerState::Armed,
            armed_by: None,
            stale: false,
            restored: false,
            note: None,
            reason: None,
        }
    }

    /// The owner's ruling: Cancel All removing a position's Stop says the position stays open
    /// with no stop. A Target, an entry or a flat series doesn't.
    #[test]
    fn cancel_all_says_when_it_leaves_a_position_with_no_stop() {
        let (held, flat, other) = (
            series(684, Right::Call),
            series(685, Right::Call),
            series(686, Right::Call),
        );
        let account = AccountId::new("paper");
        let triggers = [
            view(1, TriggerKind::Stop, held),
            view(2, TriggerKind::Target, other),
            view(3, TriggerKind::Stop, flat),
            view(4, ENTRY, other),
        ];
        let position = |c: ContractKey| i64::from(c == held || c == other);
        let removed = stops_removed(triggers.iter(), &account, &[held, flat, other], position);
        assert_eq!(removed, vec![held]);
        let text = no_stop_message(&removed).unwrap();
        assert!(text.contains("684C"), "{text}");
        assert!(text.contains("still open with no stop"), "{text}");
        assert_eq!(no_stop_message(&[]), None);
        let many = no_stop_message(&[held, other]).unwrap();
        assert!(many.contains("still open with no stop"), "{many}");
        // Not covered, another account: no message.
        let none = stops_removed(triggers.iter(), &account, &[other], position);
        assert!(none.is_empty());
        let none = stops_removed(triggers.iter(), &AccountId::new("x"), &[held], position);
        assert!(none.is_empty());
    }

    #[test]
    fn an_armed_row_says_whats_wrong_loudly() {
        let c = series(684, Right::Call);
        let mut v = view(1, TriggerKind::Stop, c);
        assert_eq!(row_warning(&v), None);
        v.restored = true;
        assert_eq!(row_warning(&v).unwrap().0, "Restored");
        v.note = Some("no usable bid".to_owned());
        assert_eq!(row_warning(&v).unwrap().0, "Waiting");
        v.stale = true;
        let (word, text) = row_warning(&v).unwrap();
        assert_eq!(word, "Stale feed");
        assert!(text.contains("can't fire"), "{text}");
    }

    #[test]
    fn only_an_end_the_trader_didnt_cause_is_news() {
        let c = series(684, Right::Call);
        let mut v = view(1, ENTRY, c);
        assert_eq!(ended_message(&v), None);
        v.reason = Some("the ask is above your max".to_owned());
        let m = ended_message(&v).unwrap();
        assert!(m.contains("Entry on 684C"), "{m}");
        assert!(m.contains("the ask is above your max"), "{m}");
        for mine in ["replaced", "canceled", "you canceled all", "you flattened"] {
            v.reason = Some(mine.to_owned());
            assert_eq!(ended_message(&v), None, "{mine}");
        }
    }

    #[test]
    fn a_series_reads_short() {
        let write = |strike: i64, right| {
            let mut s = String::new();
            let k = ContractKey::option(
                lines_feed::catalog::SPY,
                CivilDate::new(2026, 11, 9).unwrap(),
                Price(strike),
                right,
            );
            write_series(&mut s, k).unwrap();
            s
        };
        assert_eq!(write(68_400, Right::Call), "684C");
        assert_eq!(write(68_450, Right::Put), "684.5P");
    }

    #[test]
    fn tags_spread_without_overlapping_and_stay_inside() {
        let mut ys = [100.0, 104.0, 108.0];
        spread_tags(&mut ys, 20.0, 0.0, 500.0);
        assert_eq!(ys, [100.0, 120.0, 140.0]);
        let mut top = [2.0, 300.0];
        spread_tags(&mut top, 20.0, 0.0, 500.0);
        assert_eq!(top, [10.0, 300.0]);
        let mut foot = [300.0, 498.0, 499.0];
        spread_tags(&mut foot, 20.0, 0.0, 500.0);
        assert_eq!(foot, [300.0, 470.0, 490.0]);
    }
}
