//! The importer end to end on synthetic DBN files built here (never real data): contracts from
//! per-day definitions, exact prices, aggressors, atomic book batches, determinism.

use std::ffi::c_char;
use std::path::{Path, PathBuf};

use dbn::encode::{DbnEncoder, EncodeRecord};
use dbn::record::str_to_c_chars;
use dbn::{
    BidAskPair, FlagSet, InstrumentDefMsg, Mbp1Msg, Mbp10Msg, Metadata, RecordHeader, SType,
    Schema, TradeMsg, UNDEF_PRICE, rtype,
};
use lines_core::{ContractKey, Month, Price, Qty, Side};
use lines_databento::import::{ImportOptions, ImportReport, import};
use lines_feed::catalog;
use lines_feed::native::{self, Aggressor, NativeEvent};
use lines_feed::recording::Reader;

const DAY1: u64 = 1_793_577_600_000_000_000; // 2026-11-02T00:00:00Z
const DAY: u64 = 86_400_000_000_000;
const H14: u64 = 14 * 3_600_000_000_000; // 14:00 UTC
const PX: i64 = 6_000_000_000_000; // 6000.00
const TICK: i64 = 250_000_000;

fn es_z6() -> ContractKey {
    ContractKey::monthly(catalog::ES, 2026, Month::new(12).unwrap())
}

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("lines-databento-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn def(
    id: u32,
    ts: u64,
    asset: &str,
    raw: &str,
    year: u16,
    month: u8,
    class: u8,
) -> InstrumentDefMsg {
    InstrumentDefMsg {
        hd: RecordHeader::new::<InstrumentDefMsg>(rtype::INSTRUMENT_DEF, 1, id, ts),
        ts_recv: ts,
        min_price_increment: TICK,
        display_factor: 1_000_000_000,
        // Index points: ES $50, NQ $20 (in 1e-9), as the gateway sends.
        unit_of_measure_qty: if asset == "NQ" { 20 } else { 50 } * 1_000_000_000,
        unit_of_measure: str_to_c_chars("IPNT").unwrap(),
        currency: str_to_c_chars("USD").unwrap(),
        exchange: str_to_c_chars("XCME").unwrap(),
        maturity_year: year,
        maturity_month: month,
        raw_symbol: str_to_c_chars(raw).unwrap(),
        asset: str_to_c_chars(asset).unwrap(),
        instrument_class: class as c_char,
        ..Default::default()
    }
}

fn es_def(id: u32, ts: u64) -> InstrumentDefMsg {
    def(id, ts, "ES", "ESZ6", 2026, 12, b'F')
}

fn trade(id: u32, ts: u64, seq: u32, price: i64, size: u32, side: u8) -> TradeMsg {
    TradeMsg {
        hd: RecordHeader::new::<TradeMsg>(rtype::MBP_0, 1, id, ts - 1_000),
        price,
        size,
        action: b'T' as c_char,
        side: side as c_char,
        flags: FlagSet::empty().set_last(),
        depth: 0,
        ts_recv: ts,
        ts_in_delta: 0,
        sequence: seq,
    }
}

fn levels(bids: &[(i64, u32)], asks: &[(i64, u32)]) -> [BidAskPair; 10] {
    std::array::from_fn(|i| {
        let (bp, bs) = bids.get(i).copied().unwrap_or((UNDEF_PRICE, 0));
        let (ap, as_) = asks.get(i).copied().unwrap_or((UNDEF_PRICE, 0));
        BidAskPair {
            bid_px: bp,
            ask_px: ap,
            bid_sz: bs,
            ask_sz: as_,
            bid_ct: bs.min(3),
            ask_ct: as_.min(3),
        }
    })
}

