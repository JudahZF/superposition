# Superposition

Superposition is a macOS-native live VST3 host. It runs each rack's plug-ins in a separate worker process, so a plug-in that crashes or misses its deadline affects only its own rack. The audio engine keeps bounded latency and keeps the other racks playing.

**Status: alpha.** It runs real plug-ins live on Apple Silicon, but it is not a certified release. Notarized bundles, approved brand assets, and long-run hardware testing are still incomplete. Worker isolation protects availability; it is not a security sandbox.

## Features

- Direct AUHAL/CoreAudio audio with separate input and output devices, 1–64 physical channels, and 32/64/128/256-frame buffers at 48 kHz
- One worker process per rack, with up to eight serial plug-ins, rack-local fallback, and automatic worker recovery
- Per-rack mono/stereo input and stereo output routes, and per-plug-in sidechains from a physical input pair or another rack
- Worker-owned native plug-in editors; all parameter editing happens there
- Parameter scenes with ramps, CoreMIDI input, MIDI Learn, and Program Change scene recall
- Atomic session packages, autosave, crash recovery, missing-plug-in placeholders, and plug-in quarantine
- A disposable, isolated VST3 scanner with content fingerprints
- A headless mode that uses the same engine and workers as the desktop app

## Build and run

Requires an Apple Silicon Mac, macOS 14.4 or later, and the Rust toolchain in `rust-toolchain.toml` (rustup installs it automatically).

```sh
cargo build --workspace
cargo run -p superposition
```

The app finds its helper executables beside its own executable. Packaged builds use the bundle's `Helpers` directory. `SUPERPOSITION_HELPERS_DIR` overrides both, and `SUPERPOSITION_APP_SUPPORT` selects a separate catalog and quarantine directory.

## Using the app

The show screen has one column per rack, left to right ([UI design](docs/ui-design.md)). Each column shows the rack's route, a picture of each plug-in's editor, gain, meters, and a state token. Click a picture to open that plug-in's native editor, and right-click it for the slot menu. **Setup** (⌘,) holds audio devices, MIDI, the plug-in catalog, and diagnostics. Use **Rescan plug-ins** there to fill the plug-in browser.

| Key | Action |
| --- | --- |
| ⌘S | Save |
| ⌘N | Add a rack |
| ⌘← / ⌘→ | Move the selected rack |
| ⌘⇧C | Capture a scene |
| 1–8 | Recall scenes 1–8 |
| ⌘0–⌘9 | Show all racks, or a page |
| Esc | Close the topmost overlay |

Audio keeps running while you:

- add, remove, or reorder racks and plug-ins (untouched racks keep playing, and a rack with a changed plug-in keeps its other plug-ins playing);
- open or close native editors;
- save (each worker serializes plug-in state on its loading thread);
- capture or recall scenes.

A new plug-in fades in over 16 samples after it produces three good blocks. Bypass also fades over 16 samples, and it keeps the plug-in's latency, so timing does not shift.

Scenes capture up to 256 selected plug-in parameters, plus rack gain, mute, rack bypass, and slot bypass. Continuous parameters ramp, and switches change at the end of the transition. MIDI Program Change selects the matching scene.

If the audio device disconnects, audio stops. The app checks once per second for the saved devices and restarts audio when they return.

## Headless mode

```sh
target/debug/superposition --list-devices
target/debug/superposition --list-midi-ports
target/debug/superposition --scan
target/debug/superposition --headless --session /path/to/Live.superposition \
  --device "BlackHole 64ch" --frames 128 --duration-seconds 10
target/debug/superposition --headless --session /path/to/Live.superposition \
  --input-device "Built-in Microphone" --output-device "BlackHole 64ch" \
  --frames 128 --duration-seconds 10
```

Headless mode needs an explicit device and a saved session with at least one loaded rack. Use names or IDs from `--list-devices`. `--input-device none` selects output-only processing, and `--midi-port <id-or-name>` opens one MIDI input. At the end it reports callbacks, rack completions, deadline misses, fallback, closed gates, and output levels. It fails on callback errors, deadline misses, protocol faults, or an unhealthy worker. Use `--release` builds for timing checks. See [live validation](docs/live-validation.md) for measured results.

## Development

```sh
cargo xtask doctor   # platform, toolchain, workspace, and dependency-boundary checks
cargo xtask help     # lists the hardware verification commands
```

The hardware verification commands (`host-checker`, `compatibility`, `loopback`, `click-test`, `midi-timing`, `soak`, `bundle`) check captured evidence from real devices. See [testing](docs/testing.md).

To render the show screen to PNGs for visual review:

```sh
SUPERPOSITION_UI_SNAPSHOT=<dir> cargo test -p superposition --bin superposition render_show_screen_snapshots -- --ignored
```

CI (`.github/workflows/ci.yml`) runs doctor, formatting, Clippy, tests, coverage, `cargo-deny`, and `cargo-audit` on macOS. Live timing needs real Apple Silicon hardware and does not run in CI.

## Documentation

- [Architecture](docs/architecture.md)
- [UI design](docs/ui-design.md)
- [Real-time safety contract](docs/realtime-safety.md)
- [Session format](docs/session-format.md)
- [Plug-in compatibility](docs/plugin-compatibility.md)
- [Testing](docs/testing.md)
- [Live validation](docs/live-validation.md)
- [Brand assets and release provenance](docs/brand-assets.md)
- [macOS distribution](docs/macos-distribution.md)
- [Architecture decision records](docs/adr/)

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT), at your option. `third-party/vst3-host` is a patched copy of the MIT-licensed `vst3-host` crate; see its `SUPERPOSITION.md`.
