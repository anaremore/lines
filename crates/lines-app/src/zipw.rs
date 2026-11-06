//! A minimal zip writer for diagnostics bundles: deflate entries, local headers, central
//! directory. No zip64 (a bundle is a few MB, far under 4 GB), no encryption. Built on
//! `flate2` and `crc32fast`, which the dependency tree already has (via `image`), so it adds
//! no new crate family. Any unzip tool (Explorer, Finder, `unzip`) reads the result.

use std::io::Write;

use flate2::Compression;
use flate2::write::DeflateEncoder;

struct Entry {
    name: String,
    crc: u32,
    compressed: u32,
    size: u32,
    offset: u32,
    time: u16,
    date: u16,
}

/// Builds a zip archive in memory.
pub struct ZipWriter {
    out: Vec<u8>,
    entries: Vec<Entry>,
    time: u16,
    date: u16,
}

/// A DOS date and time (the zip format's own) from Unix seconds, UTC.
pub fn dos_time(unix_secs: i64) -> (u16, u16) {
    let days = unix_secs.div_euclid(86_400);
    let secs = unix_secs.rem_euclid(86_400);
    // Civil from days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    let year = (y - 1980).clamp(0, 127) as u16;
    let date = (year << 9) | ((m as u16) << 5) | d as u16;
    let time = (((secs / 3600) as u16) << 11)
        | ((((secs / 60) % 60) as u16) << 5)
        | ((secs % 60) / 2) as u16;
    (time, date)
}

impl ZipWriter {
    /// Entries are stamped with `unix_secs` (UTC).
    pub fn new(unix_secs: i64) -> ZipWriter {
        let (time, date) = dos_time(unix_secs);
        ZipWriter {
            out: Vec::new(),
            entries: Vec::new(),
            time,
            date,
        }
    }

    /// Add a file (`/`-separated path inside the archive).
    pub fn add(&mut self, name: &str, data: &[u8]) -> std::io::Result<()> {
        let mut enc = DeflateEncoder::new(Vec::new(), Compression::default());
        enc.write_all(data)?;
        let compressed = enc.finish()?;
        let crc = crc32fast::hash(data);
        let too_big =
            |n: usize| u32::try_from(n).map_err(|_| std::io::Error::other("zip entry over 4 GB"));
        let e = Entry {
            name: name.to_owned(),
            crc,
            compressed: too_big(compressed.len())?,
            size: too_big(data.len())?,
            offset: too_big(self.out.len())?,
            time: self.time,
            date: self.date,
        };
        let o = &mut self.out;
        o.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        o.extend_from_slice(&20u16.to_le_bytes()); // version needed
        o.extend_from_slice(&0x0800u16.to_le_bytes()); // UTF-8 names
        o.extend_from_slice(&8u16.to_le_bytes()); // deflate
        o.extend_from_slice(&e.time.to_le_bytes());
        o.extend_from_slice(&e.date.to_le_bytes());
        o.extend_from_slice(&e.crc.to_le_bytes());
        o.extend_from_slice(&e.compressed.to_le_bytes());
        o.extend_from_slice(&e.size.to_le_bytes());
        o.extend_from_slice(&(e.name.len() as u16).to_le_bytes());
        o.extend_from_slice(&0u16.to_le_bytes()); // extra
        o.extend_from_slice(e.name.as_bytes());
        o.extend_from_slice(&compressed);
        self.entries.push(e);
        Ok(())
    }

