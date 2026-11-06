//! Segment files: one contract, one trade date, append-only (decision 0019).
//!
//! Format v1 (little-endian):
//! ```text
//! header:  b"LINESEG\0" | u16 version=1 | i64 created_wall_ms
//!          | str source | str encoding | str product_symbol | contract | date
//!          str = u16 len + UTF-8; contract as in `.lfr` v3; date = u16 year | u8 month | u8 day
//! block:   u32 len | u32 crc32(payload) | payload
//! payload: i64 time_us | lines-events-1 events   (one atomic batch = one feed frame)
//! ```
//! `source` names who supplied the data (`coinbase`, `cqg`, …); `encoding` is
//! `lines-events-1`. A block is valid only if it is complete and its CRC matches.
//!
//! **Torn tail.** A crash mid-append can leave a partial or garbled last block. Opening a
//! segment for writing scans it and truncates at the first invalid block: it is never
//! "repaired" with guesses. There is no resynchronisation marker, so corruption in the middle
//! of a file also cuts the rest of that file; the number of bytes dropped is reported.
//!
//! **Readers** are read-only and may run while the writer appends: a reader stops at the first
//! incomplete block without consuming it, and can be polled again later.

use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use lines_core::{CivilDate, ContractKey};

use super::crc::crc32;
use crate::recording::{read_contract, read_n, read_str, write_contract, write_str};

pub const MAGIC: &[u8; 8] = b"LINESEG\0";
pub const VERSION: u16 = 1;
pub const ENCODING: &str = crate::native::PROTOCOL;
/// Largest block accepted (a sanity bound against a garbled length).
const MAX_BLOCK: u32 = 64 * 1024 * 1024;
const BLOCK_HEADER: u64 = 8;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SegmentHeader {
    pub created_wall_ms: i64,
    pub source: String,
    pub encoding: String,
    pub product: String,
    pub contract: Option<ContractKey>,
    pub date: CivilDate,
}

impl SegmentHeader {
    fn write(&self, w: &mut impl Write) -> io::Result<()> {
        w.write_all(MAGIC)?;
        w.write_all(&VERSION.to_le_bytes())?;
        w.write_all(&self.created_wall_ms.to_le_bytes())?;
        write_str(w, &self.source)?;
        write_str(w, &self.encoding)?;
        write_str(w, &self.product)?;
        write_contract(w, self.contract)?;
        w.write_all(&self.date.year.to_le_bytes())?;
        w.write_all(&[self.date.month, self.date.day])
    }

    pub fn read(r: &mut impl Read) -> io::Result<SegmentHeader> {
        let magic: [u8; 8] = read_n(r)?;
        if &magic != MAGIC {
            return Err(bad("not a Lines store segment"));
        }
        let version = u16::from_le_bytes(read_n(r)?);
        if version != VERSION {
            return Err(bad(format!(
                "segment format v{version} is not supported (this build reads v{VERSION})"
            )));
        }
        let created_wall_ms = i64::from_le_bytes(read_n(r)?);
        let source = read_str(r)?;
        let encoding = read_str(r)?;
        let product = read_str(r)?;
        let contract = read_contract(r)?;
        let year = u16::from_le_bytes(read_n(r)?);
        let [month, day] = read_n::<2, _>(r)?;
        let date = CivilDate::new(year, month, day).ok_or_else(|| bad("bad segment date"))?;
        Ok(SegmentHeader {
            created_wall_ms,
            source,
            encoding,
            product,
            contract,
            date,
        })
    }

    /// Same data stream (the creation time may differ).
    fn same_stream(&self, other: &SegmentHeader) -> bool {
        (
            &self.source,
            &self.encoding,
            &self.product,
            self.contract,
            self.date,
        ) == (
            &other.source,
            &other.encoding,
            &other.product,
            other.contract,
            other.date,
        )
    }
}

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// One stored batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub time_us: i64,
    /// `lines-events-1` events.
    pub events: Vec<u8>,
}

/// Why a reader stopped.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum End {
    /// Clean end of the file (so far).
    Eof,
    /// An incomplete block at the end (being written, or a torn tail).
    Partial,
    /// A complete block whose CRC (or length) is wrong.
    Corrupt,
}

pub struct SegmentReader {
    r: BufReader<File>,
    pub header: SegmentHeader,
    /// Offset of the next unread block.
    offset: u64,
    pub blocks: u64,
}

impl SegmentReader {
    pub fn open(path: &Path) -> io::Result<SegmentReader> {
        let mut r = BufReader::with_capacity(1 << 20, File::open(path)?);
        let header = SegmentHeader::read(&mut r)?;
        let offset = r.stream_position()?;
        Ok(SegmentReader {
            r,
            header,
            offset,
            blocks: 0,
        })
    }

    /// Offset just past the last valid block read.
    pub fn valid_end(&self) -> u64 {
        self.offset
    }

