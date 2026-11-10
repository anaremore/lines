//! The live connector on synthetic gateway traffic built here (never real data, never the
//! network): the handshake with the key kept out of every frame, definitions checked before
//! market data, the trades replay as a closed history snapshot, books exact, every chunk
//! boundary, reconnects, the gateway's errors, a recording (two connections) replayed through
//! the public feed whatever the replaying machine's settings, and the real TCP transport
//! against a fake gateway on loopback.
//!
//! The runner here plays the transport's part: after each frame it feeds the session's sent
//! lines (not the auth line) back as recorded echoes, which carry the replay plan.
#![cfg(feature = "live")]

use std::ffi::c_char;
use std::sync::Arc;

use dbn::encode::{DbnEncoder, EncodeRecord};
use dbn::record::str_to_c_chars;
use dbn::{
    BidAskPair, ErrorCode, ErrorMsg, FlagSet, InstrumentDefMsg, Mbp1Msg, Mbp10Msg, Metadata,
    RecordHeader, SType, SymbolMappingMsg, SystemCode, SystemMsg, TradeMsg, UNDEF_PRICE, rtype,
};
use lines_core::{Aggressor, Bar, ContractKey, Month, Price, Product, Qty, Side, UtcMillis};
use lines_databento::key::ApiKey;
use lines_databento::live::session::{
    AUTH_PLACEHOLDER, BookPlan, CONNECTION_MARKER, DEPTH_REFUSED, DatabentoSession, NO_BOOK,
    REPLAY_SETTLE_MS, REPLAY_WINDOW_MS, SENT_PREFIX, TOP_OF_BOOK,
};
use lines_databento::live::transport::{LiveTransport, cram_response};
use lines_databento::live::{Floors, ReplayLimit};
use lines_feed::catalog;
use lines_feed::provider::{
    Frame, MarketEvent, Outgoing, ProviderSession, SessionError, StartContext,
};
use lines_feed::transport::Transport;

const T0: u64 = 1_794_060_000_000_000_000; // 2026-11-12T14:40:00Z, in ns
const PX: i64 = 6_000_000_000_000; // 6000.00
const TICK: i64 = 250_000_000;
const ID: u32 = 42;
const MIN: i64 = 60_000;
/// Made up, 32 characters.
const KEY: &str = "db-TESTKEYTESTKEYTESTKEYTESTKEYT";

fn es() -> Product {
    catalog::get(catalog::ES).unwrap()
}

fn z6() -> ContractKey {
    ContractKey::monthly(catalog::ES, 2026, Month::new(12).unwrap())
}

fn ms(ns: u64) -> UtcMillis {
    UtcMillis((ns / 1_000_000) as i64)
}

fn now() -> UtcMillis {
    ms(T0 + 3_600_000_000_000)
}

fn ctx(history_from: Option<UtcMillis>) -> StartContext {
    StartContext {
        now: now(),
        history_from,
        bar_period_ms: MIN,
        history_ms: 3 * 86_400_000,
    }
}

/// The replay start for a cold start: the window's edge, up to a bar boundary.
fn window_start() -> i64 {
    UtcMillis(now().0 - REPLAY_WINDOW_MS + MIN - 1)
        .floor_to(MIN)
        .0
}

fn def(tick: i64) -> InstrumentDefMsg {
    InstrumentDefMsg {
        hd: RecordHeader::new::<InstrumentDefMsg>(rtype::INSTRUMENT_DEF, 1, ID, T0),
        ts_recv: T0,
        min_price_increment: tick,
        display_factor: 1_000_000_000,
        unit_of_measure_qty: 50_000_000_000,
        unit_of_measure: str_to_c_chars("IPNT").unwrap(),
        currency: str_to_c_chars("USD").unwrap(),
        exchange: str_to_c_chars("XCME").unwrap(),
        maturity_year: 2026,
        maturity_month: 12,
        raw_symbol: str_to_c_chars("ESZ6").unwrap(),
        asset: str_to_c_chars("ES").unwrap(),
        instrument_class: b'F' as c_char,
        ..Default::default()
    }
}

fn mapping() -> SymbolMappingMsg {
    SymbolMappingMsg::new(
        ID,
        T0,
        SType::RawSymbol,
        "ESZ6",
        SType::InstrumentId,
        "42",
        T0,
        T0 + 86_400_000_000_000,
    )
    .unwrap()
}

fn trade(ts: u64, price: i64, size: u32, side: u8) -> TradeMsg {
    TradeMsg {
        hd: RecordHeader::new::<TradeMsg>(rtype::MBP_0, 1, ID, ts),
        price,
        size,
        action: b'T' as c_char,
        side: side as c_char,
        flags: FlagSet::empty().set_last(),
        depth: 0,
        ts_recv: ts + 1_000,
        ts_in_delta: 0,
        sequence: 1,
    }
}

fn mbp(ts: u64, last: bool, bid: (i64, u32), ask: (i64, u32), action: u8) -> Mbp10Msg {
    let mut flags = FlagSet::empty();
    if last {
        flags.set_last();
    }
    let levels = std::array::from_fn(|i| {
        if i == 0 {
            BidAskPair {
                bid_px: bid.0,
                ask_px: ask.0,
                bid_sz: bid.1,
                ask_sz: ask.1,
                bid_ct: 1,
                ask_ct: 1,
            }
        } else {
            BidAskPair {
                bid_px: UNDEF_PRICE,
                ask_px: UNDEF_PRICE,
                ..Default::default()
            }
        }
    });
    Mbp10Msg {
        hd: RecordHeader::new::<Mbp10Msg>(rtype::MBP_10, 1, ID, ts),
        price: bid.0,
        size: 1,
        action: action as c_char,
        side: b'B' as c_char,
        flags,
        depth: 0,
        ts_recv: ts + 1_000,
        ts_in_delta: 0,
        sequence: 1,
        levels,
    }
}

fn replay_done(ts: u64) -> SystemMsg {
    SystemMsg::new(ts, Some(SystemCode::ReplayCompleted), "Finished replay").unwrap()
}

/// A DBN stream: version-3 metadata, then the records `f` writes.
fn dbn(f: impl FnOnce(&mut DbnEncoder<&mut Vec<u8>>)) -> Vec<u8> {
    let meta = Metadata::builder()
        .dataset("GLBX.MDP3")
        .schema(None)
        .start(T0)
        .stype_in(Some(SType::RawSymbol))
        .stype_out(SType::InstrumentId)
        .build();
    let mut out = Vec::new();
    {
        let mut e = DbnEncoder::new(&mut out, &meta).unwrap();
        f(&mut e);
    }
    out
}

