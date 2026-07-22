# Testing strategy

Testing is organized around failure containment and deadline behavior rather than only functional output. Static verification can demonstrate contracts and dependency boundaries; it cannot certify the device-attached timing gates.

## Layers

1. **Model and protocol:** unit/property tests for versioning, bounds, parser rejection, stable IDs, parameter-scene semantics, encode/decode compatibility, and slot ownership/generation transitions.
2. **Shared memory:** deterministic tests for layout validation, publication ordering, slot ownership, sequence rollover, full/empty queues, and stale-result rejection.
3. **Engine and gate:** simulated workers that are prompt, late, crashed, malformed, silent, or restart repeatedly. Assert that an affected rack uses fallback while unrelated racks continue.
4. **Adapter and helper:** VST3 fixtures for discovery, bus negotiation, automation, MIDI, state bounds, and editor lifecycle. Run scanning and hostile fixtures outside the host process.
5. **System feasibility:** dedicated Apple Silicon hardware tests with the direct AUHAL callback active, load/stress injection, and deadline/fallback counters. This is the Phase 1 hard gate, not a unit-test substitute.

## Static verification

The standard static verification set is:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo nextest run --workspace --all-features
cargo deny check
cargo audit
cargo llvm-cov nextest --workspace --all-features
cargo xtask phase0-report
```

`cargo xtask phase0-report` runs the static foundation set, records each command's exit status and SHA-256-hashed stdout/stderr under `target/phase0/logs/`, and atomically publishes `target/phase0/foundation-report.json`. It records the baseline revision plus a SHA-256 manifest of the tracked patch and untracked source files (qualification documents are excluded as evidence, not source). It returns failure when a static command or the Cargo-metadata dependency boundary fails. The report explicitly leaves hardware certification pending.

The repository configures Cargo to build through an executable `rustc` wrapper under `tools/llvm-tools/`. The wrapper reads the pinned channel from `rust-toolchain.toml` and delegates to that Rustup compiler, so `cargo-llvm-cov` discovers matching `llvm-tools-preview` binaries from the compiler sysroot even when the shell's Cargo/Rust compiler comes from another installation. The configured `LLVM_COV` and `LLVM_PROFDATA` wrappers use the same dynamic Rustup lookup for child commands.

`cargo xtask doctor` reports whether the workspace is ready to run those checks, including `nextest`, `cargo-deny`, `cargo-audit`, and `cargo-llvm-cov`. Its implementation-readiness result is intentionally separate from hardware certification. The doctor also lists phase-specific tooling such as `VST3_SDK_DIR`, CMake, and Xcode; their absence does not make a static foundation check into a hardware failure.

Hosted macOS CI runs the static checks, a cross-target Apple Silicon compile, and short synthetic IPC/fault smokes. It does not run `device-feasibility`, a 30-minute matrix, loopback, external MIDI, installed plug-in corpus, or soak certification. LLVM coverage in CI covers the testable static workspace scope; it is not a real-time performance measurement.

## Real-time checks

Test builds include callback instrumentation that records queue overflow, slot misses, callback classifications, and diagnostic handoff. Instrumentation must not itself hide misses. Stress reports need percentile and worst-case duration, misses per rack, recovery outcome, and environment details.

The C AUHAL shim validates one fixed-size interleaved stereo buffer before it enters Rust, rejects reentrancy without waiting, and requires callback retirement before the renderer is dropped. Unit tests exercise invalid callback shapes, coherent telemetry, callback retirement, failed teardown ownership, and rejected reentrancy. The detailed unsafe/FFI rules are in [real-time safety](realtime-safety.md).

## Fixtures and regression policy

Keep minimal legal fixtures and generated protocol/session corpus inputs under source control; store third-party plug-ins only when their license permits redistribution. Every fixed incident should add the smallest deterministic regression at the layer where the contract was violated. Fuzzing targets parsers and protocol decoders, never production audio callbacks.

No test result grants broad plug-in compatibility. VST3 SDK samples and installed plug-ins run through scanner/worker helpers only. The main app and engine remain outside every `sp-vst3`, `vst3`, and `vst3-host` dependency path, which `cargo xtask doctor` checks from Cargo metadata.

## Phase 1 feasibility commands and certification scope

All Phase 1 runs use the fixed 48 kHz stereo client contract. Before any attached-device run, route **BlackHole 64ch** channels 1–2 as the default output and confirm the device accepts the requested fixed 128- or 256-frame callback. BlackHole's physical stream has 64 channels; the AUHAL client is explicitly mapped to physical channels 1–2 and silences the remaining physical channels. Reports retain the physical channel count while callback telemetry proves a fixed valid stereo client buffer. Run one device command at a time; do not share the route with a DAW or capture process. The output tree is raw qualification evidence and remains ignored under `target/phase1/`.

```bash
# Short development-only synthetic preflights. These never establish attached-device timing.
cargo xtask ipc-feasibility --racks 2 --frames 128 --duration-seconds 2 \
  --output-dir target/phase1/preflight/ipc-2r-128f
