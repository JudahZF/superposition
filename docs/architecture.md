# Architecture

## Purpose, status, and boundary

Superposition is a macOS audio host that treats plug-ins as untrusted, latency-sensitive native code. The host process owns session orchestration, device integration, scheduling, UI coordination, persistence, and recovery policy; helpers own the code paths that load VST3 bundles.

This repository is an alpha, not a certified product. It has fixed shared-memory protocol/layout types, helper-only VST3 scanning, retained serial-rack workers, and a duplex product mixer/endpoint composition. The desktop app can select independent input/output devices and per-rack mono/stereo channels, dispatch audio and bounded MIDI to indexed rack workers, mix validated completions, edit normalized parameters, persist component/controller state, open or focus worker-owned native-editor windows, and replace a failed rack worker on its alternate stable bank. Long-running hardware timing evidence remains incomplete.

## Rack topology and failure scope

A project is modeled as ordered racks. A rack is the intended isolation and recovery unit: it owns its plug-in chain, routing configuration, parameter scenes, and worker lifecycle. The alpha topology deliberately excludes arbitrary split/recombine paths, feedback, and general plug-in delay compensation. A plug-in may take a stereo sidechain from a physical input pair, in the same block, or from another rack's post-fader output, one block late. That delay keeps racks independent and parallel, so sidechains need no cycle check.

The product supervisor starts one retained worker per loaded rack and places a gate between that worker and the real-time graph. Explicit rack indices are retained even when only a sparse subset of racks has workers. The gate accepts only a live generation and block ticket. A deadline miss abandons the request, renders rack-local fallback, and requests recovery before that bank can be reused. A crash or malformed result also closes the gate immediately. The dispatcher publishes the current input block, then validates stereo completion metadata, bounded MIDI and automation, ticket identity, and finite output within the callback deadline. Each rack pre-maps two stable banks. On a worker fault, a lock-free control/callback handshake quiesces that rack, the supervisor reaps the old worker and starts one replacement on the alternate bank with the suspected slot deactivated, and the callback switches generations at a block boundary. The retired bank is reset only after the old worker is gone; unaffected racks continue dispatching throughout.

## Device I/O: direct AUHAL

macOS uses the direct AUHAL boundary in `sp-audio-io-macos`; `cpal` is not a workspace dependency. The C shim creates `kAudioUnitSubType_HALOutput`, binds the selected route, captures input through `AudioUnitRender`, and installs an interleaved `f32` playback callback. Product operation uses 48 kHz and 32/64/128/256-frame buffers, with up to 64 physical channels and mono/stereo routes per rack. `ProductRenderer` dispatches gathered rack inputs, each sidechained slot's aux audio, and callback-safe CoreMIDI events to workers, mixes valid completions to their selected output channels, and enforces native callback retirement before Rust drops the renderer. Separate input/output devices use a private CoreAudio aggregate with drift compensation; the host does not implement its own cross-device resampler. Device support and timing still require live validation.

[ADR 0010](adr/0010-direct-auhal-backend.md) records this deliberate deviation from the original `cpal` recommendation.

## Data and control planes

The data plane uses `#[repr(C)]`, fixed-layout `SharedBank` mappings. A bank contains the protocol header, fixed rack descriptors, fixed-capacity block slots, and an atomic parameter-feedback sidecar; it contains no Rust references, heap containers, strings, or Rust enum fields. Each block slot holds the rack input, a stereo sidechain region for each of the eight plug-in slots, the output, and bounded MIDI and events. A request bitmask marks the sidechain regions written for that block; the worker feeds silence to an active sidechain input whose region is not marked. See [real-time safety](realtime-safety.md) for the copy paths. Payload is written before a release publication, and the peer acquire-loads the state before it reads that payload. Slot tickets carry generation and sequence; a stale completion is rejected rather than mixed.

On macOS, `sp-shared-memory-macos` creates a named POSIX mapping sized to exactly one `SharedBank`, initializes it before handing its name to a worker, and gives the mapping's owner access only through the safe protocol APIs. The product dispatcher has preallocated per-rack output buffers and uses the Darwin continuous clock to bound completion observation. It receives no control IPC and does not launch, log, allocate, or sleep from the callback path.

`SharedBank::new` builds the fixed layout in a box, field by field. The mapping
creator copies that initialized bank into its exclusive mapping; replacement
does the same only after the old worker exits. The product endpoint separately
retains its original renderer across a successful stop. It makes the renderer
available for a later start only after native callback retirement is proven;
uncertain teardown latches a fault and prevents endpoint reuse.
When audio starts after a bank handoff, the app supplies the active bank index
with both mappings; it rejects an unfinished handoff instead of assuming bank 0.
While stopped, only the retained endpoint can acknowledge recovery for a rack
whose signal it still owns. This keeps the app from advancing that handshake
through a second path.

