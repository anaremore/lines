//! Fuzz harnesses for every decoder of untrusted bytes (roadmap "Parser fuzzing").
//!
//! Each harness takes arbitrary bytes and must return normally: no panic, no unbounded
//! allocation, no endless loop, whatever the input. They run the *real* pipeline, not just
//! the parser: decoded frames go through the provider session and into the chart store, as
//! live and replay do, so arithmetic on hostile values (prices, times, sizes) is covered too.
//!
//! The same functions back the libFuzzer targets (`/fuzz/fuzz_targets/`, nightly, their own
//! workspace) and the stable smoke tests (`tests/smoke.rs`, every PR). Seeds come from [`seeds`]: synthetic only
//! (decision 0010).

pub mod disk;
pub mod frames;
pub mod seeds;

use std::io::Cursor;

use lines_core::{ContractKey, Month, Product, UtcMillis};
use lines_feed::catalog;
use lines_feed::coinbase::CoinbaseProvider;
use lines_feed::cqg::{CqgProvider, LogonParams};
use lines_feed::kraken::KrakenProvider;
use lines_feed::native::{self, NativeProvider};
use lines_feed::provider::{self, FrameKind, Provider, StartContext};
use lines_feed::recording::Reader;
use lines_feed::store::ChartStore;
use lines_feed::{coinbase, cqg, kraken};
use lines_sierra::format::{DepthReader, ScidReader};
use prost::Message;

/// Session clock at the start of every run (any fixed instant works; seeds are built
/// around it).
pub const BASE_MS: i64 = 1_793_368_800_000; // 2026-11-06T14:00:00Z
const PERIOD_MS: i64 = 60_000;
/// Bounds that keep one input's work proportional to its size.
const MAX_RECONNECTS: usize = 8;
const MAX_RECORDS: usize = 20_000;

pub use disk::disk_store;

/// Raw protobuf decode of one CQG `ServerMsg`.
pub fn cqg_decode(data: &[u8]) {
    let _ = cqg_proto::web_api_2::ServerMsg::decode(data);
}

/// A CQG session (ES) fed a sequence of frames ([`frames`]), events applied to a store.
/// Config bit 0: session knows its contract (ESZ26) or not (pre-v3 replay).
pub fn cqg_session(data: &[u8]) {
    let (cfg, ops) = frames::split(data);
    let es = catalog::get(catalog::ES).expect("ES");
    let contract =
        (cfg & 1 == 1).then(|| ContractKey::monthly(es.id, 2026, Month::new(12).expect("Z")));
    let provider = CqgProvider::new(LogonParams::default());
    drive(&provider, es, contract, frames::Ops::new(ops));
}

/// A Coinbase session (BTC/USD, or ETH/USD with config bit 0) fed websocket text and HTTP
/// bodies.
pub fn coinbase_session(data: &[u8]) {
    let (cfg, ops) = frames::split(data);
    let id = if cfg & 1 == 1 {
        catalog::ETH_USD
    } else {
        catalog::BTC_USD
    };
    let product = catalog::get(id).expect("spot product");
    let contract = Some(ContractKey::spot(id));
    drive(&CoinbaseProvider, product, contract, frames::Ops::new(ops));
}

/// Kraken: spot (WebSocket v2 JSON, checksummed book, REST AssetPairs/OHLC/Trades) or
/// perpetuals (Futures feed JSON, sequenced book, REST instruments/charts/history), chosen by
/// config bits 0-1 (BTC spot, ETH spot, BTC perp, ETH perp).
pub fn kraken_session(data: &[u8]) {
    let (cfg, ops) = frames::split(data);
    let id = [
        catalog::BTC_USD_KR,
        catalog::ETH_USD_KR,
        catalog::BTC_PERP_KR,
        catalog::ETH_PERP_KR,
    ][cfg as usize % 4];
    let product = catalog::get(id).expect("Kraken product");
    let contract = Some(ContractKey::spot(id));
    drive(
        &KrakenProvider::default(),
        product,
        contract,
        frames::Ops::new(ops),
    );
}

/// `lines-events-1` frames through the native (replay-only) session.
pub fn native_session(data: &[u8]) {
    let (cfg, ops) = frames::split(data);
    let id = [
        catalog::NQ,
        catalog::ES,
        catalog::MNQ,
        catalog::BTC_USD,
        catalog::SPY,
    ][cfg as usize % 5];
    let product = catalog::get(id).expect("product");
    drive(&NativeProvider, product, None, frames::Ops::new(ops));
    // The bare decoder too, on the whole input.
    let mut out = Vec::new();
    let _ = native::decode(data, &mut out);
}

/// A whole `.lfr` file: header, records, then a replay of its server frames through the
/// provider the header names (as `--source replay` does).
pub fn recording(data: &[u8]) {
    let Ok(mut reader) = Reader::new(data) else {
        return;
    };
    let h = reader.header.clone();
    let mut frames = Vec::new();
    while frames.len() < MAX_RECORDS {
        match reader.next_record() {
            Ok(Some(r)) => {
                if r.direction == lines_feed::recording::Direction::FromServer {
                    frames.push(r.frame);
                }
            }
            Ok(None) | Err(_) => break,
        }
    }
    let provider: Box<dyn Provider> = match h.provider.as_str() {
        provider::CQG if cqg::protocol_compatible(&h.protocol) => {
            Box::new(CqgProvider::new(LogonParams::default()))
        }
        provider::COINBASE if coinbase::protocol_compatible(&h.protocol) => {
            Box::new(CoinbaseProvider)
        }
        native::PROVIDER if native::protocol_compatible(&h.protocol) => Box::new(NativeProvider),
        provider::KRAKEN if kraken::protocol_compatible(&h.protocol) => {
            Box::new(KrakenProvider::default())
        }
        _ => return,
    };
    let Some(product) = catalog::by_symbol(&h.product) else {
        return;
    };
    if h.contract.is_some_and(|k| k.product != product.id) || !provider.supports(&product) {
        return;
    }
    let ops = frames.into_iter().map(|f| frames::Op {
        advance_ms: 0,
        payload: f.data.clone(),
        frame: Some(f),
    });
    drive(provider.as_ref(), product, h.contract, ops);
}

