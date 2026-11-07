//! The latency HUD's order windows (roadmap 6b): nothing is recorded while the HUD is hidden;
//! shown, click → sent and the paper broker's simulated ack round trip land in the shared
//! windows on the core's clock. One test (the HUD switch is process-wide).

use std::time::Instant;

use lines_core::{ContractKey, Month, Price, Qty, UtcMillis, latency};
use lines_feed::catalog;
use lines_trading::{
    AccountConfig, AccountId, Command, Money, OrderKind, OrderSpec, PaperBroker, PaperConfig, Side,
    Source, TradingCore,
};

fn es() -> ContractKey {
    ContractKey::monthly(catalog::ES, 2026, Month::new(12).unwrap())
}

fn order(i: i64) -> Command {
    Command::PlaceOrder(OrderSpec {
        account: AccountId::new("paper-1"),
        contract: es(),
        side: Side::Buy,
        qty: Qty(1),
        kind: OrderKind::Limit {
            price: Price(400_000 + i * 25),
        },
    })
}

#[test]
fn order_latency_is_recorded_only_while_the_hud_is_shown() {
    let mut broker = PaperBroker::new(PaperConfig {
        latency_ms: 40,
        check_sessions: false,
        ..Default::default()
    });
    broker.open_account(AccountId::new("paper-1"), Money::whole(1_000_000));
    let mut core = TradingCore::new(Box::new(broker));
    core.add_account(AccountConfig::paper("paper-1", 1_000_000), None);
    let hud = core.order_latency();
    let t0 = UtcMillis::parse_iso("2026-11-10T15:00:00Z").unwrap().0;

    // Hidden (the default): orders go out and are acked; nothing is recorded.
    assert!(!latency::enabled(), "off by default");
    core.submit(UtcMillis(t0), Source::Test, Instant::now(), order(0));
    core.tick(UtcMillis(t0 + 100));
    core.note_time(UtcMillis(t0 + 100));
    {
        let h = hud.lock().unwrap();
        assert_eq!(h.at_ms, None);
        assert_eq!(h.click_sent.count(t0 + 100), 0);
        assert_eq!(h.ack_rtt.count(t0 + 100), 0);
    }

    // Shown: one sample per order sent, and the 40 ms simulated ack.
    latency::set_enabled(true);
    for i in 1..=3 {
        let t = t0 + 1_000 * i;
        core.submit(UtcMillis(t), Source::Test, Instant::now(), order(i));
        core.tick(UtcMillis(t + 100));
    }
    let h = hud.lock().unwrap();
    let now = h.at_ms.expect("the core's clock");
    assert!(now >= t0 + 3_040, "the newest ack");
    assert_eq!(h.click_sent.count(now), 3);
    let ack = h.ack_rtt.summary(now);
    assert_eq!(ack.n, 3);
    // 40 ms = 40 000 µs, reported as its bucket's upper edge (at most 25% above).
    let p50 = ack.p50_us.unwrap();
    assert!((40_000..=50_000).contains(&p50), "{p50}");
    drop(h);
    latency::set_enabled(false);
}
