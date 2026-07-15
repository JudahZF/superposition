//! Builds the fixed-signature shim around Darwin's variadic `shm_open` call.

fn main() {
    cc::Build::new()
        .file("src/shm_open.c")
        .compile("superposition_shm_open");
}
