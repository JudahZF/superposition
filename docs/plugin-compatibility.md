# Plug-in compatibility

## Initial adapter boundary

The initial plug-in format is VST3, behind `sp-vst3` and executed only in scanner/worker helpers. The adapter translates declared audio buses, bounded event/MIDI data, parameter changes, processing calls, and state transfer into host-owned types. The engine and session layer do not depend on VST3 SDK types or lifetime rules.

The disposable scanner validates bundle layout and Mach-O architecture before optional SDK enumeration. The parent computes a SHA-256 fingerprint over canonical path, entry metadata, and bundle file contents; unchanged results are reused from an atomically replaced JSON cache. Retained worker helpers rebuild and process fixed serial multi-slot racks, while editor lifecycle is isolated in worker-owned AppKit windows. Passing the alpha path does not certify all VST3 plug-ins.

## Compatibility tiers

- **Supported candidate:** discovered, scanned out of process, and passed the current automated smoke suite on the specified macOS/Apple Silicon configuration.
- **Best effort:** can be loaded experimentally but has not met the suite; the worker/gate remains mandatory.
- **Unsupported:** requires undeclared capabilities, crashes/hangs during scan, violates the negotiated layout, has an incompatible architecture/signature, or needs a feature outside the initial graph contract.

Discovery data and compatibility results are facts with timestamps and environment details, not permanent vendor promises. A new plug-in version is a new candidate.

## Native editor ownership

The worker owns the plug-in and any native editor window it creates. The host requests open, close, focus, geometry, and lifecycle changes over the control plane, but does not reparent or draw into the plug-in's native view hierarchy. This avoids crossing plug-in framework/lifetime boundaries in the host. If the worker exits, the host removes its proxy surface, keeps that rack dry during bounded recovery, and opens any later editor in the replacement worker.

## Non-goals

The first phase excludes generic AU/AAX adapters, universal sandbox compatibility, in-process fallback, arbitrary bus/sidechain layouts, and a promise to host plug-ins that block, assume a different thread model, or require prohibited entitlements. See [ADR 0004](adr/0004-vst3-adapter.md) and [ADR 0005](adr/0005-worker-owned-native-windows.md).