const GREETING: &[u8] = b"lsg_version=0.6.2\ncram=CHALLENGE-123\n";
const SUCCESS: &[u8] = b"success=1|session_id=7\n";

/// A connection as the gateway sends it: the greeting, the logon answer, then DBN (sent only
/// after the subscriptions and `start_session`).
fn connection(body: Vec<u8>) -> Vec<Vec<u8>> {
    vec![GREETING.to_vec(), SUCCESS.to_vec(), body]
}

/// The usual connection: definition and its replay done, three replayed trades, the trades
/// replay done, then a live trade, a book and a heartbeat.
fn stream_body() -> Vec<u8> {
    dbn(|e| {
        e.encode_record(&mapping()).unwrap();
        e.encode_record(&def(TICK)).unwrap();
        e.encode_record(&replay_done(T0)).unwrap();
        e.encode_record(&trade(T0 + 1_000_000, PX, 2, b'B'))
            .unwrap();
        e.encode_record(&trade(T0 + 2_000_000, PX - TICK, 1, b'A'))
            .unwrap();
        e.encode_record(&trade(T0 + 3_000_000, PX, 4, b'B'))
            .unwrap();
        e.encode_record(&replay_done(T0 + 3_000_000)).unwrap();
        e.encode_record(&trade(T0 + 5_000_000, PX + TICK, 1, b'B'))
            .unwrap();
        // One event in two records: only its end applies.
        e.encode_record(&mbp(T0 + 6_000_000, false, (PX - TICK, 9), (PX, 4), b'A'))
            .unwrap();
        e.encode_record(&mbp(T0 + 6_000_000, true, (PX - TICK, 5), (PX, 7), b'A'))
            .unwrap();
        e.encode_record(&SystemMsg::heartbeat(T0 + 7_000_000))
            .unwrap();
    })
}

fn session(floors: &Floors, limit: ReplayLimit) -> DatabentoSession {
    // The older tests here exercise the depth path: `mbp-10` asked for.
    DatabentoSession::new(&es(), Some(z6()), floors.clone(), limit, "test")
        .unwrap()
        .with_book_plan(BookPlan::new(true))
}

fn cold() -> DatabentoSession {
    session(&Floors::default(), ReplayLimit::default())
}

type Run = (Vec<MarketEvent>, Vec<String>, Option<SessionError>);

/// Feed timed frames, echoing the session's sent lines as the transport does; the events,
/// the outgoing lines, the first error.
fn run_at(s: &mut DatabentoSession, c: StartContext, frames: &[(&[u8], UtcMillis)]) -> Run {
    let mut events = Vec::new();
    let mut out = Vec::new();
    s.start(c, &mut out);
    let mut echoed = out.len();
    for (data, at) in frames {
        if let Err(e) = s.on_frame(&Frame::binary(data.to_vec()), *at, &mut events, &mut out) {
            return (events, lines(&out), Some(e));
        }
        let sent: Vec<String> = lines(&out[echoed..]);
        echoed = out.len();
        for l in sent.iter().filter(|l| !l.starts_with("auth=")) {
            let echo = [SENT_PREFIX, l.as_bytes()].concat();
            s.on_frame(&Frame::binary(echo), *at, &mut events, &mut Vec::new())
                .unwrap();
        }
    }
    (events, lines(&out), None)
}

fn run(s: &mut DatabentoSession, frames: &[&[u8]]) -> Run {
    let timed: Vec<(&[u8], UtcMillis)> = frames.iter().map(|f| (*f, now())).collect();
    run_at(s, ctx(None), &timed)
}

fn run_conn(s: &mut DatabentoSession, frames: &[Vec<u8>]) -> Run {
    let refs: Vec<&[u8]> = frames.iter().map(|f| f.as_slice()).collect();
    run(s, &refs)
}

fn lines(out: &[Outgoing]) -> Vec<String> {
    out.iter()
        .map(|o| match o {
            Outgoing::Frame(f) => String::from_utf8(f.data.to_vec()).unwrap(),
            Outgoing::HttpGet(u) => format!("GET {u}"),
        })
        .collect()
}

fn trades_line(start_ms: i64) -> String {
    format!(
        "schema=trades|stype_in=raw_symbol|symbols=ESZ6|snapshot=0|is_last=1|start={}\n",
        start_ms as u64 * 1_000_000
    )
}

#[test]
fn the_handshake_keeps_the_key_out_and_subscribes_as_designed() {
    let (_, out, err) = run(&mut cold(), &[GREETING, SUCCESS]);
    assert!(err.is_none(), "{err:?}");
    assert_eq!(out.len(), 5, "{out:#?}");
    assert_eq!(
        out[0],
        format!(
            "auth={AUTH_PLACEHOLDER}|dataset=GLBX.MDP3|encoding=dbn|compression=none|ts_out=0|client=test|heartbeat_interval_s=10|slow_reader_behavior=warn\n"
        )
    );
    assert_eq!(
        out[1],
        "schema=definition|stype_in=raw_symbol|symbols=ESZ6|snapshot=0|is_last=1|start=0\n"
    );
    // No stored bars and 3 days wanted: the replay window's edge, on a bar boundary.
    assert_eq!(out[2], trades_line(window_start()));
    assert_eq!(
        out[3],
        "schema=mbp-10|stype_in=raw_symbol|symbols=ESZ6|snapshot=0|is_last=1\n"
    );
    assert_eq!(out[4], "start_session\n", "bare, not start_session=0");
    for l in &out {
        assert!(!l.contains(KEY) && !l.contains(&cram_response("CHALLENGE-123", KEY)));
    }
}

