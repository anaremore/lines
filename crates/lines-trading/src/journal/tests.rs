use lines_core::{ContractKey, Month, Price, Qty, UtcMillis};
use lines_feed::catalog;

use super::stats::{GroupBy, Stats, exchange_hour, grouped};
use super::store::{self, JournalWriter};
use super::*;
use crate::command::Source;
use crate::event::{Event, FillEvent, PositionView};
use crate::market::MarketInput;
use crate::types::{AccountId, CommandId, Money, OrderId, Side};

fn es() -> ContractKey {
    ContractKey::monthly(catalog::ES, 2026, Month::new(12).unwrap())
}

fn acct() -> AccountId {
    AccountId::new("paper")
}

/// ES: 6000.00 is 600_000 price units; one unit (0.01 point) is $0.50 per contract.
fn px(points: f64) -> Price {
    Price((points * 100.0).round() as i64)
}

fn dollars(d: f64) -> Money {
    Money((d * Money::UNITS_PER_WHOLE as f64).round() as i128)
}

/// 2026-11-05 (Thursday) 15:00 UTC plus `s` seconds: inside the ES session of trade date
/// 2026-11-05.
fn at(s: i64) -> UtcMillis {
    UtcMillis(UtcMillis::parse_iso("2026-11-05T15:00:00Z").unwrap().0 + s * 1000)
}

fn fill(id: &str, order: u64, side: Side, p: f64, q: i64, fee: f64, s: i64) -> Event {
    Event::Fill(FillEvent {
        account: acct(),
        order: OrderId(order),
        contract: es(),
        side,
        price: px(p),
        qty: Qty(q),
        fee: dollars(fee),
        realised: Money::ZERO, // the journal computes its own (FIFO)
        trade_id: id.into(),
        reconciled: false,
        at: at(s),
    })
}

fn command(order: u64, source: Source) -> Event {
    Event::Command {
        id: CommandId(order),
        source,
        at: at(0),
        result: Ok(vec![OrderId(order)]),
    }
}

fn feed(j: &mut Journal, events: &[Event]) -> Vec<Trade> {
    events.iter().flat_map(|e| j.on_event(e)).collect()
}

#[test]
fn partials_adds_and_scale_outs_are_one_trade_with_exact_money() {
    let mut j = Journal::new();
    let closed = feed(
        &mut j,
        &[
            command(1, Source::Dom),
            command(2, Source::Chart),
            command(3, Source::Hotkey),
            // Buy 3 in two partials, add 1, scale out 2, then out 2.
            fill("f1", 1, Side::Buy, 6000.00, 2, 0.62, 0),
            fill("f2", 1, Side::Buy, 6000.25, 1, 0.31, 1),
            fill("f3", 2, Side::Buy, 5999.50, 1, 0.31, 30),
            fill("f4", 3, Side::Sell, 6002.00, 2, 0.62, 90),
            fill("f5", 3, Side::Sell, 6001.00, 2, 0.62, 120),
        ],
    );
    assert_eq!(closed.len(), 1, "one round trip");
    let t = &closed[0];
    assert_eq!(
        (t.side, t.max_qty, t.entry_qty, t.exit_qty),
        (Side::Buy, 4, 4, 4)
    );
    // FIFO: 2@6000.00 and 1@6000.25 and 1@5999.50 closed at 2@6002 and 2@6001:
    // (2.00×2 + 1.75×1 + 1.75... ) = sells 6002×2 + 6001×2 − buys (6000×2 + 6000.25 + 5999.5)
    // = 24006 − 23999.75 = 6.25 points = $312.50.
    assert_eq!(t.realized, dollars(312.50));
    assert_eq!(t.fees, dollars(2.48));
    assert_eq!(t.net(), dollars(310.02));
    assert_eq!(t.avg_entry(), Some(599_993.75));
    assert_eq!(t.avg_exit(), Some(600_150.0));
    // (6001.50 − 5999.9375) / 0.25 = 6.25 ticks per contract.
    assert_eq!(t.ticks(25), Some(6.25));
    assert_eq!((t.entry_time, t.exit_time), (at(0), at(120)));
    assert_eq!(t.duration_ms(), 120_000);
    assert_eq!(t.trade_date.to_string(), "2026-11-05");
    assert_eq!(t.fills.len(), 5);
    let sources: Vec<_> = t.fills.iter().map(|f| f.source).collect();
    assert_eq!(
        sources,
        [
            Some(Source::Dom),
            Some(Source::Dom),
            Some(Source::Chart),
            Some(Source::Hotkey),
            Some(Source::Hotkey)
        ]
    );
    assert_eq!(
        t.id,
        format!("f1@{}", at(0).0),
        "first fill id @ entry time"
    );
    assert_eq!(j.trades().len(), 1);
    assert!(j.open_trades().next().is_none());
}

#[test]
fn a_reversal_closes_one_trade_and_opens_the_next() {
    let mut j = Journal::new();
    let closed = feed(
        &mut j,
        &[
            command(9, Source::Rule),
            fill("a", 1, Side::Buy, 6000.00, 2, 0.62, 0),
            // Sell 5: closes the 2 long, opens 3 short. Fee $1.55 splits 2/5 and 3/5.
            fill("b", 2, Side::Sell, 6001.00, 5, 1.55, 10),
        ],
    );
    assert_eq!(closed.len(), 1);
    let long = &closed[0];
    assert_eq!((long.side, long.exit_qty), (Side::Buy, 2));
    assert_eq!(long.realized, dollars(100.0), "1 point × 2 × $50");
    assert_eq!(long.fees, dollars(0.62 + 0.62));
    let short = j.open_trades().next().unwrap().clone();
    assert_eq!(
        (short.side, short.entry_qty, short.id.as_str()),
        (Side::Sell, 3, "b/r")
    );
    assert_eq!(
        short.fees,
        dollars(0.93),
        "the rest of the fee, nothing lost"
    );
    assert_eq!(long.fees + short.fees, dollars(0.62 + 1.55));
    // Rule-sourced flatten closes it.
    let closed = feed(&mut j, &[fill("c", 9, Side::Buy, 6002.00, 3, 0.93, 20)]);
    let short = &closed[0];
    assert_eq!(short.realized, dollars(-150.0));
    assert_eq!(short.fills[1].source, Some(Source::Rule));
    assert_eq!(j.trades().len(), 2);
}

#[test]
fn reconciled_and_repeated_fills_count_once() {
    let mut j = Journal::new();
    feed(&mut j, &[fill("x1", 1, Side::Sell, 6000.00, 1, 0.31, 0)]);
    // The same fill again (a replayed stream): ignored.
    feed(&mut j, &[fill("x1", 1, Side::Sell, 6000.00, 1, 0.31, 0)]);
    assert_eq!(j.open_trades().next().unwrap().entry_qty, 1);
    // The broker filled the exit while disconnected: reconciliation reports it with no P&L
    // of its own, possibly twice. The journal's FIFO still gets the P&L right.
    let recon = Event::Fill(FillEvent {
        account: acct(),
        order: OrderId(2),
        contract: es(),
        side: Side::Buy,
        price: px(5998.00),
        qty: Qty(1),
        fee: Money::ZERO,
        realised: Money::ZERO,
        trade_id: "recon-2-1".into(),
        reconciled: true,
        at: at(60),
    });
    let closed = feed(&mut j, &[recon.clone(), recon]);
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0].realized, dollars(100.0));
    assert!(closed[0].fills[1].reconciled);
    assert_eq!(j.trades().len(), 1);
}

/// Broker fill ids repeat across runs (paper order ids restart when the paper file is reset,
/// or lag after a crash): a new run's fill with an id an earlier run used is a new fill, never
/// "already counted", and its trade never replaces the earlier one.
#[test]
fn fill_ids_from_an_earlier_run_never_block_new_fills() {
    let mut first = Journal::new();
    feed(
        &mut first,
        &[fill("paper-1-1", 1, Side::Buy, 6000.00, 1, 0.0, 0)],
    );
    let old = feed(
        &mut first,
        &[fill("paper-2-1", 2, Side::Sell, 6001.00, 1, 0.0, 10)],
    );
    // The next run loads the file, and the same ids come again for new orders.
    let mut next = Journal::from_records([Record::Trade(Box::new(old[0].clone()))]);
    feed(
        &mut next,
        &[fill("paper-1-1", 1, Side::Buy, 6000.00, 1, 0.0, 600)],
    );
    let new = feed(
        &mut next,
        &[fill("paper-2-1", 2, Side::Sell, 5999.00, 1, 0.0, 610)],
    );
    assert_eq!(new.len(), 1, "counted");
    assert_eq!(next.trades().len(), 2, "both kept");
    assert_ne!(next.trades()[0].id, next.trades()[1].id);
    assert_eq!(next.trades()[0].realized, dollars(50.0));
    assert_eq!(next.trades()[1].realized, dollars(-50.0));
}

