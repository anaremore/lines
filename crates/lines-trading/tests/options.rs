//! Paper options in the trading core (decision 0028, phase 3): fills on the option's own quote,
//! the multiplier, the far-from-mid guard, expiry, guardrails and prop rules, stale and
//! one-sided quotes. Everything is deterministic: time and quotes are scripted.

use std::time::Instant;

use lines_core::{CivilDate, ContractKey, Price, Qty, Right, UtcMillis};
use lines_feed::catalog;
use lines_trading::guardrails::Guardrails;
use lines_trading::risk::{self, OptionGuard};
use lines_trading::rules::presets;
use lines_trading::{
    AccountConfig, AccountId, BracketSpec, Command, Event, FillModel, MarketInput, Money, OrderId,
    OrderKind, OrderSpec, OrderState, PaperBroker, PaperConfig, RejectCode, Side, Source,
    TradingCore,
};

const LAT: i64 = 20;
/// Monday 2026-11-09, 10:00 ET: the regular session.
const T0: &str = "2026-11-09T15:00:00Z";

fn series(strike: i64, right: Right) -> ContractKey {
    ContractKey::option(
        catalog::SPY,
        CivilDate::new(2026, 11, 9).unwrap(),
        Price(strike * 100),
        right,
    )
}

fn call() -> ContractKey {
    series(684, Right::Call)
}

fn spy() -> ContractKey {
    ContractKey::equity(catalog::SPY)
}

fn acct() -> AccountId {
    AccountId::new("paper-1")
}

struct Rig {
    core: TradingCore,
    t: i64,
    events: Vec<Event>,
}

impl Rig {
    fn with(model: FillModel, account: AccountConfig) -> Rig {
        let mut cfg = PaperConfig {
            latency_ms: LAT,
            fill_model: model,
            ..Default::default()
        };
        cfg.fees.insert(catalog::SPY.0, Money::cents(65));
        let mut b = PaperBroker::new(cfg);
        b.open_account(account.id.clone(), Money::whole(account.starting_balance));
        let mut core = TradingCore::new(Box::new(b));
        core.add_account(account, None);
        Rig {
            core,
            t: UtcMillis::parse_iso(T0).unwrap().0,
            events: Vec::new(),
        }
    }

    fn new() -> Rig {
        Rig::with(
            FillModel::TradeThrough,
            AccountConfig::paper("paper-1", 100_000),
        )
    }

    fn now(&self) -> UtcMillis {
        UtcMillis(self.t)
    }

    fn collect(&mut self) {
        self.events.extend(self.core.drain());
    }

