//! Raw feed recording (`.lfr`): every frame in both directions, *before* decoding, with a
//! monotonic timestamp. Replaying a file therefore exercises the exact same session code as
//! a live connection, for any provider (the header says which one).
//!
//! Format v2 (little-endian):
//! ```text
//! header:  b"LINESFR\0" | u16 format_version=2 | i64 start_wall_ms
//!          | str provider | str protocol | str instrument_symbol      (str = u16 len + UTF-8)
//! record:  u8 tag | u64 nanos_since_start | u32 len | len bytes
//!          tag bit 0: direction (0 = server->client, 1 = client->server)
//!          tag bits 1-2: kind (0 = binary, 1 = text, 2 = http response)
//! ```
//! Recordings can contain licensed exchange data: keep them out of git (decision 0010).

use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;

use crate::provider::{Frame, FrameKind};

pub const MAGIC: &[u8; 8] = b"LINESFR\0";
pub const FORMAT_VERSION: u16 = 2;
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
    /// Internal (catalog) instrument symbol, e.g. "ES". Never a vendor symbol.
    pub instrument: String,
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
}

/// Non-blocking recorder: frames are handed to a dedicated writer thread, so disk I/O never
/// stalls the network task. Dropping the recorder flushes and closes the file.
pub struct Recorder {
    tx: Option<mpsc::Sender<Msg>>,
    start: Instant,
    worker: Option<thread::JoinHandle<io::Result<()>>>,
}

fn write_str(w: &mut impl Write, s: &str) -> io::Result<()> {
    let len = u16::try_from(s.len()).map_err(|_| io::Error::other("header string too long"))?;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(s.as_bytes())
}

impl Recorder {
    pub fn create(
        path: &Path,
        provider: &str,
        protocol: &str,
        instrument: &str,
    ) -> io::Result<Recorder> {
        if let Some(dir) = path.parent()
            && !dir.as_os_str().is_empty()
        {
            std::fs::create_dir_all(dir)?;
        }
        let mut w = BufWriter::with_capacity(1 << 20, File::create(path)?);
        let start_wall_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        w.write_all(MAGIC)?;
        w.write_all(&FORMAT_VERSION.to_le_bytes())?;
        w.write_all(&start_wall_ms.to_le_bytes())?;
        write_str(&mut w, provider)?;
        write_str(&mut w, protocol)?;
        write_str(&mut w, instrument)?;
        let (tx, rx) = mpsc::channel::<Msg>();
        let worker = thread::Builder::new().name("lines-recorder".into()).spawn(
            move || -> io::Result<()> {
                let mut last_flush = Instant::now();
                loop {
                    match rx.recv_timeout(Duration::from_millis(250)) {
                        Ok(Msg::Record(tag, nanos, payload)) => {
                            w.write_all(&[tag])?;
                            w.write_all(&nanos.to_le_bytes())?;
                            w.write_all(&(payload.len() as u32).to_le_bytes())?;
                            w.write_all(&payload)?;
                        }
                        Ok(Msg::Flush) => w.flush()?,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                    if last_flush.elapsed() > Duration::from_secs(1) {
                        w.flush()?;
                        last_flush = Instant::now();
                    }
                }
                w.flush()
            },
        )?;
        Ok(Recorder {
            tx: Some(tx),
            start: Instant::now(),
            worker: Some(worker),
        })
    }

    pub fn record(&self, direction: Direction, frame: &Frame) {
        let nanos = self.start.elapsed().as_nanos() as u64;
        let tag = direction as u8 | (kind_bits(frame.kind) << 1);
        if let Some(tx) = &self.tx {
            // Bytes clone is a refcount bump, not a copy.
            let _ = tx.send(Msg::Record(tag, nanos, frame.data.clone()));
        }
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
        if version != FORMAT_VERSION {
            return Err(bad(format!(
                "recording format v{version} is not supported (this build reads v{FORMAT_VERSION}); re-record it"
            )));
        }
        let start_wall_ms = i64::from_le_bytes(read_n(&mut r)?);
        let provider = read_str(&mut r)?;
        let protocol = read_str(&mut r)?;
        let instrument = read_str(&mut r)?;
        Ok(Reader {
            r,
            header: Header {
                start_wall_ms,
                provider,
                protocol,
                instrument,
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
            let mut buf = vec![0u8; len as usize];
            self.r.read_exact(&mut buf)?;
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

fn read_n<const N: usize, R: Read>(r: &mut R) -> io::Result<[u8; N]> {
    let mut b = [0u8; N];
    r.read_exact(&mut b)?;
    Ok(b)
}

fn read_str<R: Read>(r: &mut R) -> io::Result<String> {
    let len = u16::from_le_bytes(read_n(r)?) as usize;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    String::from_utf8(buf).map_err(|_| bad("header string is not UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_truncation() {
        let dir = std::env::temp_dir().join(format!("lines-rec-test-{}", std::process::id()));
        let path = dir.join("t.lfr");
        let rec =
            Recorder::create(&path, "coinbase", "coinbase-exchange-ws-v1", "BTC/USD").unwrap();
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
        assert_eq!(r.header.instrument, "BTC/USD");
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
}
