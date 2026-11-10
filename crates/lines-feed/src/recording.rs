//! Raw feed recording (`.lfr`): every inbound frame (HTTP bodies included), *before* decoding,
//! with a monotonic timestamp. Outbound frames are recorded as their time only, with no data
//! (they can carry credentials: decision 0020); files from older builds hold their bytes, and
//! replay skips them either way. Replaying a file therefore exercises the exact same session code as
//! a live connection, for any provider (the header says which one).
//!
//! Providers so far: `cqg` (raw protobuf frames), `coinbase` (JSON text + HTTP bodies), and
//! `lines` (normalized events incl. order-book depth, see `native.rs`; used for imported
//! data such as Sierra Chart files). The container format is unchanged by `lines`.
//!
//! Format v3 (little-endian):
//! ```text
//! header:  b"LINESFR\0" | u16 format_version=3|4 | i64 start_wall_ms
//!          | str provider | str protocol | str product_symbol         (str = u16 len + UTF-8)
//!          | contract                                                  (v3 and v4)
//! contract: u8 kind (0 = unknown, 1 = monthly, 2 = spot; v4 also 3 = equity, 4 = option)
//!          | if kind != 0: u32 product_id | if kind == 1: u16 year | u8 month
//!          | if kind == 4: u16 year | u8 month | u8 day (expiry) | i64 strike | u8 right
//!                          (strike in the underlying's price units; right 0 = call, 1 = put)
//! record:  u8 tag | u64 nanos_since_start | u32 len | len bytes
//!          tag bit 0: direction (0 = server->client, 1 = client->server)
//!          tag bits 1-2: kind (0 = binary, 1 = text, 2 = http response)
//! ```
//! v3 adds the contract key (decision 0013). v4 (decision 0028) adds the equity and option
//! contract kinds and nothing else, so a writer writes v4 **only** for those and v3 for
//! everything else: a futures or spot recording is byte-for-byte what earlier builds wrote, and
//! they still read it. v2 files (same layout without `contract`) are still read, with the
//! contract reported as unknown. v1 files are rejected.
//!
//! Recordings can contain licensed exchange data: keep them out of git (decision 0010).

use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;

use lines_core::{CivilDate, ContractKey, Delivery, Month, Price, ProductId, Right};

use crate::provider::{Frame, FrameKind};

pub const MAGIC: &[u8; 8] = b"LINESFR\0";
/// The newest version this build writes (only for contracts that need it: [`version_for`]).
pub const FORMAT_VERSION: u16 = 4;
/// Oldest version this build still reads (as "contract unknown").
pub const OLDEST_READABLE: u16 = 2;
const MAX_FRAME: u32 = 64 * 1024 * 1024;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Direction {
    FromServer = 0,
    ToServer = 1,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    pub start_wall_ms: i64,
    /// Provider id, e.g. "cqg" or "coinbase".
    pub provider: String,
    /// Provider wire protocol version, e.g. "cqg-webapi-2.230".
    pub protocol: String,
    /// Catalog product symbol, e.g. "ES". Never a vendor symbol.
    pub product: String,
    /// The contract the data belongs to. `None` = unknown (v2 files, or a writer that
    /// didn't know it).
    pub contract: Option<ContractKey>,
    /// Format version the file was written with (writers always write the current one).
    pub version: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub direction: Direction,
    pub since_start: Duration,
    pub frame: Frame,
}

impl Record {
    /// Wall-clock time the frame was recorded.
    pub fn wall_ms(&self, header: &Header) -> i64 {
        header.start_wall_ms + self.since_start.as_millis() as i64
    }
}

fn kind_bits(k: FrameKind) -> u8 {
    match k {
        FrameKind::Binary => 0,
        FrameKind::Text => 1,
        FrameKind::Http => 2,
    }
}

enum Msg {
    Record(u8, u64, Bytes),
    Flush,
    /// Finish the file now (a recording stopped while its connection goes on).
    Stop,
}

/// Non-blocking recorder: frames are handed to a dedicated writer thread, so disk I/O never
/// stalls the network task. Dropping the recorder flushes and closes the file.
pub struct Recorder {
    /// Set by [`Recorder::stop`]: frames after it are not recorded.
    stopped: std::sync::atomic::AtomicBool,
    tx: Option<mpsc::Sender<Msg>>,
    start: Instant,
    worker: Option<thread::JoinHandle<io::Result<()>>>,
}