/// The replay is history: one snapshot of bars closed at the newest replayed millisecond (no
/// double count with stored bars, by `lines_core::stitch`), then live data.
#[test]
fn the_trades_replay_is_a_closed_history_snapshot() {
    let mut s = cold();
    let (events, _, err) = run_conn(&mut s, &connection(stream_body()));
    assert!(err.is_none(), "{err:?}");
    let bucket = ms(T0).floor_to(MIN);
    let expect = vec![
        // The connection's book starts empty.
        MarketEvent::BookClear,
        // History older than the replay window is a gap, never invented.
        MarketEvent::HistoryGap {
            from: UtcMillis(now().0 - 3 * 86_400_000),
            to: UtcMillis(window_start()),
        },
        MarketEvent::Ready,
        MarketEvent::History {
            bars: vec![Bar {
                time: bucket,
                open: Price(600_000),
                high: Price(600_000),
                low: Price(599_975),
                close: Price(600_000),
                volume: Qty(7),
            }],
            as_of: Some(ms(T0 + 3_000_000)),
        },
        MarketEvent::HistoryComplete,
        MarketEvent::Trade {
            time: ms(T0 + 5_000_000),
            price: Price(600_025),
            qty: Qty(1),
            aggressor: Some(Aggressor::Buy),
        },
        MarketEvent::BookClear,
        MarketEvent::BookLevel {
            side: Side::Bid,
            price: Price(599_975),
            qty: Qty(5),
            orders: 1,
        },
        MarketEvent::BookLevel {
            side: Side::Ask,
            price: Price(600_000),
            qty: Qty(7),
            orders: 1,
        },
        MarketEvent::Bid(Price(599_975)),
        MarketEvent::Ask(Price(600_000)),
    ];
    assert_eq!(events, expect);
    assert_eq!((s.stats().replayed_trades, s.stats().trades), (3, 1));
    assert_eq!(s.stats().heartbeats, 1);
}

/// Open and close follow trade time, not arrival order; `as_of` is the newest time.
#[test]
fn snapshot_bars_follow_trade_time() {
    let body = dbn(|e| {
        e.encode_record(&def(TICK)).unwrap();
        e.encode_record(&replay_done(T0)).unwrap();
        e.encode_record(&trade(T0 + 2_000_000, PX, 1, b'B'))
            .unwrap();
        e.encode_record(&trade(T0 + 1_000_000, PX - TICK, 1, b'A'))
            .unwrap(); // earlier, arriving later
        e.encode_record(&replay_done(T0 + 2_000_000)).unwrap();
    });
    let (events, _, _) = run_conn(&mut cold(), &connection(body));
    let h = events
        .iter()
        .find_map(|e| match e {
            MarketEvent::History { bars, as_of } => Some((bars.clone(), *as_of)),
            _ => None,
        })
        .unwrap();
    assert_eq!(h.0[0].open, Price(599_975));
    assert_eq!(h.0[0].close, Price(600_000));
    assert_eq!(h.1, Some(ms(T0 + 2_000_000)));
}

/// Every (re)connect replays from the newest stored bar, so the snapshot replaces it whole.
#[test]
fn a_reconnect_replays_from_the_newest_stored_bar() {
    let bar = UtcMillis(ms(T0).0 - 5 * MIN);
    let (events, out, err) = run_at(
        &mut cold(),
        ctx(Some(bar)),
        &[(GREETING, now()), (SUCCESS, now())],
    );
    assert!(err.is_none());
    assert_eq!(out[2], trades_line(bar.0));
    assert_eq!(
        events,
        [MarketEvent::BookClear],
        "inside the window: no gap"
    );
}

#[test]
fn every_chunk_boundary_decodes_the_same() {
    let body = stream_body();
    let (whole, out_whole, _) = run(&mut cold(), &[GREETING, SUCCESS, &body]);
    for i in 1..body.len() {
        let (a, b) = body.split_at(i);
        let (events, out, err) = run(&mut cold(), &[GREETING, SUCCESS, a, b]);
        assert!(err.is_none(), "split at {i}: {err:?}");
        assert_eq!(events, whole, "split at {i}");
        assert_eq!(out, out_whole, "split at {i}");
    }
    let mut frames: Vec<&[u8]> = vec![GREETING, SUCCESS];
    frames.extend(body.chunks(1));
    assert_eq!(run(&mut cold(), &frames).0, whole, "byte by byte");
    // The handshake split anywhere too.
    let hs = [GREETING, SUCCESS].concat();
    for i in 1..hs.len() {
        let (a, b) = hs.split_at(i);
        let (events, _, err) = run(&mut cold(), &[a, b, &body]);
        assert!(err.is_none(), "handshake split at {i}: {err:?}");
        assert_eq!(events, whole, "handshake split at {i}");
    }
}

#[test]
fn market_data_waits_for_the_checked_definition() {
    let body = dbn(|e| {
        e.encode_record(&mbp(T0 + 1_000, true, (PX - TICK, 1), (PX, 1), b'A'))
            .unwrap();
        e.encode_record(&def(TICK)).unwrap();
    });
    let (events, _, _) = run_conn(&mut cold(), &connection(body));
    let ready = events
        .iter()
        .position(|e| *e == MarketEvent::Ready)
        .unwrap();
    let level = events
        .iter()
        .position(|e| matches!(e, MarketEvent::BookLevel { .. }))
        .unwrap();
    assert!(ready < level, "{events:?}");

    // A definition that disagrees with the catalog (tick) is fatal.
    let bad = dbn(|e| {
        e.encode_record(&def(TICK * 2)).unwrap();
    });
    let (events, _, err) = run_conn(&mut cold(), &connection(bad));
    assert!(matches!(err, Some(SessionError::BadMetadata(_))), "{err:?}");
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, MarketEvent::BookLevel { .. }))
    );
}

/// No replay (`LINES_DATABENTO_REPLAY_HOURS=0`): the wanted history is a gap; history
/// completes with the definitions' replay; trades are live from the start.
#[test]
fn without_a_replay_the_missing_history_is_a_gap() {
    let mut s = session(&Floors::default(), ReplayLimit { max_ms: 0 });
    let body = dbn(|e| {
        e.encode_record(&def(TICK)).unwrap();
        e.encode_record(&replay_done(T0)).unwrap();
        e.encode_record(&trade(T0 + 1_000_000, PX, 1, b'B'))
            .unwrap();
    });
    let (events, out, _) = run_conn(&mut s, &connection(body));
    assert_eq!(
        out[2],
        "schema=trades|stype_in=raw_symbol|symbols=ESZ6|snapshot=0|is_last=1\n"
    );
    assert_eq!(
        events[1],
        MarketEvent::HistoryGap {
            from: UtcMillis(now().0 - 3 * 86_400_000),
            to: now()
        }
    );
    assert!(events.contains(&MarketEvent::HistoryComplete));
    assert!(matches!(events.last(), Some(MarketEvent::Trade { .. })));
}