cargo xtask fault-matrix --racks 2 --frames 128 \
  --output-dir target/phase1/preflight/fault-2r-128f

# Official synthetic matrix: all 1/2/4/8-rack × 128/256-frame cells, 1,800 s each.
cargo xtask ipc-matrix --duration-seconds 1800 \
  --output-dir target/phase1/ipc-matrix

# Official attached-device no-op matrix. The harness automatically samples host and worker
# `proc_pid_rusage` energy before and after every cell; an external JSON import is optional.
cargo xtask device-matrix --duration-seconds 1800 \
  --output-dir target/phase1/device-matrix

# Calibrated CPU-load evidence uses the deterministic 10%-of-period targets: 267 us at 128
# frames and 534 us at 256. Reports retain requested and observed busy duration per worker.
cargo xtask device-feasibility --racks 8 --frames 128 --duration-seconds 1800 \
  --compute-load-mode calibrated-cpu --compute-load-micros 267 \
  --output-dir target/phase1/calibrated-load/8r-128f
cargo xtask device-feasibility --racks 8 --frames 256 --duration-seconds 1800 \
  --compute-load-mode calibrated-cpu --compute-load-micros 534 \
  --output-dir target/phase1/calibrated-load/8r-256f

# Attached fault isolation at both claimed frame sizes. Each command faults rack 1 after a
# baseline completion. A zero exit status proves rack-local fallback by the current or next
# callback and continued progress for the unaffected rack.
cargo xtask device-feasibility --racks 2 --frames 128 --duration-seconds 30 \
  --fault-mode self-crash --fault-target-rack 1 --fault-trigger-sequence 2 \
  --output-dir target/phase1/fault-isolation/self-crash-2r-128f
cargo xtask device-feasibility --racks 2 --frames 128 --duration-seconds 30 \
  --fault-mode hang-after-claim --fault-target-rack 1 --fault-trigger-sequence 2 \
  --output-dir target/phase1/fault-isolation/hang-2r-128f
cargo xtask device-feasibility --racks 2 --frames 256 --duration-seconds 30 \
  --fault-mode self-crash --fault-target-rack 1 --fault-trigger-sequence 2 \
  --output-dir target/phase1/fault-isolation/self-crash-2r-256f
cargo xtask device-feasibility --racks 2 --frames 256 --duration-seconds 30 \
  --fault-mode hang-after-claim --fault-target-rack 1 --fault-trigger-sequence 2 \
  --output-dir target/phase1/fault-isolation/hang-2r-256f
# Also retain late-completion, malformed-completion, and stale-completion reports for diagnosis.

# Real Again VST3 smoke. The command builds/locates the SDK fixture, has the disposable scanner
# accept it, then permits only the isolated worker to process one finite unity-gain stereo block.
VST3_SDK_DIR=/Users/judahfuller/SDKs/vst3sdk \
  cargo xtask vst3-smoke --sdk /Users/judahfuller/SDKs/vst3sdk

