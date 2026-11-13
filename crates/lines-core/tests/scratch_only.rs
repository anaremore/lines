//! Every temp folder a test, fixture or screenshot run makes goes through
//! `lines_core::scratch::Scratch`, which removes it when dropped (a failed test included):
//! a bare call to the standard temp folder anywhere else in the workspace's code is how test runs once left
//! thousands of `lines-*` folders in `%TEMP%`.

use std::path::Path;

fn rust_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            if p.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            rust_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

#[test]
fn temp_folders_are_made_only_by_scratch() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut files = Vec::new();
    rust_files(&crates, &mut files);
    assert!(files.len() > 100, "found the workspace's sources");
    // The needle split so this file doesn't match itself.
    let needle = ["temp", "_dir()"].concat();
    let offenders: Vec<String> = files
        .iter()
        .filter(|p| !p.ends_with(Path::new("lines-core/src/scratch.rs")))
        .filter(|p| std::fs::read_to_string(p).is_ok_and(|s| s.contains(&needle)))
        .map(|p| p.display().to_string())
        .collect();
    assert!(
        offenders.is_empty(),
        "use lines_core::scratch::Scratch for temp folders (removed on drop), not a bare \
         `{needle}`: {offenders:#?}"
    );
}