/// Sierra `.scid` + `.depth` bytes through the importer, in memory. Layout: config byte
/// (bit 0: a depth file exists), `u32` LE length of the `.scid` part, the `.scid` bytes, then
/// the `.depth` bytes. Symbol `NQZ5.CME`, day 2025-09-11 (what the seeds use).
///
/// Whatever the importer writes must read back as a valid recording of valid
/// `lines-events-1` frames.
pub fn sierra_import(data: &[u8]) {
    let Some((&cfg, rest)) = data.split_first() else {
        return;
    };
    let Some((len, rest)) = rest.split_first_chunk::<4>() else {
        return;
    };
    let n = (u32::from_le_bytes(*len) as usize).min(rest.len());
    let (scid, depth) = rest.split_at(n);
    let Ok(scid) = ScidReader::new(Cursor::new(scid)) else {
        return;
    };
    let depth = if cfg & 1 == 1 {
        match DepthReader::new(depth) {
            Ok(d) => Some(d),
            Err(_) => return,
        }
    } else {
        None
    };
    let src = lines_sierra::ImportSource {
        symbol: "NQZ5.CME",
        date: "2025-09-11",
        price_multiplier: None,
    };
    let Ok((_report, out)) = lines_sierra::import_from(&src, scid, depth, || Ok(Vec::new())) else {
        return;
    };
    let mut r = Reader::new(&out[..]).expect("importer output has a valid header");
    let mut events = Vec::new();
    while let Some(rec) = r.next_record().expect("importer output has valid records") {
        assert_eq!(rec.frame.kind, FrameKind::Binary);
        events.clear();
        native::decode(&rec.frame.data, &mut events).expect("importer output decodes");
    }
}

/// Run frames through a fresh session and store the way the connection driver does,
/// reconnecting (new session, same store) after a session error, a bounded number of times.
fn drive(
    provider: &dyn Provider,
    product: Product,
    contract: Option<ContractKey>,
    ops: impl IntoIterator<Item = frames::Op>,
) -> ChartStore {
    let store = ChartStore::new(
        product.clone(),
        contract,
        provider.capabilities(),
        provider.id(),
        PERIOD_MS,
        5_000,
    );
    let mut now = UtcMillis(BASE_MS);
    let mut reconnects = 0;
    let mut events = Vec::new();
    let mut out = Vec::new();
    let mut session = None;
    for op in ops {
        now = UtcMillis(now.0.saturating_add(op.advance_ms));
        let s = match &mut session {
            Some(s) => s,
            None => {
                if reconnects > MAX_RECONNECTS {
                    break;
                }
                reconnects += 1;
                let Ok(mut s) = provider.new_session(&product, contract) else {
                    break;
                };
                let history_from = store.update(|d| {
                    d.begin_connection();
                    d.series.last().map(|b| b.time)
                });
                out.clear();
                s.start(
                    StartContext {
                        now,
                        history_from,
                        bar_period_ms: PERIOD_MS,
                        history_ms: 3 * 24 * 60 * 60_000,
                    },
                    &mut out,
                );
                session.insert(s)
            }
        };
        events.clear();
        out.clear();
        let result = match &op.frame {
            Some(f) => s.on_frame(f, now, &mut events, &mut out),
            None => {
                let _ = s.keepalive(now);
                Ok(())
            }
        };
        store.update(|d| d.apply(&events));
        if result.is_err() {
            session = None;
        }
    }
    // What the UI reads after a frame.
    {
        let d = store.read();
        let _ = (d.series.len(), d.contract_label(), d.history.label());
    }
    store
}

/// How deep an input drove a session harness: (bars, book levels, decode errors). For tests
/// that check seeds reach real state rather than being rejected at the first frame.
#[doc(hidden)]
pub fn session_depth(target: &str, data: &[u8]) -> Option<(usize, usize, u64)> {
    let (cfg, ops) = frames::split(data);
    let ops = frames::Ops::new(ops);
    let store = match target {
        "cqg_session" => {
            let es = catalog::get(catalog::ES)?;
            let k = (cfg & 1 == 1)
                .then(|| ContractKey::monthly(es.id, 2026, Month::new(12).expect("Z")));
            drive(&CqgProvider::new(LogonParams::default()), es, k, ops)
        }
        "coinbase_session" => {
            let id = if cfg & 1 == 1 {
                catalog::ETH_USD
            } else {
                catalog::BTC_USD
            };
            drive(
                &CoinbaseProvider,
                catalog::get(id)?,
                Some(ContractKey::spot(id)),
                ops,
            )
        }
        "kraken_session" => {
            let id = [
                catalog::BTC_USD_KR,
                catalog::ETH_USD_KR,
                catalog::BTC_PERP_KR,
                catalog::ETH_PERP_KR,
            ][cfg as usize % 4];
            let p = catalog::get(id)?;
            drive(
                &KrakenProvider::default(),
                p,
                Some(ContractKey::spot(id)),
                ops,
            )
        }
        _ => return None,
    };
    let d = store.read();
    let book = d.book.len(lines_core::Side::Bid) + d.book.len(lines_core::Side::Ask);
    Some((d.series.len(), book, d.stats.decode_errors))
}
