//! Builds narrow C shims for Darwin APIs whose SDK declarations use opaque pointer typedefs.

fn main() {
    println!("cargo:rerun-if-changed=src/shm_open.c");
    println!("cargo:rerun-if-changed=src/request_wake.c");
    println!("cargo:rerun-if-changed=src/audio_thread.c");
    cc::Build::new()
        .file("src/shm_open.c")
        .file("src/request_wake.c")
        .file("src/audio_thread.c")
        .compile("superposition_macos_shims");
}
