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

pub const TARGETS: [&str; 8] = [
    "cqg_decode",
    "cqg_session",
    "coinbase_session",
    "native_session",
    "recording",
    "sierra_import",
    "disk_store",
    "kraken_session",
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
    // Audit findings 2 and 6: ids 0 and u64::MAX, and a heartbeat jumping 2^40 ids ahead
    // (filling it used to walk the id range), then a fill answer and keepalive ticks.
    let mut hostile = cb.clone();
    for j in [
        r#"{"type":"match","trade_id":0,"side":"buy","size":"1","price":"75416","time":"2026-11-06T14:00:07Z"}"#,
        r#"{"type":"match","trade_id":18446744073709551615,"side":"buy","size":"1","price":"75416","time":"2026-11-06T14:00:07Z"}"#,
        r#"{"type":"heartbeat","last_trade_id":18446744073709551615}"#,
        r#"{"type":"heartbeat","last_trade_id":1099511627776}"#,
        r#"{"type":"match","trade_id":1099511628000,"side":"buy","size":"1","price":"75416","time":"2026-11-06T14:00:08Z"}"#,
    ] {
        hostile.push((Some(FrameKind::Text), 1, Some(Frame::text(j.to_owned()))));
    }
    hostile.push((None, 5, None));
    hostile.push((
        Some(FrameKind::Http),
        0,
        Some(Frame::http(
            "https://api.exchange.coinbase.com/products/BTC-USD/trades?after=1099511628000&limit=1000
             [{\"time\":\"2026-11-06T14:00:07Z\",\"trade_id\":0,\"price\":\"1\",\"size\":\"1\"}]",
        )),
    ));
    v.push(seed(
        "coinbase_session",
        "hostile-ids",
        encode_ops(0, &hostile),
    ));
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
    // Decision 0028: option quotes, on SPY (its chain) and on ES (rejected: no options).
    let chain = chain_frames();
    v.push(seed(
        "native_session",
        "chain-spy",
        session_input(4, &chain),
    ));
    v.push(seed(
        "native_session",
        "chain-on-es",
        session_input(1, &chain),
    ));

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
    // Decision 0028: a v4 file (an option contract in the header) and an equity's.
    v.push(seed(
        "recording",
        "lines-v4-option",
        lfr(
            native::PROVIDER,
            native::PROTOCOL,
            "SPY",
            Some(ContractKey::option(
                catalog::SPY,
                lines_core::CivilDate::new(2026, 12, 18).unwrap(),
                Price(45_050),
                lines_core::Right::Put,
            )),
            &nat,
        ),
    ));
    v.push(seed(
        "recording",
        "lines-v4-equity",
        lfr(
            native::PROVIDER,
            native::PROTOCOL,
            "SPY",
            Some(ContractKey::equity(catalog::SPY)),
            &nat,
        ),
    ));
    // Audit finding 5: files from older builds hold outbound frames, a CQG logon among them;
    // replay skips them (new recordings write their time only, no bytes).
    v.push(seed(
        "recording",
        "cqg-old-outbound-logon",
        cqg_logon_lfr(&cqg[..20]),
    ));

    v.push(seed("sierra_import", "ticks-depth", sierra_input(true)));
    v.push(seed("sierra_import", "ticks-only", sierra_input(false)));
    v.extend(disk_seeds(&nat));
    let spot = kraken_script(catalog::BTC_USD_KR);
    let perp = kraken_script(catalog::BTC_PERP_KR);
    v.push(seed("kraken_session", "spot", encode_ops(0, &spot)));
    v.push(seed("kraken_session", "perp", encode_ops(2, &perp)));
    let protocol = lines_feed::kraken::KrakenProvider::default().protocol();
    let frames_of =
        |s: &[ScriptOp]| -> Vec<Frame> { s.iter().filter_map(|(_, _, f)| f.clone()).collect() };
    v.push(seed(
        "recording",
        "kraken-spot-v3",
        lfr(
            "kraken",
            &protocol,
            "BTC/USD.KR",
            Some(ContractKey::spot(catalog::BTC_USD_KR)),
            &frames_of(&spot),
        ),
    ));
    v.push(seed(
        "recording",
        "kraken-perp-v3",
        lfr(
            "kraken",
            &protocol,
            "BTC-PERP.KR",
            Some(ContractKey::spot(catalog::BTC_PERP_KR)),
            &frames_of(&perp),
        ),
    ));
    v
}

/// A Kraken-shaped exchange for BTC spot or the BTC perpetual: answers the session's own REST
/// requests (metadata, candles, trades) with synthetic bodies, then streams websocket messages
/// (book with a correct checksum for spot, sequenced deltas for futures, trades, ticker).
fn kraken_script(id: lines_core::ProductId) -> Vec<ScriptOp> {
    use lines_feed::kraken::num::book_checksum;
    let product = catalog::get(id).expect("Kraken product");
    let mut s = lines_feed::kraken::KrakenProvider::default()
        .new_session(&product, Some(ContractKey::spot(id)))
        .expect("session");
    let mut out = Vec::new();
    s.start(ctx(), &mut out);
    let secs = BASE_MS / 1000;
    let body_for = |url: &str| -> String {
        if url.contains("AssetPairs") {
            r#"{"error":[],"result":{"XXBTZUSD":{"altname":"XBTUSD","wsname":"XBT/USD","pair_decimals":1,"lot_decimals":8,"tick_size":"0.1"}}}"#.into()
        } else if url.contains("OHLC") {
            let row =
                |t: i64, p: &str| format!(r#"[{t},"{p}","{p}","{p}","{p}","{p}","0.50000000",3]"#);
            format!(
                r#"{{"error":[],"result":{{"XXBTZUSD":[{},{},{}],"last":{}}}}}"#,
                row(secs - 180, "75600.0"),
                row(secs - 120, "75601.5"),
                row(secs - 60, "75602.8"),
                secs - 60
            )
        } else if url.contains("Trades") {
            format!(
                r#"{{"error":[],"result":{{"XXBTZUSD":[["75602.8","0.10000000",{}.25,"b","l","",101],["75602.9","0.20000000",{}.5,"s","m","",102]],"last":"{}000000000"}}}}"#,
                secs + 1,
                secs + 2,
                secs + 2
            )
        } else if url.contains("instruments") {
            r#"{"result":"success","instruments":[{"symbol":"PF_ETHUSD","tickSize":0.1,"contractValueTradePrecision":3},{"symbol":"PF_XBTUSD","type":"flexible_futures","tickSize":1,"contractValueTradePrecision":4}]}"#.into()
        } else if url.contains("charts") {
            let c = |t: i64, p: &str| {
                format!(
                    r#"{{"time":{t},"open":"{p}","high":"{p}","low":"{p}","close":"{p}","volume":"0.25"}}"#
                )
            };
            format!(
                r#"{{"candles":[{},{}],"more_candles":false}}"#,
                c(BASE_MS - 120_000, "75600"),
                c(BASE_MS - 60_000, "75601")
            )
        } else if url.contains("lastTime") {
            // Older pages: nothing more (ends the backwards paging).
            r#"{"result":"success","history":[]}"#.into()
        } else {
            // Futures trade history, newest page.
            r#"{"result":"success","history":[{"time":"2026-11-06T14:00:02.000Z","trade_id":2,"price":75647,"size":0.0045,"side":"buy","type":"fill","uid":"h2"},{"time":"2026-11-06T14:00:01.000Z","trade_id":1,"price":75646,"size":0.0002,"side":"sell","type":"fill","uid":"h1"}]}"#.into()
        }
    };
    let mut script = Vec::new();
    let mut events = Vec::new();
    for _ in 0..40 {
        let Some(url) = out.iter().find_map(|o| match o {
            Outgoing::HttpGet(u) => Some(u.clone()),
            Outgoing::Frame(_) => None,
        }) else {
            break;
        };
        out.clear();
        let f = Frame::http(format!("{url}\n{}", body_for(&url)));
        events.clear();
        let _ = s.on_frame(&f, UtcMillis(BASE_MS), &mut events, &mut out);
        script.push((Some(FrameKind::Http), 0, Some(f)));
    }
    let text = |j: String| (Some(FrameKind::Text), 1u8, Some(Frame::text(j)));
    if lines_feed::kraken::is_futures(&product) {
        for j in [
            r#"{"feed":"book_snapshot","product_id":"PF_XBTUSD","seq":100,"bids":[{"price":75647.0,"qty":0.0001},{"price":75646.0,"qty":0.25}],"asks":[{"price":75648.0,"qty":0.1436}]}"#,
            r#"{"feed":"trade_snapshot","product_id":"PF_XBTUSD","trades":[{"uid":"b","side":"buy","time":1793368803000,"qty":0.0045,"price":75647.0,"seq":2},{"uid":"a","side":"sell","time":1793368802500,"qty":0.0002,"price":75646.0,"seq":1}]}"#,
            r#"{"feed":"book","product_id":"PF_XBTUSD","side":"buy","seq":101,"price":75611.0,"qty":0.5156}"#,
            r#"{"feed":"trade","product_id":"PF_XBTUSD","uid":"c","side":"sell","time":1793368804000,"qty":0.01,"price":75647.0}"#,
            r#"{"feed":"ticker","product_id":"PF_XBTUSD","bid":75647.0,"ask":75648.0,"last":75647.0}"#,
            r#"{"feed":"book","product_id":"PF_XBTUSD","side":"sell","seq":102,"price":75650.0,"qty":0}"#,
        ] {
            script.push(text(j.to_owned()));
        }
    } else {
        // Spot book: price units of 0.1, quantity units of 1e-8; the checksum covers the book.
        let check = book_checksum(
            [(756_029i64, 100_000_000i64), (756_037, 13_879_137)].into_iter(),
            [(756_028i64, 7_607_609i64)].into_iter(),
        );
        script.push(text(format!(
            r#"{{"channel":"book","type":"snapshot","data":[{{"symbol":"BTC/USD","bids":[{{"price":75602.8,"qty":0.07607609}}],"asks":[{{"price":75602.9,"qty":1.0}},{{"price":75603.7,"qty":0.13879137}}],"checksum":{check},"timestamp":"2026-11-06T14:00:03.000000Z"}}]}}"#
        )));
        let check2 = book_checksum(
            [(756_029i64, 100_000_000i64), (756_037, 13_879_137)].into_iter(),
            [(756_028i64, 55_008_441i64)].into_iter(),
        );
        script.push(text(format!(
            r#"{{"channel":"book","type":"update","data":[{{"symbol":"BTC/USD","bids":[{{"price":75602.8,"qty":0.55008441}}],"asks":[],"checksum":{check2},"timestamp":"2026-11-06T14:00:03.500000Z"}}]}}"#
        )));
        for j in [
            r#"{"channel":"trade","type":"snapshot","data":[{"symbol":"BTC/USD","side":"buy","price":75602.9,"qty":0.01,"ord_type":"market","trade_id":102,"timestamp":"2026-11-06T14:00:02.500000Z"}]}"#,
            r#"{"channel":"trade","type":"update","data":[{"symbol":"BTC/USD","side":"sell","price":75602.8,"qty":0.25,"ord_type":"limit","trade_id":103,"timestamp":"2026-11-06T14:00:04.000000Z"}]}"#,
            r#"{"channel":"ticker","type":"update","data":[{"symbol":"BTC/USD","bid":75602.8,"bid_qty":0.55,"ask":75602.9,"ask_qty":1.0,"last":75602.8,"volume":10.0,"vwap":75600.0,"low":75000.0,"high":76000.0,"change":1.0,"change_pct":0.01}]}"#,
            r#"{"channel":"heartbeat"}"#,
            r#"{"channel":"status","type":"update","data":[{"system":"online","api_version":"v2"}]}"#,
        ] {
            script.push(text(j.to_owned()));
        }
    }
    script.push((None, 30, None));
    script
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

/// A few SPY trades and option quotes (`lines-events-1` tag 5).
fn chain_frames() -> Vec<Frame> {
    let t0 = BASE_MS * 1000;
    let mut b = Vec::new();
    native::encode(
        &NativeEvent::Trade {
            time_us: t0,
            price: Price(68_412),
            qty: Qty(100),
            aggressor: Aggressor::Buy,
        },
        &mut b,
    );
    for (i, (strike, right)) in [
        (68_300, lines_core::Right::Call),
        (68_400, lines_core::Right::Call),
        (68_400, lines_core::Right::Put),
        (68_500, lines_core::Right::Put),
    ]
    .into_iter()
    .enumerate()
    {
        native::encode(
            &NativeEvent::OptionQuote {
                time_us: t0 + i as i64,
                key: lines_core::OptionKey {
                    expiry: lines_core::CivilDate::new(2026, 11, 9).unwrap(),
                    strike: Price(strike),
                    right,
                },
                quote: lines_core::OptionQuote {
                    bid: Some(Price(100 + i as i64)),
                    ask: Some(Price(103 + i as i64)),
                    last: (i % 2 == 0).then_some(Price(101)),
                    volume: Qty(1_000),
                    open_interest: 20_000,
                    iv_bp: Some(1_200),
                    delta_e4: Some(if right == lines_core::Right::Call {
                        5_000
                    } else {
                        -5_000
                    }),
                    time: UtcMillis(BASE_MS),
                },
            },
            &mut b,
        );
    }
    vec![Frame::binary(b)]
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

/// A CQG recording as builds before the fix wrote it: the session's outbound logon (with a
/// synthetic password) first, then inbound frames.
fn cqg_logon_lfr(inbound: &[Frame]) -> Vec<u8> {
    let es = catalog::get(catalog::ES).expect("ES");
    let provider = CqgProvider::new(LogonParams {
        user_name: Some("fuzz-user".into()),
        password: Some("fuzz-password".into()),
        ..Default::default()
    });
    let mut session = provider.new_session(&es, Some(esz26())).expect("session");
    let mut out = Vec::new();
    session.start(ctx(), &mut out);
    let header = Header {
        start_wall_ms: BASE_MS,
        provider: "cqg".into(),
        protocol: provider.protocol(),
        product: "ES".into(),
        contract: Some(esz26()),
        version: lines_feed::recording::FORMAT_VERSION,
    };
    let mut w = Writer::new(Vec::new(), &header).expect("in memory");
    for o in &out {
        if let Outgoing::Frame(f) = o {
            w.write(Direction::ToServer, Duration::ZERO, f)
                .expect("in memory");
        }
    }
    for (i, f) in inbound.iter().enumerate() {
        w.write(
            Direction::FromServer,
            Duration::from_millis(i as u64 * 10),
            f,
        )
        .expect("in memory");
    }
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
