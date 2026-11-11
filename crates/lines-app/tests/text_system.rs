//! Guards on GPUI's platform text system, on the main thread (no libtest harness).
//!
//! - macOS and Linux: it must be the real one. Built without `gpui_platform/font-kit` (macOS)
//!   or without its `wayland`/`x11` backends (Linux), GPUI substitutes a no-op text system that
//!   lists no fonts and rasterizes nothing, so every label in the app is blank (on Linux there
//!   is no window either).
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

/// The design width of the glyph "l" (its stem) in `f` (not on Linux: there it's the advance).
#[cfg(not(target_os = "linux"))]
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
    // Without its `wayland`/`x11` backends GPUI's Linux text system is the no-op one, which
    // keeps nothing `add_fonts` gives it.
    #[cfg(target_os = "linux")]
    assert!(
        !names.is_empty(),
        "GPUI's Linux text system lists no fonts: built without `gpui_platform/wayland`/`x11`"
    );
    for family in [lines_fonts::SANS, lines_fonts::MONO] {
        assert!(
            names.iter().any(|n| n == family),
            "{family} isn't listed after loading"
        );
        let weights =
            [FontWeight::NORMAL, FontWeight::MEDIUM, FontWeight::SEMIBOLD].map(|weight| {
                let mut f = font(family);
                f.weight = weight;
                f
            });
        // Each weight is its own file, not the regular one synthesized: its design bounds
        // of "l" (the stem) are wider at each step (a simulation leaves design bounds alone).
        // Advances can't tell: Plex Mono's are one width at every weight.
        #[cfg(not(target_os = "linux"))]
        {
            let w = weights.each_ref().map(|f| stem(ts.as_ref(), f));
            assert!(
                w[0] < w[1] && w[1] < w[2],
                "{family}: stem widths by weight {w:?}"
            );
        }
        // Linux (cosmic-text): its glyph bounds are the advance, so the stem can't tell for
        // Plex Mono. There each loaded face is its own font id and a weight picks the nearest
        // face, so three ids mean three files (a missing weight would reuse another's id).
        // On a machine with IBM Plex installed, a system face can stand in for a missing
        // embedded one (CI's image has no Plex; `docs/known-gaps.md`, UI font).
        #[cfg(target_os = "linux")]
        {
            let ids = weights.each_ref().map(|f| {
                width(ts.as_ref(), f, "l");
                ts.font_id(f).expect("font")
            });
            assert!(
                ids[0] != ids[1] && ids[1] != ids[2] && ids[0] != ids[2],
                "{family}: one face serves several weights {ids:?}"
            );
        }
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