/// Two trades saved with one id (they can't be written that way now, but a file is data):
/// both are kept, the second as `#2`, and an edit of the id goes to the first.
#[test]
fn colliding_ids_on_load_are_kept_apart() {
    let a = closed(10.0, 5, &[]);
    let b = closed(-5.0, 5, &[]);
    let j = Journal::from_records([
        Record::Trade(Box::new(a)),
        Record::Trade(Box::new(b)),
        Record::Edit(Edit {
            id: "t5".into(),
            tags: vec!["first".into()],
            note: String::new(),
            excursion: None,
        }),
    ]);
    assert_eq!(j.trades().len(), 2);
    assert_eq!(j.trade("t5").unwrap().tags, ["first"]);
    assert_eq!(j.trade("t5#2").unwrap().realized, dollars(-5.0));
    // Seeded trades after restarts on different days get distinct ids too.
    let mut k = Journal::new();
    for day in [0, 86_400] {
        k.on_event(&Event::Position(PositionView {
            account: acct(),
            contract: es(),
            qty: 1,
            avg_price: Some(600_000.0),
            unrealised: Money::ZERO,
            marked_at: None,
        }));
        feed(
            &mut k,
            &[fill(&format!("s{day}"), 9, Side::Sell, 6000.0, 1, 0.0, day)],
        );
    }
    assert_eq!(k.trades().len(), 2);
    assert_ne!(k.trades()[0].id, k.trades()[1].id);
}

#[test]
fn mae_and_mfe_follow_the_market_while_open() {
    let mut j = Journal::new();
    feed(&mut j, &[fill("m1", 1, Side::Buy, 6000.00, 2, 0.0, 0)]);
    let trade = |p: f64| MarketInput::Trade {
        time: at(1),
        price: px(p),
        qty: Qty(1),
    };
    j.on_market(
        es(),
        &[
            trade(5998.00),
            trade(5995.00),
            MarketInput::Bid(px(5990.00)),
        ],
    );
    // Another contract's prices don't touch it.
    let other = ContractKey::monthly(catalog::ES, 2027, Month::new(3).unwrap());
    j.on_market(other, &[trade(5900.00)]);
    // Scale out 1 at 6004: realized +$200, then the market runs to 6010, back to 6003.
    feed(&mut j, &[fill("m2", 2, Side::Sell, 6004.00, 1, 0.0, 30)]);
    j.on_market(es(), &[trade(6010.00), MarketInput::Last(px(6003.00))]);
    let t = &feed(&mut j, &[fill("m3", 2, Side::Sell, 6003.00, 1, 0.0, 60)])[0];
    // Worst: 2 × −5 points = −$500 (the bid isn't a trade); best: $200 + 10 points × 1.
    assert_eq!(t.mae, dollars(-500.0));
    assert_eq!(t.mfe, dollars(700.0));
    assert_eq!((t.low, t.high), (px(5995.00), px(6010.00)));
    // Ticks are those dollars at the largest size, 2 × $12.50 a tick: deliberately not the 40 t
    // the price ran from the entry, since only 1 contract was left for the last of that run.
    let product = catalog::get(t.contract.product).unwrap();
    assert_eq!(t.excursion_ticks(&product), Some((-20.0, 28.0)));
    assert_eq!(t.realized, dollars(350.0));
}

/// Dollars = ticks × quantity × tick value, however the size was built: a price measured from the
/// final average entry would read 36 t against an add to a winner that was never under water.
#[test]
fn excursion_ticks_follow_the_dollars_through_a_scale_in() {
    let product = catalog::get(es().product).unwrap();
    let mut j = Journal::new();
    let t = &feed(
        &mut j,
        &[
            fill("w1", 1, Side::Buy, 6000.00, 1, 0.0, 0),
            fill("w2", 2, Side::Buy, 6010.00, 9, 0.0, 10),
            fill("w3", 3, Side::Sell, 6011.00, 10, 0.0, 20),
        ],
    )[0];
    assert_eq!((t.mae, t.mfe), (Money::ZERO, dollars(1000.0)));
    assert_eq!(t.excursion_ticks(&product), Some((0.0, 8.0)));

    // Averaging down: 1 contract's −$500 before the add, over the 10 of the largest size.
    let mut j = Journal::new();
    feed(&mut j, &[fill("d1", 1, Side::Buy, 6000.00, 1, 0.0, 0)]);
    j.on_market(
        es(),
        &[MarketInput::Trade {
            time: at(1),
            price: px(5990.00),
            qty: Qty(1),
        }],
    );
    let t = &feed(
        &mut j,
        &[
            fill("d2", 2, Side::Buy, 5990.00, 9, 0.0, 10),
            fill("d3", 3, Side::Sell, 5995.00, 10, 0.0, 20),
        ],
    )[0];
    assert_eq!(t.mae, dollars(-500.0));
    assert_eq!(t.excursion_ticks(&product).map(|e| e.0), Some(-4.0));
}

#[test]
fn trade_dates_and_hours_follow_the_exchange() {
    let mut j = Journal::new();
    // 23:30 UTC on 2026-11-05 is 17:30 CT: the session of trade date 2026-11-06.
    let evening = UtcMillis::parse_iso("2026-11-05T23:30:00Z").unwrap();
    let e = |id: &str, side, s: i64| {
        Event::Fill(FillEvent {
            account: acct(),
            order: OrderId(1),
            contract: es(),
            side,
            price: px(6000.0),
            qty: Qty(1),
            fee: Money::ZERO,
            realised: Money::ZERO,
            trade_id: id.into(),
            reconciled: false,
            at: UtcMillis(evening.0 + s * 1000),
        })
    };
    let t = &feed(&mut j, &[e("d1", Side::Buy, 0), e("d2", Side::Sell, 5)])[0];
    assert_eq!(t.trade_date.to_string(), "2026-11-06");
    assert_eq!(stats::entry_hour(t), 17, "Chicago time");
    assert_eq!(
        exchange_hour(catalog::ES, at(0)),
        9,
        "15:00 UTC = 09:00 CST"
    );
}

#[test]
fn a_position_from_before_the_journal_is_seeded_and_marked() {
    let mut j = Journal::new();
    j.on_event(&Event::Position(PositionView {
        account: acct(),
        contract: es(),
        qty: -2,
        avg_price: Some(600_050.0),
        unrealised: Money::ZERO,
        marked_at: None,
    }));
    let t = feed(&mut j, &[fill("s1", 4, Side::Buy, 6000.00, 2, 0.0, 0)]);
    assert_eq!(t.len(), 1);
    assert!(t[0].incomplete);
    // Its entry time is unknown: it dates from the first fill the journal saw.
    assert_eq!((t[0].entry_time, t[0].exit_time), (at(0), at(0)));
    assert_eq!(t[0].trade_date.to_string(), "2026-11-05");
    assert_eq!(t[0].realized, dollars(50.0));
    // A position the journal already tracks is left alone.
    feed(&mut j, &[fill("s2", 5, Side::Buy, 6000.00, 1, 0.0, 1)]);
    j.on_event(&Event::Position(PositionView {
        account: acct(),
        contract: es(),
        qty: 1,
        avg_price: Some(500_000.0),
        unrealised: Money::ZERO,
        marked_at: None,
    }));
    assert!(!j.open_trades().next().unwrap().incomplete);
}

fn closed(net: f64, exit: i64, tags: &[&str]) -> Trade {
    let mut t = new_trade(
        format!("t{exit}"),
        acct(),
        es(),
        Side::Buy,
        at(exit - 10),
        lines_core::CivilDate::new(2026, 11, if exit < 1000 { 5 } else { 6 }).unwrap(),
        px(6000.0),
    );
    t.exit_time = at(exit);
    t.realized = dollars(net);
    t.tags = tags.iter().map(|s| s.to_string()).collect();
    t
}

