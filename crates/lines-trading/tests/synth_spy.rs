//! The synthetic SPY session, replayed into the core: its quotes stay inside the age limits all
//! day, so it trades, at the ask of the moment rather than an old one.

use std::collections::HashMap;
use std::time::Instant;

use lines_core::{CivilDate, ContractKey, OptionKey, Price, Qty, Right, UtcMillis};
use lines_feed::catalog;
use lines_feed::provider::MarketEvent;
use lines_feed::recorded::RecordedEvents;
use lines_feed::synth_chain;
use lines_trading::{
    AccountConfig, AccountId, Command, Event, MarketInput, OrderKind, OrderSpec, PaperBroker,
    PaperConfig, Side, Source, TradingCore, risk,
};

const LAT: i64 = 20;

#[test]
fn the_synthetic_session_trades_on_fresh_quotes_all_day() {
    let dir = std::env::temp_dir().join(format!("lines-synth-spy-{}", std::process::id()));
    let path = dir.join("spy.lfr");
    synth_chain::write_spy_chain(&path).unwrap();
    let mut replay = RecordedEvents::open(&path).unwrap();

    let id = AccountId::new("paper-1");
    let account = AccountConfig::paper("paper-1", 1_000_000);
    let mut broker = PaperBroker::new(PaperConfig {
        latency_ms: LAT,
        ..Default::default()
    });
    broker.open_account(id.clone(), lines_trading::Money::whole(1_000_000));
    let mut core = TradingCore::new(Box::new(broker));
    core.add_account(account, None);

    let series = |(y, m, d): (u16, u8, u8)| OptionKey {
        expiry: CivilDate::new(y, m, d).unwrap(),
        // In the money wherever the day's walk goes, so it always has a bid.
        strike: Price(67_000),
        right: Right::Call,
    };
    // The two ends of the file: today's, and the December quarterly.
    let traded = [
        series(synth_chain::EXPIRIES[0]),
        series(synth_chain::EXPIRIES[5]),
    ];
    let contract = |k: OptionKey| ContractKey::option(catalog::SPY, k.expiry, k.strike, k.right);
    let open = UtcMillis::parse_iso(synth_chain::OPEN).unwrap().0;

    let mut asks: HashMap<OptionKey, Price> = HashMap::new();
    let mut seen: HashMap<OptionKey, UtcMillis> = HashMap::new();
    let (mut sent, mut filled) = (0, 0);
    while let Some((at, events)) = replay.next_batch().unwrap() {
        for e in events {
            let MarketEvent::OptionQuote { key, quote } = e else {
                continue;
            };
            // Every series, every quote: inside its limit since the last one.
            if let Some(before) = seen.insert(key, quote.time) {
                let age = quote.time.0 - before.0;
                let limit = risk::expiry_quote_stale_ms(catalog::SPY, key.expiry, quote.time);
                assert!(age <= limit, "{key:?}: {age} ms against {limit}");
            }
            if traded.contains(&key) {
                asks.insert(key, quote.ask.unwrap());
                core.market(
                    at,
                    contract(key),
                    &[MarketInput::OptionQuote {
                        bid: quote.bid,
                        ask: quote.ask,
                        last: quote.last,
                        time: quote.time,
                    }],
                );
            }
        }
        // Every half hour, with each quote as old as it gets in the file: buy at the ask.
        let elapsed = at.0 - open;
        if elapsed % 1_800_000 != 38_000 {
            continue;
        }
        for key in traded {
            let ask = asks[&key];
            let command = core.submit(
                at,
                Source::Dom,
                Instant::now(),
                Command::PlaceOrder(OrderSpec {
                    account: id.clone(),
                    contract: contract(key),
                    side: Side::Buy,
                    qty: Qty(1),
                    kind: OrderKind::Limit { price: ask },
                }),
            );
            sent += 1;
            core.tick(UtcMillis(at.0 + LAT + 1));
            for e in core.drain() {
                match e {
                    Event::Command { id: c, result, .. } if c == command => {
                        assert!(result.is_ok(), "{key:?} at {at:?}: {result:?}");
                    }
                    Event::Fill(f) => {
                        assert_eq!(f.price, ask, "{key:?} at {at:?}");
                        filled += 1;
                    }
                    _ => {}
                }
            }
        }
    }
    assert_eq!(seen.len(), 6 * 41 * 2);
    assert_eq!(sent, 13 * 2, "half-hourly through the session");
    assert_eq!(filled, sent);
    let _ = std::fs::remove_dir_all(dir);
}