    /// The next valid block, or why there is none. An incomplete block is not consumed, so a
    /// reader running alongside the writer can call this again once more data has landed.
    pub fn next_block(&mut self) -> io::Result<Result<Block, End>> {
        let mut head = [0u8; 8];
        match read_full(&mut self.r, &mut head)? {
            0 => return self.rewind(End::Eof),
            8 => {}
            _ => return self.rewind(End::Partial),
        }
        let len = u32::from_le_bytes(head[0..4].try_into().expect("4 bytes"));
        let crc = u32::from_le_bytes(head[4..8].try_into().expect("4 bytes"));
        if !(8..=MAX_BLOCK).contains(&len) {
            return self.rewind(End::Corrupt);
        }
        let mut payload = vec![0u8; len as usize];
        if read_full(&mut self.r, &mut payload)? < len as usize {
            return self.rewind(End::Partial);
        }
        if crc32(&payload) != crc {
            return self.rewind(End::Corrupt);
        }
        self.offset += BLOCK_HEADER + len as u64;
        self.blocks += 1;
        let time_us = i64::from_le_bytes(payload[0..8].try_into().expect("8 bytes"));
        payload.drain(..8);
        Ok(Ok(Block {
            time_us,
            events: payload,
        }))
    }

    fn rewind(&mut self, why: End) -> io::Result<Result<Block, End>> {
        self.r.seek(SeekFrom::Start(self.offset))?;
        Ok(Err(why))
    }
}

/// Read until `buf` is full or end of file; returns the bytes read.
fn read_full(r: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// What opening a segment for writing found.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct OpenReport {
    /// Valid blocks already in the file.
    pub blocks: u64,
    /// Bytes cut from the end (torn tail or corruption).
    pub truncated_bytes: u64,
    /// Time of the last valid block.
    pub last_time_us: Option<i64>,
}

/// Batched appends with a bounded flush/fsync policy (see [`FlushPolicy`]).
pub struct SegmentWriter {
    file: File,
    pending: Vec<u8>,
    policy: FlushPolicy,
    last_write: Instant,
    last_sync: Instant,
    unsynced: bool,
    pub bytes: u64,
}

/// When appends reach the disk.
///
/// Blocks are buffered in memory and written when `max_buffer` bytes are pending or
/// `write_every` has passed since the last write; written data is fsynced (`sync_data`) at
/// most every `sync_every`, and always on close. So a process crash loses at most
/// `write_every` of data, a power loss at most `write_every + sync_every`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct FlushPolicy {
    pub max_buffer: usize,
    pub write_every: Duration,
    pub sync_every: Duration,
}

impl Default for FlushPolicy {
    fn default() -> Self {
        FlushPolicy {
            max_buffer: 256 * 1024,
            write_every: Duration::from_millis(200),
            sync_every: Duration::from_secs(1),
        }
    }
}

