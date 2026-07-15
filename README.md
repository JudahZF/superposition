# Superposition

Superposition is a macOS-native live VST3 host alpha focused on bounded latency and isolating third-party plug-ins from the audio engine. The repository contains the **Phase 0** contracts, **Phase 1** synthetic and CoreAudio-attached feasibility harnesses, isolated VST3 scanning/processing, supervision, session packages, MIDI/scenes, and an egui live-rack application.

## Status

Available now:

- Versioned model/protocol contracts, fixed shared-memory banks, rack gates, and design tokens
- `cargo xtask doctor`, `ipc-feasibility`, `ipc-matrix`, `fault-matrix`, and `device-feasibility`
- Notification-based CoreAudio output harness (`sp-audio-io-macos`)
- Disposable SDK-backed VST3 scanner plus SHA-256 content fingerprints and an atomic scan cache
- Process supervisor, atomic session packages with recovery markers
- Allocation-free, lock-free CoreMIDI callback ingress with a dedicated note-off safety lane
- Interactive live-rack shell, real CoreAudio start/stop, and a component gallery (`⌘G`)
- Verification commands: `host-checker`, `compatibility`, `loopback`, `click-test`, `midi-timing`, `soak [--smoke]`, `bundle` (entitlement templates under `packaging/entitlements/`)

Still not a shippable public release: multi-plug-in worker control IPC, worker-owned native editors, notarized bundles, approved brand assets, and long-run hardware certification remain incomplete. Worker isolation is an availability boundary, not a security sandbox.

## Hard Phase 1 feasibility gate

Before product UI investment, an Apple Silicon machine must still prove the hard gate with an active device callback:

```sh
cargo xtask device-feasibility --racks 8 --frames 128 --duration-seconds 1800
cargo xtask ipc-matrix --duration-seconds 1800 --output-dir target/phase1/ipc-matrix
cargo xtask fault-matrix --racks 8 --frames 128 --output-dir target/phase1/fault-matrix
```

Synthetic preflight remains useful for development:

```sh
cargo xtask ipc-feasibility --racks 8 --frames 128 --duration-seconds 30
```

Reports label `phase1_hard_gate_certified=false` until official long-run evidence is collected on dedicated hardware.

## Documentation

- [Architecture](docs/architecture.md)
- [Real-time safety contract](docs/realtime-safety.md)
- [Session format](docs/session-format.md)
- [Plug-in compatibility](docs/plugin-compatibility.md)
- [Testing strategy](docs/testing.md)
- [Brand assets and release provenance](docs/brand-assets.md)
- [macOS distribution](docs/macos-distribution.md)
- [Architecture decision records](docs/adr/)

## Workspace

Run `cargo xtask doctor` for platform, toolchain, scaffold, and dependency-boundary diagnostics. `cargo xtask help` lists every verification command.

Run `SUPERPOSITION_COMPONENT_GALLERY=1 cargo run -p superposition` to open the deterministic gallery used for visual/accessibility review.

## CI

`.github/workflows/ci.yml` runs doctor, formatting, Clippy, tests, `cargo-deny`, and `cargo-audit`, plus short synthetic Phase 1 smokes. Long CoreAudio qualification still requires dedicated Apple Silicon hardware.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT), at your option.
