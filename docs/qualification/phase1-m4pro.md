# Phase 1 M4 Pro feasibility qualification record

- **Reference host:** Apple M4 Pro (`Mac16,8`), macOS 27.0, Rust 1.95, Xcode 27.0, CMake 4.1.2
- **Audio route:** BlackHole 64ch at 48 kHz, channels 1–2 as default stereo output
- **VST3 SDK:** `/Users/judahfuller/SDKs/vst3sdk`
- **Baseline revision:** `6c3a616e631641aa971b4a36560c441f20d40ccd`
- **Gate status:** Pending — no 30-minute attached-device matrix has been completed in this workflow
- **Certification:** `false`

This record is a controlled qualification procedure and status document. Raw reports, AUHAL captures, SDK build output, energy logs, worker logs, and manifests remain under `target/phase1/` and are not source-controlled. Only `cargo xtask phase1-report` may change a final aggregate result to `certified=true`.

## Required evidence

| Evidence | Required state | Raw artifact location |
| --- | --- | --- |
| Synthetic matrix | 1/2/4/8 workers × 128/256 frames; 1,800 s per cell | `target/phase1/ipc-matrix/` |
| Attached-device matrix | Active AUHAL callback proof for the same eight cells; 1,800 s per cell | `target/phase1/device-matrix/` |
| Calibrated load | Active-device 8-worker bounded `calibrated-cpu` spin at 128 **and** 256 frames | `target/phase1/calibrated-load/` |
| Fault isolation | Target-fault fallback by the current or next callback with every unaffected rack progressing, for self-crash and hang at 128 **and** 256 frames | `target/phase1/fault-isolation/` |
| Real VST3 | Scanner accepts SDK Again before isolated worker processes finite unity-gain stereo output | `target/phase1/vst3-smoke/` |
| Consolidation | Manifests/report IDs agree and every mandatory field validates | `target/phase1/phase1-report.{json,md,manifest.json}` |

Each attached-device report must state active-callback proof, exact device format, requested and observed duration, p99.9, p99.99 status/value, and maximum timing values for request→claim, processing, completion observation, observe/dispatch, and full callback duration; deadline/fallback/overrun/protocol/worker-exit counters; CPU snapshots; safe control-thread heartbeat snapshots for every mapped worker; and internal host/worker energy snapshots and deltas with PIDs, raw nanojoules, joules, availability/errors, and duration. A histogram with fewer than 10,000 observations records its p99.99 as unavailable with `statistically_underpowered` status while preserving its actual maximum. Worker IDs/PIDs/generations/bank identities and the per-artifact gate result are retained. Fault reports also include the trigger sequence, first target fallback block, and a `fallback_by_current_or_next_block` proof. The JSON and Markdown SHA-256 digests are committed in the manifest published last. An unavailable CPU, heartbeat, or energy field is a certification blocker, not a zero-valued result.

The harness captures the internal energy contract before and after every attached window using `proc_pid_rusage` for the host and every worker PID. On the reference Xcode 27 SDK, `ri_energy_nj` is defined by the current `RUSAGE_INFO_V6` layout; V4 has billed/serviced energy but no nanjoule field, so the report records the actual V6 flavor rather than reading an uninitialized extension. Internal measured energy is mandatory for certification, including the 1,800-second matrix/calibrated windows and 30-second fault windows. A structured external energy JSON may be passed as an optional cross-check but is never mandatory manual evidence.

## Hard-gate policy

For the required eight-worker 30-minute no-op runs:

- No callback overrun, deadline miss, or protocol corruption.
- Dispatch/completion overhead p99.99 **≤ 150 µs** and observed maximum **< 400 µs**.
- Under calibrated load at **each claimed frame size**, callback p99.9 **< 70%** of the device period, callback p99.99 **< 80%**, and maximum **< one period**.
- The p99.99 limits apply only when their histogram has at least 10,000 observations. Every 1,800-second matrix and calibrated-load report must meet that sample minimum and serialize an `available` p99.99; `phase1-report` rejects an official artifact that does not. Short preflights remain non-certifying: they report `statistically_underpowered` instead of collapsing p99.99 to the maximum, while still enforcing the separate maximum and fault-counter rules.
- Kill or hang of one worker at **each claimed frame size** triggers rack-local fallback by the current or next block; unaffected racks continue.

A 128-frame failure removes the 128-frame claim. A 256-frame failure stops progression and requires a data-plane/scheduling redesign. Synthetic timing and successful short device attachment are preflight evidence only.

## Execution order

1. Restore the intended BlackHole route, then set BlackHole 64ch channels 1–2 as the fixed default output. Confirm no other process owns the route.
2. Run the short synthetic preflights, then the official synthetic matrix.
3. Run the attached-device matrix sequentially. Each cell captures host and live-worker energy automatically before and after the run; retain any optional external cross-check separately.
4. Run calibrated CPU load and each attached fault-isolation mode individually. Preserve reports even when an intentional fault occurs; a successful fault test reports the contained fallback as evidence rather than treating it as an unexplained passing no-op.
5. Run SDK Again only through `vst3-smoke`; it invokes the disposable scanner before the worker and does not permit the main app or engine to load the bundle.
6. Run `phase1-report`. Review the raw manifests and report IDs before recording the gate result here.
7. Restore the prior audio route.

