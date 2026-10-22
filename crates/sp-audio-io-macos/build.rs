//! Builds the `CoreAudio` AUHAL C shim.

fn main() {
    println!("cargo:rerun-if-changed=src/audio_io.c");
    println!("cargo:rerun-if-env-changed=SUPERPOSITION_SKIP_NATIVE_SHIMS");
    // `SUPERPOSITION_SKIP_NATIVE_SHIMS=1 cargo check --target aarch64-apple-darwin` type-checks
    // the macOS code from Linux, which has no macOS SDK. Such a build cannot link.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos")
        || std::env::var_os("SUPERPOSITION_SKIP_NATIVE_SHIMS").is_some()
    {
        return;
    }

    cc::Build::new()
        .file("src/audio_io.c")
        .compile("superposition_audio_io");

    println!("cargo:rustc-link-lib=framework=CoreAudio");
    println!("cargo:rustc-link-lib=framework=AudioToolbox");
    println!("cargo:rustc-link-lib=framework=CoreFoundation");
}
