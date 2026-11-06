//! Seed corpora, built in code from synthetic data only (decision 0010): the in-process CQG
//! mock, hand-written Coinbase-shaped JSON, encoded `lines-events-1` frames and synthetic
//! Sierra files. Nothing here comes from a real feed or a real recording.

use std::time::Duration;

use lines_core::{ContractKey, Month, Price, Qty, Side, UtcMillis};
use lines_feed::catalog;
use lines_feed::coinbase::CoinbaseProvider;
use lines_feed::cqg::mock::{MockConfig, MockTransport, SharedMockMarket};
use lines_feed::cqg::{CqgProvider, LogonParams};
use lines_feed::native::{self, Aggressor, NativeEvent};
use lines_feed::provider::{Frame, FrameKind, Outgoing, Provider, StartContext};
use lines_feed::recording::{Direction, Header, Writer};
use lines_feed::transport::Transport;
use lines_sierra::format::synth::{depth_header, depth_record, scid_header, scid_record, tick};
use lines_sierra::format::{FIRST_SUB_TRADE, LAST_SUB_TRADE, ScidRecord, unix_us_to_sc};

use crate::{BASE_MS, PERIOD_MS, frames};

/// One seed: target name, file name, bytes.
pub struct Seed {
    pub target: &'static str,
    pub name: String,
    pub data: Vec<u8>,
}

fn seed(target: &'static str, name: impl Into<String>, data: Vec<u8>) -> Seed {
    Seed {
        target,
        name: name.into(),
        data,
    }
}

pub const TARGETS: [&str; 7] = [
    "cqg_decode",
    "cqg_session",
    "coinbase_session",
    "native_session",
    "recording",
    "sierra_import",
    "disk_store",
];

/// Every target's seeds.
pub fn all() -> Vec<Seed> {
    let mut v = Vec::new();
    let cqg = cqg_mock_frames(120);
    for (i, f) in cqg.iter().enumerate().take(40) {
        v.push(seed("cqg_decode", format!("mock-{i:03}"), f.data.to_vec()));
    }
    for (cfg, n) in [(1u8, 12usize), (1, 120), (0, 60)] {
        v.push(seed(
            "cqg_session",
            format!("mock-cfg{cfg}-{n}"),
            session_input(cfg, &cqg[..n.min(cqg.len())]),
        ));
    }
    let cb = coinbase_script();
    v.push(seed("coinbase_session", "script", encode_ops(0, &cb)));
    v.push(seed("coinbase_session", "script-eth", encode_ops(1, &cb)));
    v.push(seed(
        "coinbase_session",
        "pre-url-candles",
        frames::encode(
            0,
            &[(
                Some(FrameKind::Http),
                0,
                b"[[1793368740,10.5,11,10.75,11,2.5],[1793368680,10,10.5,10,10.25,1]]",
            )],
        ),
    ));
    let nat = native_frames();
    v.push(seed("native_session", "events", session_input(0, &nat)));
    v.push(seed("native_session", "events-btc", session_input(3, &nat)));

    v.push(seed(
        "recording",
        "cqg-v3",
        lfr(
            "cqg",
            &CqgProvider::new(LogonParams::default()).protocol(),
            "ES",
            Some(esz26()),
            &cqg[..60],
        ),
    ));
    let cb_frames: Vec<Frame> = cb.iter().filter_map(|(_, _, f)| f.clone()).collect();
    v.push(seed(
        "recording",
        "coinbase-v3",
        lfr(
            "coinbase",
            &CoinbaseProvider.protocol(),
            "BTC/USD",
            Some(ContractKey::spot(catalog::BTC_USD)),
            &cb_frames,
        ),
    ));
    v.push(seed(
        "recording",
        "lines-v3",
        lfr(native::PROVIDER, native::PROTOCOL, "NQ", None, &nat),
    ));
    v.push(seed("recording", "lines-v2", lfr_v2("NQ", &nat)));

    v.push(seed("sierra_import", "ticks-depth", sierra_input(true)));
    v.push(seed("sierra_import", "ticks-only", sierra_input(false)));
    v.extend(disk_seeds(&nat));
    v
}