#[test]
fn stats_math() {
    // Net: +100, −50, −25, 0, +200, −300, +80 (in exit order; handed in shuffled).
    let trades = [
        closed(200.0, 50, &["breakout"]),
        closed(100.0, 10, &["breakout", "a+"]),
        closed(-50.0, 20, &[]),
        closed(0.0, 40, &[]),
        closed(-25.0, 30, &["breakout"]),
        closed(-300.0, 1060, &["fade"]),
        closed(80.0, 1070, &[]),
    ];
    let s = Stats::of(&trades);
    assert_eq!((s.count, s.wins, s.losses), (7, 3, 3));
    assert_eq!(s.net, dollars(5.0));
    assert_eq!(s.win_rate(), Some(3.0 / 7.0));
    assert_eq!(
        s.avg_win(),
        Some(Money(dollars(380.0).0 / 3)),
        "exact units, floored"
    );
    assert_eq!(s.avg_loss(), Some(dollars(-125.0)));
    assert_eq!(s.gross_win, dollars(380.0));
    assert_eq!(s.gross_loss, dollars(-375.0));
    assert_eq!(s.profit_factor(), Some(380.0 / 375.0));
    assert_eq!(s.expectancy(), Some(Money(dollars(5.0).0 / 7)));
    // Running: 100, 50, 25, 25, 225, −75, 5 → the worst fall is 225 → −75 = $300.
    assert_eq!(s.max_drawdown, dollars(300.0));
    // W L L 0 W L W: longest win 1, longest loss 2, ends on a win.
    assert_eq!(
        (s.longest_win_streak, s.longest_loss_streak, s.streak),
        (1, 2, 1)
    );
    // No losses: no profit factor; nothing: nothing.
    assert_eq!(Stats::of(&trades[..1]).profit_factor(), None);
    assert_eq!(Stats::of([]).win_rate(), None);

    let by_tag = grouped(&trades, GroupBy::Tag);
    assert_eq!(by_tag["breakout"].count, 3);
    assert_eq!(by_tag["breakout"].net, dollars(275.0));
    assert_eq!(by_tag[super::stats::UNTAGGED].count, 3);
    assert_eq!(by_tag["fade"].losses, 1);
    let by_date = grouped(&trades, GroupBy::TradeDate);
    assert_eq!(
        by_date.keys().collect::<Vec<_>>(),
        ["2026-11-05", "2026-11-06"]
    );
    assert_eq!(by_date["2026-11-06"].net, dollars(-220.0));
    assert_eq!(grouped(&trades, GroupBy::Product)["ES"].count, 7);
    // By account: each trade once, under its account id.
    let mut other = closed(40.0, 1080, &["fade"]);
    other.account = AccountId::new("eval");
    let with_other: Vec<Trade> = trades.iter().cloned().chain([other]).collect();
    let by_account = grouped(&with_other, GroupBy::Account);
    assert_eq!(by_account.keys().collect::<Vec<_>>(), ["eval", "paper"]);
    assert_eq!(by_account["paper"], s);
    assert_eq!(by_account["eval"].net, dollars(40.0));
    assert_eq!(grouped(&with_other, GroupBy::Tag)["fade"].count, 2);
    assert_eq!(
        grouped(&trades, GroupBy::Hour)["09"].count,
        7,
        "09:xx Chicago"
    );
}

#[test]
fn edits_last_one_wins_and_tags_are_tidy() {
    let mut j = Journal::from_records([
        Record::Trade(Box::new(closed(10.0, 5, &[]))),
        Record::Edit(Edit {
            id: "t5".into(),
            tags: vec!["first".into()],
            note: "one".into(),
            excursion: None,
        }),
        Record::Edit(Edit {
            id: "t5".into(),
            tags: vec![
                " breakout ".into(),
                "".into(),
                "breakout".into(),
                "A+".into(),
            ],
            note: "two".into(),
            excursion: None,
        }),
    ]);
    let t = j.trade("t5").unwrap();
    assert_eq!(t.tags, ["breakout", "A+"]);
    assert_eq!(t.note, "two");
    assert!(
        j.edit(Edit {
            id: "nope".into(),
            tags: vec![],
            note: String::new(),
            excursion: None,
        })
        .is_none()
    );
}

#[test]
fn csv_quotes_and_neutralizes_formulas() {
    let mut t = closed(12.5, 5, &["a,b", "=cmd"]);
    t.note = "said \"hi\"\nthen left".into();
    let out = csv(&[t]);
    let mut lines = out.lines();
    assert!(
        lines
            .next()
            .unwrap()
            .starts_with("id,account,contract,side")
    );
    let row = out.split_once('\n').unwrap().1;
    assert!(row.starts_with("t5,paper,ESZ26,long,"), "{row}");
    assert!(row.contains(",12.50,0.00,12.50,"), "{row}");
    assert!(row.contains("\"a,b;=cmd\""), "{row}");
    assert!(row.contains("\"said \"\"hi\"\"\nthen left\""), "{row}");
    assert_eq!(quote("=1+1"), "'=1+1");
}

#[test]
fn reading_tolerates_a_torn_tail_and_refuses_newer_versions() {
    let header = "{\"journal\":1,\"source\":\"mock\"}\n";
    let trade = serde_json::to_string(&Record::Trade(Box::new(closed(10.0, 5, &[])))).unwrap();
    let edit = serde_json::to_string(&Record::Edit(Edit {
        id: "t5".into(),
        tags: vec!["x".into()],
        note: String::new(),
        excursion: None,
    }))
    .unwrap();
    let full = format!("{header}{trade}\n{edit}\n");
    let l = store::parse(full.as_bytes()).unwrap();
    assert_eq!((l.records.len(), l.torn, l.skipped), (2, false, 0));
    assert_eq!(l.valid_len, full.len() as u64);
    // A crash mid-record: the torn line is dropped, appending resumes after the last full one.
    let torn = format!("{header}{trade}\n{}", &edit[..edit.len() / 2]);
    let l = store::parse(torn.as_bytes()).unwrap();
    assert_eq!((l.records.len(), l.torn), (1, true));
    assert_eq!(l.valid_len, (header.len() + trade.len() + 1) as u64);
    // A complete but bad line in the middle is skipped and counted.
    let bad = format!("{header}not json\n{trade}\n");
    let l = store::parse(bad.as_bytes()).unwrap();
    assert_eq!((l.records.len(), l.skipped), (1, 1));
    // Empty, or torn while writing the header: an empty journal.
    assert_eq!(store::parse(b"").unwrap().records.len(), 0);
    assert!(store::parse(b"{\"journ").unwrap().torn);
    // Newer: refused.
    let newer = "{\"journal\":3,\"source\":\"mock\"}\n";
    assert!(store::parse(newer.as_bytes()).is_err());
}

/// Decision 0028: a v1 journal reads as it is; opening it for writing keeps a copy
/// (`mock.v1.jsonl`, the bytes as they were), then the same records continue under a v2
/// header, so an older build refuses the file instead of reading an option trade as spot.
#[test]
fn a_v1_journal_migrates_to_v2_when_written() {
    let dir = lines_core::scratch::Scratch::new("journal-v1");
    let path = dir.join("mock.jsonl");
    let trade = serde_json::to_string(&Record::Trade(Box::new(closed(10.0, 5, &["a"])))).unwrap();
    let v1 = format!("{{\"journal\":1,\"source\":\"mock\"}}\n{trade}\n");
    // A torn tail too: cut before the migration, never carried into either file.
    std::fs::write(&path, format!("{v1}{{\"trade\":{{\"id\"")).unwrap();
    let read = store::read(&path).unwrap().unwrap();
    assert_eq!((read.version, read.records.len(), read.torn), (1, 1, true));
    {
        // The caller's source doesn't replace the file's own.
        let w = JournalWriter::open(path.clone(), "other", Some(&read)).unwrap();
        w.append(Record::Trade(Box::new(closed(-5.0, 20, &[]))));
        assert!(w.flush(std::time::Duration::from_secs(5)));
    }
    assert_eq!(
        std::fs::read_to_string(dir.join("mock.v1.jsonl")).unwrap(),
        v1,
        "the original, kept"
    );
    let now = std::fs::read_to_string(&path).unwrap();
    assert!(
        now.starts_with("{\"journal\":2,\"source\":\"mock\"}\n"),
        "{now}"
    );
    let l = store::read(&path).unwrap().unwrap();
    assert_eq!(l.version, 2);
    assert_eq!(l.records.len(), 2);
    assert_eq!(l.records[0], read.records[0], "the v1 record, unchanged");
    // Opening again: already current, nothing moves.
    drop(JournalWriter::open(path.clone(), "mock", Some(&l)).unwrap());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), now);
}

