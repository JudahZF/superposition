# Superposition: macOS-First Rust Live VST3 Host

## Context

Build a personal, dependable live-performance VST3 host in Rust, in the same product category as LiveProfessor or Waves SuperRack. The repository is greenfield. The first release will target Apple Silicon macOS, use a rack-oriented workflow, and prioritize bounded latency, fault containment, recoverable sessions, and predictable scene recall over DAW-style flexibility.

The central feasibility question is whether isolated plug-in processing can reliably finish inside a CoreAudio block deadline. The plan therefore begins with a measured worker/shared-memory spike and treats it as a hard go/no-go gate before substantial UI work.

## Alpha Product Scope

- Apple Silicon macOS; load arm64 and universal VST3 bundles only.
- 48 kHz, `f32`, 128- and 256-frame modes.
- Up to 8 active racks, 8 serial plug-in slots per rack, mono/stereo main buses.
- Hardware input or instrument source -> ordered plug-in chain -> gain/mute/bypass -> meters -> hardware output.
- Multiple independent racks may mix to outputs, but no cycles, sends, sidechains, feedback, or split-and-recombine routing in the alpha.
- MIDI 1.0 input, MIDI Learn, notes/controllers, and scene triggers.
- Parameter-oriented scenes with ramps and recall-safe scope; no live opaque-state recall.
- Session save, autosave, crash recovery, missing-plug-in placeholders, and plug-in quarantine.
- Generic parameter editor in the main app; native editor in a worker-owned macOS window.
- Worker failure preserves the host and unaffected racks; the affected rack crossfades to delayed dry audio when possible, otherwise to silence.

## Recommended Technical Stack