/// Store segments (blocks of `i64 time_us | lines-events-1`), coverage and bar caches.
fn disk_seeds(nat: &[Frame]) -> Vec<Seed> {
    let blocks: Vec<Vec<u8>> = nat
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let mut p = (BASE_MS * 1000 + i as i64 * 1_000_000)
                .to_le_bytes()
                .to_vec();
            p.extend_from_slice(&f.data);
            p
        })
        .collect();
    let coverage = format!(
        "# lines store coverage v1: kind from_ms to_ms (inclusive)
bars {} {}
trades {} {}
",
        BASE_MS,
        BASE_MS + 3_599_999,
        BASE_MS,
        BASE_MS + 120_000
    );
    let mut bars = Vec::new(); // bar cache body after the magic
    bars.extend_from_slice(&1u16.to_le_bytes());
    bars.extend_from_slice(&PERIOD_MS.to_le_bytes());
    bars.extend_from_slice(&2u32.to_le_bytes());
    for (i, p) in [(0i64, 2_412_875i64), (1, 2_412_900)] {
        for v in [BASE_MS + i * PERIOD_MS, p, p + 50, p - 25, p + 25, 7] {
            bars.extend_from_slice(&v.to_le_bytes());
        }
    }
    let header = segment_header_bytes();
    let op = |k: Option<FrameKind>, p: &[u8]| (k, 0u8, p.to_vec());
    let enc = |cfg: u8, ops: Vec<(Option<FrameKind>, u8, Vec<u8>)>| {
        let refs: Vec<_> = ops.iter().map(|(k, a, p)| (*k, *a, p.as_slice())).collect();
        frames::encode(cfg, &refs)
    };
    let mut good: Vec<_> = blocks
        .iter()
        .map(|b| op(Some(FrameKind::Binary), b))
        .collect();
    good.push(op(None, coverage.as_bytes()));
    let mut torn = good.clone();
    torn.push(op(Some(FrameKind::Http), &[9, 0, 0, 0, 1, 2]));
    let mut corrupt = good.clone();
    corrupt.insert(2, op(Some(FrameKind::Text), &blocks[2]));
    let mut cached: Vec<_> = blocks
        .iter()
        .map(|b| op(Some(FrameKind::Binary), b))
        .collect();
    cached.push(op(None, &bars));
    let mut raw_header = vec![op(Some(FrameKind::Http), &header)];
    raw_header.extend(blocks.iter().map(|b| op(Some(FrameKind::Binary), b)));
    vec![
        seed("disk_store", "segment", enc(0, good)),
        seed("disk_store", "torn-tail", enc(0, torn)),
        seed("disk_store", "corrupt-block", enc(0, corrupt)),
        seed("disk_store", "bar-cache", enc(2, cached)),
        seed("disk_store", "raw-header", enc(1, raw_header)),
    ]
}

