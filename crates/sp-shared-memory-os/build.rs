//! Builds the narrow C shims for Darwin APIs whose SDK declarations use opaque pointer typedefs.
//! Linux uses `libc`, and the Wine bridge in `wine/` is built by `cargo xtask wine-bridge`.

fn main() {
    println!("cargo:rerun-if-changed=csrc");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    cc::Build::new()
        .file("csrc/shm_open.c")
        .file("csrc/request_wake.c")
        .file("csrc/audio_thread.c")
        .compile("superposition_macos_shims");
}