/// The trades replay is decided by the recorded plan, not this machine's setting: a session
/// configured for no replay decodes a replaying recording the same as one configured for it.
#[test]
fn a_recording_decodes_the_same_whatever_the_replay_setting() {
    let body = stream_body();
    // As recorded: the transport's echoes of what the live session sent (a replay).
    let plan: Vec<Vec<u8>> = [
        "schema=definition|stype_in=raw_symbol|symbols=ESZ6|snapshot=0|is_last=1|start=0\n"
            .to_string(),
        trades_line(window_start()),
        "schema=mbp-10|stype_in=raw_symbol|symbols=ESZ6|snapshot=0|is_last=1\n".to_string(),
        "start_session\n".to_string(),
    ]
    .iter()
    .map(|l| [SENT_PREFIX, l.as_bytes()].concat())
    .collect();
    let replay = |limit: ReplayLimit| {
        let mut s = session(&Floors::default(), limit);
        let mut events = Vec::new();
        let mut ignored = Vec::new();
        s.start(ctx(None), &mut ignored);
        let mut frames: Vec<&[u8]> = vec![GREETING, SUCCESS];
        frames.extend(plan.iter().map(|p| p.as_slice()));
        frames.push(&body);
        for f in frames {
            s.on_frame(&Frame::binary(f.to_vec()), now(), &mut events, &mut ignored)
                .unwrap();
        }
        events
    };
    assert_eq!(
        replay(ReplayLimit { max_ms: 0 }),
        replay(ReplayLimit::default())
    );
}

/// One completion for two replays: history completes once replay activity has been quiet for
/// REPLAY_SETTLE_MS, never while replayed trades keep coming.
#[test]
fn history_settles_only_after_the_replay_goes_quiet() {
    let sz = std::mem::size_of::<TradeMsg>();
    let full = dbn(|e| {
        e.encode_record(&def(TICK)).unwrap();
        e.encode_record(&replay_done(T0)).unwrap(); // the definitions' only
        e.encode_record(&trade(T0 + 1_000_000, PX, 1, b'B'))
            .unwrap();
        e.encode_record(&trade(T0 + 2_000_000, PX, 1, b'B'))
            .unwrap();
        e.encode_record(&trade(T0 + 3_000_000, PX, 1, b'B'))
            .unwrap();
    });
    let n = full.len();
    let (head, t2, t3) = (
        &full[..n - 2 * sz],
        &full[n - 2 * sz..n - sz],
        &full[n - sz..],
    );
    let at = |dt: i64| UtcMillis(now().0 + dt);
    let mut s = cold();
    let (events, _, err) = run_at(
        &mut s,
        ctx(None),
        &[
            (GREETING, at(0)),
            (SUCCESS, at(0)),
            (head, at(0)),
            (t2, at(REPLAY_SETTLE_MS - 1_000)),
            (t3, at(2 * REPLAY_SETTLE_MS - 2_000)),
        ],
    );
    assert!(err.is_none());
    assert!(
        !events.contains(&MarketEvent::HistoryComplete),
        "still replaying: {events:?}"
    );
    let mut more = Vec::new();
    s.on_frame(
        &Frame::binary(Vec::new()),
        at(3 * REPLAY_SETTLE_MS),
        &mut more,
        &mut Vec::new(),
    )
    .unwrap();
    assert!(more.contains(&MarketEvent::HistoryComplete), "{more:?}");
    assert!(more.iter().any(|e| matches!(
        e,
        MarketEvent::History { bars, .. } if bars[0].volume == Qty(3)
    )));
}

#[test]
fn the_gateways_refusals_and_warnings() {
    // A rejected logon: no retry.
    let (_, _, err) = run(
        &mut cold(),
        &[GREETING, b"success=0|error=Authentication failed.\n"],
    );
    assert!(
        matches!(&err, Some(SessionError::LogonRejected { text, .. }) if text == "Authentication failed."),
        "{err:?}"
    );
    // A DBN version this build doesn't decode: fatal, said plainly.
    let mut body = dbn(|_| {});
    body[3] = 2;
    let (_, _, err) = run_conn(&mut cold(), &connection(body));
    assert!(
        matches!(&err, Some(SessionError::BadMetadata(m)) if m.contains("DBN version 2")),
        "{err:?}"
    );
    // Skipped records (not expected with `warn`): the book clears.
    let skipped = dbn(|e| {
        e.encode_record(&def(TICK)).unwrap();
        e.encode_record(&ErrorMsg::new(
            T0,
            Some(ErrorCode::SkippedRecordsAfterSlowReading),
            "skipped",
            true,
        ))
        .unwrap();
        e.encode_record(&SystemMsg::new(T0, Some(SystemCode::SlowReaderWarning), "slow").unwrap())
            .unwrap();
    });
    let mut s = cold();
    let (events, _, err) = run_conn(&mut s, &connection(skipped));
    assert!(err.is_none());
    assert_eq!(events.last(), Some(&MarketEvent::BookClear));
    assert_eq!(
        (
            s.stats().skipped_record_notices,
            s.stats().slow_reader_warnings
        ),
        (1, 1)
    );
}

/// The gateway's error for a schema the plan leaves out.
fn refusal(schema: &str, code: Option<ErrorCode>) -> ErrorMsg {
    let text = format!("Not authorized for {schema} schema");
    ErrorMsg::new(T0, code, &text, true)
}

fn top(ts: u64, bid: (i64, u32), ask: (i64, u32)) -> Mbp1Msg {
    Mbp1Msg {
        hd: RecordHeader::new::<Mbp1Msg>(rtype::MBP_1, 1, ID, ts),
        price: bid.0,
        size: bid.1,
        action: b'A' as c_char,
        side: b'B' as c_char,
        flags: FlagSet::empty().set_last(),
        depth: 0,
        ts_recv: ts + 1_000,
        ts_in_delta: 0,
        sequence: 1,
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

fn book_schemas(out: &[String]) -> Vec<&str> {
    out.iter()
        .filter(|l| l.starts_with("schema="))
        .map(|l| l.split('|').next().unwrap())
        .collect()
}

fn told(reason: &str) -> MarketEvent {
    MarketEvent::DepthUnavailable {
        reason: reason.into(),
    }
}

/// The display factor is a display hint that varies by product (ES 0.01, GC 0.1, 6J 1e-6 on
/// the gateway) and never scales a price: any factor is accepted, the tick is the check.
#[test]
fn the_display_factor_never_refuses_a_definition() {
    let with = |factor| {
        let body = dbn(|e| {
            let mut d = def(TICK);
            d.display_factor = factor;
            e.encode_record(&d).unwrap();
            e.encode_record(&trade(T0 + 3_000, PX, 1, b'A')).unwrap();
        });
        let mut s = session(&Floors::default(), ReplayLimit { max_ms: 0 });
        run_conn(&mut s, &connection(body))
    };
    let (events, _, err) = with(10_000_000);
    assert_eq!(err, None);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, MarketEvent::Trade { price, .. } if *price == Price(600_000))),
        "{events:?}"
    );
    let (_, _, err) = with(1_000_000);
    assert_eq!(err, None);
}

