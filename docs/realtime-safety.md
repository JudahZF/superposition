# Real-time safety contract

The device callback is a deadline-bound boundary, not a general execution context. Its permitted work is bounded arithmetic; reads and writes to preallocated callback-owned or lock-free data; fixed-layout shared-memory slot exchange; bounded clock reads; one non-waiting kernel wake per published product rack; and DSP that has been reviewed to obey the same limits. This document records the concrete invariants of the current AUHAL and shared-memory foundations. It does not certify third-party plug-ins or hardware timing.

## Forbidden in a callback

The callback must not allocate or free; acquire a mutex, read/write lock, semaphore, or condition variable; wait, sleep, park, join, or block on IPC; perform filesystem, network, process, dylib, or plug-in discovery work; log, format strings, capture stacks, or emit diagnostics synchronously; take a garbage-collected/runtime lock; call Objective-C, AppKit, or unreviewed Core Foundation APIs; or invoke a plug-in in the host process.

It must not make a request whose completion is required before returning. Device configuration, property listeners, start/stop, helper launch, shared-memory creation, clock construction, telemetry snapshots, persistence, and recovery all run off the callback thread. A panic may not unwind through the C callback boundary: `OutputRenderer` and `MultiChannelDuplexRenderer` implementations must not panic, and a panic in the Rust trampoline aborts rather than running recovery machinery on the real-time thread.

## Direct AUHAL callback invariants

The current macOS backend uses direct AUHAL, not `cpal`. Product setup and
teardown run on a control thread. A route selects an optional input device and
a required output device; distinct devices use a process-private CoreAudio
aggregate clocked by the output device. The product client format is interleaved
`f32` at 48 kHz, with 0–64 input channels, 1–64 output channels, and a fixed
32-, 64-, 128-, or 256-frame block size supported by the selected devices.
When an input is selected, the callback captures it with `AudioUnitRender`
before dispatching to the Rust renderer. Per-rack mono/stereo routes select physical
channels 1–64 and are validated before activation. The output-only
`AudioEndpoint::start` path on the default device remains stereo. Separate-device
aggregate operation and long-run timing remain unverified; see
[live validation](live-validation.md).

The C render callback follows these rules before it calls Rust:

- It increments `active_callbacks`, zeros the supplied CoreAudio buffers, and uses a lock-free `writer_active` compare/exchange guard. A concurrent or reentrant entry is rejected without waiting and without calling Rust.
- It accepts exactly the configured frame count, one non-null `AudioBuffer`, the configured output channel count, and exactly `frames * output_channels * sizeof(float)` bytes. Any mismatch remains silent and is classified in atomic telemetry.
- Only after those checks does it pass the `float *` buffer and frame count to the Rust trampoline. `Rendered` means the renderer wrote the complete validated buffer; every other disposition is re-zeroed by C.
- The C callback is the sole telemetry sequence writer. Control-thread snapshots use the odd/even sequence protocol to obtain a coherent counter set. Snapshot retry is a control-plane operation and must never be moved into the callback.

The callback itself contains no C heap allocation, device-property calls, property polling, lifecycle work, or blocking sleep. Those operations exist only in AUHAL setup/teardown. The direct backend and its deviation from the original `cpal` proposal are documented in [ADR 0010](adr/0010-direct-auhal-backend.md).

Product recovery uses one atomic phase word plus bank index/generation fields per rack. The control plane requests quiescence; at a block boundary the callback abandons any live request, closes only that rack gate, and acknowledges that it will touch neither bank. The control plane may then stop and reap the old worker, reset the inactive mapping, and launch a replacement. After replacement readiness is release-published, the callback validates the prepared mapping and changes its fixed bank selector at a block boundary. Only after that acknowledgement may the control plane reset the retired mapping. The callback validates and acknowledges the reset before returning the handshake to idle. No mapping address, allocation, process operation, or IPC endpoint changes on the real-time thread.

## C/Rust pointer and renderer ownership

