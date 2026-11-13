//! Discipline guardrails (roadmap 8b) through the real core and the paper broker: denials,
//! lockouts and their events, exits never blocked, tighten-now / loosen-tomorrow, and a
//! lockout that survives a restart. The rules themselves are unit-tested in
//! `src/guardrails/tests.rs`.

use std::time::Instant;

use lines_core::{ContractKey, Month, Price, Qty, Side as BookSide, UtcMillis};
use lines_feed::catalog;
use lines_trading::guardrails::{GuardState, Guardrails, LosingStreak};
use lines_trading::persist;
use lines_trading::service::trading_file;
use lines_trading::{
    AccountConfig, AccountId, Command, Event, FillModel, MarketInput, Money, OrderId, OrderKind,
    OrderSpec, PaperBroker, PaperConfig, Reject, RejectCode, Side, Source, TradingCore,
};

const LAT: i64 = 20;
const MIN: i64 = 60_000;

fn es() -> ContractKey {
    ContractKey::monthly(catalog::ES, 2026, Month::new(12).unwrap())
}

fn px(p: f64) -> Price {
    Price((p * 100.0).round() as i64)
}

fn acct() -> AccountId {
    AccountId::new("paper-1")
}

fn paper() -> PaperConfig {
    let mut cfg = PaperConfig {
        latency_ms: LAT,
        fill_model: FillModel::TradeThrough,
        ..Default::default()
    };
    cfg.fees.insert(catalog::ES.0, Money::cents(200));
    cfg
}

struct Rig {
    core: TradingCore,
    t: i64,
    events: Vec<Event>,
}

impl Rig {
    fn new(g: Guardrails) -> Rig {
        let cfg = AccountConfig::paper("paper-1", 50_000);
        let mut b = PaperBroker::new(paper());
        b.open_account(cfg.id.clone(), Money::whole(cfg.starting_balance));
        let mut core = TradingCore::new(Box::new(b));
        core.add_account(cfg, None);
        let mut r = Rig {
            core,
            // Tuesday 2026-11-10 15:00 UTC: CME open, trade date 2026-11-10.
            t: UtcMillis::parse_iso("2026-11-10T15:00:00Z").unwrap().0,
            events: Vec::new(),
        };
        r.cmd(Command::SetGuardrails {
            account: acct(),
            guardrails: g,
        })
        .unwrap();
        r.book();
        r
    }

    fn now(&self) -> UtcMillis {
        UtcMillis(self.t)
    }

    fn collect(&mut self) {
        self.events.extend(self.core.drain());
    }

    fn cmd(&mut self, c: Command) -> Result<Vec<OrderId>, Reject> {
        let id = self
            .core
            .submit(self.now(), Source::Test, Instant::now(), c);
        self.collect();
        self.events
            .iter()
            .find_map(|e| match e {
                Event::Command { id: i, result, .. } if *i == id => Some(result.clone()),
                _ => None,
            })
            .expect("every command has a result")
    }

    fn order(&mut self, side: Side, qty: i64, kind: OrderKind) -> Result<Vec<OrderId>, Reject> {
        self.cmd(Command::PlaceOrder(OrderSpec {
            account: acct(),
            contract: es(),
            side,
            qty: Qty(qty),
            kind,
        }))
    }

    /// A market order, filled after the latency.
    fn market_fill(&mut self, side: Side, qty: i64) {
        self.order(side, qty, OrderKind::Market).expect("accepted");
        self.wait(LAT);
    }

    fn wait(&mut self, ms: i64) {
        self.t += ms;
        self.core.tick(self.now());
        self.collect();
    }

    /// A deep book around 6000: buys fill at 6000.25, sells at 5999.75.
    fn book(&mut self) {
        self.book_at(5999.75, 6000.25);
    }

    fn book_at(&mut self, bid: f64, ask: f64) {
        self.t += 1;
        let v = [
            MarketInput::BookClear,
            MarketInput::Book {
                side: BookSide::Bid,
                price: px(bid),
                qty: Qty(100),
            },
            MarketInput::Book {
                side: BookSide::Ask,
                price: px(ask),
                qty: Qty(100),
            },
            MarketInput::Last(px(bid)),
        ];
        self.core.market(self.now(), es(), &v);
        self.collect();
    }

