//! The UI gallery's quick set renders and passes the layout lint (`docs/design.md` "Checking
//! a UI change"): `lines --gallery DIR`. The full set (`--gallery-full`) is `#[ignore]`d.

use std::path::PathBuf;
use std::process::Command;

fn run(full: bool) {
    let dir: PathBuf = std::env::temp_dir().join(format!(
        "lines-gallery-{}-{}",
        if full { "full" } else { "quick" },
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_lines"));
    cmd.arg("--gallery").arg(&dir);
    if full {
        cmd.arg("--gallery-full");
    }
    let out = cmd.output().unwrap();
    let summary = std::fs::read_to_string(dir.join("gallery.txt")).unwrap_or_default();
    assert!(
        out.status.success(),
        "gallery failed ({}):\n{summary}\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_dir_all(&dir);
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