`ActiveOutput<R>` (stereo output-only) and `ActiveMultiChannelDuplex<R>` (routed
product stream) each own an opaque C output wrapper and a boxed renderer. Startup passes the
stable boxed address to C as `void *` and stores the opaque native pointer as
`NonNull`. The callback may form `&mut R` only because C has validated the
buffers, the writer guard gives it exclusive callback-time execution, and C
retains the callback pointer until retirement.

The Rust trampolines check frame geometry and raw pointers before constructing
slices. C has already proved that each pointer covers its configured interleaved
input or output byte region, and Rust constructs no larger slice. The raw `SpAudioOutput` pointer is never
dereferenced as a Rust value; it remains opaque. The C ABI records explicit
`renderer_retired` and `native_releasable` flags so Rust never infers callback
retirement from a successful-looking status alone.

The product endpoint keeps its original boxed renderer across a safe stop or
start failure. It returns that renderer to pending state only after C proves
callback retirement and native release. If teardown is uncertain, it retains
the renderer with native state, latches an endpoint fault, and refuses reuse.
After a safe stop, a borrowed endpoint proof identifies the exact retained
rack recovery signals. Only that endpoint services their stopped handshakes;
the app handles unattached racks separately. A fresh renderer receives the
current active bank index and refuses an unfinished handoff.

`Send` here transfers the renderer into the callback-owning output; it does not make the renderer `Sync`, permit two callbacks to borrow it, or permit user code to retain the C pointer. A renderer must remain valid until native retirement is confirmed. Any new callback context, observer, or raw pointer must be part of that same ownership cluster and obey the same retirement rule.

## Atomic, slot, and shared-memory ownership

- Every shared slot has one producer and one consumer for a ticket generation. A ticket has a nonzero generation and sequence; stale or mismatched completions are rejected.
- A producer writes input audio/event payload before the release-store that publishes `Requested`. A worker acquire-loads the state before consuming the payload, claims it before writing output, then publishes completion with the protocol ordering. The host acquire-loads completion before it reads output.
- Capacity, channel layout, maximum frames, event capacity, and the four-slot bank layout are fixed before activation. The callback never resizes a queue, a bank, a mixer buffer, or a device format.
- The dispatcher bounds its completion observation with a `MonotonicClock` deadline. It closes the affected rack gate for malformed metadata, non-finite output, stale ticket, unavailable slot, or deadline expiry; it never waits for a worker beyond that deadline.
- A timed-out processing slot is not casually reused. Control-plane recovery and worker death handle retirement; callback code accepts only the exact live ticket.
- Atomic telemetry uses relaxed counter increments where a coherent cross-counter relationship is not required. Publication and ownership transitions use the acquire/release protocol defined by `sp-protocol`; C's writer/telemetry sequence uses sequential consistency. Any weaker order added later requires a code-review justification.

## POSIX mapping and `mmap`/`shm_open` invariants

`SharedMemoryRegion` maps exactly `size_of::<SharedBank>()` bytes with `PROT_READ | PROT_WRITE` and `MAP_SHARED`. The creator obtains a nonce-qualified name with `O_RDWR | O_CREAT | O_EXCL` and sizes the object with `ftruncate`. `bank_init` constructs a boxed `SharedBank` field by field, then the creator copies the fully initialized bank into its exclusive mapping before publishing the name. Replacement reset uses the same boxed construction and copies only after the old worker has exited. A name collision is retried; the creator never unlinks an unknown pre-existing object.

A worker-side opener uses `shm_open` on the provided NUL-free UTF-8 name, calls `fstat` before `mmap`, and requires the backing object length to equal the exact VM-page-rounded `size_of::<SharedBank>()` established by the creator. An undersized, oversized, or otherwise incompatible object returns `InvalidData` before the code dereferences a mapping, preventing a truncated object from causing a SIGBUS during header validation. After mapping the fixed `SharedBank` prefix, it validates the header/topology and every raw slot state. Each active slot must carry a valid request ticket and bounded payload/event offsets; its owner, completion ticket, and partial or complete monotonic timing must match the published state. A malformed state or payload is rejected as `InvalidData` before the bank is exposed. The C shim supplies the typed third argument required by the variadic POSIX creation call and the typed `fstat` length result; it does not add a separate allocation or ownership layer.

