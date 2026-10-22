# Architecture

## Purpose, status, and boundary

Superposition is a macOS and Linux audio host (on Linux, also for Windows VST3 plug-ins under Wine) that treats plug-ins as untrusted, latency-sensitive native code. The host process owns session orchestration, device integration, scheduling, UI coordination, persistence, and recovery policy; helpers own the code paths that load VST3 bundles.

This repository is an alpha, not a certified product. It has fixed shared-memory protocol/layout types, helper-only VST3 scanning, retained serial-rack workers, and a duplex product mixer/endpoint composition. The desktop app can select independent input/output devices and per-rack mono/stereo channels, dispatch audio and bounded MIDI to indexed rack workers, mix validated completions, edit normalized parameters, persist component/controller state, open or focus worker-owned native-editor windows, and replace a failed rack worker on its alternate stable bank. Long-running hardware timing evidence remains incomplete.

## Rack topology and failure scope

A project is modeled as ordered racks. A rack is the intended isolation and recovery unit: it owns its plug-in chain, routing configuration, parameter scenes, and worker lifecycle. The alpha topology deliberately excludes arbitrary split/recombine paths, feedback, and general plug-in delay compensation. A plug-in may take a stereo sidechain from a physical input pair, in the same block, or from another rack's post-fader output, one block late. That delay keeps racks independent and parallel, so sidechains need no cycle check.