#[allow(clippy::too_many_arguments)]
fn mbp(
    id: u32,
    ts: u64,
    seq: u32,
    action: u8,
    side: u8,
    last: bool,
    lv: [BidAskPair; 10],
    trade_px: i64,
) -> Mbp10Msg {
    let mut flags = FlagSet::empty();
    if last {
        flags.set_last();
    }
    Mbp10Msg {
        hd: RecordHeader::new::<Mbp10Msg>(rtype::MBP_10, 1, id, ts - 1_000),
        price: trade_px,
        size: 1,
        action: action as c_char,
        side: side as c_char,
        flags,
        depth: 0,
        ts_recv: ts,
        ts_in_delta: 0,
        sequence: seq,
        levels: lv,
    }
}

fn meta(schema: Schema) -> Metadata {
    Metadata::builder()
        .dataset("GLBX.MDP3")
        .schema(Some(schema))
        .start(DAY1)
        .stype_in(Some(SType::RawSymbol))
        .stype_out(SType::InstrumentId)
        .build()
}

fn write<R: dbn::encode::DbnEncodable>(path: &Path, schema: Schema, recs: &[R]) {
    let f = std::fs::File::create(path).unwrap();
    if path.extension().is_some_and(|e| e == "zst") {
        let mut e = DbnEncoder::with_zstd(f, &meta(schema)).unwrap();
        for r in recs {
            e.encode_record(r).unwrap();
        }
    } else {
        let mut e = DbnEncoder::new(f, &meta(schema)).unwrap();
        for r in recs {
            e.encode_record(r).unwrap();
        }
    }
}

fn read(path: &Path) -> (lines_feed::recording::Header, Vec<Vec<NativeEvent>>) {
    let mut r = Reader::open(path).unwrap();
    let header = r.header.clone();
    let mut frames = Vec::new();
    while let Some(rec) = r.next_record().unwrap() {
        let mut ev = Vec::new();
        native::decode(&rec.frame.data, &mut ev).unwrap();
        frames.push(ev);
    }
    (header, frames)
}

/// A day of ES (id 42), NQ (id 43), a spread (id 44) and an id with no definition (99).
fn fixture(d: &Path, zst: bool) -> Vec<PathBuf> {
    let ext = if zst { "dbn.zst" } else { "dbn" };
    let defs = d.join(format!("defs.{ext}"));
    let trades = d.join(format!("trades.{ext}"));
    let book = d.join(format!("mbp10.{ext}"));
    let t0 = DAY1 + H14;
    write(
        &defs,
        Schema::Definition,
        &[
            es_def(42, DAY1 + 1),
            def(43, DAY1 + 1, "NQ", "NQZ6", 2026, 12, b'F'),
            def(44, DAY1 + 1, "ES", "ESZ6-ESH7", 2026, 12, b'S'),
        ],
    );
    write(
        &trades,
        Schema::Trades,
        &[
            trade(42, t0 + 10, 1, PX, 2, b'B'),
            trade(43, t0 + 11, 2, 21_000_000_000_000, 1, b'A'),
            trade(42, t0 + 20, 3, PX + 100_000_000, 1, b'A'), // 6000.10: off tick
            trade(44, t0 + 21, 4, PX, 1, b'B'),               // spread
            trade(99, t0 + 22, 5, PX, 1, b'B'),               // no definition
            trade(42, t0 + 30, 6, PX - TICK, 3, b'A'),
            trade(42, t0 + 40, 9, PX, 1, b'N'),
        ],
    );
    write(
        &book,
        Schema::Mbp10,
        &[
            // Event 1 (two records, consistent only at the last).
            mbp(
                42,
                t0 + 5,
                1,
                b'A',
                b'B',
                false,
                levels(&[(PX, 5)], &[]),
                UNDEF_PRICE,
            ),
            mbp(
                42,
                t0 + 5,
                1,
                b'A',
                b'A',
                true,
                levels(&[(PX, 5)], &[(PX + TICK, 4)]),
                UNDEF_PRICE,
            ),
            // Same key as the trade at t0+10/seq 1? No: seq 2 here, after it.
            mbp(
                42,
                t0 + 10,
                2,
                b'C',
                b'B',
                true,
                levels(&[(PX, 3)], &[(PX + TICK, 4)]),
                UNDEF_PRICE,
            ),
            // Unchanged snapshot: no batch.
            mbp(
                42,
                t0 + 25,
                7,
                b'M',
                b'A',
                true,
                levels(&[(PX, 3)], &[(PX + TICK, 4)]),
                UNDEF_PRICE,
            ),
            // The bid at 6000.00 leaves; a new one at 5999.75.
            mbp(
                42,
                t0 + 35,
                8,
                b'C',
                b'B',
                true,
                levels(&[(PX - TICK, 6)], &[(PX + TICK, 4)]),
                UNDEF_PRICE,
            ),
            // NQ book: other instrument.
            mbp(
                43,
                t0 + 36,
                9,
                b'A',
                b'B',
                true,
                levels(&[(21_000_000_000_000, 1)], &[]),
                UNDEF_PRICE,
            ),
        ],
    );
    vec![book, trades, defs]
}