The mapping pointer is held as `NonNull<SharedBank>`. `bank()` exposes only the safe protocol API, and `bank_mut()` requires exclusive access to that process's `SharedMemoryRegion`; cross-process plain-payload ownership is still controlled by the slot atomics, not by Rust's local borrow checker. `SharedMemoryRegion` is `Send` because moving its sole Rust owner transfers exclusive access to the mapping. It is deliberately not `Sync`: callers may not invent concurrent Rust aliases to a mapped bank.

A creator unlinks its POSIX name only in its own `Drop`; existing open mappings remain valid for workers that completed the launch handshake. Every successful region drop calls `munmap` for the exact bank length and closes its descriptor exactly once. An opener never unlinks the creator's name. If creation fails after this process created the name, cleanup closes and unlinks only that known object.

## Mach continuous clock and deadline use

`MonotonicClock::new()` calls `mach_timebase_info` off the real-time thread and rejects a zero numerator or denominator. The callback may call only `mach_continuous_time()` through `now_ticks`; it takes no arguments, allocates nothing, and uses the same process-independent tick domain for host and worker timing. Duration/tick conversion uses saturating integer arithmetic and is prepared from the cached timebase.

The product dispatcher derives a 75%-of-device-period completion budget from the fixed 48 kHz frame count. It publishes all open rack requests first, then sample the continuous clock while performing nonblocking acquire-based completion sweeps. A valid completion records both worker publication and host observation ticks. Unresolved product requests are abandoned at the absolute bound, closing only the affected rack and requesting supervisor recovery. An abandoned slot cannot be reused until the worker is reaped and its bank is retired. Protocol faults and worker loss also close immediately. Wet output comes from the current input block; dry bypass adds the reported plug-in latency without an extra host block. Report serialization and wall-clock sleeps are control-plane work and are never callback operations.

## MIDI and diagnostic handoff

User-interactive QoS experiments did not resolve the measured eight-rack failures
and were removed. Idle production workers use the public macOS 14.4+
`os_sync_wait_on_address_with_timeout` API on a dedicated shared-memory wake
sequence. Each worker snapshots the sequence before scanning slots and waits
only if no request was observed. A failed claim after observing a request retries
instead of sleeping through an already-consumed wake. Completion polling first
checks the atomic state and does not take slot ownership until `Complete`; the
owned snapshot still rechecks state before reading any plain payload. The atomic
compare-and-wait prevents a publication
between the scan and wait from being lost. The maximum 8 ms idle wait keeps
control, heartbeat, and editor polling active when audio is stopped. Waiting is
worker-only.

The processing thread applies a preemptible Mach time constraint after claiming
an audio block. Its period comes from that block's actual 32/64/128/256-frame cadence
at 48 kHz, not the plug-in's maximum buffer allocation. Computation is one quarter
of the period and constraint is one half. A thread-bound handle checks policy
application, rejects an already-real-time caller, and restores ordinary scheduling
before queued control work and on exit. Between active blocks it keeps
the policy while waiting. The wait is capped by the remaining two-period idle
budget; after that timeout the loop demotes. This is not Audio Workgroup membership
and does not constitute device scheduling certification.

Plug-in resize requests are drained by the worker's main-thread service and
applied directly to its native window. The audio worker no longer polls the
resize mutex, transfers geometry through a channel, or demotes its scheduling
policy on an editor timer. The latest pending size is cleared across editor
close/open; weak service handles cannot retain an unloaded plug-in. This
removes an avoidable scheduling risk, not a guarantee of arbitrary-editor
timing. Earlier live checks are recorded in [live validation](live-validation.md).
Host-requested window resizing runs on the worker main thread and uses the
dimensions returned by the plug-in's SDK resize call, including fixed-size or
constrained editors.

