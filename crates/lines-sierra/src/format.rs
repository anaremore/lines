//! Sierra Chart `.scid` (intraday) and `.depth` (market depth) file readers.
//!
//! Streaming only: buffered sequential reads plus positioned single-record reads for binary
//! search, so memory stays bounded regardless of file size (files are 1–6 GB).
//!
//! Layouts (little-endian) as verified against real files on 2026-11-06:
//! - `.scid`: 56-byte header (`SCID`, u32 header_size=56, u32 record_size=40, u16 version, zero
//!   reserved), 40-byte records `i64 time, f32 open, high, low, close, u32 num_trades,
//!   total_volume, bid_volume, ask_volume`.
//! - `.depth`: 64-byte header (`SCDD`, u32 header_size=64, u32 record_size=24, u32 version, zero
//!   reserved), 24-byte records `i64 time, u8 command, u8 flags, u16 num_orders, f32 price,
//!   u32 quantity, u32 reserved`.
//!
//! Times are `SCDateTimeMS`: i64 microseconds since 1899-12-30 00:00:00 UTC.

use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

/// Microseconds between 1899-12-30 and 1970-01-01 (25,569 days).
pub const SC_EPOCH_OFFSET_US: i64 = 25_569 * 86_400 * 1_000_000;

/// `open` marker on the first record of an unbundled trade (one aggressor order split into
/// its fills). Determined empirically (Sierra removed its format docs): on 2025-09-11 and
/// 2025-10-15 NQZ5 data it always opens a bracket closed by [`LAST_SUB_TRADE`], with 0–98
/// `open == 0` records between, mostly at different prices (sweeps). Not duplicates.
pub const FIRST_SUB_TRADE: f32 = f32::from_bits(0xfd70_9f01); // -1.99900197e37

/// `open` marker on the last record of an unbundled trade.
pub const LAST_SUB_TRADE: f32 = f32::from_bits(0xfd70_9ef9); // -1.99900095e37

/// Sierra time → Unix microseconds.
#[inline]
pub fn sc_to_unix_us(sc: i64) -> i64 {
    sc - SC_EPOCH_OFFSET_US
}

/// Unix microseconds → Sierra time.
#[inline]
pub fn unix_us_to_sc(us: i64) -> i64 {
    us + SC_EPOCH_OFFSET_US
}

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn i64_at(b: &[u8], o: usize) -> i64 {
    i64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}