The product supervisor starts one retained worker per loaded rack (one per segment of a [mixed rack](#mixed-racks)) and places a gate between that worker and the real-time graph. Explicit rack indices are retained even when only a sparse subset of racks has workers. The gate accepts only a live generation and block ticket. A deadline miss abandons the request, renders rack-local fallback, and requests recovery before that bank can be reused. A crash or malformed result also closes the gate immediately. The dispatcher publishes the current input block, then validates stereo completion metadata, bounded MIDI and automation, ticket identity, and finite output within the callback deadline. Each rack pre-maps two stable banks. On a worker fault, a lock-free control/callback handshake quiesces that rack, the supervisor reaps the old worker and starts one replacement on the alternate bank with the suspected slot deactivated, and the callback switches generations at a block boundary. The retired bank is reset only after the old worker is gone; unaffected racks continue dispatching throughout.

## Device I/O

`ProductRenderer` and the rack dispatcher live in the platform-neutral `sp-audio-io` crate. Each backend owns its streams and an endpoint with the same inherent API, so the app selects one through `platform_audio`. A session runs at 48 or 96 kHz (`sample_rate_hz` in its audio settings); the renderer times scene ramps, MIDI offsets, and worker deadlines at that rate, and an endpoint refuses to start at any other.

### macOS: direct AUHAL

macOS uses the direct AUHAL boundary in `sp-audio-io-macos`; `cpal` is not a workspace dependency. The C shim creates `kAudioUnitSubType_HALOutput`, binds the selected route, captures input through `AudioUnitRender`, and installs an interleaved `f32` playback callback. Product operation uses 48 kHz and 32/64/128/256-frame buffers, with up to 64 physical channels and mono/stereo routes per rack. `ProductRenderer` dispatches gathered rack inputs, each sidechained slot's aux audio, and callback-safe CoreMIDI events to workers, mixes valid completions to their selected output channels, and enforces native callback retirement before Rust drops the renderer. Separate input/output devices use a private CoreAudio aggregate with drift compensation; the host does not implement its own cross-device resampler. AUHAL runs at 48 kHz only. Device support and timing still require live validation.

### Linux: JACK

Linux uses JACK (PipeWire's JACK library or jackd) through `quanta-audio-io`, in `sp-audio-io-linux`. JACK exposes one device, the server, which owns the rate and quantum: a route starts only when they equal the session rate and block size (set the PipeWire quantum with `pw-metadata -n settings 0 clock.force-quantum N`). The callback interleaves JACK's planar ports into preallocated buffers for the renderer. `quanta-audio-io` keeps the callback until the stream is dropped and deactivates JACK before freeing it, so the callback returns the renderer from its `Drop`; a stopped endpoint keeps its renderer exactly as on macOS. If the JACK server died, the callback is leaked and the endpoint latches a fault.

[ADR 0010](adr/0010-direct-auhal-backend.md) records this deliberate deviation from the original `cpal` recommendation.

## Device I/O: Tunnel

`sp-audio-io-tunnel` makes Superposition a Tunnel device, so a Tunnel mixer such as Lambda can use its racks as inserts and send-returns. It embeds `tunnel-host` and `tunnel-device` at Lambda's revision. `TunnelDevice` runs for as long as the app wants Tunnel and declares two endpoints with persistent IDs: "Superposition inputs" (sink) and "Superposition outputs" (source), each with up to 64 channels. The Tunnel master adopts the device and makes every route; Superposition only exposes the endpoints. `TunnelAudioEndpoint` implements `AudioEndpoint` with the same inherent API as the JACK endpoint. Its device ID is `tunnel:<device name>`.

A Tunnel session is clocked by Tunnel: one cycle of 12 or 24 frames on the Tunnel network thread. That is too short for worker round trips, so the session has two halves. On the network thread, the device's audio app only converts 24-bit samples and moves whole cycles through two preallocated SPSC rings; it does not allocate, lock, or wait, and it wakes the render thread with `Thread::unpark` when a product block is complete. The render thread (`SCHED_FIFO` through `AudioThreadPolicy`, ordinary scheduling when denied) takes 32–256 frames from the input ring, runs `ProductRenderer` once, and pushes the output.

The network side plays a lead-in of silence before it reads the output ring. Output media time `t` therefore always carries the input presented at `t − latency`, with latency = 2 × block + 24 frames: one block to gather input, up to one block for the render (workers have 75 % of a block), and the largest Tunnel cycle, because blocks and cycles do not divide. The latency is fixed for the session, whatever cycle the master chooses, and the app reports it so a Lambda insert can compensate; Tunnel flow latencies come on top. At 96 kHz a 64-frame block adds 152 frames (1.58 ms). There is one clock, so there is no drift and no resampling.

Output that is late plays as silence for the missing frames, counts as an underrun, and is discarded when it comes, so later frames keep the latency. An input cycle that does not fit (the render thread is far behind) counts as an overrun and starts a resync: the render thread drops stale input, the network side drops stale output, and the lead-in plays again. In `CallbackTelemetry` a callback is one product block (`rendered` or `silenced`) or one underrun or overrun cycle (`invalid_bytes`). Stopping stops the render thread and returns the original renderer; if the thread does not finish, the endpoint latches a fault. The simulator test runs the same adapter with the render step inside the cycle (`InlineApp`), because the simulator runs in virtual time on one thread.

## Data and control planes

The data plane uses `#[repr(C)]`, fixed-layout `SharedBank` mappings. A bank contains the protocol header, fixed rack descriptors, fixed-capacity block slots, and an atomic parameter-feedback sidecar; it contains no Rust references, heap containers, strings, or Rust enum fields. Each block slot holds the rack input, a stereo sidechain region for each of the eight plug-in slots, the output, and bounded MIDI and events. A request bitmask marks the sidechain regions written for that block; the worker feeds silence to an active sidechain input whose region is not marked. See [real-time safety](realtime-safety.md) for the copy paths. Payload is written before a release publication, and the peer acquire-loads the state before it reads that payload. Slot tickets carry generation and sequence; a stale completion is rejected rather than mixed.

`sp-shared-memory-os` creates a named POSIX mapping sized to exactly one `SharedBank`, initializes it before handing its name to a worker, and gives the mapping's owner access only through the safe protocol APIs. macOS uses the continuous Mach clock and `os_sync` address waits; Linux uses `/dev/shm`, `CLOCK_MONOTONIC` nanoseconds, and a process-shared futex on the request-wake word. The product dispatcher has preallocated per-rack output buffers and uses that clock to bound completion observation.

A Windows plug-in worker runs under Wine against a Linux host. It is the same worker built for `x86_64-pc-windows-gnu`; it opens the host's bank through Wine's `Z:` drive (`Z:\dev\shm\<name>`), which Wine backs with a shared mapping of the same pages. Wine traps raw Linux syscalls from PE code, so the futex wait/wake, the `CLOCK_MONOTONIC` clock, and `SCHED_FIFO` come from `sp_wine_bridge`: a builtin PE DLL with a unixlib (`crates/sp-shared-memory-os/wine`, built by `cargo xtask wine-bridge`). Wine pairs the two halves only when it finds the DLL through `WINEDLLPATH` in its `x86_64-windows`/`x86_64-unix` layout. `cargo xtask wine-check` runs the Windows client under Wine against a Linux bank and times the round trips. The bank layout has only fixed-width fields, so both builds validate the same header.

`cargo xtask windows-helpers` puts `sp-plugin-worker.exe` and `sp-plugin-scanner.exe` in `<helpers>/windows/` and the bridge in `<helpers>/wine-bridge/`, beside the native helpers. With them and Wine present, `ProductRuntime` keeps one `sp_wine::WineEnvironment`: the shared prefix (`<app support>/wine`, or `SUPERPOSITION_WINE_PREFIX`), the private display, and one persistent `wineserver`. The prefix is prepared on first use (scan, worker launch, or installer); a stale server from a crashed host is ended first, and Wine Mono and Gecko are not installed automatically. A display restart restarts the server, so Windows workers recover on the new display. Every Wine process gets `WINEPREFIX`, `WINEDLLPATH` (the bridge), `WINEDEBUG=-all`, and the private `DISPLAY` and `XAUTHORITY`, never `WAYLAND_DISPLAY`. A Windows worker starts as `wine <helpers>/windows/sp-plugin-worker.exe --worker … --sample-rate <rate> --bundle Z:\…\Plugin.vst3`; bank names are unchanged, and every bundle path the worker sees goes through `Z:`. Wine keeps the process the supervisor started, so its PID, stdio, and kill work as for a native worker. With a warm server, the worker answers its first health request in about 0.1 s.

A rack with a Windows plug-in is pipelined: the dispatcher publishes its request after the block's observation pass and mixes the result one block later, under the same deadline and fallback rules, and delays the rack's dry path by one block so fallback and bypass stay aligned. Other racks keep the same-block deadline. Audio start marks these racks pipelined (`ProductRenderer::set_rack_pipelined`) from their first block, and a live layout change adds their lanes with `with_pipelined(true)`. Setup's Diagnostics lists these racks with the extra block of latency.

### Mixed racks

A rack may mix Linux and Windows plug-ins. Each run of same-platform slots is a segment with its own worker and bank pair; a missing plug-in joins the segment before it. The host's lane is the first segment's worker, pipelined, and the host never sends audio to the others. After its own plug-ins, a segment's worker forwards the block to the next segment's active bank (`sp_shared_memory_os::ChainLink`), waits for it, and completes its own request with the result, so all segments run in series inside the one extra block. Each worker numbers its slots from 0: it forwards the events and sidechains of later slots renumbered, and MIDI to every segment. A forwarded request carries the rack request's publication tick, one hop margin earlier per segment (an eighth of the block, at least 100 µs), and each segment waits until one block period after the tick it received, less one margin, so a segment finishes before the one waiting on it. A waiting link spins for 200 µs, then sleeps in 100 µs slices; a Windows worker yields instead, because a short sleep under Wine lasts a millisecond. Each bank header carries the worker's own plug-in latency and its total, which includes the segments after it, so the host's dry path follows the whole chain.

The host starts later segments first, last to first, and then points each worker at the next with the `Rechain` control operation (a bank name, the next segment's first slot, and a skipped latency), each followed by a health request that fences the worker's latency publication. Control operations go to the worker that runs the slot, with its own slot number. A later segment fails when its worker exits or stops its heartbeat, or when the worker before it misses its deadline: that worker then plays its own output delayed by the downstream latency (a preallocated delay line) and increments `chain_link_failures` in its bank header, which the host polls. The host stops the failed worker, rechains the worker before it past the stopped segment, which then forwards its output delayed by the skipped latency, so the segments after it keep processing and the rack's latency does not change, and starts a replacement on the segment's other bank with the saved states. A new link forgives up to 32 misses before the next segment is first heard, because a worker's first blocks can be slow; a new segment is heard after three good blocks and fades in over 16 samples. A segment that fails three times in ten seconds stays bypassed until the rack reloads. The first segment's worker recovers as any rack worker: the whole rack plays its dry fallback meanwhile, and the new worker is rechained before the lane switches to it. Planned restarts and in-place slot edits do not apply to mixed racks; such changes rebuild the rack. The segments live only in the running rack: a session stores its plug-ins, and the split follows each plug-in's cataloged platform.

Workers set plug-ins up for one sample rate, passed as `--sample-rate` (48000 or 96000), which also sizes the processing thread's scheduling policy. Audio start restarts any rack worker that runs at another rate, with its captured state.

On Linux the processing thread requests `SCHED_FIFO` (priority `SUPERPOSITION_RT_PRIORITY`, default 70). Without `RLIMIT_RTPRIO` it keeps ordinary scheduling and warns once; `SUPERPOSITION_REQUIRE_REALTIME=1` makes that an error. It receives no control IPC and does not launch, log, allocate, or sleep from the callback path.

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

Each worker's control channel is its stdin and stdout: length-prefixed `sp-protocol` frames, requests in and responses out, one request at a time. The same pipes work on macOS, on Linux, and for a Windows worker under Wine, whose standard handles are the host's pipes (Wine's `AF_UNIX` support to a Linux peer is not relied on). Before any plug-in loads, the worker moves the inherited streams to private descriptors and points its standard output at standard error, and on Windows also moves loaded C runtimes' descriptor 1, so plug-in output cannot corrupt a frame. Reader threads on both ends collect whole frames, so a missed deadline never leaves half a frame in a pipe; after an error the host stops using that worker's pipes. The first health response follows the worker's first plug-in load and may take up to the long control deadline. When the host closes the channel, the worker shuts down; there is no reconnection.

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

On Linux the same requests, threads, and ownership apply to X11. Every editor
renders on an engine-owned private X server, never the user's desktop.
`sp-supervisor::PrivateDisplay` starts `Xvfb` (from `SUPERPOSITION_XVFB` or
`PATH`) with `-displayfd`, no TCP listener, a 3840×2160 screen, and a private
MIT cookie, and restarts it on the same display number if it exits.
`ProcessSupervisor::set_display` gives helper children its `DISPLAY` and
`XAUTHORITY`; children never inherit the desktop's `DISPLAY`, `XAUTHORITY`, or
`WAYLAND_DISPLAY`. Before plug-ins load, the worker creates the ICCCM and EWMH
atoms a window manager would; JUCE otherwise names a property with atom 0 on a
bare server, and Xlib exits the process. The loading thread owns one top-level
window per open editor and attaches the plug-in view as `X11EmbedWindowID`.
Each pass services the editor's VST3 `IRunLoop` timers and file descriptors,
which JUCE, VSTGUI, and DPF editors need in order to draw, then handles window
events. `WM_DELETE_WINDOW` is the deferred close, as the red button is on macOS.
Each window carries `WM_NAME` and `_NET_WM_NAME` (plug-in, rack, and slot),
`_NET_WM_PID`, and `CARDINAL` `_SUPERPOSITION_RACK` and zero-based
`_SUPERPOSITION_SLOT` properties, so the window streamer can map it to its
slot. Preview pictures come from an X `GetImage` of the window.

The Windows worker owns a top-level Win32 window per open editor and attaches
the plug-in view as `kPlatformTypeHWND`. Wine maps the window to an X11 window
on the private display, titled like the Linux one, with `_NET_WM_PID` set to the
worker's PID. PE code cannot call Xlib, so the worker finds that X11 window
through Wine's `__wine_x11_whole_window` property and labels it with
`_SUPERPOSITION_RACK` and `_SUPERPOSITION_SLOT` through `sp_wine_bridge`, whose
unixlib uses the `libX11` that Wine's X11 driver already loaded. The streamer
and app therefore treat Windows and Linux editor windows alike. Windows editors
run on window messages, so the loading thread dispatches its message queue on
every pass, with or without an open editor; it also starts OLE before plug-ins
load. Preview pictures are a GDI copy of the window's client area, which
includes the plug-in's child window. Setup's "Run Windows program…" starts an
installer or licence manager (`.exe`, or `.msi` through `msiexec`) in the shared
prefix; its windows appear through the streamer like editors.

The app sees that display only through `sp-window-streamer`, which the runtime starts on it with
a socket in `$XDG_RUNTIME_DIR/superposition-<pid>/`. The streamer tracks every top-level and
popup window, captures changed regions (Composite, DAMAGE, MIT-SHM), and sends them as
zstd-compressed 64×64 tiles (`sp-window-stream` protocol: one controller, any number of
viewers). The UI keeps one texture per window, draws each top-level window as an egui window and
each popup over its parent, and forwards pointer, wheel, keys, and text, which the streamer injects
with XTEST. Closing an editor's egui window closes the editor through its worker; closing any
other window (an installer, a licence manager) sends `WM_DELETE_WINDOW`. If Xvfb or the streamer
dies, the runtime restarts it and the UI reconnects.

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

## Remote operation

`superposition --serve` runs the engine and the normal UI against a headless `egui::Context`;
`superposition --connect HOST` opens a window that shows it (`sp-remote`). The client sends each
frame's `egui::RawInput`; the engine runs a UI frame for that input, tessellates it, and sends the
meshes and texture changes, which the client paints. The client therefore shows exactly the
engine's UI, and streamed plug-in editors arrive as textures in it. The transport is QUIC with
TLS 1.3 and self-signed identities. An unknown client pairs once with the six-digit code the
engine prints: SPAKE2 over the connection, confirmed by HMACs over both certificate
fingerprints, after which each side pins the other. One client controls; other clients watch
until they take control. A client that joins late first receives every live texture in full.
The engine renders only for input or a requested repaint, so an idle engine and client use
almost no CPU. On macOS the served UI works the same, but native editors stay on the Mac's own
display.

## Package direction and VST3 boundary

`sp-model` defines durable domain types. `sp-protocol` defines versioned wire records. `sp-shared-memory` owns safe fixed-layout and slot primitives, while `sp-shared-memory-os` owns the platform mapping, clock, wake, and scheduling FFI (macOS, Linux, and Windows under Wine). `sp-engine` owns graph/gate/mixing foundations. `sp-audio-io` contains platform-neutral endpoint types, callback contracts, the product renderer, and the rack dispatcher; `sp-audio-io-macos` owns the direct AUHAL implementation and `sp-audio-io-linux` the JACK implementation. `sp-supervisor`, `sp-session`, `sp-midi`, and `sp-ui` remain outside the callback dependency path.

Bundle discovery and scanning are platform-specific. macOS searches the
system and user `Library/Audio/Plug-Ins/VST3` folders and reads the bundle's
Mach-O header. Linux searches each absolute folder in `VST3_PATH`, then
`~/.vst3`, `/usr/lib/vst3`, and `/usr/local/lib/vst3`, and reads the ELF header
of the shared object in `Contents/<arch>-linux`. A bundle is supported when it
holds code for the running architecture; `vst3-host` loads the binary from the
architecture folder that matches the running target.

On Linux with Wine, the scanner also searches the prefix's
`drive_c/Program Files/Common Files/VST3` and each absolute folder in
`SUPERPOSITION_WINDOWS_VST3_PATH`, for bundle folders and single-file `.vst3`
DLLs. The Windows scanner runs each one under Wine with `Z:` paths, the private
display, and a 30-second deadline, and reads the PE header of the DLL in
`Contents/<arch>-win`. Timeouts and crashes count toward quarantine exactly as
for native scans. The catalog records each plug-in's platform (`native` or
`windows`; catalogs written before this field read as native).

`sp-vst3` is the sole VST3 adapter boundary. Its SDK feature is enabled by `sp-plugin-worker` and `sp-plugin-scanner` only. The main app and `sp-engine` must not transitively reach `sp-vst3`, `vst3`, or `vst3-host`; direct dependency edges remain limited to the helper/adapter chain. No app, engine, model, protocol, session, or UI type may expose VST3 SDK types. `cargo xtask doctor` verifies that dependency rule from Cargo metadata. The scanner and worker helper capabilities are partial foundations, not a general VST3 compatibility assertion.

See [ADR 0001](adr/0001-rack-topology.md), [ADR 0002](adr/0002-worker-per-rack-gate.md), [ADR 0003](adr/0003-fixed-shared-memory.md), [ADR 0004](adr/0004-vst3-adapter.md), [ADR 0007](adr/0007-no-general-pdc-or-splits.md), and [ADR 0010](adr/0010-direct-auhal-backend.md).