/// The default: `mbp-1`, never `mbp-10`. The book is the inside only, and the feed says so.
#[test]
fn the_default_book_is_the_inside_and_says_so() {
    let mut s = DatabentoSession::new(
        &es(),
        Some(z6()),
        Floors::default(),
        ReplayLimit { max_ms: 0 },
        "t",
    )
    .unwrap();
    let body = dbn(|e| {
        e.encode_record(&mapping()).unwrap();
        e.encode_record(&def(TICK)).unwrap();
        e.encode_record(&top(T0 + 1_000, (PX - TICK, 5), (PX, 7)))
            .unwrap();
        e.encode_record(&top(T0 + 2_000, (PX, 2), (PX + TICK, 3)))
            .unwrap();
        e.encode_record(&trade(T0 + 3_000, PX, 1, b'A')).unwrap();
    });
    let (events, out, err) = run_conn(&mut s, &connection(body));
    assert_eq!(err, None);
    assert_eq!(
        book_schemas(&out),
        ["schema=definition", "schema=trades", "schema=mbp-1"]
    );
    assert!(events.contains(&told(TOP_OF_BOOK)), "{events:?}");
    assert!(
        events.contains(&MarketEvent::Bid(Price(600_000))),
        "{events:?}"
    );
    assert!(
        events.contains(&MarketEvent::Ask(Price(600_025))),
        "{events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, MarketEvent::Trade { .. }))
    );
    // The inside moved up: the old levels go, one level per side remains.
    let mut book = lines_core::OrderBook::default();
    for e in &events {
        match e {
            MarketEvent::BookClear => book.clear(),
            MarketEvent::BookLevel {
                side,
                price,
                qty,
                orders,
            } => book.set(*side, *price, *qty, *orders),
            _ => {}
        }
    }
    assert_eq!(book.levels(Side::Bid).count(), 1);
    assert_eq!(book.levels(Side::Ask).count(), 1);
    assert_eq!(book.levels(Side::Bid).next().unwrap().0, Price(600_000));
    assert_eq!(book.levels(Side::Ask).next().unwrap().0, Price(600_025));
}

/// `mbp-10` asked for and refused, the gateway's words whatever its code: no failure. The next
/// connection (the gateway closes this one) and other feeds ask for `mbp-1`, and never again
/// for `mbp-10`, so there's no reconnect loop. Trades before the close aren't lost.
#[test]
fn a_refused_depth_schema_falls_back_to_the_inside() {
    for code in [None, Some(ErrorCode::InvalidSubscription)] {
        let plan = BookPlan::new(true);
        let new =
            || session(&Floors::default(), ReplayLimit { max_ms: 0 }).with_book_plan(plan.clone());
        let body = dbn(|e| {
            e.encode_record(&mapping()).unwrap();
            e.encode_record(&def(TICK)).unwrap();
            e.encode_record(&trade(T0 + 5_000_000, PX + TICK, 1, b'B'))
                .unwrap();
            e.encode_record(&refusal("mbp-10", code)).unwrap();
        });
        let (events, out, err) = run_conn(&mut new(), &connection(body));
        assert_eq!(err, None, "not a failure");
        assert_eq!(
            book_schemas(&out),
            ["schema=definition", "schema=trades", "schema=mbp-10"]
        );
        assert!(events.contains(&told(DEPTH_REFUSED)), "{events:?}");
        assert!(
            events
                .iter()
                .any(|e| matches!(e, MarketEvent::Trade { .. }))
        );

        for _ in 0..2 {
            // The reconnect's note comes from what it subscribed (as a replay would see it).
            let (events, out, err) = run(&mut new(), &[GREETING, SUCCESS]);
            assert_eq!(err, None);
            assert!(events.contains(&told(TOP_OF_BOOK)), "{events:?}");
            assert_eq!(
                book_schemas(&out),
                ["schema=definition", "schema=trades", "schema=mbp-1"]
            );
            assert_eq!(out.last().unwrap(), "start_session\n");
        }
    }
}

/// The note comes from the recording, not the replaying machine's settings: an `mbp-1`
/// recording says top of book whatever the plan, an `mbp-10` one never does.
#[test]
fn the_book_note_follows_the_recording() {
    let body = || dbn(|e| e.encode_record(&def(TICK)).unwrap());
    for want_depth in [false, true] {
        let mut s = session(&Floors::default(), ReplayLimit { max_ms: 0 })
            .with_book_plan(BookPlan::new(want_depth));
        let frames: Vec<Vec<u8>> = vec![
            CONNECTION_MARKER.to_vec(),
            GREETING.to_vec(),
            SUCCESS.to_vec(),
            [
                SENT_PREFIX,
                b"schema=mbp-1|stype_in=raw_symbol|symbols=ESZ6|snapshot=0|is_last=1\n",
            ]
            .concat(),
            [SENT_PREFIX, b"start_session\n"].concat(),
            body(),
        ];
        let mut events = Vec::new();
        s.start(ctx(None), &mut Vec::new());
        for f in &frames {
            s.on_frame(
                &Frame::binary(f.clone()),
                now(),
                &mut events,
                &mut Vec::new(),
            )
            .unwrap();
        }
        let notes: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, MarketEvent::DepthUnavailable { .. }))
            .collect();
        assert_eq!(
            notes,
            [&told(TOP_OF_BOOK)],
            "plan wants depth: {want_depth}"
        );
    }
    // A recorded mbp-10 connection: no note, whatever this machine wants.
    let mut s =
        session(&Floors::default(), ReplayLimit { max_ms: 0 }).with_book_plan(BookPlan::new(false));
    let mut events = Vec::new();
    s.start(ctx(None), &mut Vec::new());
    for f in [
        CONNECTION_MARKER.to_vec(),
        GREETING.to_vec(),
        SUCCESS.to_vec(),
        [
            SENT_PREFIX,
            b"schema=mbp-10|stype_in=raw_symbol|symbols=ESZ6|snapshot=0|is_last=1\n",
        ]
        .concat(),
        [SENT_PREFIX, b"start_session\n"].concat(),
    ] {
        s.on_frame(&Frame::binary(f), now(), &mut events, &mut Vec::new())
            .unwrap();
    }
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, MarketEvent::DepthUnavailable { .. }))
    );
}

