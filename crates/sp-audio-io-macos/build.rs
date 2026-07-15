//! Builds the `CoreAudio` harness shim.

fn main() {
    println!("cargo:rerun-if-changed=src/audio_io.c");

    cc::Build::new()
        .file("src/audio_io.c")
        .compile("superposition_audio_io");

    println!("cargo:rustc-link-lib=framework=CoreAudio");
    println!("cargo:rustc-link-lib=framework=AudioToolbox");
    println!("cargo:rustc-link-lib=framework=CoreFoundation");
}