Open, focus, close, and resize requests use loading-thread editor handles while
the processing thread retains the rack runtime and DSP. Opening an already-open
editor focuses it. The native red close button records a deferred request; the
loading thread detaches the plug-in view before closing its window. The app
starts Open asynchronously and polls its control reply, so it does not wait for
third-party editor setup in the UI update loop. Open and close use finite
30-second control timeouts, independent of unchanged audio block deadlines.
The revised editor lifecycle passed repeated open/close checks during continuous
BlackHole audio: Pro-Q 4 at 32/64 frames and Archetype Nolly X at 64 frames.
These short checks do not establish long-run or all-device reliability.

After publishing a request, the product callback increments that bank's wake
sequence and calls `os_sync_wake_by_address_any` once with the shared-memory flag.
No waiter is a normal result. Other wake errors and maximum observed wake-call
ticks are recorded atomically. This narrowly scoped kernel wake is a deliberate
addition to the callback contract, not permission to wait on a synchronization
primitive. The underlying kernel call is not a hard real-time guarantee; live
timing evidence must include its cost. The shared-memory ABI is version 9 and
requires matching app/helpers. Version 9 raises the rack capacity from 8 to 64; every
per-rack callback array is preallocated for 64, and per-block telemetry visits all 64
slots with bounded, allocation-free work. Live timing evidence so far covers eight racks. The dispatcher deadline and failure criteria are
unchanged. On a miss,
fixed atomic diagnostics distinguish an unclaimed request
from in-progress work or a late completed slot and retain the last request,
claim, and observation ticks. This instrumentation does not log from the
callback or read worker-owned non-atomic payloads.

Generic parameter metadata is cached before the worker is published to audio.
Opening generic controls during playback reads that cache and host-owned values;
it does not synchronously query the plug-in. Slider writes enter the bounded
callback event queue. Native editor changes made during playback are refreshed
into the host model through a bounded shared-memory parameter sidecar without
waiting for audio to stop. Opening, focusing, resizing, or closing the native
editor does not hand the rack runtime to the
loading thread or stop audio. Opaque-state capture and batched parameter reads
also run on the loading thread beside DSP, so explicit Save and scene capture do
not pause audio. Loading and state restore still use the control handoff with
audio stopped. The
30-second dirty autosave
uses only host-known model values and the last saved opaque capture; it performs
no worker query and does not stop audio. Host-known MIDI and scene updates also
refresh the optional generic inspector's normalized values. The plug-in's
worker-owned native editor is the primary editor action. UI worker status uses
the local session and atomic recovery/heartbeat state, not a periodic
`QueryHealth` RPC; startup still uses a bounded control health request.