fn run(inputs: Vec<PathBuf>, out: &Path) -> anyhow::Result<ImportReport> {
    import(&ImportOptions {
        inputs,
        contract: es_z6(),
        out: out.to_path_buf(),
    })
}

#[test]
fn imports_one_contract_exactly() {
    let d = dir("full");
    let out = d.join("es.lfr");
    let r = run(fixture(&d, false), &out).unwrap();
    let (h, frames) = read(&out);
    assert_eq!(
        (h.provider.as_str(), h.protocol.as_str(), h.product.as_str()),
        ("lines", "lines-events-1", "ES")
    );
    assert_eq!(h.contract, Some(es_z6()));

    let ms = |ns: u64| ((ns - 1_000) / 1_000) as i64; // ts_event in µs
    let t0 = DAY1 + H14;
    let expect: Vec<Vec<NativeEvent>> = vec![
        // Book event 1: clear + both sides.
        vec![
            NativeEvent::BookClear {
                time_us: ms(t0 + 5),
            },
            NativeEvent::BookLevel {
                time_us: ms(t0 + 5),
                side: Side::Bid,
                price: Price(600000),
                qty: Qty(5),
                orders: 3,
            },
            NativeEvent::BookLevel {
                time_us: ms(t0 + 5),
                side: Side::Ask,
                price: Price(600025),
                qty: Qty(4),
                orders: 3,
            },
        ],
        // The trade (seq 1) before the book update at t0+10 (seq 2).
        vec![NativeEvent::Trade {
            time_us: ms(t0 + 10),
            price: Price(600000),
            qty: Qty(2),
            aggressor: Aggressor::Buy,
        }],
        vec![NativeEvent::BookLevel {
            time_us: ms(t0 + 10),
            side: Side::Bid,
            price: Price(600000),
            qty: Qty(3),
            orders: 3,
        }],
        vec![NativeEvent::Trade {
            time_us: ms(t0 + 30),
            price: Price(599975),
            qty: Qty(3),
            aggressor: Aggressor::Sell,
        }],
        vec![
            NativeEvent::BookLevel {
                time_us: ms(t0 + 35),
                side: Side::Bid,
                price: Price(600000),
                qty: Qty(0),
                orders: 0,
            },
            NativeEvent::BookLevel {
                time_us: ms(t0 + 35),
                side: Side::Bid,
                price: Price(599975),
                qty: Qty(6),
                orders: 3,
            },
        ],
        vec![NativeEvent::Trade {
            time_us: ms(t0 + 40),
            price: Price(600000),
            qty: Qty(1),
            aggressor: Aggressor::Unknown,
        }],
    ];
    assert_eq!(frames, expect);
    assert_eq!(r.trades_written, 3);
    assert_eq!(
        r.trades_rejected, 1,
        "6000.10 is off the tick: counted, not rounded"
    );
    assert_eq!(r.unmapped, 1);
    assert_eq!(r.other_instruments, 3, "NQ trade, spread trade, NQ book");
    assert_eq!(r.book.batches, 3);
    assert_eq!(r.frames_written, 6);
    assert!(!r.trades_from_mbp10);
    assert_eq!(r.definitions, 3);
}

