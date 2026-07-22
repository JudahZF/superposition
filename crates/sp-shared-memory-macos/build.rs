//! Builds narrow C shims for Darwin APIs whose SDK declarations use opaque pointer typedefs.

fn main() {
    println!("cargo:rerun-if-changed=src/shm_open.c");
    println!("cargo:rerun-if-changed=src/process_energy.c");
    cc::Build::new()
        .file("src/shm_open.c")
        .file("src/process_energy.c")
        .compile("superposition_macos_shims");
}
