//! Guard: on macOS, GPUI's platform text system must be the real one. Built without
//! `gpui_platform/font-kit`, GPUI substitutes a no-op text system that lists no fonts and
//! rasterizes nothing, so every label in the app is blank while everything else works.
//! Other platforms always have a real text system; there this test does nothing.

#[cfg(target_os = "macos")]
fn main() {
    use gpui::{FontRun, font, px};

    let ts = gpui_platform::current_platform(true).text_system();
    let names = ts.all_font_names();
    assert!(
        !names.is_empty(),
        "GPUI's macOS text system lists no fonts: built without `gpui_platform/font-kit`"
    );
    let text = "1234.25";
    let id = ts.font_id(&font(".SystemUIFont")).expect("system UI font");
    let line = ts.layout_line(
        text,
        px(12.0),
        &[FontRun {
            len: text.len(),
            font_id: id,
        }],
    );
    let glyphs: usize = line.runs.iter().map(|r| r.glyphs.len()).sum();
    assert!(
        glyphs > 0 && line.width > px(0.0),
        "{text:?} shaped to no glyphs"
    );
    println!(
        "text system ok: {} fonts, {glyphs} glyphs for {text:?}",
        names.len()
    );
}

#[cfg(not(target_os = "macos"))]
fn main() {}