#[test]
fn output_is_byte_identical_whatever_the_argument_order_and_compression() {
    let d = dir("det");
    let inputs = fixture(&d, false);
    run(inputs.clone(), &d.join("a.lfr")).unwrap();
    let mut rev = inputs.clone();
    rev.reverse();
    run(rev, &d.join("b.lfr")).unwrap();
    let a = std::fs::read(d.join("a.lfr")).unwrap();
    assert_eq!(a, std::fs::read(d.join("b.lfr")).unwrap());

    let dz = dir("det-zst");
    run(fixture(&dz, true), &dz.join("c.lfr")).unwrap();
    assert_eq!(
        a,
        std::fs::read(dz.join("c.lfr")).unwrap(),
        "zstd input: same output"
    );
    assert!(!d.join("a.lfr.part").exists());
}

#[test]
fn instrument_ids_map_per_day() {
    let d = dir("days");
    let day2 = DAY1 + DAY;
    let defs = d.join("defs.dbn");
    // Id 42 is ES on day 1 and NQ on day 2; day 3 has no definitions.
    write(
        &defs,
        Schema::Definition,
        &[
            es_def(42, DAY1 + 1),
            def(42, day2 + 1, "NQ", "NQZ6", 2026, 12, b'F'),
        ],
    );
    let trades = d.join("trades.dbn");
    write(
        &trades,
        Schema::Trades,
        &[
            trade(42, DAY1 + H14, 1, PX, 1, b'B'),
            trade(42, day2 + H14, 2, PX, 1, b'B'),
            trade(42, day2 + DAY + H14, 3, PX, 1, b'B'),
        ],
    );
    let r = run(vec![defs, trades], &d.join("es.lfr")).unwrap();
    assert_eq!(
        (r.trades_written, r.other_instruments, r.unmapped),
        (1, 1, 1)
    );
}

#[test]
fn a_definition_that_disagrees_with_the_catalog_stops_the_import() {
    let d = dir("bad");
    let defs = d.join("defs.dbn");
    let mut bad = es_def(42, DAY1 + 1);
    bad.min_price_increment = 100_000_000; // 0.10, ES ticks 0.25
    write(&defs, Schema::Definition, &[bad]);
    let trades = d.join("trades.dbn");
    write(
        &trades,
        Schema::Trades,
        &[trade(42, DAY1 + H14, 1, PX, 1, b'B')],
    );
    let e = run(vec![defs, trades], &d.join("es.lfr")).unwrap_err();
    assert!(e.to_string().contains("BadMetadata"), "{e}");
    assert!(!d.join("es.lfr").exists());

    let mut wrong_symbol = es_def(42, DAY1 + 1);
    wrong_symbol.raw_symbol = str_to_c_chars("ESH7").unwrap();
    write(&d.join("defs.dbn"), Schema::Definition, &[wrong_symbol]);
    let e = run(
        vec![d.join("defs.dbn"), d.join("trades.dbn")],
        &d.join("es.lfr"),
    )
    .unwrap_err();
    assert!(e.to_string().contains("no definition for ESZ6"), "{e}");
}

#[test]
fn without_definitions_nothing_is_guessed() {
    let d = dir("nodefs");
    let trades = d.join("trades.dbn");
    write(
        &trades,
        Schema::Trades,
        &[trade(42, DAY1 + H14, 1, PX, 1, b'B')],
    );
    let e = run(vec![trades], &d.join("es.lfr")).unwrap_err();
    assert!(e.to_string().contains("no definition"), "{e}");
}