pub(crate) fn write_str(w: &mut impl Write, s: &str) -> io::Result<()> {
    let len = u16::try_from(s.len()).map_err(|_| io::Error::other("header string too long"))?;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(s.as_bytes())
}

/// Synchronous `.lfr` writer with caller-supplied timestamps. Used directly by importers
/// (whose "time since start" is market time) and by [`Recorder`]'s writer thread.
pub struct Writer<W: Write> {
    w: W,
}

impl Writer<BufWriter<File>> {
    pub fn create(path: &Path, header: &Header) -> io::Result<Self> {
        if let Some(dir) = path.parent()
            && !dir.as_os_str().is_empty()
        {
            std::fs::create_dir_all(dir)?;
        }
        Writer::new(
            BufWriter::with_capacity(1 << 20, File::create(path)?),
            header,
        )
    }
}

impl<W: Write> Writer<W> {
    pub fn new(mut w: W, header: &Header) -> io::Result<Self> {
        w.write_all(MAGIC)?;
        w.write_all(&version_for(header.contract).to_le_bytes())?;
        w.write_all(&header.start_wall_ms.to_le_bytes())?;
        write_str(&mut w, &header.provider)?;
        write_str(&mut w, &header.protocol)?;
        write_str(&mut w, &header.product)?;
        write_contract(&mut w, header.contract)?;
        Ok(Writer { w })
    }

    pub fn write(
        &mut self,
        direction: Direction,
        since_start: Duration,
        frame: &Frame,
    ) -> io::Result<()> {
        let nanos = u64::try_from(since_start.as_nanos()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("frame offset {since_start:?} does not fit a recording"),
            )
        })?;
        self.write_raw(
            direction as u8 | (kind_bits(frame.kind) << 1),
            nanos,
            &frame.data,
        )
    }

    fn write_raw(&mut self, tag: u8, nanos: u64, payload: &[u8]) -> io::Result<()> {
        let len = u32::try_from(payload.len())
            .map_err(|_| io::Error::other("frame larger than 4 GiB"))?;
        self.w.write_all(&[tag])?;
        self.w.write_all(&nanos.to_le_bytes())?;
        self.w.write_all(&len.to_le_bytes())?;
        self.w.write_all(payload)
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.w.flush()
    }

    pub fn finish(mut self) -> io::Result<W> {
        self.w.flush()?;
        Ok(self.w)
    }
}

impl Recorder {
    pub fn create(
        path: &Path,
        provider: &str,
        protocol: &str,
        product: &str,
        contract: Option<ContractKey>,
    ) -> io::Result<Recorder> {
        let start_wall_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let header = Header {
            start_wall_ms,
            provider: provider.into(),
            protocol: protocol.into(),
            product: product.into(),
            contract,
            version: version_for(contract),
        };
        let mut w = Writer::create(path, &header)?;
        let (tx, rx) = mpsc::channel::<Msg>();
        let worker = thread::Builder::new().name("lines-recorder".into()).spawn(
            move || -> io::Result<()> {
                let mut last_flush = Instant::now();
                loop {
                    match rx.recv_timeout(Duration::from_millis(250)) {
                        Ok(Msg::Record(tag, nanos, payload)) => {
                            w.write_raw(tag, nanos, &payload)?
                        }
                        Ok(Msg::Flush) => w.flush()?,
                        Ok(Msg::Stop) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                    if last_flush.elapsed() > Duration::from_secs(1) {
                        w.flush()?;
                        last_flush = Instant::now();
                    }
                }
                w.finish().map(|_| ())
            },
        )?;
        Ok(Recorder {
            stopped: std::sync::atomic::AtomicBool::new(false),
            tx: Some(tx),
            start: Instant::now(),
            worker: Some(worker),
        })
    }

    pub fn record(&self, direction: Direction, frame: &Frame) {
        if self.is_stopped() {
            return;
        }
        let nanos = self.start.elapsed().as_nanos() as u64;
        let tag = direction as u8 | (kind_bits(frame.kind) << 1);
        if let Some(tx) = &self.tx {
            // Bytes clone is a refcount bump, not a copy.
            let _ = tx.send(Msg::Record(tag, nanos, frame.data.clone()));
        }
    }

    /// Finish the file now: its writer thread writes what it holds and closes it; frames
    /// after this are dropped. Never blocks (the file is closed on the writer thread).
    pub fn stop(&self) {
        use std::sync::atomic::Ordering;
        if !self.stopped.swap(true, Ordering::AcqRel)
            && let Some(tx) = &self.tx
        {
            let _ = tx.send(Msg::Stop);
        }
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped.load(std::sync::atomic::Ordering::Acquire)
    }

    pub fn flush(&self) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Msg::Flush);
        }
    }

    /// Flush, close, and surface any I/O error from the writer thread.
    pub fn finish(mut self) -> io::Result<()> {
        self.close()
    }

    fn close(&mut self) -> io::Result<()> {
        self.tx.take();
        match self.worker.take() {
            Some(h) => h
                .join()
                .unwrap_or_else(|_| Err(io::Error::other("recorder thread panicked"))),
            None => Ok(()),
        }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        if let Err(e) = self.close() {
            tracing::error!("recording write failed: {e}");
        }
    }
}

