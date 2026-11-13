//! Compiles the one-function C shim that sets mimalloc's purge delay. The shim
//! is C so it names `mi_option_purge_delay` from mimalloc's own header instead
//! of hard-coding the enum index, which differs between mimalloc versions.

use std::env;

fn main() {
    println!("cargo:rerun-if-changed=src/shim.c");
    let musl = env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("musl");
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if !musl || !(arch == "x86_64" || arch == "aarch64") {
        return;
    }
    let include = env::var("DEP_MIMALLOC_INCLUDE_DIR")
        .expect("libmimalloc-sys did not export its include directory");
    cc::Build::new()
        .include(include)
        .file("src/shim.c")
        .compile("bore_alloc_shim");
}