/// A paper reset that clears its journal: that account's trades go, from memory and from the
/// file (kept first as a dated `.bak`); another account's stay, and appends continue after.
#[test]
fn clearing_an_account_rewrites_the_file_and_keeps_a_copy() {
    let dir = lines_core::scratch::Scratch::new("journal-clear");
    let path = dir.join("mock.jsonl");
    let mine = closed(10.0, 5, &["a"]);
    let mut theirs = closed(-5.0, 20, &[]);
    theirs.account = AccountId::new("other");
    theirs.id = "other-1".into();
    let mut j = Journal::from_records([
        Record::Trade(Box::new(mine.clone())),
        Record::Trade(Box::new(theirs.clone())),
    ]);
    assert_eq!(j.clear_account(&acct()), 1);
    assert_eq!(j.trades(), std::slice::from_ref(&theirs));
    assert!(j.trade(&mine.id).is_none());
    assert!(
        j.trade("other-1").is_some(),
        "ids still find the trades left"
    );

    let w = JournalWriter::open(path.clone(), "mock", None).unwrap();
    w.append(Record::Trade(Box::new(mine)));
    w.append(Record::Trade(Box::new(theirs.clone())));
    assert!(w.flush(std::time::Duration::from_secs(5)));
    let before = std::fs::read_to_string(&path).unwrap();
    w.rewrite(vec![Record::Trade(Box::new(theirs.clone()))]);
    w.append(Record::Trade(Box::new(closed(3.0, 30, &[]))));
    assert!(w.flush(std::time::Duration::from_secs(5)));
    let l = store::read(&path).unwrap().unwrap();
    assert_eq!(
        l.records.len(),
        2,
        "the other account's trade, then the new one"
    );
    assert_eq!(l.records[0], Record::Trade(Box::new(theirs)));
    let baks: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("mock-") && n.ends_with(".jsonl.bak"))
        .collect();
    assert_eq!(baks.len(), 1, "{baks:?}");
    assert_eq!(std::fs::read_to_string(dir.join(&baks[0])).unwrap(), before);
    drop(w);
}