    /// A losing round trip of one lot (buy the ask, sell the bid).
    fn losing_trade(&mut self) {
        self.market_fill(Side::Buy, 1);
        self.market_fill(Side::Sell, 1);
    }

    /// Guardrail events so far: (state, reason, until).
    fn guard_events(&self) -> Vec<(GuardState, Option<String>, Option<UtcMillis>)> {
        self.events
            .iter()
            .filter_map(|e| match e {
                Event::Guardrail {
                    state,
                    reason,
                    until,
                    ..
                } => Some((*state, reason.clone(), *until)),
                _ => None,
            })
            .collect()
    }

    fn denied(&mut self, side: Side, qty: i64) -> String {
        let r = self
            .order(side, qty, OrderKind::Limit { price: px(5990.00) })
            .expect_err("denied");
        assert_eq!(r.code, RejectCode::Guardrail, "{r:?}");
        r.message
    }
}

#[test]
fn a_losing_streak_cools_down_then_ends_on_time() {
    let mut r = Rig::new(Guardrails {
        losing_streak: Some(LosingStreak {
            losses: 2,
            cooldown_min: 5,
        }),
        ..Default::default()
    });
    r.losing_trade();
    assert!(r.guard_events().is_empty());
    r.losing_trade();
    let start = r.now();
    let ev = r.guard_events();
    assert_eq!(ev.len(), 1, "{ev:?}");
    assert_eq!(ev[0].0, GuardState::Cooldown);
    assert_eq!(ev[0].1.as_deref(), Some("2 losses in a row"));
    let until = ev[0].2.unwrap();
    assert!(until.0 - start.0 <= 5 * MIN && until.0 - start.0 > 5 * MIN - 1000);
    assert_eq!(
        r.denied(Side::Buy, 1),
        "Cooldown: 2 losses in a row, 5 min left"
    );
    // The core wakes for the cooldown's end and reports it.
    assert_eq!(r.core.next_wake(), Some(until));
    r.wait(until.0 - r.t);
    assert_eq!(r.guard_events().last().unwrap().0, GuardState::Active);
    r.order(Side::Buy, 1, OrderKind::Limit { price: px(5990.00) })
        .expect("allowed again");
}

#[test]
fn a_lockout_never_blocks_exits() {
    let mut r = Rig::new(Guardrails {
        max_daily_loss: Some(200),
        ..Default::default()
    });
    r.market_fill(Side::Buy, 3);
    // A resting exit and a resting add, before the loss.
    let exit = r
        .order(Side::Sell, 1, OrderKind::Limit { price: px(6010.00) })
        .unwrap()[0];
    let add = r
        .order(Side::Buy, 1, OrderKind::Limit { price: px(5980.00) })
        .unwrap()[0];
    r.wait(LAT);
    // Down 2 points: 3 × 2.25 × $50 + fees > $200.
    r.book_at(5998.00, 5998.25);
    let ev = r.guard_events();
    assert_eq!(ev.last().unwrap().0, GuardState::Locked, "{ev:?}");
    assert_eq!(
        ev.last().unwrap().1.as_deref(),
        Some("daily loss limit of 200 reached")
    );
    // The lock lasts the trade date: 2026-11-10's ETH session closes 22:00 UTC (16:00 CT).
    assert_eq!(
        ev.last().unwrap().2,
        Some(UtcMillis::parse_iso("2026-11-10T22:00:00Z").unwrap())
    );
    // Entries: denied, adding or reversing alike.
    let m = r.denied(Side::Buy, 1);
    assert_eq!(
        m,
        "Locked: daily loss limit of 200 reached, until the next trade date"
    );
    r.denied(Side::Sell, 5);
    // A quantity increase on the resting add is an entry too; on the exit it isn't
    // (2 left to close beyond the working exit of 1).
    let e = r
        .cmd(Command::ModifyOrder {
            account: acct(),
            order: add,
            qty: Some(Qty(2)),
            price: None,
            move_bracket: false,
        })
        .unwrap_err();
    assert_eq!(e.code, RejectCode::Guardrail);
    // Exits: a reducing order (2 left to close), a modify of the exit, cancel, cancel all,
    // flatten.
    r.order(Side::Sell, 2, OrderKind::Limit { price: px(6012.00) })
        .expect("reducing");
    r.cmd(Command::ModifyOrder {
        account: acct(),
        order: exit,
        qty: None,
        price: Some(px(6011.00)),
        move_bracket: false,
    })
    .expect("a price change");
    r.wait(LAT);
    r.cmd(Command::CancelOrder {
        account: acct(),
        order: add,
    })
    .expect("cancel");
    r.cmd(Command::CancelAll {
        account: acct(),
        contract: None,
    })
    .expect("cancel all");
    r.wait(LAT);
    r.cmd(Command::Flatten {
        account: acct(),
        contract: None,
    })
    .expect("flatten");
    r.wait(LAT);
    assert_eq!(r.core.position(&acct(), es()), 0);
    assert_eq!(r.core.guard_state(&acct()), GuardState::Locked);
}