/// Other wordings of a refusal drop only the book; a bad key still reads as a bad key.
#[test]
fn refusal_wordings_and_auth_errors() {
    for text in [
        "Schema mbp-10 not authorized",
        "mbp-10: not authorized for this plan",
    ] {
        let body = dbn(|e| {
            e.encode_record(&ErrorMsg::new(T0, None, text, true))
                .unwrap();
        });
        let (events, _, err) = run_conn(&mut cold(), &connection(body));
        assert_eq!(err, None, "{text}");
        assert!(events.contains(&told(DEPTH_REFUSED)), "{text}");
    }
    let body = dbn(|e| {
        e.encode_record(&ErrorMsg::new(
            T0,
            Some(ErrorCode::AuthFailed),
            "Not authorized: bad key",
            true,
        ))
        .unwrap();
    });
    let (_, _, err) = run_conn(&mut cold(), &connection(body));
    assert!(
        matches!(err, Some(SessionError::LogonRejected { .. })),
        "{err:?}"
    );
}

/// `mbp-1` refused too: trades only, said plainly, and no loop.
#[test]
fn a_refused_inside_leaves_the_trades() {
    let plan = Arc::new(BookPlan::default());
    let new =
        || session(&Floors::default(), ReplayLimit { max_ms: 0 }).with_book_plan(plan.clone());
    let body = dbn(|e| e.encode_record(&refusal("mbp-1", None)).unwrap());
    let (events, _, err) = run_conn(&mut new(), &connection(body));
    assert_eq!(err, None);
    assert!(events.contains(&told(NO_BOOK)), "{events:?}");
    let (events, out, err) = run(&mut new(), &[GREETING, SUCCESS]);
    assert_eq!(err, None);
    assert!(events.contains(&told(NO_BOOK)));
    assert_eq!(book_schemas(&out), ["schema=definition", "schema=trades"]);
}

/// Without trades or definitions there's nothing to show: a clear, final error, no retry loop.
#[test]
fn a_refused_required_schema_stops_with_a_clear_error() {
    for schema in ["trades", "definition"] {
        let body = dbn(|e| {
            e.encode_record(&refusal(schema, None)).unwrap();
        });
        let (_, _, err) = run_conn(&mut cold(), &connection(body));
        let err = err.expect("refused");
        assert!(!err.is_retryable(), "{err:?}");
        assert!(
            matches!(&err, SessionError::Unavailable(m)
                if m.contains("not on your plan") && m.contains(schema)),
            "{err:?}"
        );
    }
    // Other errors keep their meaning.
    let body = dbn(|e| {
        e.encode_record(&ErrorMsg::new(T0, None, "gateway restarting", true))
            .unwrap();
    });
    let (_, _, err) = run_conn(&mut cold(), &connection(body));
    assert!(err.expect("error").is_retryable());
}

/// A replay that aged out: a gap up to the start Databento names, a retryable error, and the
/// next connection starts no earlier than that (shared floor), and never after now.
#[test]
fn an_aged_out_replay_moves_the_next_start_forward() {
    let floors = Floors::default();
    let earliest_ns = T0 + 3_000_000_000_000; // 50 minutes after T0
    let aged = dbn(|e| {
        e.encode_record(&ErrorMsg::new(
            T0,
            Some(ErrorCode::ReplayDataAgedOut),
            &format!("replay data aged out; earliest available start {earliest_ns}"),
            true,
        ))
        .unwrap();
    });
    let (events, _, err) = run_conn(
        &mut session(&floors, ReplayLimit::default()),
        &connection(aged),
    );
    assert!(err.as_ref().is_some_and(|e| e.is_retryable()), "{err:?}");
    assert!(events.contains(&MarketEvent::HistoryGap {
        from: UtcMillis(window_start()),
        to: ms(earliest_ns),
    }));
    assert_eq!(floors.lock().unwrap()[&z6()], earliest_ns);
    let (_, out, _) = run(
        &mut session(&floors, ReplayLimit::default()),
        &[GREETING, SUCCESS],
    );
    assert_eq!(out[2], trades_line(ms(earliest_ns).0));
    // A floor after now never puts the start in the future.
    floors
        .lock()
        .unwrap()
        .insert(z6(), (now().0 as u64 + 600_000) * 1_000_000);
    let (_, out, _) = run(
        &mut session(&floors, ReplayLimit::default()),
        &[GREETING, SUCCESS],
    );
    assert_eq!(out[2], trades_line(now().floor_to(MIN).0));
}

/// The owner's live failure (2026-11-10): a pane whose stored bars ended more than a day ago
/// asked for the replay window's edge, and Databento refused it as a subscription, "Invalid
/// start time. Must be <time> or later, or 0", which stopped the feed ("failed", no last
/// price). Now it is like an aged-out replay: a gap to the time named, a retryable error, and
/// the next connection starts there.
#[test]
fn a_refused_start_reconnects_from_the_earliest_named() {
    let floors = Floors::default();
    let earliest = UtcMillis(window_start() + 9 * MIN);
    let (y, mo, d, h, mi, s) = earliest.to_civil();
    let text = format!(
        "Invalid start time. Must be {y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z or later, or 0"
    );
    let refused = dbn(|e| {
        e.encode_record(&ErrorMsg::new(
            T0,
            Some(ErrorCode::InvalidSubscription),
            &text,
            true,
        ))
        .unwrap();
    });
    let (events, _, err) = run_conn(
        &mut session(&floors, ReplayLimit::default()),
        &connection(refused),
    );
    assert!(
        err.as_ref().is_some_and(|e| e.is_retryable()),
        "the feed reconnects, never stops: {err:?}"
    );
    assert!(events.contains(&MarketEvent::HistoryGap {
        from: UtcMillis(window_start()),
        to: earliest,
    }));
    assert_eq!(floors.lock().unwrap()[&z6()], earliest.0 as u64 * 1_000_000);
    let (_, out, _) = run(
        &mut session(&floors, ReplayLimit::default()),
        &[GREETING, SUCCESS],
    );
    assert_eq!(
        out[2],
        trades_line(earliest.0),
        "the next start is the one named"
    );
    // Another invalid subscription (a symbol) still stops the feed.
    let other = dbn(|e| {
        e.encode_record(&ErrorMsg::new(
            T0,
            Some(ErrorCode::InvalidSubscription),
            "Invalid symbol",
            true,
        ))
        .unwrap();
    });
    let (_, _, err) = run_conn(&mut cold(), &connection(other));
    assert!(err.is_some_and(|e| !e.is_retryable()));
}