/// The segment header the store writes (via its own writer, in a scratch directory).
fn segment_header_bytes() -> Vec<u8> {
    use lines_feed::disk::segment::{FlushPolicy, SegmentWriter};
    // Unique per call: tests build seeds on several threads at once.
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("lines-fuzz-seed-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("h.lseg");
    let (w, _) = SegmentWriter::open(&path, &crate::disk::header(), FlushPolicy::default())
        .expect("scratch segment");
    w.close().expect("close");
    let bytes = std::fs::read(&path).expect("read back");
    let _ = std::fs::remove_dir_all(&dir);
    bytes
}

fn esz26() -> ContractKey {
    ContractKey::monthly(catalog::ES, 2026, Month::new(12).expect("Z"))
}

fn session_input(cfg: u8, frames: &[Frame]) -> Vec<u8> {
    let ops: Vec<_> = frames
        .iter()
        .map(|f| (Some(f.kind), 0u8, f.data.as_ref()))
        .collect();
    frames::encode(cfg, &ops)
}

type ScriptOp = (Option<FrameKind>, u8, Option<Frame>);

fn encode_ops(cfg: u8, ops: &[ScriptOp]) -> Vec<u8> {
    let ops: Vec<_> = ops
        .iter()
        .map(|(k, a, f)| (*k, *a, f.as_ref().map_or(&[][..], |f| f.data.as_ref())))
        .collect();
    frames::encode(cfg, &ops)
}

fn ctx() -> StartContext {
    StartContext {
        now: UtcMillis(BASE_MS),
        history_from: None,
        bar_period_ms: PERIOD_MS,
        history_ms: 3 * 24 * 60 * 60_000,
    }
}

fn frozen_clock() -> UtcMillis {
    UtcMillis(BASE_MS)
}

/// Server frames from the CQG mock answering a real session: logon, symbol resolution, bar
/// history, then quotes, trades and depth.
pub fn cqg_mock_frames(n: usize) -> Vec<Frame> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    rt.block_on(async {
        let cfg = MockConfig {
            quotes_per_sec: 2_000,
            quotes_per_frame: 8,
            history_bars: 20,
            ..MockConfig::default()
        };
        let market = SharedMockMarket::new(&cfg, UtcMillis(BASE_MS));
        let mut t = MockTransport::with_clock(cfg, market.0, frozen_clock);
        let es = catalog::get(catalog::ES).expect("ES");
        let mut s = CqgProvider::new(LogonParams::default())
            .new_session(&es, Some(esz26()))
            .expect("session");
        let mut out = Vec::new();
        s.start(ctx(), &mut out);
        let mut frames = Vec::new();
        let mut events = Vec::new();
        while frames.len() < n {
            for o in out.drain(..) {
                if let Outgoing::Frame(f) = o {
                    t.send(f).await.expect("mock accepts session frames");
                }
            }
            let Ok(Ok(Some(f))) = tokio::time::timeout(Duration::from_secs(5), t.recv()).await
            else {
                break;
            };
            // Keep seeds small enough for the u16 frame length.
            if f.data.len() >= u16::MAX as usize {
                continue;
            }
            events.clear();
            s.on_frame(&f, UtcMillis(BASE_MS), &mut events, &mut out)
                .expect("session accepts mock frames");
            frames.push(f);
        }
        assert!(
            frames.len() >= 20,
            "the mock produced only {}",
            frames.len()
        );
        frames
    })
}