fn f32_at(b: &[u8], o: usize) -> f32 {
    f32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

// ---------------------------------------------------------------------------------------
// .scid

pub const SCID_HEADER: usize = 56;
pub const SCID_RECORD: usize = 40;

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct ScidRecord {
    /// Sierra time (µs since 1899-12-30).
    pub time: i64,
    pub open: f32,
    pub high: f32,
    pub low: f32,
    pub close: f32,
    pub num_trades: u32,
    pub total_volume: u32,
    pub bid_volume: u32,
    pub ask_volume: u32,
}

impl ScidRecord {
    pub fn parse(b: &[u8]) -> ScidRecord {
        ScidRecord {
            time: i64_at(b, 0),
            open: f32_at(b, 8),
            high: f32_at(b, 12),
            low: f32_at(b, 16),
            close: f32_at(b, 20),
            num_trades: u32_at(b, 24),
            total_volume: u32_at(b, 28),
            bid_volume: u32_at(b, 32),
            ask_volume: u32_at(b, 36),
        }
    }

    /// How this record relates to Sierra's trade unbundling. `Bar` = not a tick record.
    pub fn kind(&self) -> TickKind {
        match self.open.to_bits() {
            0 => TickKind::Plain,
            b if b == FIRST_SUB_TRADE.to_bits() => TickKind::FirstSubTrade,
            b if b == LAST_SUB_TRADE.to_bits() => TickKind::LastSubTrade,
            _ => TickKind::Bar,
        }
    }

    pub fn is_tick(&self) -> bool {
        self.kind() != TickKind::Bar
    }
}

pub fn parse_scid_header(h: &[u8]) -> io::Result<u16> {
    if h.len() < SCID_HEADER || &h[0..4] != b"SCID" {
        return Err(bad("not a .scid file (magic)"));
    }
    let (hs, rs) = (u32_at(h, 4), u32_at(h, 8));
    if hs as usize != SCID_HEADER || rs as usize != SCID_RECORD {
        return Err(bad(format!(
            "unexpected .scid header_size {hs} / record_size {rs}"
        )));
    }
    Ok(u16::from_le_bytes([h[12], h[13]]))
}

/// Streaming `.scid` reader with binary search by time.
pub struct ScidReader<R: Read + Seek> {
    r: BufReader<R>,
    pub version: u16,
    pub records: u64,
    pos: u64,
}

impl ScidReader<File> {
    pub fn open(path: &Path) -> io::Result<Self> {
        ScidReader::new(File::open(path)?)
    }
}

impl<R: Read + Seek> ScidReader<R> {
    pub fn new(mut inner: R) -> io::Result<Self> {
        let len = inner.seek(SeekFrom::End(0))?;
        inner.seek(SeekFrom::Start(0))?;
        let mut r = BufReader::with_capacity(1 << 20, inner);
        let mut h = [0u8; SCID_HEADER];
        r.read_exact(&mut h)?;
        let version = parse_scid_header(&h)?;
        let body = len - SCID_HEADER as u64;
        if !body.is_multiple_of(SCID_RECORD as u64) {
            tracing::warn!("scid: {} trailing bytes ignored", body % SCID_RECORD as u64);
        }
        Ok(ScidReader {
            r,
            version,
            records: body / SCID_RECORD as u64,
            pos: 0,
        })
    }

    fn read_at(&mut self, idx: u64) -> io::Result<ScidRecord> {
        self.r.seek(SeekFrom::Start(
            SCID_HEADER as u64 + idx * SCID_RECORD as u64,
        ))?;
        let mut b = [0u8; SCID_RECORD];
        self.r.read_exact(&mut b)?;
        Ok(ScidRecord::parse(&b))
    }

    /// Position at the first record with `time >= sc_time` (records are time-ordered, so a
    /// lower-bound binary search: O(log n) positioned reads).
    pub fn seek_to_time(&mut self, sc_time: i64) -> io::Result<()> {
        let (mut lo, mut hi) = (0u64, self.records);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.read_at(mid)?.time < sc_time {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        self.pos = lo;
        self.r.seek(SeekFrom::Start(
            SCID_HEADER as u64 + lo * SCID_RECORD as u64,
        ))?;
        Ok(())
    }

    pub fn next_record(&mut self) -> io::Result<Option<ScidRecord>> {
        if self.pos >= self.records {
            return Ok(None);
        }
        let mut b = [0u8; SCID_RECORD];
        self.r.read_exact(&mut b)?;
        self.pos += 1;
        Ok(Some(ScidRecord::parse(&b)))
    }
}

/// Classification of a `.scid` record by its `open` field.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum TickKind {
    /// `open == 0`: a standalone trade, or a middle fill inside an unbundled-trade bracket.
    Plain,
    FirstSubTrade,
    LastSubTrade,
    /// Any other `open`: an aggregated bar record, not a tick.
    Bar,
}

// ---------------------------------------------------------------------------------------
// .depth

pub const DEPTH_HEADER: usize = 64;
pub const DEPTH_RECORD: usize = 24;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DepthCommand {
    ClearBook,
    AddBid,
    AddAsk,
    ModifyBid,
    ModifyAsk,
    DeleteBid,
    DeleteAsk,
    Unknown(u8),
}

impl DepthCommand {
    pub fn from_u8(c: u8) -> Self {
        match c {
            1 => DepthCommand::ClearBook,
            2 => DepthCommand::AddBid,
            3 => DepthCommand::AddAsk,
            4 => DepthCommand::ModifyBid,
            5 => DepthCommand::ModifyAsk,
            6 => DepthCommand::DeleteBid,
            7 => DepthCommand::DeleteAsk,
            x => DepthCommand::Unknown(x),
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct DepthRecord {
    pub time: i64,
    pub command: DepthCommand,
    pub flags: u8,
    pub num_orders: u16,
    pub price: f32,
    pub quantity: u32,
    pub reserved: u32,
}

impl DepthRecord {
    pub fn parse(b: &[u8]) -> DepthRecord {
        DepthRecord {
            time: i64_at(b, 0),
            command: DepthCommand::from_u8(b[8]),
            flags: b[9],
            num_orders: u16::from_le_bytes([b[10], b[11]]),
            price: f32_at(b, 12),
            quantity: u32_at(b, 16),
            reserved: u32_at(b, 20),
        }
    }

    pub fn end_of_batch(&self) -> bool {
        self.flags & 1 != 0
    }
}

pub fn parse_depth_header(h: &[u8]) -> io::Result<u32> {
    if h.len() < DEPTH_HEADER || &h[0..4] != b"SCDD" {
        return Err(bad("not a .depth file (magic)"));
    }
    let (hs, rs) = (u32_at(h, 4), u32_at(h, 8));
    if hs as usize != DEPTH_HEADER || rs as usize != DEPTH_RECORD {
        return Err(bad(format!(
            "unexpected .depth header_size {hs} / record_size {rs}"
        )));
    }
    Ok(u32_at(h, 12))
}

/// Streaming `.depth` reader yielding whole batches (records up to and including one with
/// the end-of-batch flag). A final batch without the flag is returned with `complete=false`.
pub struct DepthReader<R: Read> {
    r: BufReader<R>,
    pub version: u32,
}

impl DepthReader<File> {
    pub fn open(path: &Path) -> io::Result<Self> {
        DepthReader::new(File::open(path)?)
    }
}

impl<R: Read> DepthReader<R> {
    pub fn new(inner: R) -> io::Result<Self> {
        let mut r = BufReader::with_capacity(1 << 20, inner);
        let mut h = [0u8; DEPTH_HEADER];
        r.read_exact(&mut h)?;
        let version = parse_depth_header(&h)?;
        Ok(DepthReader { r, version })
    }

    /// Fill `batch` (cleared first). Returns `Ok(None)` at end of file, else `Some(complete)`.
    pub fn next_batch(&mut self, batch: &mut Vec<DepthRecord>) -> io::Result<Option<bool>> {
        batch.clear();
        let mut b = [0u8; DEPTH_RECORD];
        loop {
            match self.r.read_exact(&mut b) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    return Ok(if batch.is_empty() { None } else { Some(false) });
                }
                Err(e) => return Err(e),
            }
            let rec = DepthRecord::parse(&b);
            batch.push(rec);
            if rec.end_of_batch() {
                return Ok(Some(true));
            }
        }
    }
}

// ---------------------------------------------------------------------------------------
// Synthetic builders (used by tests in this crate; never real data).

#[cfg(test)]
pub(crate) mod synth {
    use super::*;

    pub fn scid_header() -> Vec<u8> {
        let mut h = vec![0u8; SCID_HEADER];
        h[0..4].copy_from_slice(b"SCID");
        h[4..8].copy_from_slice(&(SCID_HEADER as u32).to_le_bytes());
        h[8..12].copy_from_slice(&(SCID_RECORD as u32).to_le_bytes());
        h[12..14].copy_from_slice(&1u16.to_le_bytes());
        h
    }

    pub fn scid_record(r: &ScidRecord) -> Vec<u8> {
        let mut b = Vec::with_capacity(SCID_RECORD);
        b.extend_from_slice(&r.time.to_le_bytes());
        for f in [r.open, r.high, r.low, r.close] {
            b.extend_from_slice(&f.to_le_bytes());
        }
        for u in [r.num_trades, r.total_volume, r.bid_volume, r.ask_volume] {
            b.extend_from_slice(&u.to_le_bytes());
        }
        b
    }

    pub fn depth_header() -> Vec<u8> {
        let mut h = vec![0u8; DEPTH_HEADER];
        h[0..4].copy_from_slice(b"SCDD");
        h[4..8].copy_from_slice(&(DEPTH_HEADER as u32).to_le_bytes());
        h[8..12].copy_from_slice(&(DEPTH_RECORD as u32).to_le_bytes());
        h[12..16].copy_from_slice(&1u32.to_le_bytes());
        h
    }

    pub fn depth_record(
        time: i64,
        cmd: u8,
        flags: u8,
        orders: u16,
        price: f32,
        qty: u32,
    ) -> Vec<u8> {
        let mut b = Vec::with_capacity(DEPTH_RECORD);
        b.extend_from_slice(&time.to_le_bytes());
        b.push(cmd);
        b.push(flags);
        b.extend_from_slice(&orders.to_le_bytes());
        b.extend_from_slice(&price.to_le_bytes());
        b.extend_from_slice(&qty.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        b
    }

    pub fn tick(time: i64, price: f32, bid: f32, ask: f32, vol: u32, buy: bool) -> ScidRecord {
        ScidRecord {
            time,
            open: 0.0,
            high: ask,
            low: bid,
            close: price,
            num_trades: 1,
            total_volume: vol,
            bid_volume: if buy { 0 } else { vol },
            ask_volume: if buy { vol } else { 0 },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::synth::*;
    use super::*;
    use std::io::Cursor;

    #[test]
    fn time_conversion() {
        // 1970-01-01 is SC day 25569.
        assert_eq!(sc_to_unix_us(SC_EPOCH_OFFSET_US), 0);
        // 2025-09-11T00:03:48.061Z
        let unix_us = 1_757_549_028_061_000;
        assert_eq!(sc_to_unix_us(unix_us_to_sc(unix_us)), unix_us);
        assert_eq!(unix_us_to_sc(0), 2_209_161_600_000_000);
    }

    #[test]
    fn scid_header_validation() {
        assert_eq!(parse_scid_header(&scid_header()).unwrap(), 1);
        let mut h = scid_header();
        h[0] = b'X';
        assert!(parse_scid_header(&h).is_err());
        let mut h = scid_header();
        h[8] = 41;
        assert!(parse_scid_header(&h).is_err());
        assert!(parse_scid_header(&h[..10]).is_err());
    }

    #[test]
    fn scid_roundtrip_seek_and_header_only() {
        let mut file = scid_header();
        let recs: Vec<_> = (0..1000)
            .map(|i| tick(i * 10, 100.0 + i as f32, 99.0, 101.0, 1, i % 2 == 0))
            .collect();
        for r in &recs {
            file.extend(scid_record(r));
        }
        let mut rd = ScidReader::new(Cursor::new(file.clone())).unwrap();
        assert_eq!(rd.records, 1000);
        rd.seek_to_time(5_005).unwrap();
        assert_eq!(rd.next_record().unwrap().unwrap(), recs[501]);
        rd.seek_to_time(5_000).unwrap();
        assert_eq!(rd.next_record().unwrap().unwrap().time, 5_000);
        rd.seek_to_time(i64::MAX).unwrap();
        assert!(rd.next_record().unwrap().is_none());
        assert!(recs[0].is_tick());
        assert_eq!(
            ScidRecord {
                open: FIRST_SUB_TRADE,
                ..recs[0]
            }
            .kind(),
            TickKind::FirstSubTrade
        );
        assert_eq!(
            ScidRecord {
                open: LAST_SUB_TRADE,
                ..recs[0]
            }
            .kind(),
            TickKind::LastSubTrade
        );
        assert_eq!(FIRST_SUB_TRADE, -1.999_002e37_f32);
        assert_eq!(LAST_SUB_TRADE, -1.999_001e37_f32);
        assert!(
            !ScidRecord {
                open: 100.0,
                ..recs[0]
            }
            .is_tick()
        );
        // Header-only file (56 bytes, as on disk for some contracts).
        let mut empty = ScidReader::new(Cursor::new(scid_header())).unwrap();
        assert_eq!(empty.records, 0);
        assert!(empty.next_record().unwrap().is_none());
    }

    #[test]
    fn depth_batches_and_every_command() {
        let mut file = depth_header();
        for (cmd, flags) in [
            (1u8, 0u8),
            (2, 0),
            (3, 1),
            (4, 0),
            (5, 0),
            (6, 0),
            (7, 1),
            (9, 0),
        ] {
            file.extend(depth_record(cmd as i64, cmd, flags, 2, 100.25, 3));
        }
        let mut rd = DepthReader::new(Cursor::new(file)).unwrap();
        let mut b = Vec::new();
        assert_eq!(rd.next_batch(&mut b).unwrap(), Some(true));
        let cmds: Vec<_> = b.iter().map(|r| r.command).collect();
        assert_eq!(
            cmds,
            vec![
                DepthCommand::ClearBook,
                DepthCommand::AddBid,
                DepthCommand::AddAsk
            ]
        );
        assert_eq!(rd.next_batch(&mut b).unwrap(), Some(true));
        let cmds: Vec<_> = b.iter().map(|r| r.command).collect();
        assert_eq!(
            cmds,
            vec![
                DepthCommand::ModifyBid,
                DepthCommand::ModifyAsk,
                DepthCommand::DeleteBid,
                DepthCommand::DeleteAsk
            ]
        );
        assert_eq!((b[0].num_orders, b[0].price, b[0].quantity), (2, 100.25, 3));
        assert_eq!(
            rd.next_batch(&mut b).unwrap(),
            Some(false),
            "unterminated final batch"
        );
        assert_eq!(b[0].command, DepthCommand::Unknown(9));
        assert_eq!(rd.next_batch(&mut b).unwrap(), None);
    }

    #[test]
    fn depth_header_validation() {
        assert_eq!(parse_depth_header(&depth_header()).unwrap(), 1);
        let mut h = depth_header();
        h[4] = 65;
        assert!(parse_depth_header(&h).is_err());
        assert!(DepthReader::new(Cursor::new(b"SCDD".to_vec())).is_err());
    }
}
