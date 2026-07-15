# Architecture

## Purpose and boundary

Superposition is planned as a macOS audio host whose process boundary treats plug-ins as untrusted, latency-sensitive native code. The host process owns project orchestration, audio-device integration, scheduling, UI coordination, persistence, and recovery policy. A helper process owns a rack's plug-in runtime.

This is an architecture target, not evidence that an end-to-end engine, VST3 adapter, worker, or shared-memory transport is operational.

## Rack topology

A project will contain ordered racks. A rack is the isolation and recovery unit: it owns its plug-in chain, routing configuration, parameter scenes, and worker lifecycle. Racks may be connected only by the engine's declared graph edges; plug-ins do not receive arbitrary access to another rack's memory or process.

The supervisor will launch **one worker per rack** and place a gate between that rack and the real-time graph. The gate validates protocol generation, liveness, and block sequence before accepting a worker result. A crash, protocol violation, or deadline failure closes only that rack's gate and yields a deterministic fallback; it must not take down the host or make a different rack wait.

## Data and control planes

The audio plane uses preallocated, fixed-layout shared-memory slots for the negotiated maximum channel count, frame count, events, and control records. Slot ownership and sequence counters make producer/consumer progress explicit. The Phase 1 synthetic preflight maps one `SharedBank` per no-op worker through macOS POSIX shared memory. A separate harness-only CoreAudio output path in `sp-audio-io-macos` can attach a fixed 48 kHz stereo render callback after notification-based device setup; the xtask feasibility commands still use a synthetic callback and do not certify the hard gate through an active device. The callback exchanges slot indices and bounded metadata; bulk audio and bounded events remain in the shared mapping.

A separate non-real-time control plane will handle discovery, lifecycle, parameter metadata, state transfer, diagnostics, and UI requests. It may use ordinary IPC and persistence, but it cannot be a dependency of callback progress.

## Planned package direction

`sp-model` defines durable domain types. `sp-protocol` defines versioned wire records. `sp-shared-memory` owns layout and slot primitives. `sp-engine` schedules racks, gates, dry-delay fallback, `LiveBlockPlanner`, and the product `RealtimeRackMixer`. `sp-audio-io` / `sp-audio-io-macos` adapt device callbacks and implement the product `AudioEndpoint`. `sp-vst3` confines bundle inspection and (behind the helper-only `sdk` feature) real `vst3-host` load/process/state APIs. `sp-supervisor` owns helper lifecycle, `RackSupervisor` dual-bank recovery, and fingerprint quarantine. `sp-session` persists atomic session packages with `SessionController` save/autosave/recovery. `sp-midi` owns bounded ingress queues, `midir` device input, and parameter-only scene ramps. `sp-ui` owns design tokens and renderer-free component models consumed by the egui shell. `sp-test-support` sits at the edges. The app and helpers compose these layers; lower layers must not depend on the app or UI.

## Deliberate exclusions

Phase 1 will not promise general plug-in delay compensation, arbitrary graph splits/merges, cross-rack sample-accurate routing, or universal compatibility. The initial graph must stay within the fixed shared-memory contract and the declared rack topology. See ADRs [0001](adr/0001-rack-topology.md), [0002](adr/0002-worker-per-rack-gate.md), [0003](adr/0003-fixed-shared-memory.md), and [0007](adr/0007-no-general-pdc-or-splits.md).