/// A Coinbase-shaped exchange: answers the session's own REST requests (captured from a
/// real session started like the harness starts it), then streams websocket messages.
fn coinbase_script() -> Vec<ScriptOp> {
    let btc = catalog::get(catalog::BTC_USD).expect("BTC");
    let mut s = CoinbaseProvider
        .new_session(&btc, Some(ContractKey::spot(btc.id)))
        .expect("session");
    let mut out = Vec::new();
    s.start(ctx(), &mut out);
    let mut script = Vec::new();
    let text = |j: &str| Some(Frame::text(j.to_owned()));
    script.push((
        Some(FrameKind::Text),
        0,
        text(
            r#"{"type":"subscriptions","channels":[{"name":"matches","product_ids":["BTC-USD"]}]}"#,
        ),
    ));
    // Answer every REST request in order: empty candle pages except the newest, then trades.
    let mut events = Vec::new();
    for _ in 0..40 {
        let Some(url) = out.iter().find_map(|o| match o {
            Outgoing::HttpGet(u) => Some(u.clone()),
            Outgoing::Frame(_) => None,
        }) else {
            break;
        };
        out.clear();
        let body = if url.contains("/candles") {
            // Rows outside the requested window are ignored by the session; keep them
            // plausible: [time, low, high, open, close, volume].
            "[[1793368740,75400.5,75420,75410.25,75415.71,2.5],[1793368680,75390,75410.5,75400,75405.25,1.125]]".to_owned()
        } else {
            r#"[{"time":"2026-11-06T14:00:02.5Z","trade_id":1002,"price":"75415.72","size":"0.01","side":"sell"},{"time":"2026-11-06T14:00:01Z","trade_id":1001,"price":"75415.71","size":"0.5","side":"buy"}]"#.to_owned()
        };
        let f = Frame::http(format!("{url}\n{body}"));
        events.clear();
        let _ = s.on_frame(&f, UtcMillis(BASE_MS), &mut events, &mut out);
        script.push((Some(FrameKind::Http), 0, Some(f)));
    }
    for j in [
        r#"{"type":"snapshot","product_id":"BTC-USD","bids":[["75415.71","0.5"],["75415.70","1.25"]],"asks":[["75415.72","0.00000001"],["75416","3"]]}"#,
        r#"{"type":"last_match","trade_id":1003,"maker_order_id":"a","taker_order_id":"b","side":"buy","size":"0.001","price":"75415.72","product_id":"BTC-USD","sequence":4,"time":"2026-11-06T14:00:03Z"}"#,
        r#"{"type":"ticker","price":"75415.72","best_bid":"75415.71","best_ask":"75415.72","product_id":"BTC-USD","sequence":5,"time":"2026-11-06T14:00:03.1Z"}"#,
        r#"{"type":"l2update","product_id":"BTC-USD","time":"2026-11-06T14:00:03.2Z","changes":[["buy","75415.71","0"],["sell","75415.73","2"]]}"#,
        r#"{"type":"match","trade_id":1004,"maker_order_id":"a","taker_order_id":"b","side":"sell","size":"0.25","price":"75415.70","product_id":"BTC-USD","sequence":6,"time":"2026-11-06T14:00:04.123456Z"}"#,
        r#"{"type":"match","trade_id":1007,"side":"buy","size":"1","price":"75416","product_id":"BTC-USD","sequence":9,"time":"2026-11-06T14:00:05Z"}"#,
        r#"{"type":"heartbeat","last_trade_id":1007,"product_id":"BTC-USD","sequence":10,"time":"2026-11-06T14:00:06Z"}"#,
    ] {
        script.push((Some(FrameKind::Text), 1, text(j)));
    }
    // Keep-alive ticks retry the id hole (1005..1006) by REST; answer it.
    script.push((None, 5, None));
    script.push((
        Some(FrameKind::Http),
        0,
        Some(Frame::http(
            "https://api.exchange.coinbase.com/products/BTC-USD/trades?after=1007&limit=1000\n\
             [{\"time\":\"2026-11-06T14:00:04.5Z\",\"trade_id\":1006,\"price\":\"75416\",\"size\":\"1\",\"side\":\"buy\"},\
             {\"time\":\"2026-11-06T14:00:04.4Z\",\"trade_id\":1005,\"price\":\"75415.9\",\"size\":\"2\",\"side\":\"buy\"}]",
        )),
    ));
    script.push((
        Some(FrameKind::Text),
        0,
        text(r#"{"type":"error","message":"Failed to subscribe","reason":"BTC-XYZ is not a valid product"}"#),
    ));
    script
}

/// `lines-events-1` frames: book snapshot, updates, BBO and trades.
fn native_frames() -> Vec<Frame> {
    let t0 = BASE_MS * 1000;
    let enc = |evs: &[NativeEvent]| {
        let mut b = Vec::new();
        for e in evs {
            native::encode(e, &mut b);
        }
        Frame::binary(b)
    };
    let level = |dt: i64, side, price, qty, orders| NativeEvent::BookLevel {
        time_us: t0 + dt,
        side,
        price: Price(price),
        qty: Qty(qty),
        orders,
    };
    vec![
        enc(&[
            NativeEvent::BookClear { time_us: t0 },
            level(0, Side::Bid, 2_412_875, 5, 3),
            level(0, Side::Bid, 2_412_850, 7, 4),
            level(0, Side::Ask, 2_412_900, 4, 2),
            level(0, Side::Ask, 2_412_925, 9, 5),
        ]),
        enc(&[NativeEvent::Trade {
            time_us: t0 + 1_500,
            price: Price(2_412_900),
            qty: Qty(2),
            aggressor: Aggressor::Buy,
        }]),
        enc(&[
            level(2_000, Side::Ask, 2_412_900, 2, 1),
            NativeEvent::Bbo {
                time_us: t0 + 2_000,
                bid: Some(Price(2_412_875)),
                ask: Some(Price(2_412_900)),
            },
        ]),
        enc(&[NativeEvent::Trade {
            time_us: t0 + 61_000_000,
            price: Price(2_412_875),
            qty: Qty(1),
            aggressor: Aggressor::Sell,
        }]),
        enc(&[
            level(62_000_000, Side::Bid, 2_412_875, 0, 0),
            NativeEvent::Bbo {
                time_us: t0 + 62_000_000,
                bid: None,
                ask: Some(Price(2_412_900)),
            },
        ]),
    ]
}

fn lfr(
    provider: &str,
    protocol: &str,
    product: &str,
    contract: Option<ContractKey>,
    frames: &[Frame],
) -> Vec<u8> {
    let header = Header {
        start_wall_ms: BASE_MS,
        provider: provider.into(),
        protocol: protocol.into(),
        product: product.into(),
        contract,
        version: lines_feed::recording::FORMAT_VERSION,
    };
    let mut w = Writer::new(Vec::new(), &header).expect("in memory");
    for (i, f) in frames.iter().enumerate() {
        w.write(
            Direction::FromServer,
            Duration::from_millis(i as u64 * 10),
            f,
        )
        .expect("in memory");
    }
    // One client frame too: replay skips them.
    w.write(Direction::ToServer, Duration::ZERO, &Frame::text("{}"))
        .expect("in memory");
    w.finish().expect("in memory")
}

/// A v2 file (no contract in the header) written by hand: still readable.
fn lfr_v2(product: &str, frames: &[Frame]) -> Vec<u8> {
    let v3 = lfr(native::PROVIDER, native::PROTOCOL, product, None, frames);
    // v3 = magic | version | start | 3 strings | contract byte (0 = unknown) | records.
    let strings = [native::PROVIDER, native::PROTOCOL, product]
        .iter()
        .map(|s| 2 + s.len())
        .sum::<usize>();
    let contract_at = 8 + 2 + 8 + strings;
    assert_eq!(v3[contract_at], 0);
    let mut v2 = v3[..contract_at].to_vec();
    v2[8..10].copy_from_slice(&2u16.to_le_bytes());
    v2.extend_from_slice(&v3[contract_at + 1..]);
    v2
}

/// Synthetic NQ ticks and depth on 2025-09-11 (prices ×100 as f32, like Sierra's NQ files).
fn sierra_input(with_depth: bool) -> Vec<u8> {
    let day_us = 1_757_548_800_000_000i64; // 2025-09-11T00:00:00Z
    let sc = |s: i64, ms: i64| unix_us_to_sc(day_us + s * 1_000_000 + ms * 1000);
    let px = |p: f64| (p * 100.0) as f32;
    let mut scid = scid_header();
    let mut t = |r: ScidRecord| scid.extend(scid_record(&r));
    t(tick(
        sc(-10, 0),
        px(24000.0),
        px(23999.75),
        px(24000.0),
        1,
        true,
    ));
    t(tick(
        sc(0, 500),
        px(24128.75),
        px(24128.75),
        px(24129.0),
        2,
        false,
    ));
    t(tick(
        sc(1, 500),
        px(24129.0),
        px(24128.75),
        px(24129.0),
        1,
        true,
    ));
    t(ScidRecord {
        open: FIRST_SUB_TRADE,
        ..tick(sc(7, 0), px(24129.5), px(24128.75), px(24129.5), 1, true)
    });
    t(tick(
        sc(7, 0),
        px(24129.75),
        px(24128.75),
        px(24129.5),
        1,
        true,
    ));
    t(ScidRecord {
        open: LAST_SUB_TRADE,
        ..tick(sc(7, 0), px(24130.0), px(24128.75), px(24129.5), 2, true)
    });
    t(ScidRecord {
        open: 100.0,
        ..tick(sc(8, 0), px(1.0), 0.0, 0.0, 1, true)
    });
    t(tick(sc(86_400, 0), px(1.0), 0.0, 0.0, 1, true));

    let mut d = depth_header();
    let mut r = |time, cmd, flags, orders, price, qty| {
        d.extend(depth_record(time, cmd, flags, orders, price, qty))
    };
    r(sc(1, 0), 1, 0, 0, 0.0, 0);
    r(sc(1, 0), 2, 0, 1, px(24128.75), 5);
    r(sc(1, 0), 3, 1, 1, px(24129.0), 4);
    r(sc(2, 0), 7, 0, 0, px(24129.0), 0);
    r(sc(2, 0), 3, 1, 2, px(24129.5), 6);
    r(sc(3, 0), 4, 0, 0, px(24128.75), 9);
    r(sc(3, 0) - 5_000, 6, 0, 0, px(24100.0), 0);
    r(sc(3, 0), 9, 1, 0, px(24128.0), 1);
    r(sc(5, 0), 5, 0, 0, px(24131.0), 2);

    let mut v = vec![with_depth as u8];
    v.extend_from_slice(&(scid.len() as u32).to_le_bytes());
    v.extend_from_slice(&scid);
    if with_depth {
        v.extend_from_slice(&d);
    }
    v
}