/// Sequential reader for `.lfr` files.
pub struct Reader<R: Read> {
    r: R,
    pub header: Header,
}

impl Reader<BufReader<File>> {
    pub fn open(path: &Path) -> io::Result<Self> {
        Reader::new(BufReader::with_capacity(1 << 20, File::open(path)?))
    }
}

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

impl<R: Read> Reader<R> {
    pub fn new(mut r: R) -> io::Result<Self> {
        let mut magic = [0u8; 8];
        r.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(bad("not a Lines feed recording"));
        }
        let version = u16::from_le_bytes(read_n(&mut r)?);
        if !(OLDEST_READABLE..=FORMAT_VERSION).contains(&version) {
            return Err(bad(format!(
                "recording format v{version} is not supported (this build reads v{OLDEST_READABLE}..=v{FORMAT_VERSION}); re-record it"
            )));
        }
        let start_wall_ms = i64::from_le_bytes(read_n(&mut r)?);
        let provider = read_str(&mut r)?;
        let protocol = read_str(&mut r)?;
        let product = read_str(&mut r)?;
        let contract = if version >= 3 {
            read_contract(&mut r, version >= 4)?
        } else {
            None
        };
        Ok(Reader {
            r,
            header: Header {
                start_wall_ms,
                provider,
                protocol,
                product,
                contract,
                version,
            },
        })
    }

    /// Next record, `Ok(None)` at clean end of file. A truncated final record (e.g. the app
    /// was killed mid-write) is treated as end of file.
    pub fn next_record(&mut self) -> io::Result<Option<Record>> {
        let mut tag = [0u8; 1];
        match self.r.read_exact(&mut tag) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }
        let direction = if tag[0] & 1 == 0 {
            Direction::FromServer
        } else {
            Direction::ToServer
        };
        let kind = match (tag[0] >> 1) & 0b11 {
            0 => FrameKind::Binary,
            1 => FrameKind::Text,
            2 => FrameKind::Http,
            _ => return Err(bad(format!("bad record tag {:#x}", tag[0]))),
        };
        if tag[0] >> 3 != 0 {
            return Err(bad(format!("bad record tag {:#x}", tag[0])));
        }
        let rest = (|| -> io::Result<(u64, Bytes)> {
            let nanos = u64::from_le_bytes(read_n(&mut self.r)?);
            let len = u32::from_le_bytes(read_n(&mut self.r)?);
            if len > MAX_FRAME {
                return Err(bad(format!("frame too large: {len}")));
            }
            // Grow with the bytes actually present: a corrupt length must not allocate up to
            // MAX_FRAME before the read finds the file is shorter.
            let mut buf = Vec::new();
            (&mut self.r).take(len as u64).read_to_end(&mut buf)?;
            if buf.len() < len as usize {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            Ok((nanos, Bytes::from(buf)))
        })();
        match rest {
            Ok((nanos, data)) => Ok(Some(Record {
                direction,
                since_start: Duration::from_nanos(nanos),
                frame: Frame { kind, data },
            })),
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                tracing::warn!("recording ends with a truncated frame; ignoring it");
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }
}

pub(crate) fn read_n<const N: usize, R: Read>(r: &mut R) -> io::Result<[u8; N]> {
    let mut b = [0u8; N];
    r.read_exact(&mut b)?;
    Ok(b)
}

/// The version a recording of `contract` is written as: v4 only for the kinds v4 added.
pub fn version_for(contract: Option<ContractKey>) -> u16 {
    if needs_v4(contract) {
        FORMAT_VERSION
    } else {
        3
    }
}

/// Equity and option contracts exist only from `.lfr` v4 / segment v2 on.
pub(crate) fn needs_v4(contract: Option<ContractKey>) -> bool {
    matches!(
        contract.map(|k| k.delivery),
        Some(Delivery::Equity | Delivery::Option(_))
    )
}

