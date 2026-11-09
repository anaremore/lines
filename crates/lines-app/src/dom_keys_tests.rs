//! Order hotkeys end to end without GPUI: a keystroke (`commands::command_for`) → the order key
//! gate (`dom_trade::key_order`: the DOM's order hotkeys, then the trading switch) → the paper
//! core on its thread → the cache the panes read. The same path `trade_bar::run_key` takes from
//! the key-down handler, so guardrails, risk and prop rules apply exactly as for clicks.

use std::collections::BTreeMap;
use std::time::Instant;

use gpui::Keystroke;
use lines_core::{ContractKey, Month, Price, Qty, UtcMillis};
use lines_feed::registry::FeedRegistry;
use lines_feed::{FeedConfig, Source as FeedSource, catalog};
use lines_trading::{
    AccountConfig, AccountId, Command as TradeCommand, CommandId, Guardrails, MarketInput,
    OrderKind, RejectCode, Side, Source,
};

use crate::commands::{Command, TradeCmd, command_for};
use crate::dom_trade::{
    self, ArmEvent, ArmPrefs, Arming, ENABLE_TO_TRADE, EntryCtx, HOTKEYS_OFF, KEYS_HIDDEN, Refusal,
};
use crate::trading::{Trading, TradingSetup};

fn es() -> ContractKey {
    ContractKey::monthly(catalog::ES, 2026, Month::new(12).unwrap())
}

fn px(p: f64) -> Price {
    Price((p * 100.0).round() as i64)
}

const BID: f64 = 5999.75;
const ASK: f64 = 6000.00;

/// A Tuesday afternoon, market open.
fn t0() -> UtcMillis {
    UtcMillis::parse_iso("2026-11-10T15:00:00Z").unwrap()
}

fn start(guardrails: Option<Guardrails>) -> Trading {
    let mut setup = TradingSetup::manual();
    setup.accounts = Some(vec![AccountConfig::paper("paper", 50_000)]);
    if let Some(g) = guardrails {
        setup.guardrails = BTreeMap::from([("paper".to_owned(), g)]);
    }
    let base = FeedConfig::new(FeedSource::Replay {
        path: "unused.lfr".into(),
        speed: 0.0,
        stop_at: None,
    });
    let t = Trading::start(setup, FeedRegistry::new(), base);
    t.handle().advance_to(t0());
    market(&t, 0);
    t
}

/// The book (one bid, one ask) and a print between them, `ms` after the start: time moves past
/// the paper ack latency and market orders fill.
fn market(t: &Trading, ms: i64) {
    t.handle().market(
        es(),
        vec![
            MarketInput::BookClear,
            MarketInput::Book {
                side: lines_core::Side::Bid,
                price: px(BID),
                qty: Qty(50),
            },
            MarketInput::Book {
                side: lines_core::Side::Ask,
                price: px(ASK),
                qty: Qty(50),
            },
            MarketInput::Trade {
                time: UtcMillis(t0().0 + ms),
                price: px(ASK),
                qty: Qty(1),
            },
        ],
    );
    t.settle();
}

fn armed() -> Arming {
    let mut a = Arming::default();
    a.apply(ArmEvent::Toggle, ArmPrefs::default());
    a
}

fn account() -> AccountId {
    AccountId::new("paper")
}

/// What `trade_bar::entry_ctx` reads for the pane: position and working orders from the cache.
fn ctx(t: &Trading) -> EntryCtx {
    let s = t.cache.read();
    EntryCtx {
        account: account(),
        contract: es(),
        qty: Qty(2),
        position: s.position(&account(), es()).map_or(0, |p| p.qty),
        bracket: None,
        tick: 25,
        best_bid: Some(px(BID)),
        best_ask: Some(px(ASK)),
        last: Some(px(ASK)),
    }
}

/// The trading command a key sends ("ctrl" is Cmd on macOS).
fn key(keys: &str) -> TradeCmd {
    let ctrl = if cfg!(target_os = "macos") {
        "cmd"
    } else {
        "ctrl"
    };
    let s = format!("{ctrl}-{keys}");
    match command_for(&Keystroke::parse(Box::leak(s.into_boxed_str())).unwrap()) {
        Some(Command::Trade(tc)) => tc,
        other => panic!("{keys}: {other:?}"),
    }
}

