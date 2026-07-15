# Testing strategy

Testing is organized around failure containment and deadline behavior rather than only functional output.

## Layers

1. **Model and protocol:** unit/property tests for versioning, bounds, parser rejection, stable IDs, parameter-scene semantics, and encode/decode compatibility.
2. **Shared memory:** deterministic tests for layout validation, publication ordering, slot ownership, sequence rollover, full/empty queues, and stale-result rejection.
3. **Engine and gate:** simulated workers that are prompt, late, crashed, malformed, silent, or restart repeatedly. Assert that an affected rack uses fallback while unrelated racks continue.
4. **Adapter and helper:** VST3 fixtures for discovery, bus negotiation, automation, MIDI, state bounds, and editor lifecycle. Run scanning and hostile fixtures outside the host process.
5. **System feasibility:** Apple Silicon hardware tests with the device callback active, load/stress injection, and deadline/fallback counters. This is the hard Phase 1 gate, not a unit-test substitute.

## Real-time checks

Test builds will include callback instrumentation that records allocation attempts, lock/blocking hooks where platform support permits, queue overflow, slot misses, callback duration, and diagnostic handoff. Instrumentation must not itself change callback behavior enough to hide misses. Stress reports need percentile and worst-case duration, misses per rack, recovery outcome, and environment details.

## Fixtures and regression policy

Keep minimal legal fixtures and generated protocol/session corpus inputs under source control; store third-party plug-ins only when their license permits redistribution. Every fixed incident should add the smallest deterministic regression at the layer where the contract was violated. Fuzzing targets parsers and protocol decoders, never production audio callbacks.

No test result grants broad plug-in compatibility. CI enforces static checks; dedicated Apple Silicon runs establish the timing evidence required for release candidates.

## Phase 1 IPC and device harnesses

- `cargo xtask ipc-feasibility --racks <1|2|4|8> --frames <128|256> --duration-seconds <seconds>` — synthetic callback paced by sleeps; POSIX shared-memory workers.
- `cargo xtask ipc-matrix --duration-seconds <seconds> --output-dir <dir>` — all eight cells (1/2/4/8 × 128/256).
- `cargo xtask fault-matrix --racks <2|4|8> --frames <128|256> --output-dir <dir>` — kill/hang/delay/late/malformed/stale cases with recovery evidence; hang-after-claim requires observed Processing + claim tick.
- `cargo xtask device-feasibility --racks <1|2|4|8> --frames <128|256> --duration-seconds <seconds>` — CoreAudio AUHAL callback drives the same IPC observe/dispatch path and sets `coreaudio_callback_attached=true` when callbacks fire.

Use 1,800 seconds on dedicated Apple Silicon for official evidence. Hosted CI only runs short synthetic smokes.

`cargo xtask soak --smoke` runs a short fault-matrix as a development preflight; it does not certify the hard gate. `device-feasibility` requires a default output device that can run fixed 48 kHz stereo callbacks (for example BlackHole).

## UI gallery

`SUPERPOSITION_COMPONENT_GALLERY=1 cargo run -p superposition` opens deterministic fixtures for every component family. It covers online/connecting/offline, active/bypassed/empty, ready/loading/faulted/missing, nominal/warning/clip, editable/read-only parameters, scene states, and the explicit dry-bypass fault banner. Use it for keyboard, VoiceOver, and 1x/2x Retina capture passes; `⌘G` toggles it during a normal run.

## Dependency audit scope

The workspace and release target are aarch64 macOS. `quick-xml` advisories `RUSTSEC-2026-0194` and `RUSTSEC-2026-0195` are ignored only because the affected versions enter `Cargo.lock` through Linux-specific xcb/Wayland edges and are not compiled into macOS binaries. The exceptions are recorded in `.cargo/audit.toml` and `deny.toml`; remove them before adding any Linux target. Unmaintained transitive-crate notices remain visible warnings.