pub(crate) fn write_contract(w: &mut impl Write, c: Option<ContractKey>) -> io::Result<()> {
    let Some(k) = c else {
        return w.write_all(&[0]);
    };
    let kind = match k.delivery {
        Delivery::Monthly { .. } => 1u8,
        Delivery::Spot => 2,
        Delivery::Equity => 3,
        Delivery::Option(_) => 4,
    };
    w.write_all(&[kind])?;
    w.write_all(&k.product.0.to_le_bytes())?;
    match k.delivery {
        Delivery::Monthly { year, month } => {
            w.write_all(&year.to_le_bytes())?;
            w.write_all(&[month.number()])?;
        }
        Delivery::Option(o) => {
            w.write_all(&o.expiry.year.to_le_bytes())?;
            w.write_all(&[o.expiry.month, o.expiry.day])?;
            w.write_all(&o.strike.0.to_le_bytes())?;
            w.write_all(&[match o.right {
                Right::Call => 0,
                Right::Put => 1,
            }])?;
        }
        Delivery::Spot | Delivery::Equity => {}
    }
    Ok(())
}

/// A contract as [`write_contract`] wrote it. `v4_kinds`: the file's version allows equity
/// and option contracts (a v3 file claiming one is damaged).
pub(crate) fn read_contract<R: Read>(r: &mut R, v4_kinds: bool) -> io::Result<Option<ContractKey>> {
    let kind = read_n::<1, _>(r)?[0];
    if kind == 0 {
        return Ok(None);
    }
    let product = ProductId(u32::from_le_bytes(read_n(r)?));
    match kind {
        1 => {
            let year = u16::from_le_bytes(read_n(r)?);
            let month =
                Month::new(read_n::<1, _>(r)?[0]).ok_or_else(|| bad("bad contract month"))?;
            Ok(Some(ContractKey::monthly(product, year, month)))
        }
        2 => Ok(Some(ContractKey::spot(product))),
        3 if v4_kinds => Ok(Some(ContractKey::equity(product))),
        4 if v4_kinds => {
            let year = u16::from_le_bytes(read_n(r)?);
            let [month, day] = read_n::<2, _>(r)?;
            let expiry =
                CivilDate::new(year, month, day).ok_or_else(|| bad("bad option expiry"))?;
            let strike = Price(i64::from_le_bytes(read_n(r)?));
            if strike.0 <= 0 {
                return Err(bad("bad option strike"));
            }
            let right = match read_n::<1, _>(r)?[0] {
                0 => Right::Call,
                1 => Right::Put,
                x => return Err(bad(format!("bad option right {x}"))),
            };
            let k = ContractKey::option(product, expiry, strike, right);
            // A catalog product must list options (an extension's products aren't known here).
            if crate::catalog::get(product).is_some_and(|p| !p.lists(&k)) {
                return Err(bad("an option on a product that lists none"));
            }
            Ok(Some(k))
        }
        k => Err(bad(format!("bad contract kind {k}"))),
    }
}