The separate control plane is reserved for discovery, helper lifecycle, parameters, state transfer, diagnostics, and UI requests. It may use ordinary IPC and persistence, but it cannot be a dependency of callback progress.

Native Open, Close, Focus, and Resize requests go to the worker's initial AppKit
thread through editor handles that do not own DSP. The processing thread keeps
the rack runtime while these requests run. The AppKit thread owns the top-level
`NSWindow`, attaches only its content `NSView` to the VST3 controller, pumps
events, and performs focus, resize, detach, and close. Open focuses an existing
window. The red close button defers closing until the plug-in view detaches.
Weak service handles deliver bounded visualization messages and parameter
updates and drain editor resize requests without retaining an unloaded plug-in.
The app begins Open asynchronously, then polls its reply so UI updates and
meters can continue. Open and Close have 30-second control timeouts; the audio
callback deadline does not change. State capture and batched parameter reads
(`BeginStateCapture`, `ReadStateChunk`, `ReleaseStateTransfer`, `ReadParameters`)
run on the same loading thread as the editor, so Save and scene capture never
pause the rack. VST3 permits `getState` and `getParamNormalized` there while
`process` runs. Live captures use the upper half of the transfer-ID space, so
chunk and release requests route to their owner without shared state. Loading
and state restore still use the ordinary control handoff with stopped audio.
Destructive slot replacement and shutdown route through the main-thread owner
so an attached view is removed before its controller is destroyed. The main
application never receives a plug-in view or SDK pointer. Repeated live open/close
checks passed on BlackHole for Pro-Q 4 and Archetype Nolly X; see [live validation](live-validation.md).

Native editor parameter callbacks publish latest values into the worker's
fixed-ID atomic mirror. Its loading-thread service writes a per-slot
shared-memory sidecar with epoch, revision, and overflow counters; a separate
mapping lets that thread write while DSP retains its mapping. The app reads
sidecar snapshots off the audio thread and validates generation, slot identity,
and writable parameter IDs before changing the session model. Overflow marks
the mirror incomplete, so explicit Save remains the full-state capture path;
it does not stop audio. The sidecar does not make the entire worker lock-free: vendored
VST3 `ParameterChanges` now preallocates 4,096 queues and 8,192 points per
container. Its block-time operations use atomic, nonblocking access; capacity
or contention losses are counted in the sidecar overflow total. This bounds
the host container, not third-party plug-in code.

Protocol 7 also retains counted restart requests per slot and flag. The app
observes requests without consuming them and plans a rack-local, quiescent
replacement for component reload or I/O change. Latency changes only retune the
dry delay, because some plug-ins report one on every activation. Replacement
state capture and worker launch run off the UI and audio threads. Other flags
remain diagnostic; planned maintenance still needs live validation, including
native gestures followed by a worker crash.

The scanner is the sole owner of fresh scan-failure quarantine ingestion;
cached failures do not add another count. A quarantined plug-in is rejected
before worker launch with its name in the load error, including when it is a
later rack slot. The Setup page lists quarantined plug-ins and allows a retry for one
fingerprint without clearing other failure records.

## Package direction and VST3 boundary

`sp-model` defines durable domain types. `sp-protocol` defines versioned wire records. `sp-shared-memory` owns safe fixed-layout and slot primitives, while `sp-shared-memory-macos` owns POSIX mapping and Darwin-clock FFI. `sp-engine` owns graph/gate/mixing foundations. `sp-audio-io` contains platform-neutral endpoint types, and `sp-audio-io-macos` owns the direct AUHAL implementation. `sp-supervisor`, `sp-session`, `sp-midi`, and `sp-ui` remain outside the callback dependency path.

`sp-vst3` is the sole VST3 adapter boundary. Its SDK feature is enabled by `sp-plugin-worker` and `sp-plugin-scanner` only. The main app and `sp-engine` must not transitively reach `sp-vst3`, `vst3`, or `vst3-host`; direct dependency edges remain limited to the helper/adapter chain. No app, engine, model, protocol, session, or UI type may expose VST3 SDK types. `cargo xtask doctor` verifies that dependency rule from Cargo metadata. The scanner and worker helper capabilities are partial foundations, not a general VST3 compatibility assertion.

See [ADR 0001](adr/0001-rack-topology.md), [ADR 0002](adr/0002-worker-per-rack-gate.md), [ADR 0003](adr/0003-fixed-shared-memory.md), [ADR 0004](adr/0004-vst3-adapter.md), [ADR 0007](adr/0007-no-general-pdc-or-splits.md), and [ADR 0010](adr/0010-direct-auhal-backend.md).
