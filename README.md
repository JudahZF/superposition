# Superposition

Superposition is a live VST3 host for macOS and Linux. On Linux it can also run Windows VST3 plug-ins under Wine. It runs each rack's plug-ins in a separate worker process, so a plug-in that crashes or misses its deadline affects only its own rack. The audio engine keeps bounded latency and keeps the other racks playing.

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

macOS requires an Apple Silicon Mac, macOS 14.4 or later, and the Rust toolchain in `rust-toolchain.toml` (rustup installs it automatically).

```sh
cargo build --workspace
cargo run -p superposition
```

Linux (x86_64) builds inside the Nix dev shell, which pins Rust, JACK, Xvfb, mingw, and the Wine build that Windows plug-ins use:

```sh
nix develop -c cargo build --workspace
nix develop -c cargo xtask windows-helpers   # Windows worker, scanner, and Wine bridge
nix develop -c cargo run -p superposition
nix develop -c cargo xtask wine-check        # Windows worker boundary under Wine
```

Windows VST3 plug-ins run in one shared Wine prefix (`<app support>/wine`, or `SUPERPOSITION_WINE_PREFIX`). Install them with **Setup → Plug-ins → Run Windows program…** (an `.exe` or `.msi`; its windows appear in the editor area), then **Rescan plug-ins**. Bundles in the prefix's `Program Files/Common Files/VST3` folder, and in any folder listed in `SUPERPOSITION_WINDOWS_VST3_PATH`, are scanned under Wine. A rack can mix Linux and Windows plug-ins. A rack that holds any Windows plug-in is heard one buffer late.

Audio runs through JACK (PipeWire's JACK library works). The server's rate and quantum must match the session, for example `pw-metadata -n settings 0 clock.force-quantum 128`. For real-time scheduling, grant `RLIMIT_RTPRIO` (the NixOS module below does this). Without it, native threads ask rtkit, if it runs; Windows workers under Wine need `RLIMIT_RTPRIO`.

On NixOS, the flake's module applies the system settings: `rtprio` 95 for the `audio` group, rtkit, the `ntsync` driver, the pinned Wine build (`SUPERPOSITION_WINE`), and Xvfb. Add your user to the `audio` group and log in again.

```nix
{
  imports = [ superposition.nixosModules.default ];
  programs.superposition.enable = true;
  users.users.<name>.extraGroups = [ "audio" ];
}
```

The app finds its helper executables beside its own executable, the Windows ones in `windows/` and the Wine bridge in `wine-bridge/` there. Packaged builds use the bundle's `Helpers` directory. `SUPERPOSITION_HELPERS_DIR` overrides both, and `SUPERPOSITION_APP_SUPPORT` selects a separate catalog and quarantine directory.

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

## Remote operation

```sh
superposition --serve [--bind ADDR] [--name NAME] [--pair]   # engine without a window
superposition --connect HOST[:PORT] [--pair CODE] [--viewer] # show and drive it
```

The engine prints a six-digit pairing code on its first start (or with `--pair`); a client uses it
once, then both sides remember each other. The client shows the engine's whole UI, including
plug-in editors on Linux engines. One client controls at a time; others watch and may take
control.

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

On Linux, the dev shell can type-check the macOS code. zig supplies the C headers, and the Objective-C shims are skipped, so nothing links:

```sh
nix develop -c env SUPERPOSITION_SKIP_NATIVE_SHIMS=1 cargo clippy --workspace --all-targets --target aarch64-apple-darwin
```

CI (`.github/workflows/ci.yml`) runs doctor, formatting, Clippy, tests, coverage, `cargo-deny`, and `cargo-audit` on macOS. On Linux, it runs doctor, formatting, the NixOS module check, Clippy for Linux and macOS, the tests, the JACK test, and the Wine check. Live timing needs real audio hardware and does not run in CI.

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