#[test]
fn tighten_now_loosen_with_the_next_trade_date() {
    let mut r = Rig::new(Guardrails {
        max_position: Some(5),
        ..Default::default()
    });
    r.order(Side::Buy, 3, OrderKind::Limit { price: px(5990.00) })
        .expect("3 of 5");
    // Tighter: at once.
    r.cmd(Command::SetGuardrails {
        account: acct(),
        guardrails: Guardrails {
            max_position: Some(3),
            ..Default::default()
        },
    })
    .unwrap();
    assert_eq!(
        r.denied(Side::Buy, 1),
        "Max position: would be 4, your limit is 3"
    );
    // Looser (off): not today.
    r.cmd(Command::SetGuardrails {
        account: acct(),
        guardrails: Guardrails::default(),
    })
    .unwrap();
    r.denied(Side::Buy, 1);
    // The next trade date: the looser setting is in force.
    r.t = UtcMillis::parse_iso("2026-11-11T15:00:00Z").unwrap().0;
    r.book();
    r.order(Side::Buy, 10, OrderKind::Limit { price: px(5990.00) })
        .expect("no limit any more");
}

#[test]
fn max_trades_counts_round_trips_through_partial_fills() {
    let mut r = Rig::new(Guardrails {
        max_trades_per_day: Some(2),
        ..Default::default()
    });
    // Trade 1: in with 2 orders, out with 2.
    r.market_fill(Side::Buy, 1);
    r.market_fill(Side::Buy, 2);
    r.market_fill(Side::Sell, 2);
    r.market_fill(Side::Sell, 1);
    assert!(r.guard_events().is_empty());
    // Trade 2 opens; a reversal now would start a third: denied (selling 1 still closes).
    r.market_fill(Side::Buy, 1);
    let m = r.denied(Side::Sell, 2);
    assert_eq!(m, "Max trades: 1 of 2 today");
    r.market_fill(Side::Sell, 1);
    let ev = r.guard_events();
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].0, GuardState::Locked);
    assert_eq!(ev[0].1.as_deref(), Some("max 2 trades reached"));
    r.denied(Side::Buy, 1);
}

/// A paper reset clears the P&L shown, but the daily-loss guardrail still counts the day's loss
/// from before it: losing up to the limit, resetting, and losing it again isn't possible.
#[test]
fn a_reset_carries_the_days_loss_into_the_daily_loss_guardrail() {
    let mut r = Rig::new(Guardrails {
        max_daily_loss: Some(200),
        ..Default::default()
    });
    // Down about $125 (1 contract, 2.25 points + fees), flat, not locked.
    r.market_fill(Side::Buy, 1);
    r.book_at(5997.50, 5997.75);
    r.market_fill(Side::Sell, 1);
    assert_eq!(r.core.guard_state(&acct()), GuardState::Active);
    r.cmd(Command::ResetPaper {
        account: acct(),
        clear_journal: false,
    })
    .unwrap();
    assert!(
        r.events
            .iter()
            .any(|e| matches!(e, Event::AccountReset { .. }))
    );
    // Another $125 down the same trade date: over $200 with what came before the reset.
    r.book();
    r.market_fill(Side::Buy, 1);
    r.book_at(5997.50, 5997.75);
    let ev = r.guard_events();
    assert_eq!(ev.last().map(|e| e.0), Some(GuardState::Locked), "{ev:?}");
}

