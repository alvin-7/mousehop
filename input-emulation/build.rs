//! Builds the small C bridge the macOS keyboard backend needs.
//!
//! `NXEventData` is a large SDK union, so it stays in C and the Rust side only
//! sees fixed width integers and an opaque handle. The bridge is compiled for
//! macOS targets only: no other platform needs the IOKit headers, and gating
//! here keeps `CARGO_CFG_TARGET_OS` (not the host) the deciding factor when
//! cross compiling.

fn main() {
    println!("cargo:rerun-if-changed=src/nx_key_bridge.c");
    println!("cargo:rerun-if-changed=src/nx_key_bridge.h");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }

    cc::Build::new()
        .file("src/nx_key_bridge.c")
        .warnings(true)
        .compile("mousehop_nx_key_bridge");
}
