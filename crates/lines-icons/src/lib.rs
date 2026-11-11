//! The line icons Lines draws with: [Lucide](https://lucide.dev) 1.61.0 (ISC; MIT for the icons
//! derived from Feather), each file as released but for its stroke (1.6 on the 24 grid, not 2)
//! and, for `play`, `pause` and `ellipsis`, solid shapes (`fill="currentColor"`).
//! `README.md` here has the release, its hash and the one edit; `LICENSE-ISC.txt`,
//! `LICENSE-MIT.txt` and `NOTICE-feather.txt` the licence.
//!
//! No dependencies: the app serves these bytes to GPUI (`lines-app/src/icons.rs`).

/// One embedded icon: its Lucide name and its SVG.
#[derive(Clone, Copy, Debug)]
pub struct IconFile {
    pub name: &'static str,
    pub svg: &'static [u8],
}

macro_rules! icon {
    ($name:literal) => {
        IconFile {
            name: $name,
            svg: include_bytes!(concat!("../icons/", $name, ".svg")),
        }
    };
}

/// Every embedded icon, by Lucide name.
pub const ICONS: &[IconFile] = &[
    icon!("ellipsis"),
    icon!("eye-off"),
    icon!("log-in"),
    icon!("maximize"),
    icon!("panel-right"),
    icon!("pause"),
    icon!("pin"),
    icon!("play"),
    icon!("skip-back"),
    icon!("skip-forward"),
    icon!("square-arrow-out-up-right"),
    icon!("x"),
];

/// The SVG of the icon named `name`, if embedded.
pub fn svg(name: &str) -> Option<&'static [u8]> {
    ICONS.iter().find(|i| i.name == name).map(|i| i.svg)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every icon is a 24-unit Lucide SVG with the 1.6 stroke, round caps and joins; names are
    /// unique and found.
    #[test]
    fn every_icon_is_lucide_at_the_thin_stroke() {
        for (i, f) in ICONS.iter().enumerate() {
            let s = std::str::from_utf8(f.svg).unwrap();
            for want in [
                "viewBox=\"0 0 24 24\"",
                "stroke=\"currentColor\"",
                "stroke-width=\"1.6\"",
                "stroke-linecap=\"round\"",
                "stroke-linejoin=\"round\"",
            ] {
                assert!(s.contains(want), "{}: {want}", f.name);
            }
            assert!(!s.contains("stroke-width=\"2\""), "{}", f.name);
            assert!(ICONS[i + 1..].iter().all(|g| g.name != f.name));
            assert_eq!(svg(f.name).map(<[u8]>::len), Some(f.svg.len()));
        }
        assert!(svg("not-an-icon").is_none());
    }
}