/// Tightening the daily loss after a reset counts the loss from before it too.
#[test]
fn a_tighter_daily_loss_after_a_reset_counts_the_day_before_it() {
    let mut r = Rig::new(Guardrails::default());
    r.market_fill(Side::Buy, 1);
    r.book_at(5997.50, 5997.75);
    r.market_fill(Side::Sell, 1);
    r.cmd(Command::ResetPaper {
        account: acct(),
        clear_journal: false,
    })
    .unwrap();
    r.cmd(Command::SetGuardrails {
        account: acct(),
        guardrails: Guardrails {
            max_daily_loss: Some(100),
            ..Default::default()
        },
    })
    .unwrap();
    assert_eq!(r.core.guard_state(&acct()), GuardState::Locked);
}

/// A paper reset starts the account over but is no way out of a lock the trader set.
#[test]
fn a_reset_keeps_a_lockout() {
    let mut r = Rig::new(Guardrails {
        max_trades_per_day: Some(1),
        ..Default::default()
    });
    r.losing_trade();
    assert_eq!(r.core.guard_state(&acct()), GuardState::Locked);
    r.cmd(Command::ResetPaper {
        account: acct(),
        clear_journal: false,
    })
    .unwrap();
    assert!(
        r.events
            .iter()
            .any(|e| matches!(e, Event::AccountReset { .. })),
        "the flat account reset"
    );
    assert_eq!(r.core.guard_state(&acct()), GuardState::Locked);
    assert!(
        r.denied(Side::Buy, 1)
            .starts_with("Locked: max 1 trade reached")
    );
}

#[test]
fn a_lockout_survives_a_restart_and_settings_cannot_clear_it() {
    let dir = lines_core::scratch::Scratch::new("guardrails");
    let path = dir.join("paper.json");
    let mut r = Rig::new(Guardrails {
        max_trades_per_day: Some(1),
        ..Default::default()
    });
    r.losing_trade();
    assert_eq!(r.core.guard_state(&acct()), GuardState::Locked);
    let file = trading_file(&r.core, &paper()).unwrap();
    persist::save(&path, &file).unwrap();
    let t = r.t;
    drop(r);

    // The app starts again, with the guardrail switched off in settings.
    let file = persist::load(&path).unwrap().unwrap();
    let broker = PaperBroker::from_state(file.paper.clone(), file.broker.clone());
    let mut core = TradingCore::new(Box::new(broker));
    for a in file.accounts {
        let id = a.config.id.clone();
        core.add_account(a.config, Some(a.tracker));
        core.restore_guardrails(&id, a.guardrails);
    }
    let mut r = Rig {
        core,
        t: t + MIN,
        events: Vec::new(),
    };
    r.book();
    r.cmd(Command::SetGuardrails {
        account: acct(),
        guardrails: Guardrails::default(),
    })
    .unwrap();
    assert_eq!(r.core.guard_state(&acct()), GuardState::Locked);
    assert!(
        r.denied(Side::Buy, 1)
            .starts_with("Locked: max 1 trade reached")
    );
    // The next trade date ends it, and the switched-off setting applies then.
    r.t = UtcMillis::parse_iso("2026-11-11T15:00:00Z").unwrap().0;
    r.book();
    assert_eq!(r.core.guard_state(&acct()), GuardState::Active);
    assert_eq!(r.guard_events().last().unwrap().0, GuardState::Active);
    r.losing_trade();
    r.losing_trade();
    assert_eq!(r.core.guard_state(&acct()), GuardState::Active);
}

