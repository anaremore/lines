//! Windows resources for `lines.exe`: the app icon and version info.
//!
//! - Icon resource ID 1 is `assets/icon/lines.ico`. GPUI's Windows backend loads icon 1 from
//!   the executable as its window-class icon, so this also sets the runtime window and taskbar
//!   icon (no extra API needed).
//! - `VERSIONINFO`: product name "Lines", version from Cargo.
//! - The main thread's stack: 8 MiB reserved (MSVC), as on Linux and macOS.
//!
//! Compiled with the MSVC resource compiler (`rc.exe`) through `embed-resource`, pinned to the
//! version GPUI already uses. On non-Windows targets only the build revision is set.
//!
//! `LINES_BUILD_REV` (for logs and diagnostics bundles): the environment's value if set (CI),
//! else `git rev-parse --short=12 HEAD` plus `-dirty` for uncommitted changes, else
//! `unknown`.

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    build_rev();
    // Target, not host: the check works when cross-compiling too.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    // The main thread's stack: 8 MiB reserved, as Linux and macOS give it (Windows' default is
    // 1 MiB). A debug build's GPUI frames on the main thread run close to 1 MiB, so a little
    // growth anywhere overflowed it; release builds never came close. Reserve only, MSVC.
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-bins=/STACK:8388608");
    }
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets this"));
    let icon = manifest_dir.join("../../assets/icon/lines.ico");
    println!("cargo:rerun-if-changed={}", icon.display());
    let icon = icon.canonicalize().expect("assets/icon/lines.ico exists");

    let version = std::env::var("CARGO_PKG_VERSION").expect("cargo sets this");
    let mut parts = version
        .split(['.', '-', '+'])
        .map(|p| p.parse::<u16>().unwrap_or(0));
    let (major, minor, patch) = (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    );
    // RC string literals: backslashes must be escaped.
    let icon_path = icon.display().to_string().replace('\\', "\\\\");
    let icon_path = icon_path.trim_start_matches("\\\\\\\\?\\\\"); // drop the \\?\ verbatim prefix
    let rc = format!(
        r#"1 ICON "{icon_path}"

1 VERSIONINFO
FILEVERSION {major},{minor},{patch},0
PRODUCTVERSION {major},{minor},{patch},0
FILEFLAGSMASK 0x3fL
FILEFLAGS 0x0L
FILEOS 0x40004L
FILETYPE 0x1L
FILESUBTYPE 0x0L
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904b0"
        BEGIN
            VALUE "FileDescription", "Lines"
            VALUE "FileVersion", "{version}"
            VALUE "InternalName", "lines"
            VALUE "OriginalFilename", "lines.exe"
            VALUE "ProductName", "Lines"
            VALUE "ProductVersion", "{version}"
            VALUE "LegalCopyright", "FSL-1.1-ALv2"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x409, 1200
    END
END
"#
    );
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("cargo sets this")).join("lines.rc");
    std::fs::write(&out, rc).expect("write lines.rc");
    embed_resource::compile(&out, embed_resource::NONE)
        .manifest_optional()
        .expect("compile Windows resources (needs the MSVC resource compiler, rc.exe)");
}

fn build_rev() {
    println!("cargo:rerun-if-env-changed=LINES_BUILD_REV");
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets this"));
    let repo = manifest_dir.join("../..");
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(&repo)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
    };
    // Rerun when HEAD moves (checkout, commit) or the index changes (dirty state). Paths come
    // from git, so this also works in worktrees (where `.git` is a file).
    let mut watch = vec!["HEAD".to_owned(), "index".to_owned()];
    if let Some(r) = git(&["symbolic-ref", "-q", "HEAD"]) {
        watch.push(r);
    }
    for w in watch {
        if let Some(p) = git(&["rev-parse", "--git-path", &w]) {
            let p = repo.join(p);
            if p.exists() {
                println!("cargo:rerun-if-changed={}", p.display());
            }
        }
    }
    let rev = std::env::var("LINES_BUILD_REV")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| {
            let head = git(&["rev-parse", "--short=12", "HEAD"])?;
            let dirty = git(&["status", "--porcelain", "--untracked-files=no"])
                .is_some_and(|s| !s.is_empty());
            Some(if dirty { format!("{head}-dirty") } else { head })
        });
    println!(
        "cargo:rustc-env=LINES_BUILD_REV={}",
        rev.unwrap_or_else(|| "unknown".into())
    );
}
