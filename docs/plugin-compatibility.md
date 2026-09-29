# Plug-in compatibility

## Initial adapter boundary

The initial plug-in format is VST3, behind `sp-vst3` and executed only in scanner/worker helpers. The adapter translates declared audio buses, bounded event/MIDI data, parameter changes, processing calls, and state transfer into host-owned types. The engine and session layer do not depend on VST3 SDK types or lifetime rules.

The disposable scanner validates bundle layout and Mach-O architecture before optional SDK enumeration. The parent computes a SHA-256 fingerprint over canonical path, entry metadata, and bundle file contents; unchanged results are reused from an atomically replaced JSON cache. Retained worker helpers rebuild and process fixed serial multi-slot racks, while editor lifecycle is isolated in worker-owned AppKit windows. Passing the alpha path does not certify all VST3 plug-ins.

Scanner reports use a private temporary file, bounded to 4 MiB, so plug-in log
output cannot corrupt the report or hold its output pipe open. Catalog version 3
invalidates earlier derived scan results once; quarantine history is retained.
The default adapter uses `vst3-host` 0.9 for exact class selection and complete
state envelopes. Auxiliary audio buses stay inactive, and their presence does
not by itself make a mono/stereo main-bus effect incompatible. The one
exception is a sidechain: at most one stereo aux input bus. When a slot sets a
sidechain, the worker negotiates the first auxiliary audio input to stereo,
activates it, and feeds it the slot's sidechain audio. Every other auxiliary bus
stays inactive. A plug-in that declines a stereo arrangement for that bus fails
to load with a sidechain; a rebuild during playback then keeps the running
chain. The scanner
marks a class sidechain-capable when its first auxiliary input advertises two
channels; scan metadata version 3 adds this flag, so older cached scans are
rescanned. Mono-only aux inputs, such as the SDK's AGain SideChain sample, are
not used. In a short worker check with Britpressor's external sidechain on, a
loud sidechain lowered the output peak about 90-fold against a silent one. This
is a targeted check, not a compatibility certification.
The pinned local host patch and its provenance are documented in
[third-party/vst3-host/SUPERPOSITION.md](../third-party/vst3-host/SUPERPOSITION.md).

Pro-Q 4's previously blank analyzer is now visually confirmed working in the
user's 23 September 2026 20:47:55 recording. The repaired host bridge delivers
bounded immutable message copies on the main thread. Separate 30-second
BlackHole checks at 32 and 64 frames kept the editor open with no audio deadline
misses, fallback, or recovery. Each reported seven queue-full visualization
messages; all queued messages were accepted by the plug-in. These are targeted
compatibility checks, not long-run or all-device certification. See
[live validation](live-validation.md) for the evidence and limits.

## Compatibility tiers

The 23 September 2026 fresh scan of 33 local bundles returned 26 supported
candidates, five scanner crashes (TICK and four WaveShell versions), one SDK
initialization error (Tape MELLO-FI), and one Intel-only bundle (RoughRider3).
These are discovery results, not an audio-compatibility claim. The measured
ValhallaSupermassive live checks are recorded in [live validation](live-validation.md).

- **Supported candidate:** discovered, scanned out of process, and passed the current automated smoke suite on the specified macOS/Apple Silicon configuration.
- **Best effort:** can be loaded experimentally but has not met the suite; the worker/gate remains mandatory.
- **Unsupported:** requires undeclared capabilities, crashes/hangs during scan, violates the negotiated layout, has an incompatible architecture/signature, or needs a feature outside the initial graph contract.

Discovery data and compatibility results are facts with timestamps and environment details, not permanent vendor promises. A new plug-in version is a new candidate.

## Native editor ownership

The worker owns the plug-in and any native editor window it creates. The host requests open, close, focus, geometry, and lifecycle changes over the control plane, but does not reparent or draw into the plug-in's native view hierarchy. This avoids crossing plug-in framework/lifetime boundaries in the host. If the worker exits, the host removes its proxy surface, keeps that rack dry during bounded recovery, and opens any later editor in the replacement worker.

## Non-goals

The alpha excludes generic AU/AAX adapters, universal sandbox compatibility, in-process fallback, arbitrary bus layouts, mono or multiple sidechains, and a promise to host plug-ins that block, assume a different thread model, or require prohibited entitlements. See [ADR 0004](adr/0004-vst3-adapter.md) and [ADR 0005](adr/0005-worker-owned-native-windows.md).
