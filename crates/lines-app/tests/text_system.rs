//! Guards on GPUI's platform text system, on the main thread (no libtest harness).
//!
//! - macOS: it must be the real one. Built without `gpui_platform/font-kit`, GPUI substitutes
//!   a no-op text system that lists no fonts and rasterizes nothing, so every label in the app
//!   is blank while everything else works.
//! - Every platform: the embedded IBM Plex fonts (decision 0034) load, are found by family,
//!   have their three weights (not one synthesized from another), and Plex Mono's digits are
//!   all one width.

use gpui::{Font, FontRun, FontWeight, PlatformTextSystem, font, px};

fn width(ts: &dyn PlatformTextSystem, f: &Font, text: &str) -> f32 {
    let id = ts.font_id(f).unwrap_or_else(|e| panic!("{f:?}: {e:#}"));
    let line = ts.layout_line(
        text,
        px(13.0),
        &[FontRun {
            len: text.len(),
            font_id: id,
        }],
    );
    let glyphs: usize = line.runs.iter().map(|r| r.glyphs.len()).sum();
    assert!(
        glyphs > 0 && line.width > px(0.0),
        "{text:?} in {f:?} shaped to no glyphs"
    );
    f32::from(line.width)
}

/// The design width of the glyph "l" (its stem) in `f`.
fn stem(ts: &dyn PlatformTextSystem, f: &Font) -> f32 {
    width(ts, f, "l");
    let id = ts.font_id(f).expect("font");
    let line = ts.layout_line(
        "l",
        px(13.0),
        &[FontRun {
            len: 1,
            font_id: id,
        }],
    );
    let glyph = line.runs[0].glyphs[0].id;
    ts.typographic_bounds(id, glyph)
        .expect("glyph bounds")
        .size
        .width
}

fn main() {
    // Headless on Windows is a no-op text system; its windowed platform only starts
    // DirectWrite here (the GPU waits for `run`). macOS and Linux have the real one headless.
    let ts = gpui_platform::current_platform(cfg!(not(target_os = "windows"))).text_system();
    // The OS's fonts (a bare Linux CI image may have none: there the embedded ones are all).
    #[cfg(target_os = "macos")]
    {
        assert!(
            !ts.all_font_names().is_empty(),
            "GPUI's macOS text system lists no fonts: built without `gpui_platform/font-kit`"
        );
        width(ts.as_ref(), &font(".SystemUIFont"), "1234.25");
    }

    ts.add_fonts(lines_fonts::all().map(std::borrow::Cow::Borrowed).collect())
        .expect("the embedded fonts load");
    let names = ts.all_font_names();
    // Linux: Lines doesn't enable `gpui_platform`'s `wayland`/`x11` features, and without them
    // GPUI's Linux platform has GPUI's no-op text system (no fonts, no text; headless only),
    // where `add_fonts` keeps nothing (`docs/known-gaps.md`). Pin that: once a real text system
    // is there, the checks below run on Linux too.
    #[cfg(target_os = "linux")]
    if names.is_empty() {
        println!("text system: GPUI's no-op (Linux without wayland/x11); Plex checks not run");
        return;
    }
    for family in [lines_fonts::SANS, lines_fonts::MONO] {
        assert!(
            names.iter().any(|n| n == family),
            "{family} isn't listed after loading"
        );
        // Each weight is its own file, not the regular one synthesized: its design bounds
        // of "l" (the stem) are wider at each step (a simulation leaves design bounds alone).
        // Advances can't tell: Plex Mono's are one width at every weight.
        let w = [FontWeight::NORMAL, FontWeight::MEDIUM, FontWeight::SEMIBOLD].map(|weight| {
            let mut f = font(family);
            f.weight = weight;
            stem(ts.as_ref(), &f)
        });
        assert!(
            w[0] < w[1] && w[1] < w[2],
            "{family}: stem widths by weight {w:?}"
        );
    }
    // Plex Mono: every digit (and the separators prices use) is one width.
    let mono = font(lines_fonts::MONO);
    let one = width(ts.as_ref(), &mono, "1");
    for c in ["0", "4", "8", ".", ",", ":", "-", " "] {
        let w = width(ts.as_ref(), &mono, c);
        assert!(
            (w - one).abs() < 0.01,
            "Plex Mono {c:?} is {w}, '1' is {one}"
        );
    }
    println!(
        "text system ok: {} fonts, IBM Plex Sans and Mono loaded",
        names.len()
    );
}
