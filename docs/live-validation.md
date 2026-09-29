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

The checks copy the session to `/tmp` and never modify the source. Worker sockets
need short paths, so keep test directories under `/tmp`, not `$TMPDIR`.
