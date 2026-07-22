# Real-time safety contract

The device callback is a deadline-bound boundary, not a general execution context. Its permitted work is bounded arithmetic; reads and writes to preallocated callback-owned or lock-free data; fixed-layout shared-memory slot exchange; bounded clock reads; and DSP that has been reviewed to obey the same limits. This document records the concrete invariants of the current AUHAL and shared-memory foundations. It does not certify third-party plug-ins or hardware timing.

## Forbidden in a callback

The callback must not allocate or free; acquire a mutex, read/write lock, semaphore, or condition variable; wait, sleep, park, join, or block on IPC; perform filesystem, network, process, dylib, or plug-in discovery work; log, format strings, capture stacks, or emit diagnostics synchronously; take a garbage-collected/runtime lock; call Objective-C, AppKit, or unreviewed Core Foundation APIs; or invoke a plug-in in the host process.

It must not make a request whose completion is required before returning. Device configuration, property listeners, start/stop, helper launch, shared-memory creation, clock construction, telemetry snapshots, persistence, and recovery all run off the callback thread. A panic may not unwind through the C callback boundary: `PhaseOneRenderer` implementations must not panic, and a panic in the Rust trampoline aborts rather than running recovery machinery on the real-time thread.

## Direct AUHAL callback invariants

The current macOS backend is a direct duplex AUHAL implementation, not `cpal`. Its setup and teardown run on a control thread. It rejects an unsupported device before callback activation and configures one same-device route with fixed 48 kHz, interleaved stereo `f32`, 128- or 256-frame input and output. The callback captures input with `AudioUnitRender` before dispatching it to the Rust renderer; separate devices require an aggregate device at this boundary.

The C render callback follows these rules before it calls Rust:

- It increments `active_callbacks`, zeros the supplied CoreAudio buffers, and uses a lock-free `writer_active` compare/exchange guard. A concurrent or reentrant entry is rejected without waiting and without calling Rust.
- It accepts exactly the configured frame count, exactly one non-null `AudioBuffer`, two channels, and exactly `frames * 2 * sizeof(float)` bytes. Any mismatch remains silent and is classified in atomic telemetry.
- Only after those checks does it pass the `float *` buffer and frame count to the Rust trampoline. `Rendered` means the renderer wrote the complete validated buffer; every other disposition is re-zeroed by C.
- The C callback is the sole telemetry sequence writer. Control-thread snapshots use the odd/even sequence protocol to obtain a coherent counter set. Snapshot retry is a control-plane operation and must never be moved into the callback.

The callback itself contains no C heap allocation, device-property calls, property polling, lifecycle work, or blocking sleep. Those operations exist only in AUHAL setup/teardown. The direct backend and its deviation from the original `cpal` proposal are documented in [ADR 0010](adr/0010-direct-auhal-backend.md).

Product recovery uses one atomic phase word plus bank index/generation fields per rack. The control plane requests quiescence; at a block boundary the callback abandons any live request, closes only that rack gate, and acknowledges that it will touch neither bank. The control plane may then stop and reap the old worker, reset the inactive mapping, and launch a replacement. After replacement readiness is release-published, the callback validates the prepared mapping and changes its fixed bank selector at a block boundary. Only after that acknowledgement may the control plane reset the retired mapping. The callback validates and acknowledges the reset before returning the handshake to idle. No mapping address, allocation, process operation, or IPC endpoint changes on the real-time thread.

The AUHAL-attached device-feasibility renderer follows the same restriction. Its callback-owned harness contains only mapped banks, gate state, fixed request metadata, preallocated timing histograms, and atomic liveness flags. The command-driving control plane owns every `Child`, polls `try_wait`, builds any error strings, and publishes a worker exit through an atomic flag before it stops and reaps workers after callback retirement. The renderer converts only that flag, shared-memory completion state, and gate state into fixed-size counters, per-rack accepted-completion counters, first-fallback callback-block counters, and fallback outcomes; it neither polls a process nor retains a process handle. For an injected sequence-two fault, the recorded target fallback block must be the trigger block or its successor; report construction checks this only after retirement. Those atomics are copied into report objects only after `ActiveOutput::stop` proves retirement; report serialization, error strings, CPU/energy sampling, worker/bank identity collection, SHA-256 manifest generation, and publication are control-plane operations.

