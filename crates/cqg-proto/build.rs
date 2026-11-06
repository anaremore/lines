//! Generates Rust types from CQG's WebAPI .proto files using `protox` (a pure-Rust protobuf
//! compiler), so no `protoc` binary is needed on any platform.
//!
//! The .proto files are NOT vendored in this repo (their licence does not clearly permit
//! redistribution). They come from the git submodule `third_party/cqg-webapi`, pinned to an
//! exact commit of github.com/cqg/WebAPIPythonSamples.

use std::path::PathBuf;

fn main() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../third_party/cqg-webapi/proto");
    let entry = root.join("WebAPI/webapi_2.proto");
    if !entry.exists() {
        panic!(
            "\n\nCQG .proto files not found at {}.\n\
             Run:  git submodule update --init\n\n",
            entry.display()
        );
    }
    println!("cargo:rerun-if-changed={}", root.display());

    let fds = protox::compile(["WebAPI/webapi_2.proto"], [&root]).expect("compile CQG protos");

    let mut config = prost_build::Config::new();
    config.include_file("cqg_webapi.rs");
    // CQG doc comments contain text (tables, `[x]`, indented samples) that rustdoc
    // misreads as links and doctests. Keep generated docs out.
    config.disable_comments(["."]);
    config
        .compile_fds(fds)
        .expect("generate Rust from CQG protos");
}