/// A recording spanning a reconnect: each connection starts with the transport's marker, so
/// one replay session decodes both.
#[test]
fn a_recording_with_two_connections_decodes() {
    let mut s = cold();
    let b = dbn(|e| {
        e.encode_record(&def(TICK)).unwrap();
        e.encode_record(&replay_done(T0)).unwrap();
        e.encode_record(&replay_done(T0)).unwrap();
        e.encode_record(&trade(T0 + 9_000_000, PX, 7, b'A'))
            .unwrap();
    });
    let a = stream_body();
    let (events, _, err) = run(
        &mut s,
        &[
            CONNECTION_MARKER,
            GREETING,
            SUCCESS,
            &a,
            CONNECTION_MARKER,
            GREETING,
            SUCCESS,
            &b,
        ],
    );
    assert!(err.is_none(), "{err:?}");
    assert_eq!(s.stats().connections, 2);
    assert_eq!(
        events.last(),
        Some(&MarketEvent::Trade {
            time: ms(T0 + 9_000_000),
            price: Price(600_000),
            qty: Qty(7),
            aggressor: Some(Aggressor::Sell),
        })
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| **e == MarketEvent::HistoryComplete)
            .count(),
        2
    );
}

/// The replay-to-live switch splitting a millisecond: a live trade at the snapshot's `as_of`
/// (which the stitch would drop) corrects the snapshot's last bar instead.
#[test]
fn a_live_trade_in_the_snapshots_last_millisecond_corrects_its_bar() {
    let body = dbn(|e| {
        e.encode_record(&def(TICK)).unwrap();
        e.encode_record(&replay_done(T0)).unwrap();
        e.encode_record(&trade(T0 + 1_000_000, PX, 2, b'B'))
            .unwrap();
        e.encode_record(&replay_done(T0 + 1_000_000)).unwrap();
        // Live, same millisecond (different nanosecond), then later.
        e.encode_record(&trade(T0 + 1_000_500, PX - TICK, 3, b'A'))
            .unwrap();
        e.encode_record(&trade(T0 + 4_000_000, PX, 1, b'B'))
            .unwrap();
    });
    let (events, _, err) = run_conn(&mut cold(), &connection(body));
    assert!(err.is_none());
    let bucket = ms(T0).floor_to(MIN);
    let after: Vec<&MarketEvent> = events
        .iter()
        .skip_while(|e| **e != MarketEvent::HistoryComplete)
        .skip(1)
        .collect();
    assert_eq!(
        after,
        [
            &MarketEvent::History {
                bars: vec![Bar {
                    time: bucket,
                    open: Price(600_000),
                    high: Price(600_000),
                    low: Price(599_975),
                    close: Price(599_975),
                    volume: Qty(5),
                }],
                as_of: Some(ms(T0 + 1_000_000)),
            },
            &MarketEvent::Trade {
                time: ms(T0 + 4_000_000),
                price: Price(600_000),
                qty: Qty(1),
                aggressor: Some(Aggressor::Buy),
            },
        ]
    );
}

#[test]
fn the_same_frames_decode_the_same_every_time() {
    let c = connection(stream_body());
    assert_eq!(run_conn(&mut cold(), &c).0, run_conn(&mut cold(), &c).0);
}

