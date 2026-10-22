# Live audio validation

This page records what live runs have shown so far and how to repeat them. All
runs used the headless host (or the product runtime directly) on an Apple Silicon
Mac with **BlackHole 64ch** at 48 kHz. No physical interface has been tested. The
runs are short: they show the path works, not that it is reliable for a whole
show. Use optimized (`--release`) builds for timing; debug timing does not count.

## Measured results

| Check | Frames | Duration | Result |
| --- | ---: | ---: | --- |
| One rack, one effect, external stimulus | 32, 64, 128, 256 | 10–30 s | No deadline misses or fallback |
| Eight racks, one effect each, wet signal | 128, 256 | 30 s | No deadline misses or fallback |
| Eight racks, per-rack input and output routes | 32, 64 | 10 s | Each route carried only its own rack's share |
| Native editor open, closed, and reopened during audio | 32, 64 | 30 s | No deadline misses; audio kept running |
| Scene recall during audio | 32, 64 | 12 s | Recalled levels confirmed in the captured audio |
| Save and scene capture during audio | 128 | 9 s | Rack kept completing blocks; reopened package held state |
| Four racks, two-plug-in chains, separate output pairs | 128 | 90 s wet, 600 s dry | No deadline misses or fallback |
| Add, remove, and rebuild racks during audio | 128 | 4 s | The untouched rack completed every block |
| Kill one rack's worker | 128 | 15 s | That rack fell back and recovered; the other rack was unaffected |

Plug-ins used across these runs include FabFilter Pro-Q 4, Archetype Nolly X,
ValhallaSupermassive, and UADx 1176. Capture comparison through the BlackHole loop
matched the source at one buffer of delay (256 samples at 64 frames, 128 at 32).
That is the virtual loop's delay, not a physical round-trip latency.

## Linux software rig (22 October 2026)

No Linux audio hardware has been tested yet, so these are software results only. Setup:

- x86_64 NixOS, 12 cores, no real-time priority (`RLIMIT_RTPRIO` 0, no rtkit).
- A private `jackd --no-realtime` dummy server at 48 kHz.
- One rack with Dragonfly Plate Reverb (Linux VST3).
- Release build, `superposition --headless … --duration-seconds 30`.

| Block | Machine load | Blocks | Deadline misses |
| ---: | --- | ---: | ---: |
| 256 | idle (0.4) | 5 625 | 0 |
| 128 | idle (0.3) | 11 250 | 0 |
| 64 | light (1.3) | 22 474 | 6 |
| 32 | busy (6.0) | 37 622 | 211 |
| 256 | busy (≈5, parallel builds) | 5 619 | 2 |
| 256 | overloaded (≈25) | — | repeated misses; the rack gave up after 3 recoveries |

Every miss was a worker that had been woken but not scheduled before the deadline (`unclaimed`),
or a wake syscall delayed by several milliseconds. Neither is a protocol fault. A single hard miss
replaces the worker, so on an overloaded system without real-time priority, misses cascade. Below
128 frames, Linux needs real-time scheduling (see the README).

The same rig ran the desktop UI through `--serve`/`--connect` on Xvfb:
- device selection, the plug-in scan, adding a rack and plug-in, and starting audio all worked;
- X programs on the private display appeared as windows in the app and took clicks.

`cargo xtask wine-check` measured futex round trips through `sp_wine_bridge` with a Windows
client under Wine 11.18: p50 5 µs; p99 1.1 ms while the machine was busy compiling.

## Known limits

- Eight racks at 128 frames failed some earlier runs under load. The failed blocks
  were never claimed by their workers, so the cause was worker scheduling, not
  plug-in processing time. Later builds passed, but only in short runs.
- Separate input and output devices (the private aggregate) have unit coverage,
  but no live run.
- The bounded plug-in message queue can drop some visualization messages under
  pressure. Audio is not affected.
- No long-run soak with wet signal, physical MIDI timing, or physical round-trip
  latency measurement has been done.

## Repeating the checks

`tools/audio-loopback-probe.c` is a standalone AUHAL probe. It plays a stimulus
into a device and captures the return, independently of the host. Its header
comment has the build command.

The headless host reports callbacks, rack completions, deadline misses, fallback,
and output levels (see the README). The opt-in live tests are `#[ignore]`d and need
explicit environment variables, so they never run in the normal suite:

| Test | Enable with | Also needs |
| --- | --- | --- |
| `live_save_check` | `SUPERPOSITION_LIVE_SAVE_CHECK=1` | `SUPERPOSITION_LIVE_SAVE_SESSION`, `SUPERPOSITION_HELPERS_DIR` |
| `live_scene_check` | `SUPERPOSITION_LIVE_SCENE_CHECK=1` | `SUPERPOSITION_LIVE_SCENE_SESSION`, `SUPERPOSITION_LIVE_SCENE_APP_SUPPORT`, `SUPERPOSITION_LIVE_SCENE_FRAMES`, `SUPERPOSITION_HELPERS_DIR` |
| `live_editor_check` | `SUPERPOSITION_LIVE_EDITOR_CHECK=1` | `SUPERPOSITION_LIVE_EDITOR_SESSION`, `SUPERPOSITION_LIVE_EDITOR_APP_SUPPORT`, `SUPERPOSITION_LIVE_EDITOR_DEVICE`, `SUPERPOSITION_HELPERS_DIR` |
| `live_topology_check` | `SUPERPOSITION_LIVE_TOPOLOGY_CHECK=1` | `SUPERPOSITION_LIVE_TOPOLOGY_SESSION`, `SUPERPOSITION_HELPERS_DIR` |

For example:

```sh
SUPERPOSITION_LIVE_SAVE_CHECK=1 \
SUPERPOSITION_LIVE_SAVE_SESSION="$HOME/Library/Application Support/Superposition/Default.superposition" \
SUPERPOSITION_HELPERS_DIR="$PWD/target/release" \
cargo test --release -p superposition live_save_check -- --ignored --nocapture
```

The checks copy the session to `/tmp` and never modify the source.
