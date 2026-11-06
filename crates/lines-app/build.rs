//! Windows resources for `lines.exe`: the app icon and version info.
//!
//! - Icon resource ID 1 is `assets/icon/lines.ico`. GPUI's Windows backend loads icon 1 from
//!   the executable as its window-class icon, so this also sets the runtime window and taskbar
//!   icon (no extra API needed).
//! - `VERSIONINFO`: product name "Lines", version from Cargo.
//!
//! Compiled with the MSVC resource compiler (`rc.exe`) through `embed-resource`, pinned to the
//! version GPUI already uses. On non-Windows targets this script does nothing.

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // Target, not host: the check works when cross-compiling too.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
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
