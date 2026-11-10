//! The fonts Lines draws with (decision 0034): IBM Plex Sans for UI text and IBM Plex Mono for
//! prices, sizes, times and every other number. Regular, medium and semibold only (the weights
//! the type scale uses), embedded unmodified from IBM's upstream release (`README.md` here has
//! the release tags and hashes). SIL Open Font License 1.1: `LICENSE.txt` beside this crate,
//! and every release's `THIRD_PARTY_LICENSES.txt`. "Plex" is a Reserved Font Name: the files
//! and their names are never changed.
//!
//! No dependencies: the app hands [`FILES`] to GPUI's text system before its first window.

/// The UI family (font name ID 16 of the three Sans files).
pub const SANS: &str = "IBM Plex Sans";
/// The number family (font name ID 16 of the three Mono files).
pub const MONO: &str = "IBM Plex Mono";

/// One embedded font file.
#[derive(Clone, Copy, Debug)]
pub struct FontFile {
    /// The upstream file name.
    pub name: &'static str,
    /// Its family ([`SANS`] or [`MONO`]).
    pub family: &'static str,
    /// Its OS/2 weight class (400, 500 or 600).
    pub weight: u16,
    pub bytes: &'static [u8],
}

macro_rules! font {
    ($file:literal, $family:expr, $weight:literal) => {
        FontFile {
            name: $file,
            family: $family,
            weight: $weight,
            bytes: include_bytes!(concat!("../fonts/", $file)),
        }
    };
}

/// Every embedded file.
pub const FILES: [FontFile; 6] = [
    font!("IBMPlexSans-Regular.ttf", SANS, 400),
    font!("IBMPlexSans-Medium.ttf", SANS, 500),
    font!("IBMPlexSans-SemiBold.ttf", SANS, 600),
    font!("IBMPlexMono-Regular.ttf", MONO, 400),
    font!("IBMPlexMono-Medium.ttf", MONO, 500),
    font!("IBMPlexMono-SemiBold.ttf", MONO, 600),
];

/// Names a platform may list for the embedded files besides [`SANS`] and [`MONO`]: the
/// medium and semibold files' legacy family names (font name ID 1).
pub const LEGACY_NAMES: [&str; 4] = [
    "IBM Plex Sans Medm",
    "IBM Plex Sans SmBld",
    "IBM Plex Mono Medm",
    "IBM Plex Mono SmBld",
];

/// Whether `family` is one of the embedded fonts' names (as a font list shows it).
pub fn is_bundled(family: &str) -> bool {
    family == SANS || family == MONO || LEGACY_NAMES.contains(&family)
}

/// The bytes of every embedded file, in [`FILES`] order.
pub fn all() -> impl Iterator<Item = &'static [u8]> {
    FILES.iter().map(|f| f.bytes)
}

/// The embedded size in bytes (stated in the PR that added the fonts, decision 0034 §5).
pub fn total_bytes() -> usize {
    FILES.iter().map(|f| f.bytes.len()).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A big-endian u16 / u32 at `at`.
    fn u16_at(b: &[u8], at: usize) -> u16 {
        u16::from_be_bytes([b[at], b[at + 1]])
    }
    fn u32_at(b: &[u8], at: usize) -> u32 {
        u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
    }

    /// The offset of table `tag` in a TrueType file.
    fn table(b: &[u8], tag: &[u8; 4]) -> usize {
        let n = u16_at(b, 4) as usize;
        (0..n)
            .map(|i| 12 + 16 * i)
            .find(|&r| &b[r..r + 4] == tag)
            .map(|r| u32_at(b, r + 8) as usize)
            .unwrap_or_else(|| panic!("no {:?} table", std::str::from_utf8(tag)))
    }

    /// Name `id` in US English (Windows platform, UTF-16BE), if present.
    fn name(b: &[u8], id: u16) -> Option<String> {
        let off = table(b, b"name");
        let (count, strings) = (u16_at(b, off + 2) as usize, u16_at(b, off + 4) as usize);
        (0..count).map(|i| off + 6 + 12 * i).find_map(|r| {
            let (pid, lid, nid) = (u16_at(b, r), u16_at(b, r + 4), u16_at(b, r + 6));
            (pid == 3 && lid == 0x409 && nid == id).then(|| {
                let (len, at) = (u16_at(b, r + 8) as usize, u16_at(b, r + 10) as usize);
                let s = off + strings + at;
                let units: Vec<u16> = b[s..s + len]
                    .chunks(2)
                    .map(|c| u16::from_be_bytes([c[0], c[1]]))
                    .collect();
                String::from_utf16(&units).expect("UTF-16 name")
            })
        })
    }

    /// Each file is a TrueType font of the family and weight it is listed as, so a GPUI
    /// lookup by [`SANS`] or [`MONO`] and a weight finds it.
    #[test]
    fn each_file_is_the_font_it_is_listed_as() {
        for f in FILES {
            let b = f.bytes;
            assert_eq!(&b[..4], &[0, 1, 0, 0], "{}: a TrueType file", f.name);
            // The typographic family (16), else the legacy family (1) for the regular weight.
            let family = name(b, 16).or_else(|| name(b, 1)).expect("family name");
            assert_eq!(family, f.family, "{}", f.name);
            let legacy = name(b, 1).expect("legacy family");
            assert!(is_bundled(&legacy), "{}: {legacy} isn't listed", f.name);
            let weight = u16_at(b, table(b, b"OS/2") + 4);
            assert_eq!(weight, f.weight, "{}", f.name);
        }
        // About 1.1 MB in all; a weight added by mistake shows here.
        assert!(
            (1_000_000..1_250_000).contains(&total_bytes()),
            "{}",
            total_bytes()
        );
    }

    #[test]
    fn the_licence_is_the_ofl_with_the_reserved_name() {
        let ofl = include_str!("../LICENSE.txt");
        assert!(ofl.contains("SIL Open Font License, Version 1.1"));
        assert!(ofl.contains("Reserved Font Name \"Plex\""));
    }
}
