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
    assert_eq!(t.mae_ticks(25), Some(20.0));
    assert_eq!(t.mfe_ticks(25), Some(40.0));
    assert_eq!(t.realized, dollars(350.0));
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
        }),
    ]);
    let t = j.trade("t5").unwrap();
    assert_eq!(t.tags, ["breakout", "A+"]);
    assert_eq!(t.note, "two");
    assert!(
        j.edit(Edit {
            id: "nope".into(),
            tags: vec![],
            note: String::new()
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
    let dir = std::env::temp_dir().join(format!("lines-journal-v1-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
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
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn writer_appends_in_the_background_and_cuts_a_torn_tail() {
    let dir = std::env::temp_dir().join(format!("lines-journal-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("journal").join("mock.jsonl");
    {
        let w = JournalWriter::open(path.clone(), "mock", None).unwrap();
        w.append(Record::Trade(Box::new(closed(10.0, 5, &[]))));
        w.append(Record::Edit(Edit {
            id: "t5".into(),
            tags: vec!["first".into()],
            note: String::new(),
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
    let _ = std::fs::remove_dir_all(&dir);
}

/// The journal on its own thread: the trading thread only sends messages, the UI reads
/// snapshots. Market data moves MAE/MFE without publishing (open trades aren't shown); a
/// close publishes; an edit is applied there and published.
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
    });
    shared.sync();
    assert_eq!(shared.snapshot().unwrap().trades[0].tags, ["t"]);
    assert!(!shared.persistent());
    assert!(shared.flush(std::time::Duration::from_secs(5)));
}
