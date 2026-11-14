//! A plugin drawing tool's drawing in screenshot mode (decision 0020): the synthetic two-point
//! tool (`--demo-draw-tool`, builds with `demo-levels`) placed on the synthetic ES session
//! (`--plugin-drawing`) renders its levels; an unregistered tool is a failure (non-zero, no
//! file). `cargo test -p lines-app --features screenshot,demo-levels --test plugin_draw_tool`.

use std::path::Path;
use std::process::Command;

use lines_core::scratch::Scratch;

const AT: &str = "2026-11-05T19:00:00Z";
const SPAN: &str = "demo-span 2026-11-05T17:40:00Z@5998.00 2026-11-05T18:20:00Z@6012.00";

fn shoot(replay: &Path, out: &Path, extra: &[&str]) -> std::process::Output {
    let _ = std::fs::remove_file(out);
    Command::new(env!("CARGO_BIN_EXE_lines"))
        .arg("--replay")
        .arg(replay)
        .arg("--screenshot")
        .arg(out)
        .args(["--at", AT, "--layout", "chart", "--size", "900x500"])
        .args(["--price-zoom", "0.45", "--drawing-tools"])
        .args(extra)
        .output()
        .unwrap()
}

/// A PNG's width and height (its IHDR).
fn png_size(bytes: &[u8]) -> (u32, u32) {
    let be = |i: usize| u32::from_be_bytes(bytes[i..i + 4].try_into().unwrap());
    (be(16), be(20))
}

#[test]
fn a_plugin_tools_drawing_renders_its_levels() {
    let dir = Scratch::new("plugin-draw-tool");
    let rec = dir.join("synth-es.lfr");
    lines_feed::synth::write_es_sessions(&rec, false, false).unwrap();

    let plain = dir.join("plain.png");
    let r = shoot(&rec, &plain, &["--demo-draw-tool"]);
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));

    let drawn = dir.join("span.png");
    let r = shoot(
        &rec,
        &drawn,
        &["--demo-draw-tool", "--plugin-drawing", SPAN],
    );
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    let (a, b) = (
        std::fs::read(&plain).unwrap(),
        std::fs::read(&drawn).unwrap(),
    );
    assert_eq!(png_size(&b), (900, 500));
    assert_ne!(a, b, "the drawing's levels and handles are drawn");

    // Not registered (no `--demo-draw-tool`): refused before anything is drawn.
    let none = dir.join("none.png");
    let r = shoot(&rec, &none, &["--plugin-drawing", SPAN]);
    assert!(!r.status.success());
    assert!(!none.exists(), "no file on failure");
}