#[test]
fn trades_come_from_mbp10_when_no_trades_file_is_given() {
    let d = dir("mbp-trades");
    let defs = d.join("defs.dbn");
    write(&defs, Schema::Definition, &[es_def(42, DAY1 + 1)]);
    let book = d.join("mbp10.dbn");
    let t0 = DAY1 + H14;
    let lv = levels(&[(PX, 5)], &[(PX + TICK, 4)]);
    write(
        &book,
        Schema::Mbp10,
        &[
            mbp(42, t0 + 5, 1, b'A', b'B', true, lv.clone(), UNDEF_PRICE),
            // A trade record (buy aggressor at the ask) then the book after it.
            mbp(42, t0 + 9, 2, b'T', b'B', false, lv.clone(), PX + TICK),
            mbp(
                42,
                t0 + 9,
                2,
                b'C',
                b'A',
                true,
                levels(&[(PX, 5)], &[(PX + TICK, 3)]),
                UNDEF_PRICE,
            ),
        ],
    );
    let out = d.join("es.lfr");
    let r = run(vec![defs, book], &out).unwrap();
    assert!(r.trades_from_mbp10);
    let (_, frames) = read(&out);
    assert_eq!(frames.len(), 3);
    assert!(matches!(
        frames[1][..],
        [NativeEvent::Trade {
            price: Price(600025),
            aggressor: Aggressor::Buy,
            ..
        }]
    ));
    assert!(matches!(
        frames[2][..],
        [NativeEvent::BookLevel {
            side: Side::Ask,
            price: Price(600025),
            qty: Qty(3),
            ..
        }]
    ));
}

#[test]
fn a_book_clear_restarts_with_a_full_snapshot() {
    let d = dir("clear");
    let defs = d.join("defs.dbn");
    write(&defs, Schema::Definition, &[es_def(42, DAY1 + 1)]);
    let book = d.join("mbp10.dbn");
    let t0 = DAY1 + H14;
    let lv = levels(&[(PX, 5)], &[(PX + TICK, 4)]);
    write(
        &book,
        Schema::Mbp10,
        &[
            mbp(42, t0 + 5, 1, b'A', b'B', true, lv.clone(), UNDEF_PRICE),
            mbp(
                42,
                t0 + 6,
                2,
                b'R',
                b'N',
                true,
                levels(&[], &[]),
                UNDEF_PRICE,
            ),
            mbp(42, t0 + 7, 3, b'A', b'B', true, lv, UNDEF_PRICE),
        ],
    );
    let out = d.join("es.lfr");
    run(vec![defs, book], &out).unwrap();
    let (_, frames) = read(&out);
    assert_eq!(frames.len(), 3);
    // The clear record ends its own event: the clear is a batch of its own (the book is empty).
    assert!(matches!(frames[1][..], [NativeEvent::BookClear { .. }]));
    // The next snapshot is an exact diff from the empty book: both levels again.
    assert_eq!(frames[2].len(), 2);
    assert!(
        frames[2]
            .iter()
            .all(|e| matches!(e, NativeEvent::BookLevel { qty, .. } if qty.0 > 0))
    );
}

fn top(id: u32, ts: u64, seq: u32, bid: (i64, u32), ask: (i64, u32), last: bool) -> Mbp1Msg {
    let mut flags = FlagSet::empty();
    if last {
        flags.set_last();
    }
    Mbp1Msg {
        hd: RecordHeader::new::<Mbp1Msg>(rtype::MBP_1, 1, id, ts - 1_000),
        price: bid.0,
        size: 1,
        action: b'A' as c_char,
        side: b'B' as c_char,
        flags,
        depth: 0,
        ts_recv: ts,
        ts_in_delta: 0,
        sequence: seq,
        levels: [BidAskPair {
            bid_px: bid.0,
            ask_px: ask.0,
            bid_sz: bid.1,
            ask_sz: ask.1,
            bid_ct: 1,
            ask_ct: 1,
        }],
    }
}

