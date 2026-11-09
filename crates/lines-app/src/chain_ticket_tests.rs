//! The chain's order paths end to end without GPUI: what `ChainView` builds (`ticket_order`,
//! `close_command`, `click_order`, `exit_commands`) → `Trading::submit` → the paper core on its
//! thread → the cache the pane reads. The view adds only its checks and the key or click.

use std::time::Instant;

use lines_core::{CivilDate, ContractKey, OptionKey, OptionQuote, Price, Qty, Right, UtcMillis};
use lines_feed::registry::FeedRegistry;
use lines_feed::{FeedConfig, Source as FeedSource, catalog};
use lines_trading::{
    AccountConfig, AccountId, Command, MarketInput, Order, OrderKind, Side, Source,
};

use super::*;
use crate::trading::{Trading, TradingSetup};

const LATENCY_MS: i64 = 200;

fn account() -> AccountId {
    AccountId::new("paper")
}

fn key(strike: i64, right: Right) -> OptionKey {
    OptionKey {
        expiry: CivilDate::new(2026, 11, 9).unwrap(),
        strike: Price(strike * 100),
        right,
    }
}

fn contract_of(k: OptionKey) -> ContractKey {
    ContractKey::option(catalog::SPY, k.expiry, k.strike, k.right)
}

fn rules() -> lines_core::OptionRules {
    catalog::get(catalog::SPY).unwrap().options().unwrap()
}

struct Rig {
    t: Trading,
    now: i64,
}

impl Rig {
    fn new() -> Rig {
        let mut setup = TradingSetup::manual();
        setup.accounts = Some(vec![AccountConfig::paper("paper", 100_000)]);
        let base = FeedConfig::new(FeedSource::Replay {
            path: "unused.lfr".into(),
            speed: 0.0,
            stop_at: None,
        });
        let t = Trading::start(setup, FeedRegistry::new(), base);
        // Monday 10:00 ET: the regular session.
        let now = UtcMillis::parse_iso("2026-11-09T15:00:00Z").unwrap().0;
        t.handle().advance_to(UtcMillis(now));
        Rig { t, now }
    }

    fn wait(&mut self, ms: i64) {
        self.now += ms;
        self.t.handle().advance_to(UtcMillis(self.now));
        self.t.settle();
    }

    /// A quote on a contract, stamped now.
    fn quote_on(&mut self, c: ContractKey, bid: i64, ask: i64) -> OptionQuote {
        self.wait(1);
        let time = UtcMillis(self.now);
        self.t.handle().market(
            c,
            vec![MarketInput::OptionQuote {
                bid: Some(Price(bid)),
                ask: Some(Price(ask)),
                last: None,
                time,
            }],
        );
        self.t.settle();
        OptionQuote {
            bid: Some(Price(bid)),
            ask: Some(Price(ask)),
            time,
            ..Default::default()
        }
    }

    fn quote(&mut self, k: OptionKey, bid: i64, ask: i64) -> OptionQuote {
        self.quote_on(contract_of(k), bid, ask)
    }

    /// Send as the pane does, past the broker's latency.
    fn send(&mut self, c: Command) {
        self.t.submit(Source::Dom, Instant::now(), c);
        self.t.settle();
        self.wait(LATENCY_MS);
    }

    fn position(&self, c: ContractKey) -> i64 {
        self.t
            .cache
            .read()
            .position(&account(), c)
            .map_or(0, |p| p.qty)
    }

    fn orders(&self, c: ContractKey) -> Vec<Order> {
        self.t
            .cache
            .read()
            .open_orders(&account(), c)
            .cloned()
            .collect()
    }

    /// A position, bought at the ask.
    fn hold(&mut self, c: ContractKey, qty: i64, ask: i64) {
        self.send(command(account(), c, Side::Buy, qty, Price(ask), false));
        assert_eq!(self.position(c), qty, "the buy filled at the ask");
    }
}

