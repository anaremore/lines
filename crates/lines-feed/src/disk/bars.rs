//! The bar index: a cache of a contract's bars so startup can show them at once.
//!
//! Never the source of truth (decision 0019). It holds the chart's bars as of the last save
//! (provider history plus live trades) and is rebuilt from the segment files when it is
//! missing or fails its CRC; ranges the segments don't cover are fetched again.
//!
//! Format v1: `b"LINESBAR" | u16 version | i64 period_ms | u32 count | count × bar | u32 crc32`
//! with bar = `i64 time_ms | i64 open | i64 high | i64 low | i64 close | i64 volume`, the CRC
//! covering everything before it. Replaced atomically.

use std::io;
use std::path::Path;

use lines_core::{Bar, Price, Qty, UtcMillis};

use super::crc::crc32;

const MAGIC: &[u8; 8] = b"LINESBAR";
const VERSION: u16 = 1;
const BAR_BYTES: usize = 48;

pub fn save(path: &Path, period_ms: i64, bars: &[Bar]) -> io::Result<()> {
    let mut b = Vec::with_capacity(22 + bars.len() * BAR_BYTES + 4);
    b.extend_from_slice(MAGIC);
    b.extend_from_slice(&VERSION.to_le_bytes());
    b.extend_from_slice(&period_ms.to_le_bytes());
    b.extend_from_slice(&(bars.len() as u32).to_le_bytes());
    for bar in bars {
        for v in [
            bar.time.0,
            bar.open.0,
            bar.high.0,
            bar.low.0,
            bar.close.0,
            bar.volume.0,
        ] {
            b.extend_from_slice(&v.to_le_bytes());
        }
    }
    let crc = crc32(&b);
    b.extend_from_slice(&crc.to_le_bytes());
    super::write_atomic(path, &b)
}

/// The cached bars, or `None` if absent, of another bar period, or damaged.
pub fn load(path: &Path, period_ms: i64) -> Option<Vec<Bar>> {
    let b = std::fs::read(path).ok()?;
    if b.len() < 26 || &b[..8] != MAGIC {
        return None;
    }
    let (body, tail) = b.split_at(b.len() - 4);
    if crc32(body) != u32::from_le_bytes(tail.try_into().ok()?) {
        tracing::warn!("{}: bar index fails its CRC; rebuilding", path.display());
        return None;
    }
    let i64_at = |o: usize| i64::from_le_bytes(body[o..o + 8].try_into().expect("8 bytes"));
    if u16::from_le_bytes([body[8], body[9]]) != VERSION || i64_at(10) != period_ms {
        return None;
    }
    let n = u32::from_le_bytes(body[18..22].try_into().ok()?) as usize;
    if body.len() != 22 + n * BAR_BYTES {
        return None;
    }
    Some(
        (0..n)
            .map(|i| {
                let o = 22 + i * BAR_BYTES;
                Bar {
                    time: UtcMillis(i64_at(o)),
                    open: Price(i64_at(o + 8)),
                    high: Price(i64_at(o + 16)),
                    low: Price(i64_at(o + 24)),
                    close: Price(i64_at(o + 32)),
                    volume: Qty(i64_at(o + 40)),
                }
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_damage() {
        let dir = std::env::temp_dir().join(format!("lines-bars-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("bars");
        let bars: Vec<Bar> = (0..100)
            .map(|i| Bar {
                time: UtcMillis(i * 60_000),
                open: Price(i),
                high: Price(i + 2),
                low: Price(i - 1),
                close: Price(i + 1),
                volume: Qty(i * 3),
            })
            .collect();
        save(&p, 60_000, &bars).unwrap();
        assert_eq!(load(&p, 60_000).unwrap(), bars);
        assert!(load(&p, 300_000).is_none(), "other period");
        let mut raw = std::fs::read(&p).unwrap();
        raw[100] ^= 1;
        std::fs::write(&p, raw).unwrap();
        assert!(load(&p, 60_000).is_none(), "damaged");
    }
}
