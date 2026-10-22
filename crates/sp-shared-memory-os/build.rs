//! Builds the narrow C shims for Darwin APIs whose SDK declarations use opaque pointer typedefs.
//! Linux uses `libc`, and the Wine bridge in `wine/` is built by `cargo xtask wine-bridge`.

fn main() {
    println!("cargo:rerun-if-changed=csrc");
    println!("cargo:rerun-if-env-changed=SUPERPOSITION_SKIP_NATIVE_SHIMS");
    // `SUPERPOSITION_SKIP_NATIVE_SHIMS=1 cargo check --target aarch64-apple-darwin` type-checks
    // the macOS code from Linux, which has no macOS SDK. Such a build cannot link.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos")
        || std::env::var_os("SUPERPOSITION_SKIP_NATIVE_SHIMS").is_some()
    {
        return;
    }
    cc::Build::new()
        .file("csrc/shm_open.c")
        .file("csrc/request_wake.c")
        .file("csrc/audio_thread.c")
        .compile("superposition_macos_shims");
}