    fn cmd(&mut self, c: Command) -> Result<Vec<OrderId>, lines_trading::Reject> {
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

    fn spec(side: Side, qty: i64, kind: OrderKind) -> OrderSpec {
        OrderSpec {
            account: acct(),
            contract: call(),
            side,
            qty: Qty(qty),
            kind,
        }
    }

    fn try_limit(&mut self, side: Side, qty: i64, cents: i64) -> Result<OrderId, RejectCode> {
        self.cmd(Command::PlaceOrder(Rig::spec(side, qty, limit(cents))))
            .map(|v| v[0])
            .map_err(|r| r.code)
    }

    /// A limit, acked (it goes live after the latency).
    fn limit(&mut self, side: Side, qty: i64, cents: i64) -> OrderId {
        let id = self.try_limit(side, qty, cents).expect("accepted");
        self.wait(LAT + 1);
        id
    }

    fn wait(&mut self, ms: i64) {
        self.t += ms;
        self.core.tick(self.now());
        self.collect();
    }

    fn market(&mut self, contract: ContractKey, input: MarketInput) {
        self.t += 1;
        self.core.market(self.now(), contract, &[input]);
        self.collect();
    }

    /// The series' quote, stamped now; `None` is a missing side.
    fn quote(&mut self, bid: Option<i64>, ask: Option<i64>) {
        self.quote_on(call(), bid, ask);
    }

    fn quote_on(&mut self, contract: ContractKey, bid: Option<i64>, ask: Option<i64>) {
        let time = UtcMillis(self.t + 1);
        self.market(
            contract,
            MarketInput::OptionQuote {
                bid: bid.map(Price),
                ask: ask.map(Price),
                last: None,
                time,
            },
        );
    }

    fn underlying(&mut self, cents: i64) {
        let time = UtcMillis(self.t + 1);
        self.market(
            spy(),
            MarketInput::Trade {
                time,
                price: Price(cents),
                qty: Qty(100),
            },
        );
    }

    fn fills(&self, id: OrderId) -> Vec<(i64, i64)> {
        self.events
            .iter()
            .filter_map(|e| match e {
                Event::Fill(f) if f.order == id => Some((f.price.0, f.qty.0)),
                _ => None,
            })
            .collect()
    }

    fn position(&self, k: ContractKey) -> i64 {
        self.core.position(&acct(), k)
    }

    fn balance(&self) -> Money {
        self.core.balance(&acct()).unwrap()
    }

    fn state(&self, id: OrderId) -> OrderState {
        self.core.order(id).expect("known order").state.clone()
    }
}

fn limit(cents: i64) -> OrderKind {
    OrderKind::Limit {
        price: Price(cents),
    }
}

/// An order that crosses the quote takes it, at the quote's price, on either side.
#[test]
fn crossing_limits_fill_at_the_options_own_quote() {
    let mut r = Rig::new();
    r.underlying(68_400); // the underlying's price plays no part
    r.quote(Some(240), Some(242));
    let buy = r.limit(Side::Buy, 2, 245);
    assert_eq!(r.fills(buy), [(242, 2)], "the ask, not the limit");
    let sell = r.limit(Side::Sell, 1, 240);
    assert_eq!(r.fills(sell), [(240, 1)], "the bid");
    assert_eq!(r.position(call()), 1);
}

/// A resting limit fills only when a later quote goes through it, at its limit (trade-through);
/// a quote at the limit isn't enough, except with the touch model.
#[test]
fn resting_limits_fill_when_a_quote_goes_through_them() {
    let mut r = Rig::new();
    r.quote(Some(240), Some(242));
    let buy = r.limit(Side::Buy, 3, 238);
    assert_eq!(r.state(buy), OrderState::Working);
    r.quote(Some(236), Some(238));
    assert!(r.fills(buy).is_empty(), "the ask at the limit: no fill");
    r.quote(Some(235), Some(237));
    assert_eq!(r.fills(buy), [(238, 3)], "through it: at the limit");
    let sell = r.limit(Side::Sell, 3, 240);
    r.quote(Some(240), Some(242));
    assert!(r.fills(sell).is_empty());
    r.quote(Some(241), Some(243));
    assert_eq!(r.fills(sell), [(240, 3)]);

    let mut t = Rig::with(FillModel::Touch, AccountConfig::paper("paper-1", 100_000));
    t.quote(Some(240), Some(242));
    let buy = t.limit(Side::Buy, 1, 238);
    t.quote(Some(236), Some(238));
    assert_eq!(t.fills(buy), [(238, 1)], "touch: at the limit is enough");
}

/// Prints never fill an option order, on the option or on its underlying.
#[test]
fn prints_never_fill_an_option() {
    let mut r = Rig::new();
    r.quote(Some(240), Some(242));
    let buy = r.limit(Side::Buy, 1, 238);
    r.underlying(100); // far through anything
    let time = UtcMillis(r.t + 1);
    r.market(
        call(),
        MarketInput::Trade {
            time,
            price: Price(100),
            qty: Qty(50),
        },
    );
    assert!(r.fills(buy).is_empty());
    assert_eq!(r.state(buy), OrderState::Working);
}

/// A missing side never fills: no ask (or bid), a zero side and a crossed quote all leave the
/// side empty. With no two-sided quote there is no mid, so limits are refused, opening or closing.
#[test]
fn a_missing_side_never_fills() {
    let mut r = Rig::new();
    r.quote(Some(240), Some(242));
    let buy = r.limit(Side::Buy, 2, 241);
    let sell = r.limit(Side::Sell, 1, 241);
    // Neither crossed, so both rest. A quote loses its ask: the buy has nothing to take.
    assert!(r.fills(buy).is_empty() && r.fills(sell).is_empty());
    r.quote(Some(200), None);
    r.quote(Some(200), Some(0));
    assert!(r.fills(buy).is_empty(), "no ask, or a zero one: no fill");
    r.quote(None, Some(300));
    r.quote(Some(0), Some(300));
    assert!(r.fills(sell).is_empty(), "no bid, or a zero one: no fill");
    r.quote(Some(300), Some(100));
    assert!(
        r.fills(buy).is_empty() && r.fills(sell).is_empty(),
        "a crossed quote fills nothing"
    );
    assert_eq!(r.position(call()), 0);
    // No mid: an opening order is refused, whichever side is gone.
    assert_eq!(r.try_limit(Side::Buy, 1, 200), Err(RejectCode::Risk));
    r.quote(Some(200), None);
    assert_eq!(r.try_limit(Side::Buy, 1, 200), Err(RejectCode::Risk));
    r.quote(None, None);
    assert_eq!(r.try_limit(Side::Sell, 1, 200), Err(RejectCode::Risk));

    // A close too: a limit typed blind could fill far from the market on the next quote.
    let mut r = Rig::new();
    r.quote(Some(240), Some(242));
    let open = r.limit(Side::Buy, 2, 242);
    assert_eq!(r.fills(open), [(242, 2)]);
    r.quote(None, Some(5));
    let blind = r.cmd(Command::PlaceExit(Rig::spec(Side::Sell, 2, limit(3))));
    assert_eq!(blind.unwrap_err().code, RejectCode::Risk);
    // Flatten is a market order: with no bid it works on, never filled at a last price.
    let time = UtcMillis(r.t + 1);
    r.market(
        call(),
        MarketInput::OptionQuote {
            bid: None,
            ask: Some(Price(5)),
            last: Some(Price(4)),
            time,
        },
    );
    let flat = r
        .cmd(Command::Flatten {
            account: acct(),
            contract: Some(call()),
        })
        .unwrap();
    r.wait(LAT + 1);
    assert_eq!(r.position(call()), 2, "nothing to sell to");
    assert!(flat.iter().all(|id| r.fills(*id).is_empty()));
    let market = *flat.last().unwrap();
    assert_eq!(
        r.state(market),
        OrderState::Working,
        "working on, not cancelled"
    );
    // The bid comes back: it sells there.
    r.quote(Some(4), Some(5));
    assert_eq!(r.fills(market), [(4, 2)]);
    assert_eq!(r.position(call()), 0);
}

/// The age limit goes by expiry, on the option market's own date.
#[test]
fn the_quote_age_limit_goes_by_expiry() {
    let limit = |y, m, d, at: &str| {
        risk::expiry_quote_stale_ms(
            catalog::SPY,
            CivilDate::new(y, m, d).unwrap(),
            UtcMillis::parse_iso(at).unwrap(),
        )
    };
    let (same_day, later) = (10_000, 60_000);
    assert_eq!(risk::SAME_DAY_QUOTE_STALE_MS, same_day);
    assert_eq!(risk::LATER_QUOTE_STALE_MS, later);
    // 10:00 ET.
    assert_eq!(limit(2026, 11, 9, "2026-11-09T15:00:00Z"), same_day);
    assert_eq!(limit(2026, 11, 10, "2026-11-09T15:00:00Z"), later);
    assert_eq!(limit(2026, 11, 8, "2026-11-09T15:00:00Z"), same_day, "past");
    // 20:00 ET: tomorrow in UTC, still today in New York.
    assert_eq!(limit(2026, 11, 9, "2026-11-10T01:00:00Z"), same_day);
    assert_eq!(limit(2026, 11, 10, "2026-11-10T01:00:00Z"), later);
    // Anything else gets the shorter one.
    assert_eq!(
        risk::option_quote_stale_ms(
            ContractKey::equity(catalog::SPY),
            UtcMillis::parse_iso("2026-11-09T15:00:00Z").unwrap()
        ),
        same_day
    );
}

/// The core and the paper broker both use it: the same quote age is stale today and fine on a
/// later expiry, then stale there too.
#[test]
fn a_later_expiry_keeps_its_quote_longer() {
    let later = ContractKey::option(
        catalog::SPY,
        CivilDate::new(2026, 11, 13).unwrap(),
        call().option_key().unwrap().strike,
        Right::Call,
    );
    assert_eq!(
        risk::option_quote_stale_ms(call(), UtcMillis(Rig::new().t)),
        risk::SAME_DAY_QUOTE_STALE_MS,
        "the fixture's series expires today"
    );
    let mut r = Rig::new();
    r.quote(Some(240), Some(242));
    r.quote_on(later, Some(500), Some(502));
    let quoted = r.t;
    let buy = |contract| {
        Command::PlaceOrder(OrderSpec {
            contract,
            ..Rig::spec(Side::Buy, 1, limit(502))
        })
    };
    // 30 s on: today's has no mid, the later one fills at its 30-second-old ask.
    r.t = quoted + 30_000;
    let refused = r.cmd(Command::PlaceOrder(Rig::spec(Side::Buy, 1, limit(242))));
    assert_eq!(refused.unwrap_err().code, RejectCode::Risk);
    let id = r.cmd(buy(later)).unwrap()[0];
    r.wait(LAT + 1);
    assert_eq!(r.fills(id), [(502, 1)]);
    // Past 60 s the later one's stale too, and an order sent just in time rests at the broker.
    r.t = quoted + risk::LATER_QUOTE_STALE_MS - 5;
    let resting = r.cmd(buy(later)).unwrap()[0];
    r.wait(LAT + 1);
    assert!(r.fills(resting).is_empty());
    assert_eq!(r.state(resting), OrderState::Working);
    assert_eq!(
        r.cmd(buy(later)).unwrap_err().code,
        RejectCode::Risk,
        "past its limit"
    );
}

/// A stale quote fills nothing and gives no mid: an opening order is refused, a resting order
/// waits, and Flatten works on, until a fresh quote comes.
#[test]
fn a_stale_quote_never_fills() {
    let mut r = Rig::new();
    r.quote(Some(240), Some(242));
    let quoted = r.t;
    let open = r.limit(Side::Buy, 1, 242);
    assert_eq!(r.fills(open), [(242, 1)]);
    // Sent just in time, it arrives after the quote went stale: it would cross, but rests.
    r.t = quoted + risk::SAME_DAY_QUOTE_STALE_MS - 5;
    let resting = r
        .cmd(Command::PlaceExit(Rig::spec(Side::Sell, 1, limit(240))))
        .unwrap()[0];
    r.wait(LAT + 1);
    assert!(r.fills(resting).is_empty());
    assert_eq!(r.state(resting), OrderState::Working);
    // Past it: refused, opening or closing.
    r.t += 60_000;
    assert_eq!(r.try_limit(Side::Buy, 1, 241), Err(RejectCode::Risk));
    let closing = r.cmd(Command::PlaceExit(Rig::spec(Side::Sell, 1, limit(240))));
    assert_eq!(closing.unwrap_err().code, RejectCode::Risk);
    let flat = r
        .cmd(Command::Flatten {
            account: acct(),
            contract: Some(call()),
        })
        .unwrap();
    r.wait(LAT + 1);
    assert_eq!(r.position(call()), 1, "Flatten: no fill on a stale quote");
    let market = *flat.last().unwrap();
    assert_eq!(r.state(market), OrderState::Working);
    // A quote stamped long ago is no fresher for arriving now.
    let old = UtcMillis(r.t - 20 * 60_000);
    r.market(
        call(),
        MarketInput::OptionQuote {
            bid: Some(Price(300)),
            ask: Some(Price(302)),
            last: None,
            time: old,
        },
    );
    assert_eq!(r.position(call()), 1);
    // A late quote older than the newest is ignored.
    r.quote(Some(250), Some(252));
    let late = UtcMillis(r.t - 1_000);
    r.market(
        call(),
        MarketInput::OptionQuote {
            bid: Some(Price(100)),
            ask: Some(Price(102)),
            last: None,
            time: late,
        },
    );
    // The Flatten's market order worked on through all of that. The fresh quote is the first it
    // can fill on: it sells at the bid, first in line, and the position is flat.
    assert_eq!(r.fills(market), [(250, 1)]);
    assert!(r.fills(resting).is_empty());
    assert_eq!(r.position(call()), 0);
    r.wait(LAT + 1);
    assert_eq!(r.position(call()), 0);
}

/// An empty or crossed quote keeps the last mark: open P&L isn't erased.
#[test]
fn an_empty_quote_keeps_the_mark() {
    let mut r = Rig::new();
    r.quote(Some(240), Some(242));
    r.limit(Side::Buy, 1, 242);
    r.quote(Some(250), Some(252)); // mid 2.51: up $9
    let open = |r: &Rig| {
        r.events
            .iter()
            .rev()
            .find_map(|e| match e {
                Event::Position(p) if p.contract == call() => Some(p.unrealised),
                _ => None,
            })
            .unwrap()
    };
    assert_eq!(open(&r), Money::whole(9));
    r.quote(None, None);
    assert_eq!(open(&r), Money::whole(9));
    r.quote(Some(300), Some(100));
    assert_eq!(open(&r), Money::whole(9));
}

/// Money uses the multiplier, in exact types: 100 shares per contract, fees per contract.
#[test]
fn pnl_uses_the_multiplier() {
    let mut r = Rig::new();
    let start = r.balance();
    r.quote(Some(240), Some(242));
    r.limit(Side::Buy, 2, 242);
    // Marked at the mid: (250 + 252) / 2 = 2.51, up 0.09 on 2 contracts × 100 = $18.
    r.quote(Some(250), Some(252));
    let open = r
        .events
        .iter()
        .rev()
        .find_map(|e| match e {
            Event::Position(p) if p.contract == call() => Some(p.unrealised),
            _ => None,
        })
        .unwrap();
    assert_eq!(open, Money::whole(18));
    r.limit(Side::Sell, 2, 250);
    assert_eq!(r.position(call()), 0);
    // (2.50 − 2.42) × 2 × 100 = $16, less 4 fees of $0.65.
    assert_eq!(r.balance() - start, Money::whole(16) - Money::cents(260));
}

/// The guard: a limit far from the mid is refused on placing and on moving it; market orders,
/// stops and brackets are refused; the guard is configurable, never off.
#[test]
fn the_guard_refuses_what_is_far_or_not_a_limit() {
    let mut r = Rig::new();
    r.quote(Some(1_000), Some(1_002)); // mid 10.01: 25% is 2.50
    assert_eq!(r.try_limit(Side::Buy, 1, 1_252), Err(RejectCode::Risk));
    assert_eq!(r.try_limit(Side::Sell, 1, 750), Err(RejectCode::Risk));
    let ok = r.limit(Side::Buy, 1, 751);
    assert!(r.try_limit(Side::Sell, 1, 1_251).is_ok());
    // Moving a resting order is checked too.
    let moved = r.cmd(Command::ModifyOrder {
        account: acct(),
        order: ok,
        qty: None,
        price: Some(Price(600)),
    });
    assert_eq!(moved.unwrap_err().code, RejectCode::Risk);
    // A cheap option: the 10-tick floor, not a percentage of nothing.
    r.quote(Some(4), Some(6));
    assert!(r.try_limit(Side::Buy, 1, 15).is_ok());
    assert_eq!(r.try_limit(Side::Buy, 1, 16), Err(RejectCode::Risk));
    assert_eq!(r.try_limit(Side::Buy, 1, 0), Err(RejectCode::Invalid));
    r.quote(Some(1_000), Some(1_002));
    // Not limits.
    let market = r.cmd(Command::PlaceOrder(Rig::spec(
        Side::Buy,
        1,
        OrderKind::Market,
    )));
    assert_eq!(market.unwrap_err().code, RejectCode::Risk);
    let stop = r.cmd(Command::PlaceOrder(Rig::spec(
        Side::Buy,
        1,
        OrderKind::Stop {
            trigger: Price(1_100),
        },
    )));
    assert_eq!(stop.unwrap_err().code, RejectCode::Invalid);
    let bracket = r.cmd(Command::PlaceBracket(BracketSpec {
        entry: Rig::spec(Side::Buy, 1, limit(1_000)),
        take_profit: Some(Price(1_100)),
        stop_loss: None,
    }));
    assert_eq!(bracket.unwrap_err().code, RejectCode::Invalid);

    // Configured: tighter, and market orders allowed.
    let mut a = AccountConfig::paper("paper-1", 100_000);
    a.limits.option_guard = Some(OptionGuard {
        max_from_mid_pct: 5,
        min_band_ticks: 1,
        allow_market: true,
        ..Default::default()
    });
    let mut r = Rig::with(FillModel::TradeThrough, a);
    r.quote(Some(1_000), Some(1_002));
    assert_eq!(r.try_limit(Side::Buy, 1, 1_100), Err(RejectCode::Risk));
    assert!(r.try_limit(Side::Buy, 1, 1_050).is_ok());
    let m = r
        .cmd(Command::PlaceOrder(Rig::spec(
            Side::Buy,
            1,
            OrderKind::Market,
        )))
        .unwrap()[0];
    r.wait(LAT + 1);
    assert_eq!(r.fills(m), [(1_002, 1)], "market: the ask");
}

/// Options trade in the regular session only (the equity's extended hours don't count).
#[test]
fn options_trade_in_the_regular_session_only() {
    let mut r = Rig::new();
    // Monday 17:00 ET: the equity's extended hours, the option's market is closed (a later
    // expiry: today's has expired by then).
    let later = ContractKey::option(
        catalog::SPY,
        CivilDate::new(2026, 11, 13).unwrap(),
        Price(68_400),
        Right::Call,
    );
    let eve = UtcMillis::parse_iso("2026-11-09T22:00:00Z").unwrap();
    r.t = eve.0;
    r.market(
        later,
        MarketInput::OptionQuote {
            bid: Some(Price(240)),
            ask: Some(Price(242)),
            last: None,
            time: eve,
        },
    );
    let id = r
        .cmd(Command::PlaceOrder(OrderSpec {
            contract: later,
            ..Rig::spec(Side::Buy, 1, limit(242))
        }))
        .unwrap()[0];
    r.wait(LAT + 1);
    assert!(matches!(r.state(id), OrderState::Rejected { .. }));
    assert!(r.fills(id).is_empty());
    assert_eq!(r.core.position(&acct(), later), 0);
}

/// Moving a resting order across the quote after the close fills nothing.
#[test]
fn a_modify_never_fills_outside_the_session() {
    let mut r = Rig::new();
    let later = ContractKey::option(
        catalog::SPY,
        CivilDate::new(2026, 11, 13).unwrap(),
        Price(68_400),
        Right::Call,
    );
    let time = UtcMillis(r.t + 1);
    r.market(
        later,
        MarketInput::OptionQuote {
            bid: Some(Price(240)),
            ask: Some(Price(242)),
            last: None,
            time,
        },
    );
    let id = r
        .cmd(Command::PlaceOrder(OrderSpec {
            contract: later,
            ..Rig::spec(Side::Buy, 1, limit(230))
        }))
        .unwrap()[0];
    r.wait(LAT + 1);
    // 16:05 ET, the quote stamped ten seconds before: fresh, so only the session can stop it.
    r.t = UtcMillis::parse_iso("2026-11-09T21:04:50Z").unwrap().0;
    let time = r.now();
    r.market(
        later,
        MarketInput::OptionQuote {
            bid: Some(Price(240)),
            ask: Some(Price(242)),
            last: None,
            time,
        },
    );
    r.t = UtcMillis::parse_iso("2026-11-09T21:05:00Z").unwrap().0;
    r.cmd(Command::ModifyOrder {
        account: acct(),
        order: id,
        qty: None,
        price: Some(Price(245)),
    })
    .unwrap();
    r.wait(LAT + 1);
    assert!(r.fills(id).is_empty());
    assert_eq!(r.core.position(&acct(), later), 0);
}

fn at_close(r: &mut Rig) {
    r.t = UtcMillis::parse_iso("2026-11-09T21:00:00Z").unwrap().0;
    r.wait(0);
}

/// At the close, in the money settles at intrinsic against the underlying's close; out of the
/// money expires worthless; a short pays; other orders on the series are cancelled; no fee.
#[test]
fn expiry_settles_at_intrinsic_or_worthless() {
    // Long 2 calls at 2.42; the underlying closes at 686.50: intrinsic 2.50.
    let mut r = Rig::new();
    let start = r.balance();
    r.underlying(68_400);
    r.quote(Some(240), Some(242));
    r.limit(Side::Buy, 2, 242);
    let resting = r.limit(Side::Sell, 1, 300);
    r.underlying(68_650);
    at_close(&mut r);
    assert_eq!(r.position(call()), 0);
    let fee = Money::cents(130); // the entry's: settling is free
    assert_eq!(r.balance() - start, Money::whole(16) - fee);
    r.wait(LAT + 1);
    assert!(matches!(r.state(resting), OrderState::Cancelled));
    let settled: Vec<_> = r
        .events
        .iter()
        .filter_map(|e| match e {
            Event::Fill(f) if f.side == Side::Sell && f.contract == call() => {
                Some((f.price.0, f.qty.0))
            }
            _ => None,
        })
        .collect();
    assert_eq!(settled, [(250, 2)]);

    // Out of the money: worthless, the whole premium lost.
    let mut r = Rig::new();
    let start = r.balance();
    r.underlying(68_400);
    r.quote(Some(240), Some(242));
    r.limit(Side::Buy, 1, 242);
    r.underlying(68_000);
    at_close(&mut r);
    assert_eq!(r.position(call()), 0);
    assert_eq!(r.balance() - start, -Money::whole(242) - Money::cents(65));

    // Short and in the money: pays the intrinsic. A put: strike − close.
    let mut r = Rig::new();
    let put = series(684, Right::Put);
    let start = r.balance();
    r.underlying(68_400);
    let time = UtcMillis(r.t + 1);
    r.market(
        put,
        MarketInput::OptionQuote {
            bid: Some(Price(300)),
            ask: Some(Price(302)),
            last: None,
            time,
        },
    );
    r.cmd(Command::PlaceOrder(OrderSpec {
        contract: put,
        ..Rig::spec(Side::Sell, 1, limit(300))
    }))
    .unwrap();
    r.wait(LAT + 1);
    assert_eq!(r.position(put), -1);
    r.underlying(68_000); // 4.00 in the money
    at_close(&mut r);
    assert_eq!(r.position(put), 0);
    assert_eq!(r.balance() - start, -Money::whole(100) - Money::cents(65));
}

/// The close is the last price before it: a print after it doesn't count, whichever arrives.
/// With no price from that day the position waits, then settles on the next price. Expired
/// series take no new orders.
#[test]
fn expiry_uses_the_close_and_waits_for_a_price() {
    let mut r = Rig::new();
    r.underlying(68_400);
    r.quote(Some(240), Some(242));
    r.limit(Side::Buy, 1, 242);
    r.underlying(68_600);
    let start = r.balance();
    // The first print past the close: the service ticks before it reaches the marks.
    r.t = UtcMillis::parse_iso("2026-11-09T21:00:05Z").unwrap().0;
    r.core.tick(r.now());
    r.collect();
    r.underlying(70_000);
    assert_eq!(r.position(call()), 0);
    // Intrinsic 2.00 (686 − 684, not 700) against the 2.42 paid, on 100 shares.
    assert_eq!(r.balance() - start, -Money::whole(42));
    assert_eq!(r.try_limit(Side::Buy, 1, 242), Err(RejectCode::Invalid));

    // A price older than a day isn't that day's close: it waits for the next one.
    let mut r = Rig::new();
    r.quote(Some(240), Some(242));
    r.limit(Side::Buy, 1, 242);
    let then = r.t;
    r.t -= 2 * 86_400_000;
    r.underlying(60_000);
    r.t = then;
    at_close(&mut r);
    assert_eq!(r.position(call()), 1);

    // No underlying price at all: nothing to settle against, so it waits.
    let mut r = Rig::new();
    r.quote(Some(240), Some(242));
    r.limit(Side::Buy, 1, 242);
    at_close(&mut r);
    assert_eq!(r.position(call()), 1, "never invented");
    let start = r.balance();
    r.underlying(68_500);
    r.wait(0);
    assert_eq!(r.position(call()), 0, "the next price, approximately");
    // Intrinsic 1.00 against the 2.42 paid.
    assert_eq!(r.balance() - start, -Money::whole(142));
}

/// Guardrails apply to options; prop rules don't (a prop-rule account can't open one).
#[test]
fn guardrails_apply_and_prop_rules_do_not() {
    let mut r = Rig::new();
    r.cmd(Command::SetGuardrails {
        account: acct(),
        guardrails: Guardrails {
            max_trades_per_day: Some(1),
            ..Default::default()
        },
    })
    .unwrap();
    r.quote(Some(240), Some(242));
    r.limit(Side::Buy, 1, 242);
    r.limit(Side::Sell, 1, 240); // one round trip: the limit
    assert_eq!(r.position(call()), 0);
    assert_eq!(r.try_limit(Side::Buy, 1, 242), Err(RejectCode::Guardrail));

    // A max daily loss counts the multiplier: 0.50 × 100 × 1 = $50.
    let mut r = Rig::new();
    r.cmd(Command::SetGuardrails {
        account: acct(),
        guardrails: Guardrails {
            max_daily_loss: Some(40),
            ..Default::default()
        },
    })
    .unwrap();
    r.quote(Some(240), Some(242));
    r.limit(Side::Buy, 1, 242);
    r.quote(Some(190), Some(192));
    r.wait(0);
    assert_eq!(r.try_limit(Side::Buy, 1, 192), Err(RejectCode::Guardrail));
    // A close still passes a lockout.
    let close = r.cmd(Command::PlaceExit(Rig::spec(Side::Sell, 1, limit(190))));
    assert!(close.is_ok(), "{close:?}");

    // "No entries before the close" reads the option's own close (16:00 ET), not the equity's.
    let mut r = Rig::new();
    r.cmd(Command::SetGuardrails {
        account: acct(),
        guardrails: Guardrails {
            no_entries_before_close_min: Some(10),
            ..Default::default()
        },
    })
    .unwrap();
    r.t = UtcMillis::parse_iso("2026-11-09T20:55:00Z").unwrap().0;
    r.quote(Some(240), Some(242));
    assert_eq!(r.try_limit(Side::Buy, 1, 242), Err(RejectCode::Guardrail));

    // Rules can't be set while the account holds options; no position bracket on an option.
    let mut r = Rig::new();
    r.quote(Some(240), Some(242));
    r.limit(Side::Buy, 1, 242);
    let set = r.cmd(Command::SetRules {
        account: acct(),
        rules: presets().into_iter().next(),
    });
    assert_eq!(set.unwrap_err().code, RejectCode::PropRule);

    let mut a = AccountConfig::paper("paper-1", 50_000);
    a.rules = presets().into_iter().next();
    let mut r = Rig::with(FillModel::TradeThrough, a);
    r.quote(Some(240), Some(242));
    assert_eq!(r.try_limit(Side::Buy, 1, 242), Err(RejectCode::PropRule));
}

/// A close is reduce-only: it can't be larger than the position, shrinks with it and can never
/// flip or open.
#[test]
fn a_close_can_never_flip_or_open() {
    let mut r = Rig::new();
    r.quote(Some(240), Some(242));
    let flat = r.cmd(Command::PlaceExit(Rig::spec(Side::Sell, 1, limit(250))));
    assert_eq!(flat.unwrap_err().code, RejectCode::Invalid, "nothing open");
    r.limit(Side::Buy, 2, 242);
    let big = r.cmd(Command::PlaceExit(Rig::spec(Side::Sell, 3, limit(250))));
    assert_eq!(
        big.unwrap_err().code,
        RejectCode::Invalid,
        "over the position"
    );
    let wrong = r.cmd(Command::PlaceExit(Rig::spec(Side::Buy, 1, limit(240))));
    assert_eq!(wrong.unwrap_err().code, RejectCode::Invalid, "its own side");
    // Two closes for the whole position: the first takes all of it, so the second is cancelled.
    let a = r
        .cmd(Command::PlaceExit(Rig::spec(Side::Sell, 2, limit(245))))
        .unwrap()[0];
    let b = r
        .cmd(Command::PlaceExit(Rig::spec(Side::Sell, 2, limit(246))))
        .unwrap()[0];
    r.wait(LAT + 1);
    r.quote(Some(250), Some(252));
    r.wait(LAT + 1);
    assert_eq!(r.position(call()), 0, "closed, never short");
    assert_eq!(r.fills(a), [(245, 2)]);
    assert!(r.fills(b).is_empty());
    assert!(matches!(r.state(b), OrderState::Cancelled));
}

/// A bad vendor stamp from the future neither stays fresh forever nor freezes the series.
#[test]
fn a_future_stamped_quote_goes_stale_and_blocks_nothing() {
    let mut r = Rig::new();
    let ahead = UtcMillis(r.t + 24 * 3_600_000);
    r.market(
        call(),
        MarketInput::OptionQuote {
            bid: Some(Price(500)),
            ask: Some(Price(600)),
            last: None,
            time: ahead,
        },
    );
    // A real quote stamped just after still counts.
    r.t += 1_000;
    r.quote(Some(240), Some(242));
    let id = r.limit(Side::Buy, 1, 242);
    assert_eq!(r.fills(id), [(242, 1)]);
    // Another bad stamp, then nothing for longer than the stale limit: it fills nothing.
    r.t += 1_000;
    let ahead = UtcMillis(r.t + 24 * 3_600_000);
    r.market(
        call(),
        MarketInput::OptionQuote {
            bid: Some(Price(240)),
            ask: Some(Price(242)),
            last: None,
            time: ahead,
        },
    );
    r.t += 16 * 60_000;
    assert_eq!(r.try_limit(Side::Buy, 1, 242), Err(RejectCode::Risk));
}

/// Option orders are paper only until a live broker is enabled (decision 0028, phase 4).
#[test]
fn a_live_broker_takes_no_option_orders() {
    struct Live(PaperBroker);
    impl lines_trading::broker::Broker for Live {
        fn name(&self) -> &str {
            "Live"
        }
        fn capabilities(&self) -> lines_trading::broker::BrokerCaps {
            self.0.capabilities()
        }
        fn request(&mut self, now: UtcMillis, req: lines_trading::broker::BrokerRequest) {
            self.0.request(now, req);
        }
        fn market(&mut self, now: UtcMillis, contract: ContractKey, input: &MarketInput) {
            self.0.market(now, contract, input);
        }
        fn poll(&mut self, now: UtcMillis, out: &mut Vec<lines_trading::broker::BrokerEvent>) {
            self.0.poll(now, out);
        }
        fn next_wake(&self) -> Option<UtcMillis> {
            self.0.next_wake()
        }
        fn snapshot(&self) -> lines_trading::broker::BrokerSnapshot {
            self.0.snapshot()
        }
    }
    let mut b = PaperBroker::new(PaperConfig::default());
    b.open_account(acct(), Money::whole(1_000));
    let mut core = TradingCore::new(Box::new(Live(b)));
    core.add_account(AccountConfig::paper("paper-1", 1_000), None);
    let now = UtcMillis::parse_iso(T0).unwrap();
    core.market(
        now,
        call(),
        &[MarketInput::OptionQuote {
            bid: Some(Price(240)),
            ask: Some(Price(242)),
            last: None,
            time: now,
        }],
    );
    let id = core.submit(
        now,
        Source::Test,
        Instant::now(),
        Command::PlaceOrder(Rig::spec(Side::Buy, 1, limit(242))),
    );
    let refused = core.drain().into_iter().find_map(|e| match e {
        Event::Command { id: i, result, .. } if i == id => Some(result),
        _ => None,
    });
    assert_eq!(refused.unwrap().unwrap_err().code, RejectCode::Broker);
}

/// An open option position and its resting order survive a save and restart.
#[test]
fn positions_and_orders_survive_a_restart() {
    let mut r = Rig::new();
    r.quote(Some(240), Some(242));
    r.limit(Side::Buy, 2, 242);
    let resting = r.limit(Side::Sell, 1, 260);
    let file = lines_trading::service::trading_file(&r.core, &PaperConfig::default()).unwrap();
    let json = serde_json::to_string(&file).unwrap();
    let back: lines_trading::persist::TradingFile = serde_json::from_str(&json).unwrap();
    assert_eq!(back, file);
    let b = PaperBroker::from_state(PaperConfig::default(), back.broker);
    let mut core = TradingCore::new(Box::new(b));
    for a in back.accounts {
        core.add_account(a.config, Some(a.tracker));
    }
    core.rebuild(r.now());
    assert_eq!(core.position(&acct(), call()), 2);
    assert!(core.order(resting).is_some_and(|o| o.is_open()));
}

/// Quotes forwarded from a feed batch or a store's chains reach their own series.
#[test]
fn quotes_reach_their_own_series() {
    use lines_feed::provider::MarketEvent;
    use lines_trading::market::{option_quotes, seed_option_quotes};
    let key = call().option_key().unwrap();
    let quote = lines_core::OptionQuote {
        bid: Some(Price(240)),
        ask: Some(Price(242)),
        time: UtcMillis(7),
        ..Default::default()
    };
    let want = MarketInput::OptionQuote {
        bid: Some(Price(240)),
        ask: Some(Price(242)),
        last: None,
        time: UtcMillis(7),
    };
    let events = [MarketEvent::Ready, MarketEvent::OptionQuote { key, quote }];
    assert_eq!(option_quotes(catalog::SPY, &events), [(call(), want)]);
    let mut chains = lines_core::OptionChains::default();
    chains.apply(key, quote);
    assert_eq!(seed_option_quotes(catalog::SPY, &chains), [(call(), want)]);
}

/// An option's position says when its price was taken: a one-sided quote with no last keeps the
/// mark it had, so it keeps that mark's time too (a P&L on an old mid never reads fresh).
#[test]
fn an_options_mark_keeps_its_time_through_a_one_sided_quote() {
    let mut r = Rig::new();
    r.quote(Some(240), Some(242));
    let priced = r.t + 1;
    r.quote(Some(240), Some(242));
    r.limit(Side::Buy, 1, 242);
    let marked = |r: &Rig| {
        r.events.iter().rev().find_map(|e| match e {
            Event::Position(p) if p.contract == call() => Some(p.marked_at),
            _ => None,
        })
    };
    assert_eq!(marked(&r), Some(Some(UtcMillis(priced))));
    r.t += 5_000;
    r.quote(None, Some(250));
    assert_eq!(
        marked(&r),
        Some(Some(UtcMillis(priced))),
        "the old mid's time"
    );
    r.t += 1_000;
    let again = r.t + 1;
    r.quote(Some(245), Some(247));
    assert_eq!(marked(&r), Some(Some(UtcMillis(again))));
}

/// A last sale every quote repeats isn't a new price: after a two-sided quote, a one-sided one
/// carrying the same old last keeps the two-sided quote's time; a new last is a new price.
#[test]
fn a_repeated_last_sale_doesnt_freshen_an_options_mark() {
    let mut r = Rig::new();
    let quote = |r: &mut Rig, bid: Option<i64>, ask: Option<i64>, last: Option<i64>| {
        let time = UtcMillis(r.t + 1);
        r.market(
            call(),
            MarketInput::OptionQuote {
                bid: bid.map(Price),
                ask: ask.map(Price),
                last: last.map(Price),
                time,
            },
        );
        time
    };
    quote(&mut r, Some(240), Some(242), Some(110));
    r.limit(Side::Buy, 1, 242);
    let priced = quote(&mut r, Some(240), Some(242), Some(110));
    let marked = |r: &Rig| {
        r.events.iter().rev().find_map(|e| match e {
            Event::Position(p) if p.contract == call() => Some(p.marked_at),
            _ => None,
        })
    };
    let pnl = |r: &Rig| {
        r.events.iter().rev().find_map(|e| match e {
            Event::Position(p) if p.contract == call() => Some(p.unrealised),
            _ => None,
        })
    };
    let at_mid = pnl(&r);
    r.t += 5_000;
    quote(&mut r, None, Some(250), Some(110));
    assert_eq!(marked(&r), Some(Some(priced)), "the old last, repeated");
    assert_eq!(pnl(&r), at_mid, "still marked at the mid, not the old sale");
    r.t += 1_000;
    let traded = quote(&mut r, None, Some(250), Some(120));
    assert_eq!(marked(&r), Some(Some(traded)), "a new last");
}
