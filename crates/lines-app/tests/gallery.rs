//! The UI gallery's quick set renders and passes the layout lint (`docs/design.md` "Checking
//! a UI change"): `lines --gallery DIR`. The full set (`--gallery-full`) is `#[ignore]`d.

use std::process::Command;

use lines_core::scratch::Scratch;

fn run(full: bool) {
    // Held until the gallery (and every scene it ran) has exited.
    let dir = Scratch::new(if full {
        "gallery-full"
    } else {
        "gallery-quick"
    });
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_lines"));
    cmd.arg("--gallery").arg(dir.path());
    if full {
        cmd.arg("--gallery-full");
    }
    let out = cmd.output().unwrap();
    let summary = std::fs::read_to_string(dir.join("gallery.txt")).unwrap_or_default();
    // A failure keeps the scenes' PNGs and lint reports to look at.
    let kept = (!out.status.success()).then(|| dir.keep());
    assert!(
        out.status.success(),
        "gallery failed ({}), its files kept in {kept:?}:\n{summary}\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn quick_gallery_is_clean() {
    run(false);
}

#[test]
#[ignore = "every scene: ~1 min per scene; run before UI PRs"]
fn full_gallery_is_clean() {
    run(true);
}