#[test]
fn no_entries_just_before_the_close() {
    let mut r = Rig::new(Guardrails {
        no_entries_before_close_min: Some(15),
        ..Default::default()
    });
    r.market_fill(Side::Buy, 1);
    // 21:50 UTC: ten minutes before the 22:00 close.
    r.t = UtcMillis::parse_iso("2026-11-10T21:50:00Z").unwrap().0;
    r.book();
    assert_eq!(
        r.denied(Side::Buy, 1),
        "No new entries in the last 15 min before the close"
    );
    r.order(Side::Sell, 1, OrderKind::Limit { price: px(6010.00) })
        .expect("an exit");
}

/// A broker venue's account (it joins after connecting, `add_accounts`): the trader's
/// guardrails from settings apply when it joins, its lockout is saved by the venue's service
/// at once, and after a restart, with the setting switched off, it is still locked.
#[test]
fn a_venue_lockout_survives_a_restart() {
    use lines_trading::service::{Clock, TradingService};
    use std::collections::BTreeMap;
    let dir = lines_core::scratch::Scratch::new("venue-guards");
    let path = dir.join("venue.guardrails.json");
    let cfg = AccountConfig::paper("paper-1", 50_000);
    let t0 = UtcMillis::parse_iso("2026-11-10T15:00:00Z").unwrap().0;
    let book = |bid: f64, ask: f64| {
        vec![
            MarketInput::BookClear,
            MarketInput::Book {
                side: BookSide::Bid,
                price: px(bid),
                qty: Qty(100),
            },
            MarketInput::Book {
                side: BookSide::Ask,
                price: px(ask),
                qty: Qty(100),
            },
            MarketInput::Last(px(bid)),
        ]
    };
    let market = |side| {
        Command::PlaceOrder(OrderSpec {
            account: acct(),
            contract: es(),
            side,
            qty: Qty(1),
            kind: OrderKind::Market,
        })
    };
    // Start a venue: its core, the guardrails saved and wanted, the account joining.
    let start = |defaults: BTreeMap<String, Guardrails>,
                 saved: BTreeMap<String, persist::VenueGuard>| {
        let mut b = PaperBroker::new(paper());
        b.open_account(cfg.id.clone(), Money::whole(cfg.starting_balance));
        let mut core = TradingCore::new(Box::new(b));
        core.set_join_guardrails(defaults, saved);
        let s = TradingService::start_venue(core, Clock::Manual, Vec::new(), Some(path.clone()))
            .unwrap();
        let h = s.handle();
        let events = h.subscribe();
        h.advance_to(UtcMillis(t0));
        h.add_accounts(vec![cfg.clone()]);
        h.market(es(), book(5999.75, 6000.25));
        h.sync();
        (s, h, events)
    };

    // Max 1 trade per day, by account name, from settings.
    let defaults: BTreeMap<String, Guardrails> = [(
        cfg.name.clone(),
        Guardrails {
            max_trades_per_day: Some(1),
            ..Default::default()
        },
    )]
    .into();
    let (s, h, _events) = start(defaults, BTreeMap::new());
    for (side, at) in [(Side::Buy, LAT), (Side::Sell, 2 * LAT)] {
        h.submit(Source::Test, market(side));
        h.sync();
        h.advance_to(UtcMillis(t0 + at));
        h.market(es(), book(5999.75, 6000.25));
        h.sync();
    }
    // The lockout was written at once (the service is still running).
    let saved = persist::load_guards(&path).unwrap().expect("written");
    assert_eq!(saved.version, persist::GUARDS_VERSION);
    let entry = &saved.accounts["paper-1"];
    assert_eq!(entry.guardrails.state(), GuardState::Locked, "{saved:?}");
    assert!(
        entry.day.is_some(),
        "the trade date and its opening balance: {saved:?}"
    );
    assert_eq!(entry.day_open_balance, Money::whole(50_000));
    drop(s);

    // Restart with a looser setting (5 trades): still locked (looser waits for the next
    // trade date), and reported.
    let looser: BTreeMap<String, Guardrails> = [(
        cfg.name.clone(),
        Guardrails {
            max_trades_per_day: Some(5),
            ..Default::default()
        },
    )]
    .into();
    let saved = persist::load_guards(&path).unwrap().unwrap().accounts;
    let (_s, h, events) = start(looser, saved);
    let id = h.submit(Source::Test, market(Side::Buy));
    h.sync();
    let all: Vec<Event> = events.try_iter().collect();
    assert!(
        all.iter().any(|e| matches!(
            e,
            Event::Guardrail {
                state: GuardState::Locked,
                ..
            }
        )),
        "the restored lockout is reported: {all:?}"
    );
    let r = all
        .iter()
        .find_map(|e| match e {
            Event::Command { id: i, result, .. } if *i == id => Some(result.clone()),
            _ => None,
        })
        .expect("answered");
    let e = r.expect_err("still locked");
    assert_eq!(e.code, RejectCode::Guardrail, "{e:?}");
}