/// Press an order key on a DOM: built by the gate, then sent (as `run_key` does).
fn press(t: &Trading, keys: &str, hotkeys: bool, arming: Arming) -> Result<CommandId, Refusal> {
    let tc = key(keys);
    assert!(tc.needs_hotkeys(), "{keys}: an order key");
    let q = tc.quick().expect("an order key");
    let cmd = dom_trade::key_order(q, hotkeys, true, arming, || Ok(ctx(t)))?;
    let id = t.submit(Source::Hotkey, Instant::now(), cmd);
    t.settle();
    Ok(id)
}

/// Flatten or Cancel all from its key: never gated by the hotkeys or the switch.
fn press_exit(t: &Trading, keys: &str) -> CommandId {
    let tc = key(keys);
    assert!(!tc.needs_hotkeys(), "{keys}: exits are never gated");
    let cmd = match tc {
        TradeCmd::Flatten => TradeCommand::Flatten {
            account: account(),
            contract: Some(es()),
        },
        TradeCmd::CancelAll => TradeCommand::CancelAll {
            account: account(),
            contract: Some(es()),
        },
        other => panic!("{other:?}"),
    };
    let id = t.submit(Source::Hotkey, Instant::now(), cmd);
    t.settle();
    id
}

const ORDER_KEYS: [&str; 7] = [
    "alt-b", "alt-s", "alt-j", "alt-k", "alt-o", "alt-i", "alt-r",
];

#[test]
fn order_keys_map_to_their_orders_and_only_they_need_hotkeys() {
    let want = [
        TradeCmd::BuyMarket,
        TradeCmd::SellMarket,
        TradeCmd::JoinBid,
        TradeCmd::JoinAsk,
        TradeCmd::BuyAsk,
        TradeCmd::SellBid,
        TradeCmd::Reverse,
    ];
    for (k, w) in ORDER_KEYS.iter().zip(want) {
        assert_eq!(key(k), w, "{k}");
    }
    assert_eq!(key("shift-y"), TradeCmd::ToggleHotkeys);
    let needs: Vec<_> = TradeCmd::ALL
        .into_iter()
        .chain(TradeCmd::PALETTE_ONLY)
        .filter(|c| c.needs_hotkeys())
        .collect();
    assert_eq!(needs.len(), want.len(), "{needs:?}");
    assert!(want.iter().all(|w| needs.contains(w)));
}

/// Off by default: with the hotkeys off nothing is built or sent, whatever the switch says;
/// with them on and trading disabled, the switch's refusal.
#[test]
fn order_keys_are_refused_while_hotkeys_or_trading_are_off() {
    let t = start(None);
    for k in ORDER_KEYS {
        assert_eq!(
            press(&t, k, false, armed()),
            Err(Refusal(HOTKEYS_OFF)),
            "{k}"
        );
        assert_eq!(
            press(&t, k, false, Arming::default()),
            Err(Refusal(HOTKEYS_OFF)),
            "{k}"
        );
        // Hotkeys on, trading off: every order key (Reverse too) needs the switch first.
        assert_eq!(
            press(&t, k, true, Arming::default()),
            Err(Refusal(ENABLE_TO_TRADE)),
            "{k}"
        );
    }
    let s = t.cache.read();
    assert_eq!(s.open_orders(&account(), es()).count(), 0, "nothing sent");
    assert!(s.position(&account(), es()).is_none());
    let (_, _, n) = t.handle().latency();
    assert_eq!(n, 0, "no command reached the core");
}

/// A short pane with no room for the Keys chip: order keys send nothing, whatever else is on, and
/// say why. Off still says off. Flatten and Cancel all never depend on it.
#[test]
fn order_keys_send_nothing_while_the_keys_chip_is_out_of_sight() {
    let t = start(None);
    for k in ORDER_KEYS {
        let q = key(k).quick().expect("an order key");
        let hidden = dom_trade::key_order(q, true, false, armed(), || Ok(ctx(&t)));
        assert_eq!(hidden.unwrap_err(), Refusal(KEYS_HIDDEN), "{k}");
        let off = dom_trade::key_order(q, false, false, armed(), || Ok(ctx(&t)));
        assert_eq!(off.unwrap_err(), Refusal(HOTKEYS_OFF), "{k}");
    }
    let (_, _, n) = t.handle().latency();
    assert_eq!(n, 0, "no command reached the core");
    for k in ["shift-f", "shift-a"] {
        let id = press_exit(&t, k);
        assert!(
            matches!(t.cache.read().result(id), Some(Ok(_))),
            "{k} works with the chip out of sight"
        );
    }
}