# Only this aggregation command can emit certified=true. It checks the eight device reports,
# both calibrated-load reports, four attached fault-isolation reports, real VST3 smoke, SHA-256
# manifests, timing, CPU, energy, worker/bank identities, raw smoke input, and all mandatory
# threshold/counter fields.
cargo xtask phase1-report --artifact-dir target/phase1
```

`--racks` accepts `1`, `2`, `4`, or `8`; `--frames` accepts `128` or `256`. The device matrix requires **1,800 seconds per cell**. The 8-worker 30-minute cells must have zero overruns, deadline misses, and protocol corruption; dispatch/completion p99.99 **≤150 µs**; observed dispatch/completion max **<400 µs**; callback p99.9 **<70%** of the device period; callback p99.99 **<80%**; and no callback at or beyond the period. Every attached-device report retains p99.9, p99.99 status/value, and max for request→claim, processing, request→completion publication, completion publication→host observation, request→host observation, observe/dispatch, and full callback duration; requested/observed duration; active-callback proof; miss/corruption/overrun/worker-exit counters; per-worker heartbeat progression; CPU snapshots; and raw host/worker `proc_pid_rusage` energy snapshots and deltas with PIDs, nanojoules, joules, duration, availability, and errors. The process-energy measurement is mandatory; an external energy JSON is an optional cross-check only. CPU or energy `unavailable` is a certification blocker, never a zero measurement. Sleep-paced synthetic wake outliers remain in their synthetic histogram artifacts and cannot count as attached-device timing evidence. `phase1-report` reads only the required canonical artifact paths, verifies report/Markdown SHA-256 digests before schema validation, and returns evidence-incomplete with `certified=false` for any missing, unavailable, malformed, mismatched, or nonconforming field.

An empirical p99.99 requires at least **10,000 observations in that histogram**. Below that count reports serialize `p9999_micros: null` with `p9999_status: "statistically_underpowered"`; they retain the observed maximum and never substitute it for a percentile. Short/preflight reports therefore enforce active-callback proof, zero unexpected deadline misses/overruns/protocol faults, the unchanged dispatch/completion maximum **<400 µs**, callback maximum below one period, and all other observable constraints, but do not apply the 150 µs or 80%-period p99.99 limits to an unavailable percentile. This does not relax the hard gate: every 1,800-second device-matrix and calibrated-load artifact must provide at least 10,000 samples for every timing histogram, mark p99.99 `available`, and meet the original percentile and maximum limits. `phase1-report` rejects an official artifact with an unavailable or statistically underpowered p99.99, so no Phase 1 certification can be produced from short evidence.

A short attached run reports `phase1_hard_gate_certified=false` and must not be described as certification. `device-feasibility` is an output-only AUHAL timing path, not a loopback latency test.

## Phase 8 qualification evidence

Phase 8 commands are evidence gates, not success-reporting scaffolds. `compatibility` structurally parses `compatibility/corpus.toml` schema version 2 and requires every mandatory fixture to exist, scan, and meet `expected = "pass"`. The checked-in mandatory entry deliberately names a licensed installed fixture, so it fails closed until that fixture is provided.

The product AUHAL path is duplex. The `loopback` qualification command still requires externally captured `--capture` and emitted `--stimulus` f32le evidence so its latency claim is based on an independently observed physical or virtual loop; missing capture fails. `click-test --capture <f32le>` measures maximum and RMS adjacent-sample derivative from captured audio, never from fault-matrix status. `midi-timing --events <json>` requires a live CoreMIDI port and at least 100 externally recorded `coremidi` sent/received timestamp pairs, then records min/mean/p50/p95/p99/max latency. `soak --hours <n> --metrics <json>` validates a supervisor report with attached hardware, operator acknowledgement, uninterrupted processes, post-warm-up RSS/end samples (no sustained growth above 5% by default), and zero unexpected faults; `--smoke` cannot qualify it.

Every report records `certified: false`; no Phase 8 command certifies the product. Missing captures, manual review, unavailable platform input, absent hardware, malformed artifacts, or incomplete supervisor evidence return a non-zero result. Artifact field definitions and the operator checklist are in [Phase 8 artifact schema](qualification/phase8-artifact-schema.md) and [Phase 8 checklist](qualification/phase8-checklist.md).

## UI gallery

`SUPERPOSITION_COMPONENT_GALLERY=1 cargo run -p superposition` opens deterministic fixtures for every component family. It covers online/connecting/offline, active/bypassed/empty, ready/loading/faulted/missing, nominal/warning/clip, editable/read-only parameters, scene states, and the explicit dry-bypass fault banner. Use it for keyboard, VoiceOver, and 1x/2x Retina capture passes; `⌘G` toggles it during a normal run.

## Dependency audit scope

The workspace and release target are aarch64 macOS. `quick-xml` advisories `RUSTSEC-2026-0194` and `RUSTSEC-2026-0195` are ignored only because the affected versions enter `Cargo.lock` through Linux-specific xcb/Wayland edges and are not compiled into macOS binaries. The exceptions are recorded in `.cargo/audit.toml` and `deny.toml`; remove them before adding any Linux target. Unmaintained transitive-crate notices remain visible warnings.