/// A venue core with `saved` guardrail progress and `defaults`, its account joining with
/// the broker at `balance` (`None`: the broker doesn't list the account yet).
fn venue_rig(
    balance: Option<i64>,
    defaults: std::collections::BTreeMap<String, Guardrails>,
    saved: std::collections::BTreeMap<String, persist::VenueGuard>,
) -> Rig {
    let cfg = AccountConfig::paper("paper-1", 50_000);
    let mut b = PaperBroker::new(paper());
    if let Some(bal) = balance {
        b.open_account(cfg.id.clone(), Money::whole(bal));
    }
    let mut core = TradingCore::new(Box::new(b));
    core.set_join_guardrails(defaults, saved);
    let t = UtcMillis::parse_iso("2026-11-10T15:00:00Z").unwrap().0;
    core.add_accounts(UtcMillis(t), vec![cfg]);
    let mut r = Rig {
        core,
        t,
        events: Vec::new(),
    };
    r.collect();
    r.book();
    r
}

fn loss_limit(n: i64) -> std::collections::BTreeMap<String, Guardrails> {
    [(
        "paper-1".to_owned(),
        Guardrails {
            max_daily_loss: Some(n),
            ..Default::default()
        },
    )]
    .into()
}

fn saved_today(open: i64) -> std::collections::BTreeMap<String, persist::VenueGuard> {
    let mut g = lines_trading::GuardrailTracker::default();
    g.request(Guardrails {
        max_daily_loss: Some(500),
        ..Default::default()
    });
    [(
        "paper-1".to_owned(),
        persist::VenueGuard {
            guardrails: g,
            day: lines_core::CivilDate::new(2026, 11, 10),
            day_open_balance: Money::whole(open),
        },
    )]
    .into()
}

/// A same-day restart keeps today's loss: down $300 before, a $500 limit locks after $200
/// more, not $500 more.
#[test]
fn a_venue_restart_keeps_todays_loss() {
    let mut r = venue_rig(Some(49_700), loss_limit(500), saved_today(50_000));
    assert_eq!(r.core.guard_state(&acct()), GuardState::Active);
    // About $275 more (5.5 points on one lot, fees included).
    r.market_fill(Side::Buy, 1);
    r.book_at(5994.75, 5995.25);
    r.market_fill(Side::Sell, 1);
    assert_eq!(r.core.guard_state(&acct()), GuardState::Locked);
}

/// Before the broker lists the account its balance is unknown: the saved opening balance
/// waits, so the day's loss never reads as the whole balance (no false lockout).
#[test]
fn a_venue_account_not_listed_yet_is_not_locked_by_its_saved_day() {
    let r = venue_rig(None, loss_limit(500), saved_today(50_000));
    assert_eq!(r.core.guard_state(&acct()), GuardState::Active);
    assert!(
        !r.events.iter().any(|e| matches!(
            e,
            Event::Guardrail {
                state: GuardState::Locked,
                ..
            }
        )),
        "{:?}",
        r.events
    );
}