/// A synthetic recording replays through the public feed like any source, identically every
/// time, whatever this machine's replay setting (the extension registered as the `lines` app
/// does).
#[test]
fn a_synthetic_recording_replays_through_the_feed() {
    use lines_feed::recording::{Direction, FORMAT_VERSION, Header, Writer};
    use lines_feed::store::{FeedStatus, HistoryState};
    use lines_feed::{FeedConfig, Source};
    use std::sync::Arc;
    use std::time::Duration;

    let dir = std::env::temp_dir().join(format!("lines-databento-replay-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("synthetic.lfr");
    let header = Header {
        start_wall_ms: ms(T0).0,
        provider: lines_databento::live::PROVIDER.into(),
        protocol: lines_databento::live::PROTOCOL.into(),
        product: "ES".into(),
        contract: Some(z6()),
        version: FORMAT_VERSION,
    };
    // A reconnect: the second connection replays what the first saw (all four trades, from
    // the newest stored bar) and then one new trade live.
    let second = dbn(|e| {
        e.encode_record(&mapping()).unwrap();
        e.encode_record(&def(TICK)).unwrap();
        e.encode_record(&replay_done(T0)).unwrap();
        e.encode_record(&trade(T0 + 1_000_000, PX, 2, b'B'))
            .unwrap();
        e.encode_record(&trade(T0 + 2_000_000, PX - TICK, 1, b'A'))
            .unwrap();
        e.encode_record(&trade(T0 + 3_000_000, PX, 4, b'B'))
            .unwrap();
        e.encode_record(&trade(T0 + 5_000_000, PX + TICK, 1, b'B'))
            .unwrap();
        e.encode_record(&replay_done(T0 + 5_000_000)).unwrap();
        e.encode_record(&trade(T0 + 8_000_000, PX, 3, b'A'))
            .unwrap();
        e.encode_record(&mbp(T0 + 9_000_000, true, (PX - TICK, 5), (PX, 7), b'A'))
            .unwrap();
    });
    let plan = |start_ns: u64| -> Vec<Vec<u8>> {
        [
            "schema=definition|stype_in=raw_symbol|symbols=ESZ6|snapshot=0|is_last=1|start=0\n"
                .to_string(),
            format!(
                "schema=trades|stype_in=raw_symbol|symbols=ESZ6|snapshot=0|is_last=1|start={start_ns}\n"
            ),
            "schema=mbp-10|stype_in=raw_symbol|symbols=ESZ6|snapshot=0|is_last=1\n".to_string(),
            "start_session\n".to_string(),
        ]
        .iter()
        .map(|l| [SENT_PREFIX, l.as_bytes()].concat())
        .collect()
    };
    let mut w = Writer::create(&path, &header).unwrap();
    let mut i = 0u64;
    let mut put = |data: &[u8]| {
        w.write(
            Direction::FromServer,
            Duration::from_millis(i),
            &Frame::binary(data.to_vec()),
        )
        .unwrap();
        i += 1;
    };
    // Two connections as the transport records them: marker, greeting, logon answer, the
    // echoes of the sent lines, then the gateway's bytes in uneven chunks.
    for (body, start) in [
        (stream_body(), (window_start() as u64) * 1_000_000),
        (second, (ms(T0).floor_to(MIN).0 as u64) * 1_000_000),
    ] {
        put(CONNECTION_MARKER);
        put(GREETING);
        put(SUCCESS);
        for p in plan(start) {
            put(&p);
        }
        for chunk in body.chunks(97) {
            put(chunk);
        }
    }
    w.finish().unwrap();

    let replay = || {
        lines_feed::extension::register(Arc::new(
            lines_databento::live::DatabentoExtension::from_env(),
        ))
        .unwrap();
        let mut feed = lines_feed::spawn(FeedConfig::new(Source::Replay {
            path: path.clone(),
            speed: 0.0,
            stop_at: None,
        }))
        .unwrap();
        let t0 = std::time::Instant::now();
        loop {
            match feed.store.read().status.clone() {
                FeedStatus::ReplayFinished => break,
                FeedStatus::Failed(e) => panic!("replay failed: {e}"),
                _ => {}
            }
            assert!(t0.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(5));
        }
        let d = feed.store.read();
        let mut book = Vec::new();
        for side in [Side::Bid, Side::Ask] {
            for (p, l) in d.book.levels(side) {
                book.push((side, p.0, l.qty.0));
            }
        }
        let out = (d.series.bars().to_vec(), book, d.stats.rejected, d.history);
        drop(d);
        feed.shutdown();
        out
    };
    let a = replay();
    assert_eq!(a, replay(), "same recording, same result");
    let (bars, book, rejected, history) = a;
    assert_eq!(rejected, 0);
    // The replay's feed asks for 3 days; the recorded replay covers less: a gap.
    assert!(
        matches!(history, HistoryState::Incomplete { .. }),
        "{history:?}"
    );
    assert_eq!(book, [(Side::Bid, 599_975, 5), (Side::Ask, 600_000, 7)]);
    // The second connection's replay re-delivers four trades: counted once (snapshot +
    // stitch), plus its one new live trade.
    assert_eq!(
        bars.iter().map(|b| b.volume.0).sum::<i64>(),
        2 + 1 + 4 + 1 + 3
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The real TCP transport against a fake gateway on loopback: the connection marker first,
/// the challenge answered with the key's hash and bucket id, the key itself never sent, and
/// each sent line (not the auth line) echoed as a recorded frame.
#[test]
fn end_to_end_against_a_fake_gateway() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let body = dbn(|e| {
            e.encode_record(&mapping()).unwrap();
            e.encode_record(&def(TICK)).unwrap();
            e.encode_record(&replay_done(T0)).unwrap();
            e.encode_record(&replay_done(T0)).unwrap();
            e.encode_record(&trade(T0 + 1_000_000, PX, 2, b'B'))
                .unwrap();
        });
        let server = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            let (r, mut w) = sock.into_split();
            let mut r = BufReader::new(r);
            w.write_all(GREETING).await.unwrap();
            let mut received = Vec::new();
            let mut line = String::new();
            r.read_line(&mut line).await.unwrap();
            let auth_ok =
                line.starts_with(&format!("auth={}|", cram_response("CHALLENGE-123", KEY)));
            received.push(line.clone());
            w.write_all(SUCCESS).await.unwrap();
            for _ in 0..4 {
                line.clear();
                r.read_line(&mut line).await.unwrap();
                received.push(line.clone());
            }
            w.write_all(&body).await.unwrap();
            w.shutdown().await.unwrap();
            (auth_ok, received)
        });
        let mut t = LiveTransport::connect(&addr, ApiKey::new(KEY.into()).unwrap())
            .await
            .unwrap();
        let mut s = cold();
        let mut events = Vec::new();
        let mut out = Vec::new();
        let mut frames = Vec::new();
        s.start(ctx(None), &mut out);
        while let Some(f) = t.recv().await.unwrap() {
            frames.push(f.data.to_vec());
            s.on_frame(&f, now(), &mut events, &mut out).unwrap();
            for o in out.drain(..) {
                if let Outgoing::Frame(f) = o {
                    t.send(f).await.unwrap();
                }
            }
        }
        let (auth_ok, received) = server.await.unwrap();
        assert!(auth_ok, "{received:?}");
        assert!(
            received.iter().all(|l| !l.contains(KEY)),
            "the key never leaves"
        );
        assert_eq!(received[4], "start_session\n");
        assert_eq!(frames[0], CONNECTION_MARKER);
        let echoes: Vec<&Vec<u8>> = frames
            .iter()
            .filter(|f| f.starts_with(SENT_PREFIX))
            .collect();
        assert_eq!(echoes.len(), 4, "the four sent lines, not the auth line");
        assert!(
            frames
                .iter()
                .all(|f| !String::from_utf8_lossy(f).contains("auth=")),
            "the auth line is never recorded"
        );
        // Both replays complete before the trade: it is live.
        assert!(
            events
                .iter()
                .any(|e| matches!(e, MarketEvent::Trade { qty: Qty(2), .. }))
        );
    });
}

/// An aged-out replay whose message names no start: the floor is now, so the next connection
/// doesn't ask for the same failing start again.
#[test]
fn an_aged_out_replay_without_a_named_start_stops_replaying() {
    let floors = Floors::default();
    let aged = dbn(|e| {
        e.encode_record(&ErrorMsg::new(
            T0,
            Some(ErrorCode::ReplayDataAgedOut),
            "replay data aged out",
            true,
        ))
        .unwrap();
    });
    let (events, _, err) = run_conn(
        &mut session(&floors, ReplayLimit::default()),
        &connection(aged),
    );
    assert!(err.is_some());
    assert!(events.contains(&MarketEvent::HistoryGap {
        from: UtcMillis(window_start()),
        to: now(),
    }));
    let (_, out, _) = run(
        &mut session(&floors, ReplayLimit::default()),
        &[GREETING, SUCCESS],
    );
    assert_eq!(out[2], trades_line(now().floor_to(MIN).0));
}
