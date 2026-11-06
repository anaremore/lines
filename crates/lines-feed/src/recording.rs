//! Raw feed recording (`.lfr`): every WebSocket binary frame in both directions, *before*
//! decoding, with a monotonic timestamp. Replaying a file therefore exercises the exact same
//! decode + session path as a live connection.
//!
//! Format (little-endian):
//! ```text
//! header:  b"LINESFR\0" | u16 format_version | u32 proto_major | u32 proto_minor | i64 start_wall_ms
//! record:  u8 direction (0 = server->client, 1 = client->server) | u64 nanos_since_start | u32 len | len bytes
//! ```
//! Recordings can contain licensed exchange data: keep them out of git (see .gitignore).

use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;

pub const MAGIC: &[u8; 8] = b"LINESFR\0";
pub const FORMAT_VERSION: u16 = 1;
const MAX_FRAME: u32 = 64 * 1024 * 1024;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Direction {
    FromServer = 0,
    ToServer = 1,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    pub format_version: u16,
    pub proto_major: u32,
    pub proto_minor: u32,
    pub start_wall_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub direction: Direction,
    pub since_start: Duration,
    pub payload: Bytes,
}

enum Msg {
    Record(Direction, u64, Bytes),
    Flush,
}

/// Non-blocking recorder: frames are handed to a dedicated writer thread, so disk I/O never
/// stalls the network task. Dropping the recorder flushes and closes the file.
pub struct Recorder {
    tx: Option<mpsc::Sender<Msg>>,
    start: Instant,
    worker: Option<thread::JoinHandle<io::Result<()>>>,
}

impl Recorder {
    pub fn create(path: &Path, proto_major: u32, proto_minor: u32) -> io::Result<Recorder> {
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
        w.write_all(&proto_major.to_le_bytes())?;
        w.write_all(&proto_minor.to_le_bytes())?;
        w.write_all(&start_wall_ms.to_le_bytes())?;
        let (tx, rx) = mpsc::channel::<Msg>();
        let worker = thread::Builder::new().name("lines-recorder".into()).spawn(
            move || -> io::Result<()> {
                let mut last_flush = Instant::now();
                loop {
                    match rx.recv_timeout(Duration::from_millis(250)) {
                        Ok(Msg::Record(dir, nanos, payload)) => {
                            w.write_all(&[dir as u8])?;
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

    pub fn record(&self, direction: Direction, payload: &Bytes) {
        let nanos = self.start.elapsed().as_nanos() as u64;
        if let Some(tx) = &self.tx {
            // Bytes clone is a refcount bump, not a copy.
            let _ = tx.send(Msg::Record(direction, nanos, payload.clone()));
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

impl<R: Read> Reader<R> {
    pub fn new(mut r: R) -> io::Result<Self> {
        let mut magic = [0u8; 8];
        r.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not a Lines feed recording",
            ));
        }
        let format_version = u16::from_le_bytes(read_n(&mut r)?);
        if format_version != FORMAT_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported recording format version {format_version}"),
            ));
        }
        let header = Header {
            format_version,
            proto_major: u32::from_le_bytes(read_n(&mut r)?),
            proto_minor: u32::from_le_bytes(read_n(&mut r)?),
            start_wall_ms: i64::from_le_bytes(read_n(&mut r)?),
        };
        Ok(Reader { r, header })
    }

    /// Next record, `Ok(None)` at clean end of file. A truncated final record (e.g. the app
    /// was killed mid-write) is treated as end of file.
    pub fn next_record(&mut self) -> io::Result<Option<Record>> {
        let mut dir = [0u8; 1];
        match self.r.read_exact(&mut dir) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }
        let direction = match dir[0] {
            0 => Direction::FromServer,
            1 => Direction::ToServer,
            d => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("bad direction byte {d}"),
                ));
            }
        };
        let rest = (|| -> io::Result<Record> {
            let nanos = u64::from_le_bytes(read_n(&mut self.r)?);
            let len = u32::from_le_bytes(read_n(&mut self.r)?);
            if len > MAX_FRAME {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("frame too large: {len}"),
                ));
            }
            let mut buf = vec![0u8; len as usize];
            self.r.read_exact(&mut buf)?;
            Ok(Record {
                direction,
                since_start: Duration::from_nanos(nanos),
                payload: Bytes::from(buf),
            })
        })();
        match rest {
            Ok(r) => Ok(Some(r)),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_truncation() {
        let dir = std::env::temp_dir().join(format!("lines-rec-test-{}", std::process::id()));
        let path = dir.join("t.lfr");
        let rec = Recorder::create(&path, 2, 230).unwrap();
        let frames: Vec<Bytes> = (0..1000u32)
            .map(|i| Bytes::from(i.to_le_bytes().repeat(i as usize % 7 + 1)))
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
        assert_eq!((r.header.proto_major, r.header.proto_minor), (2, 230));
        let mut prev = Duration::ZERO;
        for (i, f) in frames.iter().enumerate() {
            let rec = r.next_record().unwrap().unwrap();
            assert_eq!(&rec.payload, f);
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
    fn rejects_non_recording() {
        assert!(Reader::new(&b"NOTAFILE\x01\x00"[..]).is_err());
    }
}