/// Each order key with the hotkeys and trading on: the order it names, through the core.
#[test]
fn each_order_key_places_its_order() {
    let t = start(None);
    let mut ms = 0;
    let mut step = |t: &Trading| {
        ms += 100;
        market(t, ms);
    };
    // Resting limits: join bid / ask rest at the touch.
    for (k, side, price) in [("alt-j", Side::Buy, BID), ("alt-k", Side::Sell, ASK)] {
        let id = press(&t, k, true, armed()).unwrap();
        step(&t);
        let s = t.cache.read();
        assert!(
            matches!(s.result(id), Some(Ok(ids)) if ids.len() == 1),
            "{k}"
        );
        let o: Vec<_> = s.open_orders(&account(), es()).cloned().collect();
        assert_eq!(o.len(), 1, "{k}");
        assert_eq!((o[0].side, o[0].qty), (side, Qty(2)), "{k}");
        assert_eq!(o[0].kind, OrderKind::Limit { price: px(price) }, "{k}");
        drop(s);
        press_exit(&t, "shift-a");
        step(&t);
        assert_eq!(t.cache.read().open_orders(&account(), es()).count(), 0);
    }
    // Buy at the ask: a marketable limit, filled at the ask; sell at the bid closes it.
    press(&t, "alt-o", true, armed()).unwrap();
    step(&t);
    assert_eq!(
        t.cache.read().position(&account(), es()).map(|p| p.qty),
        Some(2)
    );
    press(&t, "alt-i", true, armed()).unwrap();
    step(&t);
    assert!(t.cache.read().position(&account(), es()).is_none(), "flat");
    // Market buy, reverse (one order for twice the size), market sell back to flat.
    press(&t, "alt-b", true, armed()).unwrap();
    step(&t);
    assert_eq!(
        t.cache.read().position(&account(), es()).map(|p| p.qty),
        Some(2)
    );
    press(&t, "alt-r", true, armed()).unwrap();
    step(&t);
    assert_eq!(
        t.cache.read().position(&account(), es()).map(|p| p.qty),
        Some(-2),
        "reversed"
    );
    press(&t, "alt-b", true, armed()).unwrap();
    step(&t);
    assert!(t.cache.read().position(&account(), es()).is_none(), "flat");
    press(&t, "alt-s", true, armed()).unwrap();
    step(&t);
    assert_eq!(
        t.cache.read().position(&account(), es()).map(|p| p.qty),
        Some(-2)
    );
    // Flatten from its key closes it.
    press_exit(&t, "shift-f");
    step(&t);
    assert!(
        t.cache.read().position(&account(), es()).is_none(),
        "flattened"
    );
}

/// With no position: Reverse is refused before anything is sent; Flatten is still sent (the
/// cache may lag the core, so the UI never skips it) and does nothing.
#[test]
fn reverse_and_flatten_with_no_position() {
    let t = start(None);
    assert_eq!(
        press(&t, "alt-r", true, armed()),
        Err(Refusal("no position to reverse"))
    );
    let id = press_exit(&t, "shift-f");
    market(&t, 100);
    let s = t.cache.read();
    assert!(
        matches!(s.result(id), Some(Ok(ids)) if ids.is_empty()),
        "{:?}",
        s.result(id)
    );
    assert_eq!(s.open_orders(&account(), es()).count(), 0);
    assert!(s.position(&account(), es()).is_none());
}

/// A guardrail lockout: every order key reaches the core and is denied with the guardrail's
/// reason (the core decides, as for clicks); Flatten and Cancel all keys still act.
#[test]
fn order_keys_are_denied_during_a_lockout_and_exits_still_work() {
    let t = start(Some(Guardrails {
        max_trades_per_day: Some(1),
        ..Default::default()
    }));
    // One round trip: locked for the rest of the trade date.
    press(&t, "alt-b", true, armed()).unwrap();
    market(&t, 100);
    press(&t, "alt-s", true, armed()).unwrap();
    market(&t, 200);
    assert!(
        t.cache.read().guard(&account()).is_some(),
        "locked after one trade"
    );
    for k in ORDER_KEYS.into_iter().filter(|k| *k != "alt-r") {
        let id = press(&t, k, true, armed()).unwrap();
        let s = t.cache.read();
        let Some(Err(r)) = s.result(id) else {
            panic!("{k}: not denied: {:?}", s.result(id))
        };
        assert_eq!(r.code, RejectCode::Guardrail, "{k}");
    }
    assert_eq!(t.cache.read().open_orders(&account(), es()).count(), 0);
    for k in ["shift-a", "shift-f"] {
        let id = press_exit(&t, k);
        assert!(
            matches!(t.cache.read().result(id), Some(Ok(_))),
            "{k} works while locked"
        );
    }
}
