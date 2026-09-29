# ADR 0010: Retain direct AUHAL for macOS device I/O

- **Status:** Accepted
- **Date:** 2026-07-15

## Context

The original planning document listed `cpal` as the proposed CoreAudio device-I/O layer. The implemented macOS boundary instead uses a small C AUHAL shim in `sp-audio-io-macos`, exposed through Rust ownership and format-validation types. It creates `kAudioUnitSubType_HALOutput` directly, binds the selected default output device, configures fixed 48 kHz interleaved stereo `f32`, attaches the render callback, and has explicit stop, detach, quiescence, uninitialize, and dispose handling.

Adding `cpal` now would create a second device abstraction without addressing the constraints that motivated the direct implementation: fixed callback shapes, explicit CoreAudio format negotiation, callback-pointer retirement, and auditable teardown. The direct path now provides routed capture/playback for the product; it is still not a hardware-certification claim.

## Decision

Retain direct AUHAL as the macOS audio-device backend. Do not add `cpal` for the alpha path. Keep the C/CoreAudio surface confined to `sp-audio-io-macos`; expose platform-neutral endpoint/model types from `sp-audio-io` and keep real-time rendering behind the reviewed `OutputRenderer` and `MultiChannelDuplexRenderer` boundaries.

The device setup and teardown sequence remains a control-thread responsibility. The render callback accepts only the configured 32-, 64-, 128-, or 256-frame block, one buffer, the configured channel count, and the exact byte count before it calls Rust. A renderer remains owned until native code has stopped the unit, detached the callback, observed callback quiescence, uninitialized and disposed the unit, and explicitly retired the callback pointers.

## Consequences

- The plan's `cpal` recommendation is intentionally superseded for macOS; this ADR records the deviation rather than preserving an unused dependency recommendation.
- CoreAudio FFI, callback validation, and lifetime invariants remain local and must be documented and tested as a safety-critical boundary. See [real-time safety](../realtime-safety.md).
- Hardware timing qualification remains an external gate. Neither this decision nor hosted CI certifies long-run device timing.
- A future platform backend may implement the `sp-audio-io` boundary, but it must not weaken the fixed-capacity/realtime guarantees or change the helper-only VST3 dependency boundary.
