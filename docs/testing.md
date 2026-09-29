# Testing strategy

Testing is organized around failure containment and deadline behavior rather than only functional output. Static verification can demonstrate contracts and dependency boundaries; it cannot certify the device-attached timing gates.

## Layers

1. **Model and protocol:** unit/property tests for versioning, bounds, parser rejection, stable IDs, parameter-scene semantics, encode/decode compatibility, and slot ownership/generation transitions.
2. **Shared memory:** deterministic tests for layout validation, publication ordering, slot ownership, sequence rollover, full/empty queues, and stale-result rejection.
3. **Engine and gate:** simulated workers that are prompt, late, crashed, malformed, silent, or restart repeatedly. Assert that an affected rack uses fallback while unrelated racks continue.
4. **Adapter and helper:** VST3 fixtures for discovery, bus negotiation, automation, MIDI, state bounds, and editor lifecycle. Run scanning and hostile fixtures outside the host process.
5. **Live hardware:** Apple Silicon runs of the headless host with the direct AUHAL callback active, recording deadline and fallback counters. See [live validation](live-validation.md).

## Static verification

The standard static verification set is:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo nextest run --workspace --all-features
cargo deny check
cargo audit
cargo llvm-cov nextest --workspace --all-features
```

`cargo xtask doctor` reports whether the workspace is ready to run those checks, including `nextest`, `cargo-deny`, `cargo-audit`, and `cargo-llvm-cov`. Its implementation-readiness result is intentionally separate from hardware certification. The doctor also lists optional tooling such as `VST3_SDK_DIR`, CMake, and Xcode; their absence does not fail a static check.

Hosted macOS CI runs the static checks and a cross-target Apple Silicon compile. It does not run live audio, loopback, external MIDI, the installed plug-in corpus, or a soak. LLVM coverage in CI covers the testable static workspace scope; it is not a real-time performance measurement.

## Real-time checks

Test builds include callback instrumentation that records queue overflow, slot misses, callback classifications, and diagnostic handoff. Instrumentation must not itself hide misses. Stress reports need percentile and worst-case duration, misses per rack, recovery outcome, and environment details.

The C AUHAL shim validates each fixed-size interleaved buffer before it enters Rust, rejects reentrancy without waiting, and requires callback retirement before the renderer is dropped. Unit tests exercise invalid callback shapes, coherent telemetry, callback retirement, failed teardown ownership, and rejected reentrancy. The detailed unsafe/FFI rules are in [real-time safety](realtime-safety.md).

## Fixtures and regression policy

Keep minimal legal fixtures and generated protocol/session corpus inputs under source control; store third-party plug-ins only when their license permits redistribution. Every fixed incident should add the smallest deterministic regression at the layer where the contract was violated. Fuzzing targets parsers and protocol decoders, never production audio callbacks.

No test result grants broad plug-in compatibility. VST3 SDK samples and installed plug-ins run through scanner/worker helpers only. The main app and engine remain outside every `sp-vst3`, `vst3`, and `vst3-host` dependency path, which `cargo xtask doctor` checks from Cargo metadata.

## Hardware verification commands

These commands are evidence gates, not success-reporting scaffolds. `compatibility` structurally parses `compatibility/corpus.toml` schema version 2 and requires every mandatory fixture to exist, scan, and meet `expected = "pass"`. The checked-in mandatory entry deliberately names a licensed installed fixture, so it fails closed until that fixture is provided.

The product AUHAL path is duplex. The `loopback` qualification command still requires externally captured `--capture` and emitted `--stimulus` f32le evidence so its latency claim is based on an independently observed physical or virtual loop; missing capture fails. `click-test --capture <f32le>` measures maximum and RMS adjacent-sample derivative from captured audio only. `midi-timing --events <json>` requires a live CoreMIDI port and at least 100 externally recorded `coremidi` sent/received timestamp pairs, then records min/mean/p50/p95/p99/max latency. `soak --hours <n> --metrics <json>` validates a supervisor report with attached hardware, operator acknowledgement, uninterrupted processes, post-warm-up RSS/end samples (no sustained growth above 5% by default), and zero unexpected faults; `--smoke` cannot qualify it.

Every report records `certified: false`; no command certifies the product. Missing captures, manual review, unavailable platform input, absent hardware, malformed artifacts, or incomplete supervisor evidence return a non-zero result. Artifact field definitions and the operator checklist are in [artifact schema](qualification/artifact-schema.md) and [checklist](qualification/checklist.md).

## UI snapshots

The show screen renders headlessly for visual review. `SUPERPOSITION_UI_SNAPSHOT=<dir> cargo test -p superposition --bin superposition render_show_screen_snapshots -- --ignored` software-rasterizes a fixture show (eight racks, generated editor pictures, sidechains, live meters) into PNGs: the 1920×1080 show screen, the laptop preset with the fault line, the route and sidechain popovers, the slot menu, the plug-in picker, scene capture, the Diagnostics setup page, the offline recovery modal, and an empty session. No window or GPU is used, so it runs in any shell. The rasterizer samples egui's font atlas approximately; faint marks on inverted fills are rasterizer artifacts, not UI.

`crates/sp-ui` tests every foreground/background pairing the renderer draws against WCAG AA, and a source test fails if a palette literal, literal dimension, or egui colour constructor appears outside the token bridge. Keyboard, VoiceOver, and 1x/2x Retina passes still need the running app.

## Dependency audit scope

The workspace and release target are aarch64 macOS. `quick-xml` advisories `RUSTSEC-2026-0194` and `RUSTSEC-2026-0195` are ignored only because the affected versions enter `Cargo.lock` through Linux-specific xcb/Wayland edges and are not compiled into macOS binaries. The exceptions are recorded in `.cargo/audit.toml` and `deny.toml`; remove them before adding any Linux target. Unmaintained transitive-crate notices remain visible warnings.