impl SegmentWriter {
    /// Open (creating if needed) and truncate a torn tail. An existing file must hold the
    /// same stream.
    pub fn open(
        path: &Path,
        header: &SegmentHeader,
        policy: FlushPolicy,
    ) -> io::Result<(SegmentWriter, OpenReport)> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut report = OpenReport::default();
        let existing = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        let valid_end = if existing > 0 {
            match SegmentReader::open(path) {
                Ok(mut r) => {
                    if !r.header.same_stream(header) {
                        return Err(bad(format!(
                            "{} holds another stream ({:?}), not {:?}",
                            path.display(),
                            r.header,
                            header
                        )));
                    }
                    while let Ok(b) = r.next_block()? {
                        report.last_time_us = Some(b.time_us);
                    }
                    report.blocks = r.blocks;
                    Some(r.valid_end())
                }
                // A header cut short by a crash right after creation: start over.
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => None,
                Err(e) => return Err(e),
            }
        } else {
            None
        };
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        let end = match valid_end {
            Some(end) => {
                if end < existing {
                    report.truncated_bytes = existing - end;
                    file.set_len(end)?;
                    file.sync_data()?;
                }
                end
            }
            None => {
                report.truncated_bytes = existing;
                file.set_len(0)?;
                let mut h = Vec::new();
                header.write(&mut h)?;
                file.write_all(&h)?;
                file.sync_data()?;
                h.len() as u64
            }
        };
        file.seek(SeekFrom::Start(end))?;
        Ok((
            SegmentWriter {
                file,
                pending: Vec::with_capacity(policy.max_buffer + 4096),
                policy,
                last_write: Instant::now(),
                last_sync: Instant::now(),
                unsynced: false,
                bytes: end,
            },
            report,
        ))
    }

    /// Queue one block (one atomic batch of events).
    pub fn append(&mut self, time_us: i64, events: &[u8]) -> io::Result<()> {
        let len = (8 + events.len()) as u32;
        let start = self.pending.len();
        self.pending.extend_from_slice(&len.to_le_bytes());
        self.pending.extend_from_slice(&[0; 4]);
        self.pending.extend_from_slice(&time_us.to_le_bytes());
        self.pending.extend_from_slice(events);
        let crc = crc32(&self.pending[start + 8..]);
        self.pending[start + 4..start + 8].copy_from_slice(&crc.to_le_bytes());
        if self.pending.len() >= self.policy.max_buffer {
            self.write_pending()?;
        }
        Ok(())
    }

    fn write_pending(&mut self) -> io::Result<()> {
        if !self.pending.is_empty() {
            self.file.write_all(&self.pending)?;
            self.bytes += self.pending.len() as u64;
            self.pending.clear();
            self.unsynced = true;
        }
        self.last_write = Instant::now();
        Ok(())
    }

    /// Apply the flush policy; call regularly (e.g. after each batch and on a timer).
    pub fn tick(&mut self) -> io::Result<()> {
        if self.last_write.elapsed() >= self.policy.write_every {
            self.write_pending()?;
        }
        if self.unsynced && self.last_sync.elapsed() >= self.policy.sync_every {
            self.file.sync_data()?;
            self.unsynced = false;
            self.last_sync = Instant::now();
        }
        Ok(())
    }

    /// Write and fsync everything.
    pub fn close(mut self) -> io::Result<()> {
        self.write_pending()?;
        self.file.sync_data()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lines_core::ProductId;

    fn header() -> SegmentHeader {
        SegmentHeader {
            created_wall_ms: 1,
            source: "test".into(),
            encoding: ENCODING.into(),
            product: "ES".into(),
            contract: Some(ContractKey::monthly(
                ProductId(1),
                2026,
                lines_core::Month::new(12).unwrap(),
            )),
            date: CivilDate::new(2026, 11, 6).unwrap(),
        }
    }

    fn dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("lines-seg-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn read_all(path: &Path) -> (Vec<Block>, End) {
        let mut r = SegmentReader::open(path).unwrap();
        let mut v = Vec::new();
        loop {
            match r.next_block().unwrap() {
                Ok(b) => v.push(b),
                Err(e) => return (v, e),
            }
        }
    }

    #[test]
    fn append_reopen_and_torn_tail() {
        let path = dir("torn").join("s.lseg");
        let (mut w, rep) = SegmentWriter::open(&path, &header(), FlushPolicy::default()).unwrap();
        assert_eq!(rep, OpenReport::default());
        for i in 0..10 {
            w.append(i, &[i as u8; 30]).unwrap();
        }
        w.close().unwrap();
        let good = std::fs::metadata(&path).unwrap().len();
        assert_eq!(read_all(&path).0.len(), 10);

        // A crash mid-append: half a block at the end.
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(&[40, 0, 0, 0, 1, 2, 3, 4, 5, 6]).unwrap();
        drop(f);
        assert_eq!(read_all(&path), (read_all(&path).0, End::Partial));
        let (w, rep) = SegmentWriter::open(&path, &header(), FlushPolicy::default()).unwrap();
        assert_eq!(rep.blocks, 10);
        assert_eq!(rep.truncated_bytes, 10);
        assert_eq!(rep.last_time_us, Some(9));
        w.close().unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), good);

        // A complete last block with a bad CRC (garbled write) is cut too, never guessed at.
        let mut bytes = std::fs::read(&path).unwrap();
        let n = bytes.len();
        bytes[n - 1] ^= 0xFF;
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(read_all(&path).1, End::Corrupt);
        let (mut w, rep) = SegmentWriter::open(&path, &header(), FlushPolicy::default()).unwrap();
        assert_eq!((rep.blocks, rep.last_time_us), (9, Some(8)));
        w.append(99, b"after").unwrap();
        w.close().unwrap();
        let (blocks, end) = read_all(&path);
        assert_eq!(end, End::Eof);
        assert_eq!(blocks.len(), 10);
        assert_eq!(
            blocks[9],
            Block {
                time_us: 99,
                events: b"after".to_vec()
            }
        );
    }

    #[test]
    fn another_stream_is_refused_and_a_cut_header_restarts() {
        let path = dir("hdr").join("s.lseg");
        SegmentWriter::open(&path, &header(), FlushPolicy::default())
            .unwrap()
            .0
            .close()
            .unwrap();
        let mut other = header();
        other.product = "NQ".into();
        assert!(SegmentWriter::open(&path, &other, FlushPolicy::default()).is_err());
        std::fs::write(&path, &MAGIC[..5]).unwrap();
        let (w, rep) = SegmentWriter::open(&path, &header(), FlushPolicy::default()).unwrap();
        assert_eq!(rep.truncated_bytes, 5);
        w.close().unwrap();
        assert_eq!(read_all(&path), (vec![], End::Eof));
    }
}