## Authoritative commands

```bash
# Development preflights only; keep each under 10 seconds in this workflow.
cargo xtask ipc-feasibility --racks 2 --frames 128 --duration-seconds 2 \
  --output-dir target/phase1/preflight/ipc-2r-128f
cargo xtask fault-matrix --racks 2 --frames 128 \
  --output-dir target/phase1/preflight/fault-2r-128f

# Long gates are launched and monitored by the main session, not this workflow.
cargo xtask ipc-matrix --duration-seconds 1800 \
  --output-dir target/phase1/ipc-matrix
cargo xtask device-matrix --duration-seconds 1800 \
  --output-dir target/phase1/device-matrix
cargo xtask device-feasibility --racks 8 --frames 128 --duration-seconds 1800 \
  --compute-load-mode calibrated-cpu --compute-load-micros 267 \
  --output-dir target/phase1/calibrated-load/8r-128f
cargo xtask device-feasibility --racks 8 --frames 256 --duration-seconds 1800 \
  --compute-load-mode calibrated-cpu --compute-load-micros 534 \
  --output-dir target/phase1/calibrated-load/8r-256f
cargo xtask device-feasibility --racks 2 --frames 128 --duration-seconds 30 \
  --fault-mode self-crash --fault-target-rack 1 --fault-trigger-sequence 2 \
  --output-dir target/phase1/fault-isolation/self-crash-2r-128f
cargo xtask device-feasibility --racks 2 --frames 128 --duration-seconds 30 \
  --fault-mode hang-after-claim --fault-target-rack 1 --fault-trigger-sequence 2 \
  --output-dir target/phase1/fault-isolation/hang-2r-128f
cargo xtask device-feasibility --racks 2 --frames 256 --duration-seconds 30 \
  --fault-mode self-crash --fault-target-rack 1 --fault-trigger-sequence 2 \
  --output-dir target/phase1/fault-isolation/self-crash-2r-256f
cargo xtask device-feasibility --racks 2 --frames 256 --duration-seconds 30 \
  --fault-mode hang-after-claim --fault-target-rack 1 --fault-trigger-sequence 2 \
  --output-dir target/phase1/fault-isolation/hang-2r-256f
VST3_SDK_DIR=/Users/judahfuller/SDKs/vst3sdk \
  cargo xtask vst3-smoke --sdk /Users/judahfuller/SDKs/vst3sdk
cargo xtask phase1-report --artifact-dir target/phase1
```

The calibrated targets are deterministic rather than a preapproved threshold bypass: 267 us at 128 frames and 534 us at 256 frames (10% of the fixed 48 kHz period, rounded up). Reports retain requested and observed busy duration per worker and reject the result if measured hard-gate limits fail.

## Current evidence result

> **Historical evidence only:** the callback now publishes all racks before bounded same-callback observation and records request→completion publication, publication→host observation, and request→host observation separately. The retained short artifacts below predate that corrected behavior and schema. They remain useful historical diagnostics but must be regenerated before final qualification.

Short final-readiness checks are non-certifying. The 2-second synthetic 1/2/4/8-rack × 128/256-frame matrix and both complete short synthetic fault matrices completed with behavioral containment. Their raw histograms retain wake outliers separately from attached timing evidence: 8-rack synthetic request→completion maxima were 802 µs at 128 frames and 479 µs at 256 frames, without being relabeled as callback timings.

With BlackHole 64ch restored as the 48 kHz default output, the AUHAL client stream explicitly mapped to physical channels 1–2. Four distinct 5-second, two-rack attached runs completed at each frame size. Each run attached the callback, showed advancing heartbeats, collected positive internal host/worker process-energy deltas, and recorded zero deadline misses, callback overruns, and protocol faults. The four 5-second attached self-crash/hang tests passed at both frame sizes: fallback occurred by the current or next block and the unaffected rack remained healthy. SDK Again also passed through the scanner-then-worker-only smoke path.

The retained `128f-repetition-3` data observed a 196 µs completion-observation maximum. Its five-second/two-rack histogram has fewer than 10,000 samples, so corrected report semantics classify p99.99 as statistically underpowered rather than treating the maximum as an empirical p99.99 and applying the 150 µs limit. The observed 196 µs maximum remains retained and satisfies the unchanged short-preflight maximum rule of **<400 µs**; it does not supply evidence for the 150 µs official percentile threshold. The complete 1,800-second synthetic/device matrices and calibrated-load evidence remain intentionally unrun. The aggregate therefore remains `status: unavailable`, `certified: false`.