#[test]
fn writer_appends_in_the_background_and_cuts_a_torn_tail() {
    let dir = lines_core::scratch::Scratch::new("journal");
    let path = dir.join("journal").join("mock.jsonl");
    {
        let w = JournalWriter::open(path.clone(), "mock", None).unwrap();
        w.append(Record::Trade(Box::new(closed(10.0, 5, &[]))));
        w.append(Record::Edit(Edit {
            id: "t5".into(),
            tags: vec!["first".into()],
            note: String::new(),
            excursion: None,
        }));
    } // dropped: the thread drains, syncs and stops
    let wait = |n: usize| {
        for _ in 0..200 {
            if let Ok(Some(l)) = store::read(&path)
                && l.records.len() >= n
            {
                return l;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("records never arrived");
    };
    let l = wait(2);
    // A crash left half a record at the end.
    let mut bytes = std::fs::read(&path).unwrap();
    bytes.extend_from_slice(b"{\"edit\":{\"id\":\"t5\",\"ta");
    std::fs::write(&path, &bytes).unwrap();
    let torn = store::read(&path).unwrap().unwrap();
    assert!(torn.torn && torn.records.len() == 2);
    {
        let w = JournalWriter::open(path.clone(), "mock", Some(&torn)).unwrap();
        w.append(Record::Edit(Edit {
            id: "t5".into(),
            tags: vec!["second".into()],
            note: "n".into(),
            excursion: None,
        }));
    }
    let l2 = wait(3);
    assert!(
        !l2.torn && l2.skipped == 0,
        "the torn tail was cut, the next record is whole"
    );
    let j = Journal::from_records(l2.records);
    assert_eq!(j.trade("t5").unwrap().tags, ["second"]);
    assert_eq!(l.records.len(), 2);
}

/// A round trip of `account` whose fill ids hold the account, as a broker's can.
fn round_trip(j: &mut Journal, account: &str) -> Vec<Trade> {
    let mut events = vec![
        fill(&format!("{account}-1"), 1, Side::Buy, 6000.0, 2, 0.0, 0),
        fill(&format!("{account}-2"), 2, Side::Sell, 6001.0, 2, 0.0, 60),
    ];
    for e in &mut events {
        if let Event::Fill(f) = e {
            f.account = AccountId::new(account);
        }
    }
    feed(j, &events)
}

#[test]
fn a_closed_trade_names_one_file_without_its_account() {
    const ACCOUNT: &str = "ACCT-DISTINCTIVE-42";
    let mut j = Journal::new();
    j.set_shots(true);
    let closed = round_trip(&mut j, ACCOUNT);
    let r = j.take_shot_requests();
    assert_eq!(r.len(), 1, "one per trade, none as it opened");
    assert!(j.take_shot_requests().is_empty(), "taken once");
    let t = &closed[0];
    assert!(t.id.contains(ACCOUNT), "this id holds the account");
    assert_eq!(t.screenshots, [r[0].file.clone()], "saved with the trade");
    let name = &r[0].file;
    assert!(!name.contains(ACCOUNT) && !name.contains("DISTINCTIVE"));
    assert!(name.starts_with(&format!("{}-", t.trade_date)) && name.ends_with(".svg"));
    assert_eq!(
        name.len(),
        "2026-11-05-0123456789abcdef.svg".len(),
        "{name}"
    );
    assert!(is_shot_file_name(name));
    // Nothing downstream can write what it was never given.
    let debug = format!("{:?}", r[0].trade);
    assert!(!debug.contains("DISTINCTIVE"), "{debug}");
    assert_eq!(r[0].trade.fills.len(), 2, "every fill, to mark");
    assert_eq!(r[0].trade.net(), t.net());
    // A reversal's closing trade asks too, once; the next one asks when it closes.
    let mut rev = Journal::new();
    rev.set_shots(true);
    feed(
        &mut rev,
        &[
            fill("a", 1, Side::Buy, 6000.0, 1, 0.0, 0),
            fill("b", 2, Side::Sell, 6001.0, 2, 0.0, 10),
        ],
    );
    assert_eq!(rev.take_shot_requests().len(), 1);
    feed(&mut rev, &[fill("c", 3, Side::Buy, 6000.0, 1, 0.0, 20)]);
    assert_eq!(rev.take_shot_requests().len(), 1);
    let names: Vec<_> = rev.trades().iter().map(|t| &t.screenshots[0]).collect();
    assert_ne!(names[0], names[1], "each trade its own file");
}

/// FNV-1a's published 64-bit test vectors: the name is saved, so it can never change.
#[test]
fn shot_names_hash_with_fnv1a_64() {
    let date = CivilDate::new(2026, 11, 5).unwrap();
    assert_eq!(shot_file_name(date, ""), "2026-11-05-cbf29ce484222325.svg");
    assert_eq!(shot_file_name(date, "a"), "2026-11-05-af63dc4c8601ec8c.svg");
    assert!(is_shot_file_name("2026-11-05-af63dc4c8601ec8c.svg"));
    for bad in [
        "",
        ".svg",
        "../x.svg",
        "..\\x.svg",
        "C:\\Users\\x.svg",
        "/etc/x.svg",
        "x.exe",
        "a.b.svg",
    ] {
        assert!(!is_shot_file_name(bad), "{bad:?}");
    }
}

/// Only a trade closing live asks: not one rebuilt from the file, not one seeded from a position
/// after a restart, and not any without shots on.
#[test]
fn rebuilt_seeded_and_unasked_trades_take_no_shots() {
    let mut first = Journal::new();
    first.set_shots(true);
    let saved = round_trip(&mut first, "paper");
    let mut rebuilt = Journal::from_records([Record::Trade(Box::new(saved[0].clone()))]);
    rebuilt.set_shots(true);
    assert!(rebuilt.take_shot_requests().is_empty());
    assert_eq!(
        rebuilt.trades()[0].screenshots,
        saved[0].screenshots,
        "kept"
    );
    let mut seeded = Journal::new();
    seeded.set_shots(true);
    seeded.on_event(&Event::Position(PositionView {
        account: acct(),
        contract: es(),
        qty: 1,
        avg_price: Some(600_000.0),
        unrealised: Money::ZERO,
        marked_at: None,
    }));
    let t = feed(&mut seeded, &[fill("s", 1, Side::Sell, 6001.0, 1, 0.0, 30)]);
    assert!(t[0].incomplete);
    assert!(t[0].screenshots.is_empty() && seeded.take_shot_requests().is_empty());
    let mut off = Journal::new();
    let t = round_trip(&mut off, "paper");
    assert!(t[0].screenshots.is_empty() && off.take_shot_requests().is_empty());
}

/// The optional field: older lines without it load, and a name round-trips as a plain string.
#[test]
fn screenshot_names_round_trip_and_older_lines_load() {
    let mut j = Journal::new();
    j.set_shots(true);
    let t = round_trip(&mut j, "paper").remove(0);
    let line = serde_json::to_string(&Record::Trade(Box::new(t.clone()))).unwrap();
    assert!(
        line.contains(&format!("\"screenshots\":[\"{}\"]", t.screenshots[0])),
        "{line}"
    );
    let back: Record = serde_json::from_str(&line).unwrap();
    assert_eq!(back, Record::Trade(Box::new(t.clone())));
    // Written by an older build: no field at all. An empty list isn't written either.
    let mut old = t.clone();
    old.screenshots.clear();
    let line = serde_json::to_string(&Record::Trade(Box::new(old.clone()))).unwrap();
    assert!(!line.contains("screenshots"), "{line}");
    assert_eq!(
        serde_json::from_str::<Record>(&line).unwrap(),
        Record::Trade(Box::new(old))
    );
    // The CSV export has no such column: it never names the file.
    let csv = csv(std::slice::from_ref(&t));
    assert!(
        !csv.contains(".svg") && !csv.contains("screenshots"),
        "{csv}"
    );
}

/// A reset that clears its journal clears it on its event, in order: the trades before it go,
/// a trade made right after it (in the same batch, or before the UI could act) stays.
#[test]
fn a_reset_clears_the_journal_in_order_with_the_events() {
    let shared = JournalShared::start(Journal::new(), None);
    let mut obs = shared.observer();
    let reset = |clear_journal| Event::AccountReset {
        account: acct(),
        balance: Money::whole(50_000),
        at: at(5),
        clear_journal,
    };
    obs.events(&[fill("a1", 1, Side::Buy, 6000.0, 1, 0.0, 0)]);
    obs.events(&[fill("a2", 2, Side::Sell, 6001.0, 1, 0.0, 2)]);
    // A reset that keeps the journal: nothing goes.
    obs.events(&[reset(false)]);
    shared.sync();
    assert_eq!(shared.snapshot().unwrap().trades.len(), 1);
    // One that clears it, then a round trip in the same batch.
    obs.events(&[
        reset(true),
        fill("b1", 3, Side::Buy, 6000.0, 1, 0.0, 6),
        fill("b2", 4, Side::Sell, 6002.0, 1, 0.0, 8),
    ]);
    shared.sync();
    let trades = shared.snapshot().unwrap().trades.clone();
    assert_eq!(trades.len(), 1, "only the trade after the reset");
    assert_eq!(trades[0].entry_time, at(6));
}

/// The journal thread asks only when the journal is saved; a flush reaches the drawing thread.
#[test]
fn only_a_saved_journal_asks_and_a_flush_waits_for_the_drawing() {
    let (tx, rx) = mpsc::channel();
    let memory = JournalShared::start_with_shots(Journal::new(), None, Some(tx));
    let mut obs = memory.observer();
    obs.events(&[fill("m1", 1, Side::Buy, 6000.0, 1, 0.0, 0)]);
    obs.events(&[fill("m2", 2, Side::Sell, 6001.0, 1, 0.0, 2)]);
    memory.sync();
    let snap = memory.snapshot().unwrap();
    assert_eq!(snap.trades.len(), 1);
    assert!(snap.trades[0].screenshots.is_empty());
    assert!(rx.try_recv().is_err(), "nothing asked for");

    let dir = lines_core::scratch::Scratch::new("journal-shots");
    let writer = JournalWriter::open(dir.join("t.jsonl"), "t", None).unwrap();
    let (tx, rx) = mpsc::channel();
    let saved = JournalShared::start_with_shots(Journal::new(), Some(writer), Some(tx));
    let drawer = thread::spawn(move || {
        let mut taken = Vec::new();
        while let Ok(m) = rx.recv() {
            match m {
                ShotMsg::Take(r) => taken.push(r.file),
                ShotMsg::Repair(_) => {}
                ShotMsg::Flush(done) => {
                    let _ = done.send(());
                }
            }
        }
        taken
    });
    let mut obs = saved.observer();
    obs.events(&[fill("s1", 1, Side::Buy, 6000.0, 1, 0.0, 0)]);
    obs.events(&[fill("s2", 2, Side::Sell, 6001.0, 1, 0.0, 2)]);
    assert!(saved.flush(Duration::from_secs(10)));
    let file = saved.snapshot().unwrap().trades[0].screenshots[0].clone();
    drop(obs);
    drop(saved);
    assert_eq!(drawer.join().unwrap(), [file], "asked once, by that name");
    // The saved line carries the name.
    let on_disk = std::fs::read_to_string(dir.join("t.jsonl")).unwrap();
    assert!(on_disk.contains("\"screenshots\":[\""), "{on_disk}");
}

/// The journal on its own thread: the trading thread only sends messages, the UI reads
/// snapshots. Market data moves MAE/MFE without publishing (open trades aren't shown); a
/// close publishes; an edit is applied there and published.
/// An option has no prints here: its excursions follow its quote's mid, with the multiplier.
#[test]
fn an_options_excursions_follow_its_quote() {
    let opt = ContractKey::option(
        catalog::SPY,
        lines_core::CivilDate::new(2026, 11, 9).unwrap(),
        Price(68_400),
        lines_core::Right::Call,
    );
    let f = |id: &str, side, price, s| {
        Event::Fill(FillEvent {
            account: acct(),
            order: OrderId(1),
            contract: opt,
            side,
            price: Price(price),
            qty: Qty(1),
            fee: Money::ZERO,
            realised: Money::ZERO,
            trade_id: id.into(),
            reconciled: false,
            at: at(s),
        })
    };
    let quote = |bid, ask| MarketInput::OptionQuote {
        bid: Some(Price(bid)),
        ask: Some(Price(ask)),
        last: None,
        time: at(1),
    };
    let mut j = Journal::new();
    j.on_event(&f("a", Side::Buy, 300, 0));
    j.on_market(opt, &[quote(248, 252), quote(350, 354)]);
    // A quote without both sides moves nothing.
    j.on_market(
        opt,
        &[MarketInput::OptionQuote {
            bid: None,
            ask: Some(Price(1)),
            last: None,
            time: at(2),
        }],
    );
    let t = j.on_event(&f("b", Side::Sell, 320, 3)).pop().unwrap();
    assert_eq!(t.realized, dollars(20.0), "0.20 × 100 shares");
    assert_eq!(t.mae, dollars(-50.0), "the mid at 2.50");
    assert_eq!(t.mfe, dollars(52.0), "the mid at 3.52");
}

#[test]
fn shared_journal_runs_on_its_own_thread() {
    let shared = JournalShared::start(Journal::new(), None);
    let mut obs = shared.observer();
    let r0 = shared.revision();
    obs.events(&[fill("o1", 1, Side::Buy, 6000.0, 1, 0.0, 0)]);
    shared.sync();
    assert!(shared.revision() > r0, "a fill publishes");
    assert_eq!(shared.snapshot().unwrap().open, 1);
    let r1 = shared.revision();
    obs.market(
        es(),
        &[MarketInput::Trade {
            time: at(1),
            price: px(5990.0),
            qty: Qty(1),
        }],
    );
    shared.sync();
    assert_eq!(
        shared.revision(),
        r1,
        "market data doesn't repaint the journal"
    );
    obs.events(&[fill("o2", 2, Side::Sell, 6001.0, 1, 0.0, 2)]);
    shared.sync();
    let snap = shared.snapshot().unwrap();
    assert_eq!((snap.trades.len(), snap.open), (1, 0));
    assert_eq!(snap.trades[0].mae, dollars(-500.0));
    shared.edit(Edit {
        id: snap.trades[0].id.clone(),
        tags: vec!["t".into()],
        note: String::new(),
        excursion: None,
    });
    shared.sync();
    assert_eq!(shared.snapshot().unwrap().trades[0].tags, ["t"]);
    assert!(!shared.persistent());
    assert!(shared.flush(std::time::Duration::from_secs(5)));
}

// ---- broker (venue) accounts: observed accounts like a DTC venue's, all synthetic ---------

fn venue_acct() -> AccountId {
    AccountId::new("venue-live-1")
}

/// An execution on a broker account (an order placed in the broker's own platform: no order of
/// Lines', no fee reported).
fn vfill(id: &str, side: Side, p: f64, q: i64, s: i64) -> Event {
    vfill_on(venue_acct(), id, side, p, q, s)
}

fn vfill_on(account: AccountId, id: &str, side: Side, p: f64, q: i64, s: i64) -> Event {
    Event::Fill(FillEvent {
        account,
        order: OrderId(0),
        contract: es(),
        side,
        price: px(p),
        qty: Qty(q),
        fee: Money::ZERO,
        realised: Money::ZERO,
        trade_id: id.into(),
        reconciled: false,
        at: at(s),
    })
}

fn print(p: f64, s: i64) -> MarketInput {
    MarketInput::Trade {
        time: at(s),
        price: px(p),
        qty: Qty(1),
    }
}

fn venue_journal(estimate_fees: bool) -> Journal {
    let mut j = Journal::new();
    j.add_venue([venue_acct()], estimate_fees);
    let mut table = FeeTable::default();
    table.fees.insert(catalog::ES.0, dollars(1.25));
    table.default_fee = dollars(0.50);
    j.set_fee_table(table);
    j
}

/// A broker that reports no fees: each fill gets the product fee table's, per contract,
/// marked as an estimate; a fee the broker did report is kept. The P&L stays exact.
#[test]
fn a_broker_without_fees_gets_the_fee_table_marked_estimated() {
    let mut j = venue_journal(true);
    let mut reported = vfill("x2", Side::Sell, 6002.00, 2, 60);
    if let Event::Fill(f) = &mut reported {
        f.fee = dollars(3.00);
    }
    let closed = feed(&mut j, &[vfill("x1", Side::Buy, 6000.00, 2, 0), reported]);
    let t = &closed[0];
    assert_eq!(t.realized, dollars(200.0));
    assert_eq!(t.fills[0].fee, dollars(2.50), "2 x the table's $1.25");
    assert!(t.fills[0].fee_estimated);
    assert_eq!(t.fills[1].fee, dollars(3.00), "the broker's own");
    assert!(!t.fills[1].fee_estimated);
    assert!(t.fees_estimated());
    assert_eq!(t.fees, dollars(5.50));
    // A broker that reports fees: a zero is its fee.
    let mut k = venue_journal(false);
    let closed = feed(
        &mut k,
        &[
            vfill("y1", Side::Buy, 6000.00, 1, 0),
            vfill("y2", Side::Sell, 6001.00, 1, 10),
        ],
    );
    assert_eq!(closed[0].fees, Money::ZERO);
    assert!(!closed[0].fees_estimated());
    // Paper: always its own fees.
    let paper = feed(
        &mut venue_journal(true),
        &[
            fill("p1", 1, Side::Buy, 6000.00, 1, 0.0, 0),
            fill("p2", 2, Side::Sell, 6001.00, 1, 0.0, 10),
        ],
    );
    assert!(!paper[0].fees_estimated());
}

/// A reversal splits the estimated fee like a reported one: nothing lost or invented.
#[test]
fn an_estimated_fee_splits_on_a_reversal() {
    let mut j = venue_journal(true);
    let closed = feed(
        &mut j,
        &[
            vfill("r1", Side::Buy, 6000.00, 1, 0),
            vfill("r2", Side::Sell, 6001.00, 3, 10),
        ],
    );
    let open: Vec<&Trade> = j.open_trades().collect();
    assert_eq!(closed[0].fees + open[0].fees, dollars(1.25 * 4.0));
    assert!(closed[0].fees_estimated() && open[0].fees_estimated());
}

/// After a restart the broker may report an execution a saved trade already holds: it is
/// never counted again. A new execution still counts, and a reconciled fill's id (made from
/// Lines' order ids, which restart) never blocks one.
#[test]
fn a_saved_broker_fill_reported_again_is_never_counted_twice() {
    let mut first = venue_journal(true);
    let old = feed(
        &mut first,
        &[
            vfill("dtc-1", Side::Buy, 6000.00, 1, 0),
            vfill("dtc-2", Side::Sell, 6001.00, 1, 10),
        ],
    );
    let mut recon = vfill("recon-5-1", Side::Buy, 6000.00, 1, 20);
    if let Event::Fill(f) = &mut recon {
        f.reconciled = true;
    }
    let old2 = feed(
        &mut first,
        &[recon.clone(), vfill("dtc-3", Side::Sell, 6000.50, 1, 30)],
    );
    let mut next = Journal::from_records([
        Record::Trade(Box::new(old[0].clone())),
        Record::Trade(Box::new(old2[0].clone())),
    ]);
    next.add_venue([venue_acct()], true);
    // The reconnect reports both executions again: nothing opens, nothing closes.
    let again = feed(
        &mut next,
        &[
            vfill("dtc-1", Side::Buy, 6000.00, 1, 0),
            vfill("dtc-2", Side::Sell, 6001.00, 1, 10),
        ],
    );
    assert!(again.is_empty());
    assert_eq!(next.open_trades().count(), 0);
    assert_eq!(next.trades().len(), 2);
    // A reconciled fill with an id an earlier run used is a new fill.
    feed(&mut next, &[recon]);
    assert_eq!(
        next.open_trades().count(),
        1,
        "the reconciled id never blocks"
    );
    let closed = feed(&mut next, &[vfill("dtc-9", Side::Sell, 6002.00, 1, 40)]);
    assert_eq!(closed.len(), 1);
    assert!(closed[0].fills[0].reconciled && closed[0].fills[0].fee_estimated);
    // The same id on another account is that account's own.
    let other = AccountId::new("venue-live-2");
    next.add_venue([other.clone()], true);
    feed(
        &mut next,
        &[vfill_on(other, "dtc-1", Side::Buy, 6000.00, 1, 50)],
    );
    assert_eq!(next.open_trades().count(), 1);
}

/// A broker account's MAE/MFE come from Lines' own prints of the contract, and only when they
/// cover the whole trade; otherwise the trade says so and its excursion is never shown.
#[test]
fn broker_trades_are_measured_only_where_lines_prints_cover_them() {
    // Covered: prints before, during and near the exit.
    let mut j = venue_journal(true);
    j.on_market(es(), &[print(6000.00, -5)]);
    feed(&mut j, &[vfill("c1", Side::Buy, 6000.00, 1, 0)]);
    j.on_market(es(), &[print(5998.00, 30), print(6003.00, 90)]);
    let c = feed(&mut j, &[vfill("c2", Side::Sell, 6002.00, 1, 120)]);
    assert!(!c[0].excursion_unavailable);
    assert_eq!(c[0].excursion(), Some((dollars(-100.0), dollars(150.0))));
    // No pane on the contract: no prints at all.
    let mut j = venue_journal(true);
    feed(&mut j, &[vfill("n1", Side::Buy, 6000.00, 1, 0)]);
    let n = feed(&mut j, &[vfill("n2", Side::Sell, 6002.00, 1, 120)]);
    assert!(n[0].excursion_unavailable);
    assert_eq!(n[0].excursion(), None);
    let product = catalog::get(catalog::ES).unwrap();
    assert_eq!(n[0].excursion_ticks(&product), None);
    // A stale feed: a gap longer than FEED_GAP_MS in the middle.
    let mut j = venue_journal(true);
    j.on_market(es(), &[print(6000.00, 0)]);
    feed(&mut j, &[vfill("g1", Side::Buy, 6000.00, 1, 1)]);
    j.on_market(es(), &[print(6001.00, 60)]);
    j.on_market(es(), &[print(6001.00, 60 + FEED_GAP_MS / 1000 + 1)]);
    let g = feed(&mut j, &[vfill("g2", Side::Sell, 6002.00, 1, 300)]);
    assert!(g[0].excursion_unavailable, "a gap mid-trade");
    // Prints stop before the exit.
    let mut j = venue_journal(true);
    j.on_market(es(), &[print(6000.00, 0)]);
    feed(&mut j, &[vfill("s1", Side::Buy, 6000.00, 1, 1)]);
    let s = feed(&mut j, &[vfill("s2", Side::Sell, 6002.00, 1, 600)]);
    assert!(s[0].excursion_unavailable, "nothing near the exit");
    // The first print long after the entry.
    let mut j = venue_journal(true);
    feed(&mut j, &[vfill("l1", Side::Buy, 6000.00, 1, 0)]);
    j.on_market(es(), &[print(6001.00, 600)]);
    let l = feed(&mut j, &[vfill("l2", Side::Sell, 6002.00, 1, 601)]);
    assert!(l[0].excursion_unavailable, "nothing near the entry");
    // Paper keeps its MAE/MFE from fills and batches, with no timed print needed.
    let mut j = venue_journal(true);
    let p = feed(
        &mut j,
        &[
            fill("p1", 1, Side::Buy, 6000.00, 1, 0.0, 0),
            fill("p2", 2, Side::Sell, 6001.00, 1, 0.0, 600),
        ],
    );
    assert!(!p[0].excursion_unavailable);
}

/// Unmarked accounts (a replay's prices aren't the venue's market) and seeded trades (entry
/// unknown) are unavailable too.
#[test]
fn unmarked_and_seeded_trades_are_unavailable() {
    let mut j = venue_journal(true);
    j.add_unmarked([venue_acct()]);
    j.on_market(es(), &[print(6000.00, 0)]);
    feed(&mut j, &[vfill("u1", Side::Buy, 6000.00, 1, 1)]);
    j.on_market(es(), &[print(5990.00, 30)]);
    let u = feed(&mut j, &[vfill("u2", Side::Sell, 6001.00, 1, 60)]);
    assert!(u[0].excursion_unavailable);
    let mut j = Journal::new();
    j.on_event(&Event::Position(PositionView {
        account: acct(),
        contract: es(),
        qty: 1,
        avg_price: Some(600_000.0),
        unrealised: Money::ZERO,
        marked_at: None,
    }));
    let s = feed(&mut j, &[fill("s1", 1, Side::Sell, 6001.00, 1, 0.0, 10)]);
    assert!(s[0].incomplete && s[0].excursion_unavailable);
}

/// The CSV says where the fees came from and whether MAE/MFE were measured (blank when not).
#[test]
fn csv_names_fee_source_and_unmeasured_excursions() {
    let mut j = venue_journal(true);
    let t = feed(
        &mut j,
        &[
            vfill("e1", Side::Buy, 6000.00, 1, 0),
            vfill("e2", Side::Sell, 6001.00, 1, 10),
        ],
    );
    let out = csv(&t);
    let header = out.lines().next().unwrap();
    assert!(header.ends_with(",note,fee_source,excursion"), "{header}");
    let row = out.lines().nth(1).unwrap();
    assert!(row.ends_with(",table,unavailable"), "{row}");
    // mae, mfe, mae_ticks and mfe_ticks are empty; the fill count follows them.
    assert!(row.contains(",,,,2,"), "{row}");
    let mut paper = closed(10.0, 5, &[]);
    paper.fills.push(TradeFill {
        trade_id: "p".into(),
        at: at(0),
        side: Side::Buy,
        price: px(6000.0),
        qty: Qty(1),
        fee: Money::ZERO,
        source: None,
        reconciled: false,
        fee_estimated: false,
    });
    let row = csv(&[paper]);
    assert!(row.trim_end().ends_with(",broker,measured"), "{row}");
}

/// Consistency progress: the best trade date's share of the total, exact against the rule,
/// and the profit still needed for the best day to fit.
#[test]
fn consistency_progress_from_closed_trades() {
    use super::stats::consistency;
    let day = |d: u8, net: f64, exit: i64| {
        let mut t = closed(net, exit, &[]);
        t.trade_date = lines_core::CivilDate::new(2026, 11, d).unwrap();
        t
    };
    // Days: +600, +300 (two trades), -100: total 800, best 600 = 75 %.
    let trades = [
        day(3, 600.0, 1),
        day(4, 200.0, 2),
        day(4, 100.0, 3),
        day(5, -100.0, 4),
    ];
    let c = consistency(&trades, 30).unwrap();
    assert_eq!(c.total, dollars(800.0));
    assert_eq!(c.best_day.unwrap().1, dollars(600.0));
    assert_eq!(c.best_pct, Some(75));
    assert_eq!(c.ok, Some(false));
    // 600 <= 30 % of T when T >= 2000: 1200 more.
    assert_eq!(c.needed, dollars(1200.0));
    // Exactly at the rule is within it; a cent over is not, though both read 70 %.
    let at_rule = [day(3, 300.0, 1), day(4, 700.0, 2)];
    let c = consistency(&at_rule, 70).unwrap();
    assert_eq!((c.ok, c.needed), (Some(true), Money::ZERO));
    let over = [day(3, 300.0, 1), day(4, 700.01, 2)];
    let c = consistency(&over, 70).unwrap();
    assert_eq!(c.best_pct, Some(70));
    assert_eq!(c.ok, Some(false));
    assert!(c.needed > Money::ZERO);
    // No profit yet: nothing to measure; a losing account still needs its best day covered.
    let losing = [day(3, 100.0, 1), day(4, -300.0, 2)];
    let c = consistency(&losing, 50).unwrap();
    assert_eq!((c.best_pct, c.ok), (None, None));
    assert_eq!(c.needed, dollars(400.0), "200 needed in all, from -200");
    assert_eq!(consistency(&losing, 0), None);
    assert_eq!(consistency(&[], 30).unwrap().best_day, None);
}

/// A reversal's fee goes pro rata with the remainder on the closing side: 100 units over 3,
/// closing 1, is 34 closing and 66 opening.
#[test]
fn a_reversal_fee_remainder_goes_to_the_closing_side() {
    let mut j = Journal::new();
    let mut rev = fill("v2", 2, Side::Sell, 6001.00, 3, 0.0, 10);
    if let Event::Fill(f) = &mut rev {
        f.fee = Money(100);
    }
    let closed = feed(&mut j, &[fill("v1", 1, Side::Buy, 6000.00, 1, 0.0, 0), rev]);
    let open: Vec<&Trade> = j.open_trades().collect();
    assert_eq!(closed[0].fees, Money(34));
    assert_eq!(open[0].fees, Money(66));
}

/// A late backfill of prints stamped long before a broker trade's entry neither covers it nor
/// moves its excursion.
#[test]
fn a_backfill_from_before_the_entry_covers_nothing() {
    let mut j = venue_journal(true);
    feed(&mut j, &[vfill("b1", Side::Buy, 6000.00, 1, 600)]);
    j.on_market(es(), &[print(5990.00, 0)]);
    let b = feed(&mut j, &[vfill("b2", Side::Sell, 6001.00, 1, 690)]);
    assert!(b[0].excursion_unavailable);
    assert_eq!(b[0].low, px(6000.00), "the old price never counts");
    // An option-less empty quote is no print either (the early path, nothing open).
    let mut k = venue_journal(true);
    k.on_market(
        es(),
        &[MarketInput::OptionQuote {
            bid: None,
            ask: None,
            last: None,
            time: at(0),
        }],
    );
    feed(&mut k, &[vfill("q1", Side::Buy, 6000.00, 1, 1)]);
    let q = feed(&mut k, &[vfill("q2", Side::Sell, 6001.00, 1, 20)]);
    assert!(q[0].excursion_unavailable);
}

// ---- measuring later, and older builds' broker trades -------------------------------------

/// A broker trade closed with no print of Lines' (unavailable): `measure` from prints that
/// cover it gives its exact MAE/MFE; prints that don't (none near the entry, a gap, none near
/// the exit) give nothing, and a seeded trade is never measured.
#[test]
fn measure_follows_the_coverage_rule_and_the_trades_lots() {
    let mut j = venue_journal(true);
    feed(&mut j, &[vfill("m1", Side::Buy, 6000.00, 2, 0)]);
    feed(&mut j, &[vfill("m2", Side::Sell, 6001.00, 1, 300)]);
    let t = feed(&mut j, &[vfill("m3", Side::Sell, 6004.00, 1, 600)]).remove(0);
    assert!(t.excursion_unavailable && t.cover_checked);
    let p = |pt: f64, s: i64| (at(s), px(pt));
    // Every minute: down 3 points at 2 min (2 contracts), up 5 at 8 min (1 left after 5 min).
    let prints: Vec<_> = (0..=10)
        .map(|m| {
            let price = match m {
                2 => 5997.00,
                8 => 6005.00,
                _ => 6000.50,
            };
            p(price, m * 60)
        })
        .collect();
    let x = measure(&t, &prints).expect("covered");
    // −3 points × 2 × $50 = −$300; at 8 min: realized +$50 (1 at +1.00) + 1 × 5 points = +$300.
    assert_eq!(x.mae, dollars(-300.0));
    assert_eq!(x.mfe, dollars(300.0));
    assert_eq!((x.low, x.high), (px(5997.00), px(6005.00)));
    // A print before the entry, within the gap, starts the cover; a later one is ignored.
    let mut late = prints.clone();
    late.retain(|(at_, _)| *at_ != at(0));
    late.push(p(6000.0, -60));
    late.push(p(1.0, -60 - FEED_GAP_MS / 1000 - 1));
    assert_eq!(measure(&t, &late).map(|x| x.low), Some(px(5997.00)));
    // Not covered.
    let gap: Vec<_> = prints
        .iter()
        .copied()
        .filter(|(a, _)| a.0 < at(120).0 || a.0 > at(400).0)
        .collect();
    assert_eq!(measure(&t, &gap), None, "a gap mid-trade");
    let early: Vec<_> = prints
        .iter()
        .copied()
        .filter(|(a, _)| a.0 <= at(300).0)
        .collect();
    assert_eq!(measure(&t, &early), None, "nothing near the exit");
    assert_eq!(measure(&t, &[]), None);
    let mut seeded = t.clone();
    seeded.incomplete = true;
    assert_eq!(measure(&seeded, &prints), None);
}

/// Saved by an older build (no coverage check): a broker account's trade loads as it was and
/// turns unavailable once the account is known as a broker's; a measurement fills it in as an
/// edit (tags and note kept) that survives a reload, and a later tag edit keeps it.
#[test]
fn older_broker_trades_turn_unavailable_and_a_measurement_is_saved() {
    let mut j = venue_journal(true);
    j.on_market(es(), &[print(6000.00, 0)]);
    feed(&mut j, &[vfill("o1", Side::Buy, 6000.00, 1, 1)]);
    let mut old = feed(&mut j, &[vfill("o2", Side::Sell, 6002.00, 1, 60)]).remove(0);
    // As an older build saved it: no check, MAE/MFE from its fills.
    old.cover_checked = false;
    old.excursion_unavailable = false;
    old.tags = vec!["RI bounce".into()];
    let paper = closed(10.0, 5, &[]);
    let mut k = Journal::from_records([
        Record::Trade(Box::new(old.clone())),
        Record::Trade(Box::new(paper)),
    ]);
    assert!(
        !k.trades()[0].excursion_unavailable,
        "not known as a broker's yet"
    );
    let changed = k.add_venue([venue_acct()], true);
    assert_eq!(changed, [old.id.clone()]);
    assert!(k.trades()[0].excursion_unavailable);
    assert!(!k.trades()[1].excursion_unavailable, "paper untouched");
    assert!(k.add_venue([venue_acct()], true).is_empty(), "once");
    assert_eq!(k.unmeasured().count(), 1);
    // Measured: the edit keeps the tags.
    let x = Excursion {
        mae: dollars(-25.0),
        mfe: dollars(150.0),
        low: px(5999.50),
        high: px(6003.00),
    };
    let e = k.measured(&old.id, x).expect("applied");
    assert_eq!(e.tags, ["RI bounce"]);
    assert_eq!(
        k.trades()[0].excursion(),
        Some((dollars(-25.0), dollars(150.0)))
    );
    assert_eq!(k.measured(&old.id, x), None, "only once");
    // A reload: the trade line, then the measurement and a later tag edit.
    let tag = Edit {
        id: old.id.clone(),
        tags: vec!["A+".into()],
        note: String::new(),
        excursion: None,
    };
    let line = serde_json::to_string(&Record::Edit(e)).unwrap();
    assert!(line.contains("\"excursion\""), "{line}");
    let mut r = Journal::from_records([
        Record::Trade(Box::new(old.clone())),
        serde_json::from_str(&line).unwrap(),
        Record::Edit(tag),
    ]);
    assert!(r.add_venue([venue_acct()], true).is_empty());
    let t = &r.trades()[0];
    assert_eq!(t.excursion(), Some((dollars(-25.0), dollars(150.0))));
    assert_eq!(t.tags, ["A+"]);
    // An edit line without the field (an older build's) reads as before.
    let plain = format!(r#"{{"edit":{{"id":"{}","tags":["x"],"note":""}}}}"#, old.id);
    assert!(matches!(
        serde_json::from_str::<Record>(&plain).unwrap(),
        Record::Edit(Edit {
            excursion: None,
            ..
        })
    ));
}

/// The journal thread asks the snapshot thread to mend the newest trades at start, and a trade
/// on request (a "view"); an unmeasured broker trade that closes is sent to be measured; the
/// answer is applied and saved.
#[test]
fn repairs_are_asked_for_and_answers_saved() {
    let dir = lines_core::scratch::Scratch::new("journal-repairs");
    let path = dir.join("t.jsonl");
    let mut seed = venue_journal(true);
    seed.set_shots(true);
    feed(&mut seed, &[vfill("r1", Side::Buy, 6000.00, 1, 0)]);
    let t = feed(&mut seed, &[vfill("r2", Side::Sell, 6001.00, 1, 600)]).remove(0);
    assert!(t.excursion_unavailable && !t.screenshots.is_empty());
    let writer = JournalWriter::open(path.clone(), "t", None).unwrap();
    let (tx, rx) = mpsc::channel();
    let shared = JournalShared::start_with_shots(
        Journal::from_records([Record::Trade(Box::new(t.clone()))]),
        Some(writer),
        Some(tx),
    );
    let first = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let ShotMsg::Repair(r) = first else {
        panic!("a repair at start")
    };
    assert!(!r.asked);
    let shot = r.shot.as_ref().expect("its snapshot");
    assert!(
        shot.trade.id.is_empty() && shot.trade.account.0.is_empty(),
        "blanked"
    );
    assert!(
        r.measure.is_none(),
        "not measured before the account is known as a broker's"
    );
    // The venue's watcher lists the account (after saying whether it is unmarked): measure.
    shared.add_venue(vec![venue_acct()], true);
    let ShotMsg::Repair(r) = rx.recv_timeout(Duration::from_secs(5)).unwrap() else {
        panic!("a measurement")
    };
    assert!(r.shot.is_none());
    let m = r.measure.as_ref().expect("its measurement");
    assert_eq!(m.id, t.id);
    // The answer comes back and is saved.
    r.sink.send(
        t.id.clone(),
        Excursion {
            mae: dollars(-10.0),
            mfe: dollars(60.0),
            low: px(5999.80),
            high: px(6001.20),
        },
    );
    shared.sync();
    let snap = shared.snapshot().unwrap();
    assert!(!snap.trades[0].excursion_unavailable);
    // Asked for by a "view".
    shared.repair(t.id.clone());
    shared.sync();
    let ShotMsg::Repair(r) = rx.recv_timeout(Duration::from_secs(5)).unwrap() else {
        panic!("a repair on request")
    };
    assert!(
        r.asked && r.measure.is_none(),
        "measured already: the snapshot only"
    );
    // The snapshot thread answers the quit flush.
    thread::spawn(move || {
        for m in rx {
            if let ShotMsg::Flush(done) = m {
                let _ = done.send(());
            }
        }
    });
    assert!(shared.flush(Duration::from_secs(5)));
    drop(shared);
    // The file holds the measurement as an edit record (the trade line came from elsewhere).
    let loaded = store::read(&path).unwrap().unwrap();
    assert!(loaded.records.iter().any(|r| matches!(
        r,
        Record::Edit(Edit {
            excursion: Some(_),
            ..
        })
    )));
}