/// A ticket's send reaches the core as the button says: its side, the shared size, its limit,
/// and not reduce-only.
#[test]
fn a_ticket_sends_its_side_size_and_price() {
    let mut r = Rig::new();
    let k = key(684, Right::Call);
    let q = r.quote(k, 240, 242);
    // A sell ticket at the mid: it rests, so what reached the core can be read.
    let t = Ticket::new(k, Side::Sell, Some(&q), rules());
    let cmd = ticket_order(&t, account(), contract_of(k), 3, 0, false).unwrap();
    r.send(cmd);
    let o = r.orders(contract_of(k));
    assert_eq!(o.len(), 1);
    assert_eq!((o[0].side, o[0].qty), (Side::Sell, Qty(3)));
    assert_eq!(o[0].kind, OrderKind::Limit { price: Price(241) });
    assert!(!o[0].reduce_only);
    assert_eq!(r.position(contract_of(k)), 0);

    // A buy ticket moved to the ask fills there, at its size.
    let k = key(686, Right::Put);
    let q = r.quote(k, 300, 304);
    let mut t = Ticket::new(k, Side::Buy, Some(&q), rules());
    t.set_price(Price(304));
    r.send(ticket_order(&t, account(), contract_of(k), 2, 0, false).unwrap());
    assert_eq!(r.position(contract_of(k)), 2);

    // Nothing is built without a price.
    t.price = None;
    assert!(ticket_order(&t, account(), contract_of(k), 2, 0, false).is_err());
}

/// A ticket's Close and the row's Close reach the core reduce-only, for the whole position on
/// the reducing side, and end flat, never short.
#[test]
fn a_close_reaches_the_core_reduce_only() {
    let mut r = Rig::new();
    let k = key(684, Right::Call);
    let c = contract_of(k);
    let q = r.quote(k, 240, 242);
    r.hold(c, 2, 242);

    let mut t = Ticket::new(k, Side::Sell, Some(&q), rules());
    t.set_close(true, Some(&q), rules());
    r.send(ticket_order(&t, account(), c, 9, 2, false).unwrap());
    let o = r.orders(c);
    assert_eq!(o.len(), 1, "{o:?}");
    assert_eq!(
        (o[0].side, o[0].qty),
        (Side::Sell, Qty(2)),
        "not the size: the position"
    );
    assert_eq!(o[0].kind, OrderKind::Limit { price: Price(241) }, "the mid");
    assert!(o[0].reduce_only);
    r.quote(k, 250, 252);
    r.wait(LATENCY_MS);
    assert_eq!(r.position(c), 0, "closed, never short");

    // Nothing to close, or the position is on the ticket's own side, or a close already works.
    let err = |pos, closing| ticket_order(&t, account(), c, 1, pos, closing).unwrap_err();
    assert_eq!(err(0, false), "no position to close");
    assert_eq!(err(2, true), "a close is already working; Cancel it first");
    let mut buy = t.clone();
    buy.side = Side::Buy;
    assert!(
        ticket_order(&buy, account(), c, 1, 2, false).is_err(),
        "it would add"
    );

    // The row's Close on a short: a reduce-only buy at the mid.
    let k = key(690, Right::Put);
    let c = contract_of(k);
    let q = r.quote(k, 600, 604);
    r.send(command(account(), c, Side::Sell, 3, Price(600), false));
    assert_eq!(r.position(c), -3);
    r.send(close_command(account(), c, -3, Some(&q), rules()).unwrap());
    let o = r.orders(c);
    assert_eq!((o[0].side, o[0].qty), (Side::Buy, Qty(3)));
    assert_eq!(o[0].kind, OrderKind::Limit { price: Price(602) });
    assert!(o[0].reduce_only);
    r.quote(k, 590, 594);
    r.wait(LATENCY_MS);
    assert_eq!(r.position(c), 0, "closed, never long");
}

/// One-click takes the clicked side's own price: an ask buys there, a bid sells there.
#[test]
fn one_click_reaches_the_core_at_the_clicked_side() {
    let mut r = Rig::new();
    let k = key(684, Right::Call);
    let c = contract_of(k);
    let q = r.quote(k, 240, 242);
    r.send(click_order(account(), c, Cell::Ask, Some(&q), 2).unwrap());
    assert_eq!(r.position(c), 2);
    r.send(click_order(account(), c, Cell::Bid, Some(&q), 2).unwrap());
    assert_eq!(r.position(c), 0);
    let one = OptionQuote { ask: None, ..q };
    assert!(click_order(account(), c, Cell::Ask, Some(&one), 2).is_none());
}

