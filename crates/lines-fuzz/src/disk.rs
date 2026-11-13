//! Harness for the local market-data store (decision 0019): segment files, the coverage file
//! and the bar cache as they are found on disk after a crash, a full disk or a bad sector.
//!
//! The store's readers take paths, so each input is written to its own scratch
//! directory (removed when the run returns) and opened the way the feed opens it ([`ContractWriter::open`]: torn-tail
//! truncation, coverage, cached bars or a rebuild from segments), then the cached bars are
//! loaded into a chart store as the feed does.
//!
//! Input: `cfg:u8 op*` ([`crate::frames`]). Op kinds:
//! - 0: a segment block with a correct CRC (payload = `i64 time_us | lines-events-1`),
//! - 1: the same with a wrong CRC,
//! - 2: raw bytes appended to the segment (garbage, torn tails),
//! - 3: text appended to the coverage file.
//!
//! Config bit 0: the segment header is not written by the harness (the ops start with it);
//! bit 1: a bar cache is written from the coverage bytes (magic + CRC fixed up, so its
//! contents get past the checksum).

use std::io::Write;
use std::sync::Arc;

use lines_core::CivilDate;
use lines_core::scratch::Scratch;
use lines_feed::catalog;
use lines_feed::disk::segment::{FlushPolicy, SegmentHeader, SegmentReader, SegmentWriter};
use lines_feed::disk::writer::ContractWriter;
use lines_feed::disk::{ContractDir, StoreConfig};
use lines_feed::native::{self, NativeProvider};
use lines_feed::provider::Provider;
use lines_feed::store::ChartStore;

use crate::{PERIOD_MS, frames};

const SOURCE: &str = "fuzz";
const MAX_BLOCKS: u64 = 100_000;

pub fn date() -> CivilDate {
    CivilDate::new(2026, 11, 6).expect("date")
}

pub fn header() -> SegmentHeader {
    SegmentHeader {
        created_wall_ms: crate::BASE_MS,
        source: SOURCE.into(),
        encoding: native::PROTOCOL.into(),
        product: "NQ".into(),
        contract: None,
        date: date(),
    }
}

/// One block as the writer frames it: `u32 len | u32 crc | payload`.
pub fn block(payload: &[u8], good_crc: bool) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + payload.len());
    v.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    let crc = crc32(payload);
    v.extend_from_slice(&(if good_crc { crc } else { !crc }).to_le_bytes());
    v.extend_from_slice(payload);
    v
}

/// A bar cache file around `body` (everything after the magic), CRC appended.
pub fn bar_cache(body: &[u8]) -> Vec<u8> {
    let mut v = b"LINESBAR".to_vec();
    v.extend_from_slice(body);
    let crc = crc32(&v);
    v.extend_from_slice(&crc.to_le_bytes());
    v
}

/// CRC-32 (IEEE, zlib polynomial), as the store uses; bitwise, harness-only.
pub fn crc32(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
    }
    !c
}

pub fn disk_store(data: &[u8]) {
    let (cfg, ops) = frames::split(data);
    // Removed when this returns, early returns included.
    let root = Scratch::new("fuzz-disk");
    let nq = catalog::get(catalog::NQ).expect("NQ");
    let dir = ContractDir::new(&root, SOURCE, &nq, None);
    if std::fs::create_dir_all(&dir.path).is_err() {
        return;
    }
    let seg = dir.segment_path(date());

    // Lay the files out.
    let mut file = Vec::new();
    if cfg & 1 == 0 {
        match SegmentWriter::open(&seg, &header(), FlushPolicy::default()) {
            Ok((w, _)) => {
                if w.close().is_err() {
                    return;
                }
            }
            Err(_) => return,
        }
    }
    let mut coverage = Vec::new();
    for op in frames::Ops::new(ops) {
        match op.frame.as_ref().map(|f| f.kind) {
            Some(lines_feed::provider::FrameKind::Binary) => {
                file.extend_from_slice(&block(&op.payload, true))
            }
            Some(lines_feed::provider::FrameKind::Text) => {
                file.extend_from_slice(&block(&op.payload, false))
            }
            Some(lines_feed::provider::FrameKind::Http) => file.extend_from_slice(&op.payload),
            None => coverage.extend_from_slice(&op.payload),
        }
    }
    let ok = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&seg)
        .and_then(|mut f| f.write_all(&file));
    if ok.is_err() || std::fs::write(dir.coverage_path(), &coverage).is_err() {
        return;
    }
    if cfg & 2 != 0 && std::fs::write(dir.bars_path(), bar_cache(&coverage)).is_err() {
        return;
    }

    // A plain reader, as replay uses.
    if let Ok(mut r) = SegmentReader::open(&seg) {
        let mut events = Vec::new();
        while r.blocks < MAX_BLOCKS {
            match r.next_block() {
                Ok(Ok(b)) => {
                    events.clear();
                    let _ = native::decode(&b.events, &mut events);
                }
                _ => break,
            }
        }
    }
    let _ = lines_feed::disk::replay::header(&dir.path);
    let _ = dir.trade_coverage();

    // Open the store for writing as the feed does, then show its cached bars.
    let store = Arc::new(ChartStore::new(
        nq.clone(),
        None,
        NativeProvider.capabilities(),
        NativeProvider.id(),
        PERIOD_MS,
        5_000,
    ));
    let config = StoreConfig::new(root.to_path_buf(), SOURCE);
    if let Ok((writer, opened)) = ContractWriter::open(&config, &nq, None, store.clone()) {
        store.update(|d| d.load_cached(&opened.bars));
        drop(writer);
    }
}
