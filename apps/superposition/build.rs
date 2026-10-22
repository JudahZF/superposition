//! Declares `cfg(audio_backend)` on the platforms that have a live audio backend.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(audio_backend)");
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if matches!(target_os.as_str(), "macos" | "linux") {
        println!("cargo::rustc-cfg=audio_backend");
    }
}