/// Flatten and Cancel All act on every option series of the underlying on the account, every
/// expiry, and on nothing else: not another underlying's.
#[test]
fn the_exits_act_on_the_underlyings_options_only() {
    let mut r = Rig::new();
    let near = key(684, Right::Call);
    let later = OptionKey {
        expiry: CivilDate::new(2026, 11, 13).unwrap(),
        ..key(686, Right::Put)
    };
    let working = key(690, Right::Call);
    let other = ContractKey::option(
        catalog::QQQ,
        CivilDate::new(2026, 11, 9).unwrap(),
        Price(60_000),
        Right::Call,
    );
    r.quote(near, 240, 242);
    r.quote(later, 300, 304);
    r.quote(working, 100, 104);
    r.quote_on(other, 500, 504);
    r.hold(contract_of(near), 2, 242);
    r.hold(contract_of(later), 1, 304);
    r.hold(other, 1, 504);
    r.send(command(
        account(),
        contract_of(working),
        Side::Buy,
        1,
        Price(100),
        false,
    ));
    r.send(command(account(), other, Side::Sell, 1, Price(600), false));
    assert_eq!(r.orders(contract_of(working)).len(), 1);

    let held = |r: &Rig| held_contracts(&r.t.cache.read(), &account(), catalog::SPY);
    let mut seen = held(&r);
    seen.sort();
    let mut want = vec![contract_of(near), contract_of(later), contract_of(working)];
    want.sort();
    assert_eq!(
        seen, want,
        "the underlying's series, every expiry, not QQQ's"
    );

    // Cancel All: the orders go, the positions stay.
    for c in exit_commands(&account(), &held(&r), false) {
        r.send(c);
    }
    assert!(r.orders(contract_of(working)).is_empty());
    assert_eq!(r.position(contract_of(near)), 2);
    assert_eq!(r.position(contract_of(later)), 1);

    // Flatten: every position closes at its own quote.
    for c in exit_commands(&account(), &held(&r), true) {
        r.send(c);
    }
    assert_eq!(r.position(contract_of(near)), 0);
    assert_eq!(r.position(contract_of(later)), 0);
    assert!(held(&r).is_empty(), "nothing left to act on");
    // The other underlying is untouched.
    assert_eq!(r.position(other), 1);
    assert_eq!(r.orders(other).len(), 1);
}

/// The prop check reads the rules in force, which Settings can change, not the start's preset.
#[test]
fn prop_rules_are_read_live() {
    let r = Rig::new();
    let has = |at_start| has_rules(&r.t.cache.read(), &account(), at_start);
    // Before the core has reported the account: the starting preset.
    r.t.settle();
    assert!(!has(false));
    r.t.submit(
        Source::Test,
        Instant::now(),
        Command::SetRules {
            account: account(),
            rules: lines_trading::rules::presets()
                .into_iter()
                .nth(1)
                .map(|mut p| {
                    p.account_size = 100_000;
                    p
                }),
        },
    );
    r.t.settle();
    assert!(has(false), "rules added while running");
    assert!(ticket_refuses_prop(&r));
}

/// The order path's own check, on rules that changed while running.
fn ticket_refuses_prop(r: &Rig) -> bool {
    let cmd = command(
        account(),
        contract_of(key(684, Right::Call)),
        Side::Buy,
        1,
        Price(241),
        false,
    );
    let id = r.t.submit(Source::Test, Instant::now(), cmd);
    r.t.settle();
    let st = r.t.cache.read();
    matches!(st.result(id), Some(Err(e)) if e.message == lines_trading::risk::PROP_OPTIONS_REFUSAL)
}