/// "Download days" contents "Trades + top of book": trades plus mbp-1 become trades and
/// best bid/ask changes, exact, only when a price changes, merged in receive order.
#[test]
fn trades_and_top_of_book_convert_exactly() {
    use lines_databento::import::convert;
    let d = dir("tob");
    let t0 = DAY1 + H14;
    write(
        &d.join("defs.dbn"),
        Schema::Definition,
        &[es_def(42, DAY1 + 1)],
    );
    write(
        &d.join("trades.dbn"),
        Schema::Trades,
        &[trade(42, t0 + 20, 3, PX, 2, b'B')],
    );
    write(
        &d.join("mbp1.dbn.zst"),
        Schema::Mbp1,
        &[
            top(42, t0 + 10, 1, (PX - TICK, 5), (PX, 3), true),
            top(42, t0 + 15, 2, (PX - TICK, 9), (PX, 3), true), // sizes only: nothing
            top(42, t0 + 30, 4, (PX, 1), (PX + TICK, 4), false), // mid-event: nothing
            top(42, t0 + 31, 5, (PX, 1), (PX + TICK, 4), true),
            top(42, t0 + 40, 6, (PX + 100_000_000, 1), (PX + TICK, 4), true), // off tick
            top(42, t0 + 50, 7, (UNDEF_PRICE, 0), (PX + 2 * TICK, 1), true),  // bid empty
            top(43, t0 + 60, 8, (PX, 1), (PX + TICK, 1), true),               // no definition
        ],
    );
    let mut frames: Vec<(i64, Vec<NativeEvent>)> = Vec::new();
    let r = convert(
        &[
            d.join("mbp1.dbn.zst"),
            d.join("trades.dbn"),
            d.join("defs.dbn"),
        ],
        es_z6(),
        |t, ev| {
            frames.push((t, ev.to_vec()));
            Ok(())
        },
    )
    .unwrap();
    let us = |ns: u64| (ns / 1_000) as i64;
    let ev = |ns: u64| ((ns - 1_000) / 1_000) as i64;
    assert_eq!(
        frames,
        vec![
            (
                us(t0 + 10),
                vec![NativeEvent::Bbo {
                    time_us: ev(t0 + 10),
                    bid: Some(Price(599975)),
                    ask: Some(Price(600000)),
                }]
            ),
            (
                us(t0 + 20),
                vec![NativeEvent::Trade {
                    time_us: ev(t0 + 20),
                    price: Price(600000),
                    qty: Qty(2),
                    aggressor: Aggressor::Buy,
                }]
            ),
            (
                us(t0 + 31),
                vec![NativeEvent::Bbo {
                    time_us: ev(t0 + 31),
                    bid: Some(Price(600000)),
                    ask: Some(Price(600025)),
                }]
            ),
            (
                us(t0 + 50),
                vec![NativeEvent::Bbo {
                    time_us: ev(t0 + 50),
                    bid: None,
                    ask: Some(Price(600050)),
                }]
            ),
        ]
    );
    assert_eq!(
        (r.mbp1_records, r.quotes_written, r.quotes_rejected),
        (7, 3, 1)
    );
    assert_eq!(r.unmapped, 1);

    // A disagreeing definition stops it before anything is emitted.
    let mut bad = es_def(42, DAY1 + 1);
    bad.currency = str_to_c_chars("EUR").unwrap();
    write(&d.join("defs.dbn"), Schema::Definition, &[bad]);
    let mut n = 0;
    let e = convert(
        &[d.join("mbp1.dbn.zst"), d.join("defs.dbn")],
        es_z6(),
        |_, _| {
            n += 1;
            Ok(())
        },
    )
    .unwrap_err();
    assert!(e.to_string().contains("BadMetadata"), "{e}");
    assert_eq!(n, 0);
}