## C/Rust pointer and renderer ownership

`ActiveOutput<R>` owns both the opaque C output wrapper and `Box<R>`, where `R: PhaseOneRenderer + Send + 'static`. Startup takes the stable boxed renderer address, passes it to C as `void *`, and stores the opaque native pointer as `NonNull`. The callback may form `&mut R` only because C has already validated the output buffer, the writer guard gives it exclusive callback-time execution, and C retains the callback pointer until retirement.

The Rust trampoline checks the frame enum and both raw pointers before constructing a mutable slice. C has already proved that the pointer covers the exact interleaved stereo byte region, and Rust constructs no larger slice. The raw `SpAudioOutput` pointer is never dereferenced as a Rust value; it remains opaque. The C ABI records explicit `renderer_retired` and `native_releasable` flags so Rust never infers callback retirement from a successful-looking status alone.

`Send` here transfers the renderer into the callback-owning output; it does not make the renderer `Sync`, permit two callbacks to borrow it, or permit user code to retain the C pointer. A renderer must remain valid until native retirement is confirmed. Any new callback context, observer, or raw pointer must be part of that same ownership cluster and obey the same retirement rule.

## Atomic, slot, and shared-memory ownership

- Every shared slot has one producer and one consumer for a ticket generation. A ticket has a nonzero generation and sequence; stale or mismatched completions are rejected.
- A producer writes input audio/event payload before the release-store that publishes `Requested`. A worker acquire-loads the state before consuming the payload, claims it before writing output, then publishes completion with the protocol ordering. The host acquire-loads completion before it reads output.
- Capacity, channel layout, maximum frames, event capacity, and the four-slot bank layout are fixed before activation. The callback never resizes a queue, a bank, a mixer buffer, or a device format.
- The dispatcher bounds its completion observation with a `MonotonicClock` deadline. It closes the affected rack gate for malformed metadata, non-finite output, stale ticket, unavailable slot, or deadline expiry; it never waits for a worker beyond that deadline.
- A timed-out processing slot is not casually reused. Control-plane recovery and worker death handle retirement; callback code accepts only the exact live ticket.
- Atomic telemetry uses relaxed counter increments where a coherent cross-counter relationship is not required. Publication and ownership transitions use the acquire/release protocol defined by `sp-protocol`; C's writer/telemetry sequence uses sequential consistency. Any weaker order added later requires a code-review justification.
- Attached-device fault evidence uses a fixed `MAX_RACKS` array of atomic accepted-completion counters. The callback indexes only the existing preallocated rack vector and the bounded array; a control-plane snapshot may turn those counters into per-rack isolation evidence only after retirement. It must not add a callback-side `Vec`, map, string, lock, or report object.

## POSIX mapping and `mmap`/`shm_open` invariants

`SharedMemoryRegion` maps exactly `size_of::<SharedBank>()` bytes with `PROT_READ | PROT_WRITE` and `MAP_SHARED`. The creator obtains a nonce-qualified name with `O_RDWR | O_CREAT | O_EXCL`, sizes the object with `ftruncate`, maps it, and initializes the whole bank with `ptr::write(SharedBank::new(generation))` before it publishes the name to a worker. A name collision is retried; the creator never unlinks an unknown pre-existing object.

A worker-side opener uses `shm_open` on the provided NUL-free UTF-8 name, calls `fstat` before `mmap`, and requires the backing object length to equal the exact VM-page-rounded `size_of::<SharedBank>()` established by the creator. An undersized, oversized, or otherwise incompatible object returns `InvalidData` before the code dereferences a mapping, preventing a truncated object from causing a SIGBUS during header validation. After mapping the fixed `SharedBank` prefix, it validates the header/topology and every raw slot state. Each active slot must carry a valid request ticket and bounded payload/event offsets; its owner, completion ticket, and partial or complete monotonic timing must match the published state. A malformed state or payload is rejected as `InvalidData` before the bank is exposed. The C shim supplies the typed third argument required by the variadic POSIX creation call and the typed `fstat` length result; it does not add a separate allocation or ownership layer.