/// `--paper`'s `option` step puts what follows on that series, and refuses a bad one.
#[test]
fn the_script_trades_an_option_series() {
    let spy = catalog::get(catalog::SPY).unwrap();
    let eq = ContractKey::equity(catalog::SPY);
    let steps = crate::trading::parse_script(
        "option 2026-11-09 684 call; buy 2 @ 2.42",
        &spy,
        eq,
        &account(),
    )
    .unwrap();
    assert!(steps.iter().any(|s| matches!(
        s,
        crate::trading::Step::Command(Command::PlaceOrder(o))
            if o.contract == contract_of(key(684, Right::Call)) && o.qty == Qty(2)
    )));
    for bad in [
        "option 2026-13-09 684 call",
        "option 2026-11-09 684 sideways",
    ] {
        assert!(
            crate::trading::parse_script(bad, &spy, eq, &account()).is_err(),
            "{bad}"
        );
    }
}

/// A position's Stop comes back through the app's own start, the file and the first rebuild, and
/// fires on the first print past it. An entry doesn't come back (decision 0029).
#[test]
fn a_saved_stop_comes_back_through_the_apps_start() {
    use lines_trading::trigger::{TriggerKind, TriggerSpec};
    let dir = std::env::temp_dir().join(format!("lines-chain-stop-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let file = dir.join("paper.json");
    let k = key(684, Right::Call);
    let c = contract_of(k);
    let spy = ContractKey::equity(catalog::SPY);
    let start = |at: i64| {
        let mut setup = TradingSetup::manual();
        setup.accounts = Some(vec![AccountConfig::paper("paper", 100_000)]);
        setup.file = Some(file.clone());
        let base = FeedConfig::new(FeedSource::Replay {
            path: "unused.lfr".into(),
            speed: 0.0,
            stop_at: None,
        });
        let t = Trading::start(setup, FeedRegistry::new(), base);
        t.handle().advance_to(UtcMillis(at));
        t.settle();
        Rig { t, now: at }
    };
    let print = |r: &mut Rig, cents: i64| {
        r.wait(1);
        let time = UtcMillis(r.now);
        r.t.handle().market(
            spy,
            vec![MarketInput::Trade {
                time,
                price: Price(cents),
                qty: Qty(1),
            }],
        );
        r.t.settle();
    };
    let at = UtcMillis::parse_iso("2026-11-09T15:00:00Z").unwrap().0;
    let mut r = start(at);
    print(&mut r, 68_300);
    r.quote(k, 240, 242);
    r.hold(c, 2, 242);
    for (contract, kind, level) in [
        (c, TriggerKind::Stop, 68_000),
        (
            contract_of(key(690, Right::Put)),
            TriggerKind::Entry {
                qty: Qty(1),
                max_price: Price(1_000),
            },
            68_200,
        ),
    ] {
        r.send(Command::ArmTrigger(TriggerSpec {
            account: account(),
            contract,
            kind,
            level: Price(level),
        }));
    }
    let now = r.now;
    drop(r);

    let mut r = start(now);
    assert_eq!(r.position(c), 2, "the position is back");
    // The notice reaches the app, though it was raised before the app was listening.
    {
        let st = r.t.cache.read();
        let back: Vec<_> = st.triggers().collect();
        assert_eq!(back.len(), 1, "the Stop, not the entry");
        assert_eq!(back[0].kind, TriggerKind::Stop);
        assert!(back[0].restored, "with a notice");
    }
    r.quote(k, 150, 152);
    print(&mut r, 67_900);
    r.wait(LATENCY_MS);
    assert_eq!(r.position(c), 0, "its Stop fired");
    assert!(
        r.orders(contract_of(key(690, Right::Put))).is_empty(),
        "the entry didn't come back"
    );
    drop(r);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_exits_say_what_they_act_on() {
    assert_eq!(exit_label("SPY", true), "Flatten SPY options");
    assert_eq!(exit_label("SPY", false), "Cancel SPY options");
}

fn arm(r: &mut Rig, contract: ContractKey, kind: lines_trading::trigger::TriggerKind, level: i64) {
    r.send(Command::ArmTrigger(lines_trading::trigger::TriggerSpec {
        account: account(),
        contract,
        kind,
        level: Price(level),
    }));
}

fn live_triggers(r: &Rig) -> usize {
    r.t.cache.read().triggers().count()
}

fn underlying_print(r: &mut Rig, cents: i64) {
    let time = UtcMillis(r.now);
    r.t.handle().market(
        ContractKey::equity(catalog::SPY),
        vec![lines_trading::MarketInput::Trade {
            time,
            price: Price(cents),
            qty: Qty(1),
        }],
    );
    r.t.settle();
}

/// The owner's ruling: Cancel All takes armed triggers and leaves the position, and the message
/// says so. It reaches a series with only a trigger on it, and the chain's buttons act on it.
#[test]
fn cancel_all_takes_the_stop_and_leaves_the_position_saying_so() {
    use lines_trading::trigger::TriggerKind;
    let mut r = Rig::new();
    let k = key(684, Right::Call);
    let c = contract_of(k);
    r.quote(k, 240, 242);
    r.hold(c, 2, 242);
    underlying_print(&mut r, 68_300);
    arm(&mut r, c, TriggerKind::Stop, 68_000);
    // Only a trigger on this one.
    let other = contract_of(key(690, Right::Put));
    arm(
        &mut r,
        other,
        TriggerKind::Entry {
            qty: Qty(1),
            max_price: Price(500),
        },
        68_000,
    );
    assert_eq!(live_triggers(&r), 2);
    let (contracts, message) = {
        let st = r.t.cache.read();
        let contracts = held_contracts(&st, &account(), catalog::SPY);
        assert!(contracts.contains(&c));
        assert!(contracts.contains(&other), "a trigger alone counts");
        let removed =
            crate::chain_lines::stops_removed(st.triggers(), &account(), &contracts, |x| {
                st.position(&account(), x).map_or(0, |p| p.qty)
            });
        (contracts, crate::chain_lines::no_stop_message(&removed))
    };
    let message = message.expect("a position's Stop is going");
    assert!(message.contains("still open with no stop"), "{message}");
    for cmd in exit_commands(&account(), &contracts, false) {
        r.send(cmd);
    }
    r.wait(LATENCY_MS);
    assert_eq!(r.position(c), 2, "the position stays");
    assert_eq!(live_triggers(&r), 0, "triggers count as orders");
}

/// Flatten closes, and that ends the triggers, with no message about a stop: nothing is left to
/// stop.
#[test]
fn flatten_ends_the_triggers_and_closes() {
    use lines_trading::trigger::TriggerKind;
    let mut r = Rig::new();
    let k = key(684, Right::Call);
    let c = contract_of(k);
    r.quote(k, 240, 242);
    r.hold(c, 2, 242);
    underlying_print(&mut r, 68_300);
    arm(&mut r, c, TriggerKind::Stop, 68_000);
    arm(&mut r, c, TriggerKind::Target, 69_000);
    assert_eq!(live_triggers(&r), 2);
    let contracts = held_contracts(&r.t.cache.read(), &account(), catalog::SPY);
    for cmd in exit_commands(&account(), &contracts, true) {
        r.send(cmd);
    }
    r.wait(LATENCY_MS);
    assert_eq!(r.position(c), 0, "closed");
    assert_eq!(live_triggers(&r), 0, "and its triggers end");
}

/// A dragged Stop is the core's swap: one Stop afterward, at the new level, and if the new level
/// is refused the old one stays.
#[test]
fn moving_a_stop_goes_through_the_core() {
    use crate::chain_lines::{Line, LineSource, move_command};
    use lines_trading::trigger::TriggerKind;
    let mut r = Rig::new();
    let k = key(684, Right::Call);
    let c = contract_of(k);
    r.quote(k, 240, 242);
    r.hold(c, 2, 242);
    underlying_print(&mut r, 68_300);
    arm(&mut r, c, TriggerKind::Stop, 68_000);
    let armed = |r: &Rig| -> Vec<_> { r.t.cache.read().triggers().cloned().collect() };
    let v = armed(&r);
    assert_eq!(v.len(), 1);
    let line = Line::of(&v[0]);
    assert_eq!(line.source, LineSource::Armed(v[0].id));
    r.send(move_command(account(), c, v[0].id, Price(68_100)));
    let after = armed(&r);
    assert_eq!(after.len(), 1, "replaced");
    assert_eq!(after[0].level, Price(68_100));
    // Past the market: the core refuses and the Stop that was there stays.
    r.send(move_command(account(), c, after[0].id, Price(68_400)));
    let kept = armed(&r);
    assert_eq!(kept.len(), 1, "still protected");
    assert_eq!(kept[0].level, Price(68_100));
    // An ordinary replacement isn't news, and what ended is told once.
    let st = r.t.cache.read();
    assert!(st.triggers_ended_count() >= 1);
    assert!(
        st.triggers_ended_since(0)
            .all(|v| crate::chain_lines::ended_message(v).is_none())
    );
    assert_eq!(
        st.triggers_ended_since(st.triggers_ended_count()).count(),
        0
    );
}

/// "When SPY" is the same ticket: an entry at the level with the ticket's size and its price as
/// the max, and never a second order path. Only a buy can wait.
#[test]
fn a_when_ticket_arms_an_entry_with_its_max() {
    use lines_trading::trigger::TriggerKind;
    let k = key(684, Right::Call);
    let q = OptionQuote {
        bid: Some(Price(240)),
        ask: Some(Price(244)),
        last: None,
        volume: Qty(0),
        open_interest: 0,
        iv_bp: None,
        delta_e4: None,
        time: UtcMillis(0),
    };
    let mut t = Ticket::new(k, Side::Buy, Some(&q), rules());
    assert!(t.can_be_when());
    assert!(
        when_trigger(&t, account(), contract_of(k), 1).is_err(),
        "no level"
    );
    t.set_when(Some(Price(68_500)), Some(&q), rules());
    assert_eq!(t.price, Some(Price(244)), "the max starts at the ask");
    let Ok(Command::ArmTrigger(spec)) = when_trigger(&t, account(), contract_of(k), 3) else {
        panic!("an entry");
    };
    assert_eq!(spec.level, Price(68_500));
    assert_eq!(
        spec.kind,
        TriggerKind::Entry {
            qty: Qty(3),
            max_price: Price(244)
        }
    );
    assert!(when_trigger(&t, account(), contract_of(k), 0).is_err());
    // Back to now: at the mid again, with no level.
    t.set_when(None, Some(&q), rules());
    assert_eq!(t.when, None);
    assert_eq!(t.price, Some(Price(242)));
    // A sell can't.
    let sell = Ticket::new(k, Side::Sell, Some(&q), rules());
    assert!(!sell.can_be_when());
    // A close ends "When".
    let mut close = Ticket::new(k, Side::Buy, Some(&q), rules());
    close.set_when(Some(Price(68_500)), Some(&q), rules());
    close.set_close(true, Some(&q), rules());
    assert_eq!(close.when, None);
    assert!(!close.can_be_when());
}

/// Keys go to the field in use. The level never reads as nothing while it's typed, so its line
/// doesn't vanish; it can be far off, which `chain_lines` won't arm.
#[test]
fn a_when_tickets_keys_go_to_the_field_in_use() {
    let p = catalog::get(catalog::SPY).unwrap();
    let k = key(684, Right::Call);
    let mut t = Ticket::new(k, Side::Buy, None, rules());
    t.set_when(Some(Price(68_500)), None, rules());
    assert!(t.on_level, "the level first");
    for c in "686.5".chars() {
        assert!(t.type_char(c, &p));
    }
    assert_eq!(t.when, Some(Price(68_650)));
    assert_eq!(t.price, None, "the max isn't touched");
    for _ in 0..5 {
        assert!(t.backspace(&p));
    }
    assert!(t.when.is_some(), "empty: the last that read stays");
    for c in "686.5".chars() {
        assert!(t.type_char(c, &p));
    }
    assert_eq!(t.when, Some(Price(68_650)));
    t.step_level(true, 5);
    assert_eq!(t.when, Some(Price(68_655)));
    t.step_level(false, 5);
    assert_eq!(t.when, Some(Price(68_650)));
    t.on_level = false;
    for c in "1.25".chars() {
        assert!(t.type_char(c, &p));
    }
    assert_eq!(t.price, Some(Price(125)));
    assert_eq!(t.when, Some(Price(68_650)));
}