- Rust workspace with a pinned stable toolchain and committed `Cargo.lock`.
- [`cpal`](https://github.com/RustAudio/cpal) for CoreAudio device I/O.
- [`vst3-host`](https://docs.rs/crate/vst3-host/latest) behind an internal adapter to accelerate initial hosting work.
- [`vst3`](https://docs.rs/vst3/latest/vst3/) as the low-level escape hatch when the high-level crate lacks required lifecycle or real-time behavior.
- [`midir`](https://docs.rs/midir/latest/midir/) for MIDI 1.0 device I/O.
- `eframe`/`egui` for the main application UI.
- `objc2`/`objc2-app-kit` for worker-owned `NSWindow`/`NSView` integration.
- [`rtrb`](https://docs.rs/rtrb/latest/rtrb/) for bounded SPSC control/event queues.
- Fixed shared-memory structures with explicit atomics for per-block audio exchange.
- `serde`/JSON for versioned application state; separate opaque binary files for VST3 component/controller state.

Pin initial dependency versions after a compatibility build. In particular, keep all `vst3-host` and low-level VST3 types inside the VST3 adapter so the implementation can be replaced without changing the engine, protocol, session, or UI crates.

## Brand and Product Design Direction

Use `/Users/judahfuller/Code/quanta/brandpack.png` as the visual source of truth. Build a dark, dense, desktop-grade operational surface—not a generic dashboard or decorative hardware imitation.

### Semantic design tokens

- Canvas: Deep Navy `#0A0F16`.
- Persistent panels: Charcoal `#121820`.
- Raised cards/dialogs: Slate `#1B2330`.
- Borders/inactive outlines: Steel `#2A3242`.
- Primary text: `#E6E8EC`; secondary text: `#9AA3AE`.
- Quanta Cyan `#00E5FF`: primary actions, keyboard focus, engine online, key curves.
- Electric Blue `#007AFF`: selected/active rack, slot, tab, and secondary graph series.
- Lime `#A6FF00`: signal presence, successful completion, and healthy confirmation only.
- Red: clip, crash, timeout, destructive action, and quarantine only. Do not invent or sample the missing red from the flattened board; obtain an approved token before public release.
- Central derived scale: 4/8/12/16/24/32 px spacing; 4 px compact-control, 6 px slot, and 8 px panel radii; 1 px normal and 2 px focused borders.

No component may contain literal brand colors, typography, spacing, or status semantics outside the design-token layer.

### Typography and assets

- Sora for headings, labels, navigation, buttons, rack names, and plug-in names.
- Space Mono with tabular formatting for dB, latency, CPU, sample rate, buffer size, timestamps, IDs, and worker counters.
- Load fonts once during egui initialization. A personal alpha may use documented system fallbacks if font files are unavailable.
- The flattened PNG is direction, not a shippable asset library. Before public release, obtain licensed font files, production logo/app-icon SVGs, the approved fault-red token, and provenance/licenses for any textures or photography.

### Live-rack layout

Target an approximately 1180x720 minimum comfortable window:

1. **52 px top system bar:** product mark, session/save state, cyan engine status, Space Mono audio telemetry, MIDI status, diagnostics, and settings.
2. **248-280 px left rack navigator:** ordered rack cards with source/output, worker state, compact meters, blue selection, lime signal, and explicit fault labels/icons.
3. **Flexible center workspace:** selected rack, ordered serial plug-in slots, generic parameter editor, and explicit add/reorder/bypass/remove/native-editor actions. No graph canvas.
4. **300-360 px collapsible right inspector:** route settings, MIDI Learn, plug-in identity/latency, worker health, counters, and recovery actions.
5. **56-72 px bottom scene dock:** current/pending scenes, transition time, MIDI binding, and engine start/stop rather than DAW transport.

Collapse the inspector before compressing rack or parameter readability.

### Component and interaction rules

Build a component gallery before assembling screens. Required primitives include `SystemStatus`, `RackCard`, `PluginSlot`, `StereoMeter`, `GainFader`, `ParameterControl`, `SegmentedControl`, `ScenePad`, `WorkerHealthBadge`, `FaultBanner`, and actionable empty states.

- Use the sibling LAMDA UI files under `/Users/judahfuller/Code/quanta/lamda/src/ui/` as interaction references for compact faders, meters, controls, rack cards, and toolbar structure, but do not copy their colors, fonts, routing model, or simulated state.
- Follow the brand voice: confident, precise, clear, human. Prefer “Rack 2 missed its deadline. Dry bypass is active.” over vague error copy.
- Provide full keyboard traversal and alternatives to drag reorder; Arrow keys navigate, Space toggles, Return opens, and Escape closes.
- Show a 2 px cyan focus ring independent of hover/selection. Never communicate state by color alone.
- Support Shift fine adjustment and double-click reset only when a defined default exists.
- Expose names, roles, values, ranges, and states through AccessKit/VoiceOver.
- Respect reduced motion; only meters and bounded progress states animate. Throttle ordinary meter repainting to about 30 Hz independently of the audio engine.
- Add automated WCAG AA contrast tests for every foreground/background token pairing used by controls.

## Architecture

### Process boundaries

1. **Main application**
   - Owns CoreAudio, the immutable render graph, final mixing, MIDI input, session state, UI, scanning orchestration, and worker supervision.
   - Never loads third-party plug-in code.

2. **One runtime worker per rack**
   - Hosts all serial plug-ins in that rack in one process and one dedicated processing thread.
   - This honors plug-in isolation while avoiding an interprocess wakeup between every serial plug-in.
   - A crash removes one rack rather than the entire host. The worker publishes its current slot for best-effort fault attribution.

3. **Disposable scanner helper**
   - Loads exactly one VST3 module per invocation.
   - The parent enforces a timeout and records success, explicit error, signal, crash, or hang against that module fingerprint.

Worker isolation is an availability boundary for accidental plug-in failure, not a sandbox for malicious code.

### Main-process threads

- **AppKit/UI main thread:** egui UI and session editing; no plug-in calls.
- **CoreAudio callback:** allocation-free, lock-free, bounded render path only.
- **Engine controller:** compiles edits into inactive graph snapshots and coordinates block-boundary swaps.
- **Worker supervisor:** process lifecycle, control sockets, restart, quarantine, and state operations.
- **MIDI callbacks/service:** timestamp input, feed bounded per-producer queues, and support MIDI Learn.
- **Background services:** scanner cache, session writes, diagnostics, compatibility runs, and recovery.

### Worker threads

- **Processing thread:** waits/polls for shared-memory block requests, processes serial plug-ins using preallocated ping-pong buffers, and publishes completion through release/acquire atomics.
- **Control thread:** versioned lifecycle/state/parameter IPC over a Unix domain socket; never used by the audio callback.
- **AppKit main thread:** creates worker-owned native editor windows and hosts the plug-in's process-local `NSView`.

### Realtime block path

1. Convert device buffers into preallocated planar `f32` buffers.
2. Drain bounded MIDI/UI command queues and calculate sample offsets.
3. Copy each rack input and fixed-capacity events into its shared-memory request slot.
4. Publish all rack requests in parallel.
5. Process dry-delay paths, direct routes, and meter preparation while workers run.
6. Observe completions only until an absolute deadline below the device deadline.
7. Validate sequence, generation, output bounds, and finite samples.
8. Mix completed racks; substitute latency-matched dry fallback or a ramp to silence for late/failed racks.
9. Publish bounded diagnostics/meters and write the hardware output.

The callback must not allocate/free, lock, log, access files/network, invoke Objective-C UI work, spawn processes, serialize state, or use control IPC.

### Shared-memory protocol

Use two stable worker banks per rack so a replacement worker can be prepared without changing pointers visible to the audio thread. Each bank contains:

- `#[repr(C)]`, explicitly sized/aligned protocol header.
- Protocol/layout version and worker generation.
- Worker state, current plug-in slot, counters, timing, and heartbeat data.
- Four fixed audio/event block slots.
- Preallocated planar input/output buffers.
- Fixed-capacity input and output event arrays.
- Request/completion sequence numbers and atomic slot ownership.

Slot states: `Free`, `Requested`, `Processing`, `Complete`, `Abandoned`.

Never place Rust references, `Vec`, `String`, unrepresented enums, or serialized blobs in shared memory. Reject stale completions by sequence and generation. A timed-out processing slot is not reused until completion or process death.

### Immutable rack graph

Compile all session edits off-thread into a capacity-bounded `PreparedGraph`. Activate a prepared arena slot only at a block boundary, acknowledge activation back to the controller, and retire old resources off the audio thread.

The compiler rejects unsupported cycles, channel layouts, capacities, bus layouts, sidechains, and split/recombine paths. Track each rack's reported latency, but do not claim general plug-in delay compensation. Maintain only the local dry delay required for click-reduced bypass/failure fallback.

### VST3 adapter boundary

`sp-vst3` is used only by worker and scanner helpers. It owns:

- Module/class enumeration and lifecycle.
- Host interfaces and component/controller connection.
- Main bus negotiation/activation.
- `f32` processing setup and activation.
- Parameter metadata, formatting, gestures, and output changes.
- MIDI-to-VST event/parameter translation.
- Latency and restart notifications.
- Component/controller state capture and restoration.
- Native editor lifecycle.

Initial compatibility limits:

- One main input bus with 0, 1, or 2 channels.
- One main output bus with 1 or 2 channels.
- One event input bus.
- No sidechains, dynamic active bus changes, runtime sample-rate changes, or x86_64-only modules.

### Failure policy

- Initial worker deadline: 75% of the device period; target total callback completion below 90%.
- Discard late completions for the old sequence.
- After three consecutive misses, stop dispatching that worker bank and request supervisor recovery.
- On crash, rebuild the rack in its inactive bank, initially bypassing the suspected plug-in slot.
- Repeated failures quarantine the module fingerprint until manually cleared.
- Effects with matching topology crossfade over 64-128 samples to a continuously maintained dry delay equal to reported rack latency.
- Instruments or incompatible topologies ramp to silence.
- Crossfade back only after multiple consecutive successful blocks.

### Scanning and quarantine

Scan:

- `~/Library/Audio/Plug-Ins/VST3`
- `/Library/Audio/Plug-Ins/VST3`

For each bundle, inspect metadata and architecture without loading code, compute a fingerprint from canonical path/bundle metadata/executable metadata/content hash, reuse unchanged cache results, then launch one scanner helper with an initial 10-second timeout. Cache class IDs, vendor/version, buses, parameters, editor support, architecture, and outcome. Quarantine crash/timeout fingerprints and expose manual rescan/clear actions.

### Sessions and scenes

Use a directory package:

```text
My Set.superposition/
├── session.json
├── manifest.json
├── plugin-state/<instance-id>/component.bin
├── plugin-state/<instance-id>/controller.bin
├── plugin-state/<instance-id>/metadata.json
└── recovery/latest.json
```

- `session.json` stores settings, endpoints, racks, plug-in identities/fingerprints, normalized parameter snapshots, scenes, MIDI mappings, and references to state files.
- Preserve VST3 component and controller streams separately.
- Restore in the official order: component state, controller synchronization from component state, then controller-specific state, all while inactive/stopped/muted.
- Explicit save captures fresh opaque state one rack at a time and atomically replaces the old package.
- Autosave records the host model and parameter snapshots while referencing the last successful opaque-state capture.
- Scenes contain rack gain/mute/bypass, normalized parameter values, and transition time only. Never bind arbitrary state blobs to normal live scene recall.

### MIDI and editors

- Timestamp MIDI with a monotonic macOS clock and translate into a block/sample offset.
- Support note on/off, CC, pitch bend, channel pressure, Program Change/scene triggers, and MIDI Learn; defer SysEx and MIDI 2.0.
- Preserve note-offs and safety controls under event pressure; coalesce redundant continuous parameter changes before dropping important events.
- Main-app generic editor displays parameter name, normalized/formatted value, unit, and flags.
- Native editor opens in a top-level `NSWindow` owned by the worker. Do not attempt cross-process `NSView` embedding.

## Proposed Workspace

```text
Cargo.toml
rust-toolchain.toml
deny.toml
apps/superposition/                 # main app
helpers/sp-plugin-worker/           # one process per rack
helpers/sp-plugin-scanner/          # one module per invocation
crates/sp-model/                    # platform-neutral session model
crates/sp-protocol/                 # shared-memory and control protocol types
crates/sp-shared-memory/            # macOS mapping, banks, and slots
crates/sp-engine/                   # immutable graph and realtime callback
crates/sp-audio-io/                 # CPAL/CoreAudio configuration
crates/sp-vst3/                     # only VST3 dependency boundary
crates/sp-supervisor/               # scanner/worker lifecycle and quarantine
crates/sp-session/                  # package persistence, recovery, migrations
crates/sp-midi/                     # input, timing, mapping, and learn
crates/sp-ui/
├── src/design/                     # tokens, typography, theme, icons, accessibility
├── src/components/                 # meters, faders, rack cards, slots, scenes, faults
├── src/screens/                    # live rack, browser, editor, routing, diagnostics
└── src/component_gallery.rs        # states, accessibility, visual regression surface
crates/sp-test-support/             # fake workers, faults, signals, mock VST3
tools/xtask/                        # doctor, benchmarks, fault tests, soak, bundle
compatibility/corpus.toml
docs/architecture.md
docs/realtime-safety.md
docs/session-format.md
docs/plugin-compatibility.md
docs/testing.md
docs/brand-assets.md
docs/macos-distribution.md
docs/adr/
```

Critical implementation paths:

- `Cargo.toml`
- `crates/sp-protocol/src/shared.rs`
- `crates/sp-engine/src/realtime.rs`
- `helpers/sp-plugin-worker/src/processor.rs`
- `crates/sp-vst3/src/processing.rs`
- `crates/sp-supervisor/src/scanner.rs`
- `crates/sp-session/src/state_store.rs`
- `crates/sp-ui/src/design/tokens.rs`
- `crates/sp-ui/src/design/theme.rs`
- `crates/sp-ui/src/component_gallery.rs`
- `crates/sp-ui/src/screens/live_rack.rs`
- `docs/adr/0008-brand-design-system.md`

Dependency rules:

- Main app and engine never depend on `sp-vst3`.
- Only scanner/worker helpers link the VST3 adapter.
- UI depends on model/controller interfaces, not VST3 types.
- Session and diagnostics code never enter the realtime dependency path.

## Implementation Milestones

### Phase 0 - Workspace and contracts

Create the workspace, dependency boundaries, capacities, initial models/protocol, `xtask doctor`, CI, license checks, and ADRs for rack topology, worker granularity, shared memory, the VST3 adapter, and the brand design system. Define semantic color/spacing/type tokens, document font/logo provenance gaps, and add token contrast tests, but do not build full product screens yet.

**Gate:** workspace builds on Apple Silicon; protocol layout tests pass; main app dependency tree contains no VST3 loader; brand literals are centralized in `tokens.rs`; required foreground/background contrast pairs pass; formatting, Clippy, tests, license checks, and audit pass.

### Phase 1 - Worker feasibility spike

Before scanner completeness or UI work, implement shared-memory banks, a fake rack worker, atomic request/completion, adaptive waiting, Mach timing, configurable load, 1/2/4/8 parallel workers, and crash/hang/late modes. Add one real VST3 smoke processor after the fake path works.

Measure 48 kHz at 128 and 256 frames on the declared minimum Mac:

- Main-to-worker wake latency and scheduler outliers.
- Processing and completion-observation time.
- Total callback percentiles and deadline misses.
- CPU/energy cost and fault isolation.

**Hard gate:** for 8 no-op workers over 30 minutes, no callback overrun or protocol corruption; dispatch/completion overhead p99.99 <= 150 µs and observed max < 400 µs. Under calibrated load, callback p99.9 < 70% of the period, p99.99 < 80%, and no callback exceeds the period. Killing/hanging one worker affects only its rack and triggers bounded fallback by the current or next block.

If 128 fails, do not claim 128-frame support. If 256 fails, stop UI implementation and revise worker scheduling/data-plane design.

### Phase 2 - VST3 lifecycle and isolated scanner

Implement the adapter, serial rack lifecycle, bus validation, parameters, state order, scanner helper, cache, quarantine, and HostChecker/sample-plug-in integration.

**Gate:** main app never loads a plug-in; scanner faults identify the correct module; cache invalidates by fingerprint; arm64/universal modules scan; x86_64-only modules are rejected clearly; SDK sample plug-ins process audio/MIDI/parameters; state call-order tests pass.

### Phase 3 - CoreAudio engine and fixed rack graph

Implement device enumeration/configuration, bounded callback-size adaptation, preallocated format conversion, immutable graph arena, rack dispatch, hardware routing, gain/mute/bypass, dry delay, meters, and a realtime allocation guard.

**Gate:** no callback allocation, mutex, control IPC, or file access; graph validation tests pass; device loss safely mutes and can recover; loopback confirms expected latency; 60-minute reference run has zero overruns; swaps and bypass remain click-bounded.

### Phase 4 - Supervision and fault containment

Implement dual-bank replacement, fault counters, best-effort slot attribution, one-time recovery with suspected slot bypassed, NaN/invalid-output detection, quarantine thresholds, and wet/dry or wet/silence transitions.

**Gate:** every injected load/activation/processing/state/editor crash, hang, late block, malformed output, and disconnect leaves the main app and unaffected racks running; no unbounded wait or stale shared-memory output is accepted.

### Phase 5 - Session state and recovery

Implement versioned package persistence, separate component/controller streams, parameter snapshots, atomic writes, recovery autosave, clean-shutdown marker, migrations, and missing/changed plug-in handling.

**Gate:** save/load round-trip preserves the set; interrupted save retains the previous package; forced termination offers recovery; missing plug-ins become bypassed placeholders; opaque state is never restored during ordinary live scenes.

### Phase 6 - MIDI and scenes

Implement device selection, timing normalization, VST3 event translation, MIDI Learn, immutable mapping swaps, event-overflow policy, scene triggers, and deterministic parameter ramps.

**Gate:** virtual MIDI timing error <= 1 ms under normal load; note-offs survive queue pressure; MIDI callback allocates nothing; repeated scene runs produce deterministic transitions and never restore opaque state.

### Phase 7A - Brand design-system foundation

Only after the worker feasibility gate, load Sora/Space Mono or documented alpha fallbacks, implement the semantic token/theme layer, build the component gallery, cover interaction states, add keyboard/AccessKit metadata, and capture 1x/2x Retina visual snapshots.

**Gate:** no literal palette values exist outside `tokens.rs`; relevant components demonstrate default, hover, pressed, focused, disabled, selected, loading, success, and fault states; contrast tests pass; the gallery is keyboard-operable; VoiceOver announces name/value/state; reduced-motion behavior works; meters repaint independently at the defined UI rate.

### Phase 7B - Usable live-rack UI

Assemble the top system bar, rack navigator, center serial-chain/editor workspace, collapsible inspector, and scene dock from approved components. Add source/output selectors, plug-in browser/quarantine, controls/meters, generic editor, worker-owned native editor actions, MIDI Learn, diagnostics, save, and recovery. Do not build a graph canvas.

**Gate:** a rehearsal workflow can select devices, create/reorder racks and slots with pointer or keyboard, load/preload plug-ins, process audio/MIDI, edit long/discrete/read-only parameter sets, learn controls, recall scenes, understand a worker failure without opening diagnostics, survive a killed worker, save, relaunch, and restore. The UI remains legible at supported Retina scales.

### Phase 8 - Alpha hardening

Add compatibility corpus automation, HostChecker regressions, loopback report, click detector, MIDI report, memory tracking, fault matrix, sanitizers, and an 8-hour soak runner. Add visual-regression snapshots, keyboard-only workflow tests, a VoiceOver checklist, grayscale/color-deficiency review, long-name/truncation cases, fault-state usability tests, and meter repaint/CPU profiling.

**Gate:** 8-hour reference set at the supported block size has zero main-process crashes and unexplained overruns, no sustained memory growth above 5% after warm-up, all injected worker failures are contained, compatibility outcomes are recorded rather than silently ignored, and the branded UI passes its visual/accessibility regression suite.

### Phase 9 - Distribution preparation

For a personal alpha use local/development signing. Preserve the future security boundary: only plug-in-loading scanner/worker helpers receive `com.apple.security.cs.disable-library-validation`; the main app does not. Later add nested signing, hardened runtime, notarization, and stapling. Block public packaging until production logo/icon SVGs, licensed font files, an approved fault-red token, and documented asset provenance are available.

## Verification Tooling

Establish these standard checks:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo nextest run --workspace
cargo deny check
cargo audit
cargo llvm-cov nextest --workspace
```

Provide reproducible project commands through `cargo xtask`:

```bash
cargo xtask doctor
cargo xtask ipc-feasibility --sample-rate 48000 --block-size 128 --workers 1,2,4,8 --minutes 30
cargo xtask ipc-feasibility --sample-rate 48000 --block-size 256 --workers 1,2,4,8 --minutes 30
cargo xtask fault-matrix --sample-rate 48000 --block-size 128
cargo xtask host-checker --sdk "$VST3_SDK_DIR"
cargo xtask compatibility --manifest compatibility/corpus.toml
cargo xtask loopback --sample-rate 48000 --block-size 128 --device "BlackHole 2ch"
cargo xtask click-test --sample-rate 48000 --block-size 128 --fault worker-kill
cargo xtask midi-timing --sample-rate 48000 --block-size 128 --minutes 30
cargo xtask soak --hours 8 --sample-rate 48000 --block-size 128 --manifest compatibility/corpus.toml
```

Also use model tests for slot-state/generation transitions, Miri for platform-neutral crates, and AddressSanitizer/ThreadSanitizer in separate runs. Validate with SDK sample plug-ins plus a small real-world corpus covering no-editor, resizing, dynamic latency, state-heavy, MIDI, mono/stereo mismatch, and failure-prone cases.

## Documentation and ADRs

Before implementation makes these expensive to change, document:

1. Rack-only alpha topology.
2. One worker per rack and the feasibility go/no-go gate.
3. Fixed shared-memory layout and atomic ownership protocol.
4. Isolation of the young `vst3-host` dependency behind `sp-vst3`.
5. Worker-owned native windows and generic main-app editor.
6. Parameter scenes versus opaque session state.
7. No general PDC or split/recombine routing in the alpha.
8. macOS library-validation entitlement only on plug-in-loading helpers.
9. Brand-pack-derived semantic tokens, component semantics, accessibility rules, and asset provenance requirements.

`docs/realtime-safety.md` must state forbidden callback operations, atomic invariants, queue overflow rules, deadline/fallback policy, diagnostic handoff, and all unsafe-code assumptions. `docs/brand-assets.md` must record the brand-pack source, approved tokens, font/logo fallbacks, licenses, and the assets still required before public release.

## Top Risks

- **macOS scheduling jitter:** Phase 1 is a hard gate; independent racks run in parallel and serial plug-ins stay inside one worker.
- **Young Rust VST3 host layer:** isolate it, audit every process path, and replace deficient operations with low-level `vst3` calls without leaking that change across the application.
- **Rack-level failure scope:** a crashing plug-in removes its rack; retain the complete rack model in the main process and rebuild with the suspected slot bypassed.
- **Shared-memory races/stale output:** fixed state machine, generations, stable banks, no early slot reuse, model tests, and aggressive fault injection.
- **Unsafe plug-in state behavior:** separate state streams, official restore order, inactive-only restore, fingerprints, and parameter-only live scenes.
- **Native UI instability:** keep plug-in AppKit objects inside their worker and retain a generic editor in the main app.
- **No general PDC:** prohibit phase-sensitive split/recombine paths until compensation is deliberately implemented and tested.
- **Variable CoreAudio callback behavior:** validate actual stream behavior, use a bounded preallocated adapter, and reject devices/configurations that cannot sustain the selected mode.
- **Flattened/unlicensed brand assets:** treat the brand board as design direction only; centralize tokens and use alpha fallbacks, then require production vectors, licensed fonts, an approved fault red, and documented provenance before public distribution.

## Primary References

- Local brand source: `/Users/judahfuller/Code/quanta/brandpack.png`
- Local interaction references: `/Users/judahfuller/Code/quanta/lamda/src/ui/`
- [Steinberg VST3 SDK](https://github.com/steinbergmedia/vst3sdk)
- [Steinberg VST3 hosting and processing documentation](https://steinbergmedia.github.io/vst3_dev_portal/)
- [Steinberg persistence sequence](https://steinbergmedia.github.io/vst3_dev_portal/pages/FAQ/Persistence.html)
- [Apple real-time render guidance](https://developer.apple.com/library/archive/qa/qa1715/_index.html)
- [Apple library-validation entitlement](https://developer.apple.com/documentation/bundleresources/entitlements/com.apple.security.cs.disable-library-validation)
- [`vst3-host`](https://docs.rs/crate/vst3-host/latest)
- [`cpal`](https://github.com/RustAudio/cpal)
- [`midir`](https://docs.rs/midir/latest/midir/)
- [`rtrb`](https://docs.rs/rtrb/latest/rtrb/)