Host/MIDI parameter changes queued off the loading thread and processor output
parameter changes share a bounded controller-sync queue. The main-thread service
applies at most 128 values per pump through `setParamNormalized`. Processor
feedback also retains its separate host-observer stream. The audio side only
pushes fixed `(id, value)` entries; controller calls and controller lifetime
management remain on the main thread. This supplies the output-parameter
forwarding required by the [VST3 parameter contract](https://steinbergmedia.github.io/vst3_dev_portal/pages/Technical%2BDocumentation/API%2BDocumentation/Index.html).

Native editor values enter a fixed-ID atomic mirror in the worker, then its
main thread publishes latest values into a separate shared-memory mapping.
Each slot has 4,096 distinct-parameter cells, an instance epoch, revision, and
overflow count. The app polls snapshots on its control/UI side, validates slot
identity and writable parameter IDs, and updates its model. This sidecar is not
read by the device callback and does not carry feedback in audio blocks or
control replies. Overflow marks the mirror incomplete; explicit Save still
captures opaque plug-in state, without stopping audio.

Scene tables are prepared on the control thread. `ProductControl::publish_scenes`
sends a complete replacement through a one-slot queue. The callback swaps it in
at a block boundary, keeps parameter values it has observed, and returns the old
tables through a second queue, so the callback never allocates or frees them.

Rack layout changes use the same queue. `ProductControl::publish_topology` carries
a prepared graph, the new position of each continuing rack, and boxed dispatch
lanes for new workers, which the control plane starts first. At one block
boundary the callback permutes per-rack mixer state (gain, fades, dry-delay
history, meters) by swaps, moves continuing lanes, and pushes new lanes into
capacity reserved at construction. Lanes it drops go back through the retire
queue; their workers stop only after the control plane sees the change applied.
A rack whose worker was replaced fades from the last frame it played to
latency-matched dry audio, then fades to the new worker's output when that
worker delivers three valid blocks. Both fades last 16 samples; fault fallback
keeps its 64-sample fade. A unit test checks both fades sample for sample.
Plug-in edits normally avoid this path. The worker loads an added plug-in on its
loading thread and hands it to the processing thread, which places it between
blocks. Removal and reorder also apply between blocks; editor windows follow
their plug-in, and teardown happens on the loading thread. Each slot records the
input it receives. Until an added plug-in produces three consecutive good
blocks (no processing error, only finite samples), and whenever a plug-in is
bypassed, the plug-in still processes that input but the chain receives the
input delayed by the plug-in's latency instead of its output. Each change between
the two, including the join after warmup, fades over 16 samples. Rack latency
therefore counts bypassed plug-ins. Each worker preallocates this history for
65,536 frames of latency per slot (about 4.2 MiB); a plug-in reporting more
passes silence while unheard. A slot suspected in a worker fault is deactivated
in the replacement worker rather than bypassed, so it does not run. Rerouting a
rack's hardware channels uses the same path with every rack kept, so the move takes
effect on the next block with no restart.
Nothing in this path allocates, frees, or locks on the callback.

Plug-in sidechains use a fixed aux region in every block slot: two channels of
256 frames for each of the eight plug-in slots (16 KiB). The request carries a
bitmask of the slots whose region holds this block's audio. The prepared graph
resolves each slot's source before activation. While it writes a rack's
request, the callback fills the region of each sidechained slot. A physical
source copies two channels of the same callback input, so it is sample aligned.
A rack source copies the source rack's post-fader output from the previous
block. The mixer keeps that output in a preallocated per-rack buffer as it mixes
each block, and moves it with its rack on a layout change. The one-block delay
keeps racks independent and parallel, so no cycle check is needed. It is not
delay compensation. The source's output is what it contributes to the mix: a
muted rack gives silence, and a rack in dry fallback gives its dry audio. A rack
that has not yet rendered a block of this length also gives silence. The copy
is bounded by 64 racks, eight slots, 256 frames, and two channels. It reads
only preallocated buffers and neither allocates nor locks. The worker feeds an
active sidechain input from its slot's region only when the request marks that
slot, and silence otherwise, so a live slot edit never feeds stale audio. Route
start rejects a physical sidechain pair that the input device lacks, as it does
for rack routes.

The vendored VST3 `ParameterChanges` preallocates 4,096 queues and 8,192 point
nodes per container before processing. Block-time access uses bounded atomic
operations, with no mutex or first-use pool growth; contention and capacity
refusals increment a sticky loss counter published through the sidecar. This
does not make third-party plug-in processing real-time safe. Protocol 7 keeps
restart counts per slot and flag. The app observes them without consuming newer
requests and, for component reload or I/O change, requests a rack-local
quiescent worker replacement. A latency change needs no replacement: the worker
republishes latency and the mixer retunes the dry delay while audio continues. State capture and launch are control
work, not callback work; other flags remain diagnostic. Native-gesture-to-crash
recovery still lacks live proof.

Worker latency is release-published outside its active `process()` call. The
callback acquire-reads the selected bank's value only when that bank cannot be
reset during recovery. The mixer preallocates eight stereo dry-delay rings of
65,536 frames plus one 256-frame callback guard (about 4.2 MiB). It matches
reported latency through 65,536 frames exactly. Larger values stay reported,
but every dry path, including bypass and transitions, is silent rather than
reading the wrong history. This boundary needs live validation.

One missed audio deadline causes worker replacement and records a timeout, not
proof that the currently attributed plug-in hung. Its slots remain active on
replacement. A failed health check uses hang classification and may isolate an
attributed slot. Repeated failures still contribute to the existing quarantine
threshold only for confirmed health failures. Unattributed audio timeouts do not
quarantine a plugin fingerprint. Each rack permits three replacements in a
ten-second window; a further failure stops its old worker after callback
quiescence and keeps only that rack in dry fallback until it is reloaded.
The scanner owns scan-failure quarantine ingestion; a cached failure does not
increment its count again. The UI exposes fingerprint-specific **Allow retry**,
which clears only that plug-in's failure history. Rack-load rejection identifies
the blocked plug-in even when it is not the first slot.

Off-thread VST3 connection messages use a bounded worker-side queue. The host
copies its own message ID and typed attributes into a preallocated packet with
non-waiting `try_lock` calls. Busy storage, unknown message implementations,
oversized messages, and exhausted packets fail immediately. Each direction has
four 256 KiB packets, with at most 16 attributes and 128-byte IDs/keys. The
main-thread service reconstructs a separate message and calls the destination;
it never forwards a retained mutable source message. This adds no device
callback work. It does not certify arbitrary plug-in code as real-time safe:
message construction already allocates, and querying an unknown plug-in-owned
COM object can execute plug-in code. Transport counters do not log payloads.

CoreMIDI/midir ingress converts supported short MIDI 1.0 messages to a fixed three-byte `MidiEvent` and pushes it into preallocated SPSC rings. Note-offs and note-on-zero messages use a separate protected lane. The consumer merges both lanes in arrival order, including equal timestamps, so same-block note pairs and retriggers retain their meaning. Continuous CC, pitch-bend, and channel-pressure updates may coalesce to the latest value under pressure. Oversized, empty, SysEx, and MIDI 2.0-like messages are rejected at this alpha boundary; a full ring increments exact counters rather than allocating or blocking. The audio side retains pending events across blocks and drains into a caller-owned `BoundedMidiEvents` array, so no `Vec` conversion enters the realtime path.

The product callback also reads the continuous clock when its renderer starts and
ends. It keeps the longest duration since the last read, in permille of the
block period, in one atomic with `fetch_max`. `ProductTelemetry::take_callback_load`
swaps it to zero, so the UI sees the worst block per read. This covers the Rust
renderer, not the C shim's input capture.

The audio callback and dispatcher publish only bounded counters, compact fallback/protocol outcomes, rack IDs, and tickets. A non-real-time consumer may snapshot telemetry, enrich, persist, display, and rate-limit reports. Diagnostics may be lost under pressure; audio progress must not be.

## Teardown and failure ownership

Teardown is control-thread-only and follows a strict ownership sequence: remove device property listeners, stop the AUHAL unit, detach its callback, wait with a bounded poll for `active_callbacks == 0`, uninitialize, dispose, null C's renderer/callback pointers, snapshot final telemetry, release the native wrapper, then drop the Rust renderer. A final callback that began before stop is included in the quiescence count.

If stop, detach, quiescence, uninitialize, dispose, or native ownership validation cannot prove that the callback has retired, Rust disables safe access and deliberately leaks the complete C wrapper, AUHAL unit, renderer, and observer cluster together. It must not free the Rust renderer while C could still call it, and it must not retry a partially failed teardown from `Drop`. This intentional leak is preferable to a use-after-free on a real-time thread.

A successful short live run proves neither long-run deadline behavior nor third-party plug-in safety. Long-run timing must still be measured on physical Apple Silicon hardware.
