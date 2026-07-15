# Real-time safety contract

The device callback is a deadline-bound boundary, not a general execution context. Its only permitted work is bounded arithmetic, reads/writes to preallocated callback-owned or lock-free data, fixed-layout shared-memory slot exchange, and dispatch to explicitly reviewed DSP that obeys the same rules.

## Forbidden in a callback

The callback must not allocate or free; acquire a mutex, read/write lock, semaphore, or condition variable; wait, sleep, park, join, or block on IPC; perform filesystem, network, process, dylib, or plugin discovery work; log, format strings, capture stacks, or emit diagnostics synchronously; take a garbage-collected/runtime lock; call Objective-C, AppKit, Core Foundation APIs with unknown allocation/locking behavior; or invoke a plug-in directly in the host process. It must not make a request whose completion is required before returning.

## Atomic and ownership invariants

- Every shared slot has one producer and one consumer for a sequence generation.
- A producer publishes payload before the release-store that marks it ready; a consumer acquire-loads readiness before reading payload.
- Slot sequence numbers are monotonic modulo the documented integer range and prevent ABA/reuse ambiguity.
- Capacity, channel layout, maximum frames, and event capacity are negotiated before activation and never resize on the callback path.
- Control-plane writes cannot mutate a buffer or descriptor owned by the active callback generation.
- The audio callback never spins indefinitely; all polling is bounded by an explicitly reviewed constant.

The implementation will document each atomic order and justify any weaker-than-acquire/release operation in code review.

## Overflow, deadline, and fallback

Queues are bounded. On overflow, the producer records a counter and coalesces or drops redundant continuous controls before note-offs and safety-critical events; it never allocates a larger queue or waits. Audio-slot unavailability, an invalid sequence, a worker exit, or a missed worker deadline closes that rack's gate for the block. Effects with compatible topology crossfade to a continuously maintained latency-matched dry path; instruments and incompatible topologies ramp to silence. The callback returns on time, and recovery happens off the callback path.

CoreMIDI ingress follows the same rule: its callback copies MIDI 1.0 messages into fixed three-byte packets and pushes them to bounded `rtrb` SPSC queues. Note-offs use a separate safety lane drained before ordinary messages. Conversion into the public `Vec`-backed event model happens only on the non-callback consumer side; SysEx and MIDI 2.0 are rejected in the alpha.

The deadline is derived from the active block duration and must reserve host scheduling margin. Workers must treat a stale or late block as discardable. A late result may not be retroactively inserted into the stream.

## Diagnostics and unsafe assumptions

The callback hands counters, compact reason codes, rack IDs, and sequence numbers to a bounded diagnostic queue. A non-real-time consumer enriches, persists, displays, and rate-limits reports. Diagnostics may be lost under pressure; audio progress must not be.

This design does not make third-party plug-ins real-time safe. It assumes the platform callback is truly time-constrained, shared-memory atomics are supported between cooperating processes, clocks/scheduling can still miss deadlines, and macOS process isolation limits—not eliminates—damage from native code. The Phase 1 feasibility gate must measure these assumptions on Apple Silicon hardware.