    /// The archive's bytes.
    pub fn finish(mut self) -> Vec<u8> {
        let cd_start = self.out.len() as u32;
        let o = &mut self.out;
        for e in &self.entries {
            o.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            o.extend_from_slice(&20u16.to_le_bytes()); // version made by
            o.extend_from_slice(&20u16.to_le_bytes()); // version needed
            o.extend_from_slice(&0x0800u16.to_le_bytes());
            o.extend_from_slice(&8u16.to_le_bytes());
            o.extend_from_slice(&e.time.to_le_bytes());
            o.extend_from_slice(&e.date.to_le_bytes());
            o.extend_from_slice(&e.crc.to_le_bytes());
            o.extend_from_slice(&e.compressed.to_le_bytes());
            o.extend_from_slice(&e.size.to_le_bytes());
            o.extend_from_slice(&(e.name.len() as u16).to_le_bytes());
            o.extend_from_slice(&[0; 2 + 2 + 2 + 2 + 4]); // extra, comment, disk, attrs
            o.extend_from_slice(&e.offset.to_le_bytes());
            o.extend_from_slice(e.name.as_bytes());
        }
        let cd_len = o.len() as u32 - cd_start;
        o.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        o.extend_from_slice(&[0; 4]); // disk numbers
        let n = self.entries.len() as u16;
        o.extend_from_slice(&n.to_le_bytes());
        o.extend_from_slice(&n.to_le_bytes());
        o.extend_from_slice(&cd_len.to_le_bytes());
        o.extend_from_slice(&cd_start.to_le_bytes());
        o.extend_from_slice(&0u16.to_le_bytes()); // comment
        self.out
    }
}

/// Read back an archive written by [`ZipWriter`]: names and contents (tests, and the
/// redaction test that checks what a bundle really contains).
#[cfg(test)]
pub fn read(zip: &[u8]) -> Vec<(String, Vec<u8>)> {
    use std::io::Read;
    let u16_at = |i: usize| u16::from_le_bytes([zip[i], zip[i + 1]]) as usize;
    let u32_at =
        |i: usize| u32::from_le_bytes([zip[i], zip[i + 1], zip[i + 2], zip[i + 3]]) as usize;
    let eocd = (0..=zip.len() - 22)
        .rev()
        .find(|&i| u32_at(i) == 0x0605_4b50)
        .expect("end of central directory");
    let n = u16_at(eocd + 10);
    let mut at = u32_at(eocd + 16);
    let mut out = Vec::new();
    for _ in 0..n {
        assert_eq!(u32_at(at), 0x0201_4b50, "central directory entry");
        let (crc, csize, size) = (u32_at(at + 16), u32_at(at + 20), u32_at(at + 24));
        let name_len = u16_at(at + 28);
        let local = u32_at(at + 42);
        let name = String::from_utf8(zip[at + 46..at + 46 + name_len].to_vec()).unwrap();
        assert_eq!(u32_at(local), 0x0403_4b50, "local header");
        let data_at = local + 30 + u16_at(local + 26) + u16_at(local + 28);
        let mut data = Vec::new();
        flate2::read::DeflateDecoder::new(&zip[data_at..data_at + csize])
            .read_to_end(&mut data)
            .unwrap();
        assert_eq!((data.len(), crc32fast::hash(&data) as usize), (size, crc));
        out.push((name, data));
        at += 46 + name_len;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let mut z = ZipWriter::new(1_762_437_600); // 2025-11-06 14:00:00 UTC
        z.add("about.txt", b"lines 0.1").unwrap();
        let big: Vec<u8> = (0..200_000u32)
            .flat_map(|i| (i % 251).to_le_bytes())
            .collect();
        z.add("logs/lines.log", &big).unwrap();
        z.add("empty", b"").unwrap();
        let bytes = z.finish();
        assert!(bytes.len() < big.len(), "compressed");
        let back = read(&bytes);
        assert_eq!(back.len(), 3);
        assert_eq!(back[0], ("about.txt".into(), b"lines 0.1".to_vec()));
        assert_eq!(back[1].1, big);
        assert_eq!(back[2].1, b"");
    }

    #[test]
    fn dos_dates() {
        // 2025-11-06 14:00:00: year 45, month 11, day 6; 14:00:00.
        assert_eq!(
            dos_time(1_762_437_600),
            ((14 << 11), (45 << 9) | (11 << 5) | 6)
        );
        assert_eq!(
            dos_time(0).1,
            1 << 5 | 1,
            "before 1980: clamped to 1980-01-01"
        );
    }
}