pub(crate) fn read_str<R: Read>(r: &mut R) -> io::Result<String> {
    let len = u16::from_le_bytes(read_n(r)?) as usize;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    String::from_utf8(buf).map_err(|_| bad("header string is not UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decision 0028: a futures or spot recording is written as v3, byte for byte as before
    /// (pinned), so older builds still read it; equity and option contracts make it v4 and
    /// round-trip exactly; a v3 file claiming one of those kinds is damaged.
    #[test]
    fn v4_only_for_equities_and_options() {
        let hdr = |c| Header {
            start_wall_ms: 7,
            product: "SPY".into(),
            ..header(c)
        };
        let es = ContractKey::monthly(ProductId(1), 2026, Month::new(12).unwrap());
        let bytes = Writer::new(Vec::new(), &hdr(Some(es)))
            .unwrap()
            .finish()
            .unwrap();
        let mut pinned = Vec::new();
        pinned.extend_from_slice(MAGIC);
        pinned.extend_from_slice(&3u16.to_le_bytes());
        pinned.extend_from_slice(&7i64.to_le_bytes());
        for s in ["lines", "lines-events-1", "SPY"] {
            pinned.extend_from_slice(&(s.len() as u16).to_le_bytes());
            pinned.extend_from_slice(s.as_bytes());
        }
        pinned.extend_from_slice(&[1, 1, 0, 0, 0]);
        pinned.extend_from_slice(&2026u16.to_le_bytes());
        pinned.push(12);
        assert_eq!(bytes, pinned, "a futures recording is unchanged (v3)");
        let r = Reader::new(&bytes[..]).unwrap();
        assert_eq!((r.header.version, r.header.contract), (3, Some(es)));
        let spy = ProductId(2001);
        let opt = ContractKey::option(
            spy,
            CivilDate::new(2026, 12, 18).unwrap(),
            Price(45_050),
            Right::Put,
        );
        for k in [
            ContractKey::equity(spy),
            opt,
            ContractKey::spot(ProductId(1001)),
        ] {
            let bytes = Writer::new(Vec::new(), &hdr(Some(k)))
                .unwrap()
                .finish()
                .unwrap();
            let r = Reader::new(&bytes[..]).unwrap();
            assert_eq!(r.header.contract, Some(k));
            assert_eq!(r.header.version, version_for(Some(k)));
        }
        assert_eq!(version_for(Some(opt)), 4);
        assert_eq!(version_for(Some(es)), 3);
        assert_eq!(version_for(None), 3);
        // An option header relabelled v3: refused, never read as something else.
        let mut bad = Writer::new(Vec::new(), &hdr(Some(opt)))
            .unwrap()
            .finish()
            .unwrap();
        bad[8..10].copy_from_slice(&3u16.to_le_bytes());
        assert!(Reader::new(&bad[..]).is_err());
        // An option on a catalog product that lists none (ES) is damage too.
        let es_opt = ContractKey::option(
            ProductId(1),
            CivilDate::new(2026, 12, 18).unwrap(),
            Price(600_000),
            Right::Call,
        );
        let b = Writer::new(Vec::new(), &hdr(Some(es_opt)))
            .unwrap()
            .finish()
            .unwrap();
        assert!(Reader::new(&b[..]).is_err());
        // A zero strike or a bad right is damage.
        let mut zero = Writer::new(Vec::new(), &hdr(Some(opt)))
            .unwrap()
            .finish()
            .unwrap();
        let n = zero.len();
        zero[n - 9..n - 1].copy_from_slice(&0i64.to_le_bytes());
        assert!(Reader::new(&zero[..]).is_err());
        let mut right = Writer::new(Vec::new(), &hdr(Some(opt)))
            .unwrap()
            .finish()
            .unwrap();
        *right.last_mut().unwrap() = 7;
        assert!(Reader::new(&right[..]).is_err());
    }

    #[test]
    fn a_huge_length_on_a_short_file_is_a_truncated_record() {
        // Fuzz hardening: the reader used to allocate the claimed length (up to 64 MiB) before
        // reading; it now grows with the data and reports truncation as end of file.
        let header = Header {
            start_wall_ms: 0,
            provider: "lines".into(),
            protocol: "lines-events-1".into(),
            product: "NQ".into(),
            contract: None,
            version: FORMAT_VERSION,
        };
        let mut bytes = Writer::new(Vec::new(), &header).unwrap().finish().unwrap();
        bytes.push(0); // tag: server, binary
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&(MAX_FRAME).to_le_bytes());
        bytes.extend_from_slice(b"abc");
        let mut r = Reader::new(&bytes[..]).unwrap();
        assert_eq!(r.next_record().unwrap(), None);
    }

    #[test]
    fn offsets_beyond_u64_nanos_are_an_error_not_wrapped() {
        // Fuzz regression (Sierra import of corrupt times): the offset was cast with `as u64`.
        let header = Header {
            start_wall_ms: 0,
            provider: "lines".into(),
            protocol: "lines-events-1".into(),
            product: "NQ".into(),
            contract: None,
            version: FORMAT_VERSION,
        };
        let mut w = Writer::new(Vec::new(), &header).unwrap();
        let f = Frame::binary(vec![1]);
        let max = Duration::from_nanos(u64::MAX);
        w.write(Direction::FromServer, max, &f).unwrap();
        let e = w
            .write(Direction::FromServer, max + Duration::from_nanos(1), &f)
            .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn roundtrip_and_truncation() {
        let dir = std::env::temp_dir().join(format!("lines-rec-test-{}", std::process::id()));
        let path = dir.join("t.lfr");
        let rec = Recorder::create(
            &path,
            "coinbase",
            "coinbase-exchange-ws-v1",
            "BTC/USD",
            Some(ContractKey::spot(ProductId(1001))),
        )
        .unwrap();
        let frames: Vec<Frame> = (0..1000u32)
            .map(|i| match i % 3 {
                0 => Frame::binary(i.to_le_bytes().repeat(i as usize % 7 + 1)),
                1 => Frame::text(format!("{{\"n\":{i}}}")),
                _ => Frame::http(format!("[[{i}]]")),
            })
            .collect();
        for (i, f) in frames.iter().enumerate() {
            rec.record(
                if i % 10 == 0 {
                    Direction::ToServer
                } else {
                    Direction::FromServer
                },
                f,
            );
        }
        rec.finish().unwrap();

        let mut r = Reader::open(&path).unwrap();
        assert_eq!(r.header.provider, "coinbase");
        assert_eq!(r.header.protocol, "coinbase-exchange-ws-v1");
        assert_eq!(r.header.product, "BTC/USD");
        assert_eq!(r.header.contract, Some(ContractKey::spot(ProductId(1001))));
        assert_eq!(r.header.version, 3);
        let mut prev = Duration::ZERO;
        for (i, f) in frames.iter().enumerate() {
            let rec = r.next_record().unwrap().unwrap();
            assert_eq!(&rec.frame, f);
            assert_eq!(rec.direction == Direction::ToServer, i % 10 == 0);
            assert!(rec.since_start >= prev);
            prev = rec.since_start;
        }
        assert!(r.next_record().unwrap().is_none());

        // Chop the file mid-record: reader must stop cleanly, not error.
        let bytes = std::fs::read(&path).unwrap();
        std::fs::write(&path, &bytes[..bytes.len() - 3]).unwrap();
        let mut r = Reader::open(&path).unwrap();
        let mut n = 0;
        while r.next_record().unwrap().is_some() {
            n += 1;
        }
        assert_eq!(n, 999);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_non_recording_and_old_versions() {
        assert!(Reader::new(&b"NOTAFILE\x01\x00"[..]).is_err());
        let mut v1 = MAGIC.to_vec();
        v1.extend_from_slice(&1u16.to_le_bytes());
        let err = Reader::new(&v1[..]).err().unwrap();
        assert!(err.to_string().contains("v1"));
    }

    fn header(contract: Option<ContractKey>) -> Header {
        Header {
            start_wall_ms: 1,
            provider: "lines".into(),
            protocol: "lines-events-1".into(),
            product: "NQ".into(),
            contract,
            version: version_for(contract),
        }
    }

    #[test]
    fn v3_contract_roundtrip() {
        let z = Month::new(12).unwrap();
        for c in [
            None,
            Some(ContractKey::monthly(ProductId(2), 2025, z)),
            Some(ContractKey::spot(ProductId(1001))),
        ] {
            let mut buf = Vec::new();
            let mut w = Writer::new(&mut buf, &header(c)).unwrap();
            w.write(
                Direction::FromServer,
                Duration::from_millis(5),
                &Frame::binary(vec![1, 2, 3]),
            )
            .unwrap();
            w.finish().unwrap();
            let mut r = Reader::new(&buf[..]).unwrap();
            assert_eq!(r.header, header(c));
            assert_eq!(
                r.next_record().unwrap().unwrap().frame,
                Frame::binary(vec![1, 2, 3])
            );
        }
    }

    #[test]
    fn v2_files_read_as_contract_unknown() {
        // A v2 file: the same layout without the contract field.
        let mut v2 = MAGIC.to_vec();
        v2.extend_from_slice(&2u16.to_le_bytes());
        v2.extend_from_slice(&7i64.to_le_bytes());
        for s in ["cqg", "cqg-webapi-2.230", "ES"] {
            v2.extend_from_slice(&(s.len() as u16).to_le_bytes());
            v2.extend_from_slice(s.as_bytes());
        }
        v2.push(0); // record tag: from server, binary
        v2.extend_from_slice(&0u64.to_le_bytes());
        v2.extend_from_slice(&1u32.to_le_bytes());
        v2.push(42);
        let mut r = Reader::new(&v2[..]).unwrap();
        assert_eq!(
            (
                r.header.version,
                r.header.product.as_str(),
                r.header.contract
            ),
            (2, "ES", None)
        );
        assert_eq!(r.next_record().unwrap().unwrap().frame.data.as_ref(), &[42]);
        let mut v4 = MAGIC.to_vec();
        v4.extend_from_slice(&4u16.to_le_bytes());
        assert!(Reader::new(&v4[..]).is_err(), "newer than this build");
    }
}