The mapping pointer is held as `NonNull<SharedBank>`. `bank()` exposes only the safe protocol API, and `bank_mut()` requires exclusive access to that process's `SharedMemoryRegion`; cross-process plain-payload ownership is still controlled by the slot atomics, not by Rust's local borrow checker. `SharedMemoryRegion` is `Send` because moving its sole Rust owner transfers exclusive access to the mapping. It is deliberately not `Sync`: callers may not invent concurrent Rust aliases to a mapped bank.

A creator unlinks its POSIX name only in its own `Drop`; existing open mappings remain valid for workers that completed the launch handshake. Every successful region drop calls `munmap` for the exact bank length and closes its descriptor exactly once. An opener never unlinks the creator's name. If creation fails after this process created the name, cleanup closes and unlinks only that known object.

## Mach continuous clock and deadline use

`MonotonicClock::new()` calls `mach_timebase_info` off the real-time thread and rejects a zero numerator or denominator. The callback may call only `mach_continuous_time()` through `now_ticks`; it takes no arguments, allocates nothing, and uses the same process-independent tick domain for host and worker timing. Duration/tick conversion uses saturating integer arithmetic and is prepared from the cached timebase.

The product and Phase 1 dispatchers derive a 75%-of-device-period completion budget from the fixed 48 kHz frame count. They publish all open rack requests first, then sample the continuous clock while performing nonblocking acquire-based completion sweeps. A valid completion records both worker publication and host observation ticks. Unresolved requests are abandoned when possible and proceed to deterministic rack-local fallback at the absolute bound. The product gate retries on the next block and requests supervisor recovery only after three consecutive deadline misses; any accepted completion resets that streak. Protocol faults and worker loss still close immediately. `getrusage`, environment collection, report serialization, and wall-clock sleeps are qualification/control-plane work and are never callback operations.

## MIDI and diagnostic handoff

CoreMIDI/midir ingress converts supported short MIDI 1.0 messages to a fixed three-byte `MidiEvent` and pushes it into preallocated SPSC rings. Note-offs and note-on-zero messages use a separate protected lane and are drained before ordinary events. Continuous CC, pitch-bend, and channel-pressure updates may coalesce to the latest value under pressure. Oversized, empty, SysEx, and MIDI 2.0-like messages are rejected at this alpha boundary; a full ring increments exact counters rather than allocating or blocking. The audio side drains into a caller-owned `BoundedMidiEvents` array, so no `Vec` conversion enters the realtime path.

The audio callback and dispatcher publish only bounded counters, compact fallback/protocol outcomes, rack IDs, and tickets. A non-real-time consumer may snapshot telemetry, enrich, persist, display, and rate-limit reports. Diagnostics may be lost under pressure; audio progress must not be.

## Teardown and failure ownership

Teardown is control-thread-only and follows a strict ownership sequence: remove device property listeners, stop the AUHAL unit, detach its callback, wait with a bounded poll for `active_callbacks == 0`, uninitialize, dispose, null C's renderer/callback pointers, snapshot final telemetry, release the native wrapper, then drop the Rust renderer. A final callback that began before stop is included in the quiescence count.

If stop, detach, quiescence, uninitialize, dispose, or native ownership validation cannot prove that the callback has retired, Rust disables safe access and deliberately leaks the complete C wrapper, AUHAL unit, renderer, and observer cluster together. It must not free the Rust renderer while C could still call it, and it must not retry a partially failed teardown from `Drop`. This intentional leak is preferable to a use-after-free on a real-time thread.

A successful short AUHAL-attached feasibility run proves neither long-run deadline behavior nor third-party plug-in safety. The Phase 1 hardware gate must still measure the specified matrix on the designated Apple Silicon system.
