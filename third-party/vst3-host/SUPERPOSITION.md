# Local host integration patch

This directory contains the runtime source of the MIT-licensed `vst3-host` 0.9.0
crate from crates.io. The upstream license and README are retained. The root
workspace uses this copy through `[patch.crates-io]`; the global Cargo registry
cache is not modified.

Upstream source commit: `ed054908cfe057694d8cf037d0c39dfb5eb4c2ca`.
Original crate archive SHA-256:
`6ec579d54bd13b83c60c1fd8bb756cf234e36ccbfb4833ff756b417e64db7fea`.

Local changes:

- A main-thread service handle lets the worker deliver foreground VST3 Data
  Exchange blocks while its audio thread retains the processor. Each pump is
  bounded and does not copy payloads into the optional snapshot stream.
- Weak service handles become inert during plug-in shutdown. Queue leases keep
  payloads alive across reentrant controller callbacks, and shutdown drains
  background work before releasing the controller.
- A bounded connection-message bridge snapshots host-created `IMessage` IDs and
  typed attributes at `notify`, then delivers reconstructed messages on the
  main thread. Four reusable 256 KiB packets per connection direction limit
  memory. The audio thread does not wait for message or attribute locks; lock
  contention, unsupported plug-in-owned messages, oversize data, and a full
  queue return failure. The host's existing attribute setters still allocate.
- Read-only counters distinguish queued, delivered, rejected, and dropped
  messages. Message IDs and payloads are not logged.
- A bounded controller-sync queue forwards processor output parameters and
  deferred host/MIDI values on the loading thread, up to 128 values per pump.
  The existing host-observer stream remains independent. The extra controller
  reference is released before termination; restore discards stale queued values.
- Weak main-thread resize access removes resize polling and its scheduling
  demotion from the audio worker. Pending geometry is cleared across editor
  close/open.
- A weak, loading-thread-only native editor handle owns view operations while
  the worker retains the processor. Editor state is separate from mutable DSP
  state; teardown invalidates the handle and detaches the view before controller
  termination. An in-flight editor call retains the module until it finishes.
- Each module loads once per process and stays loaded. `GetPluginFactory` is
  called once and the first factory reference is kept; `bundleExit` is not
  called. A scan loads a bundle three times, and TICK and the Waves shells
  crashed on reload. Unloading the shared executable for one instance could
  also unmap another instance's code.
- A Plugin Compatibility Class that fails to instantiate or declines
  `getCompatibilityJSON` counts as no compatibility entries, as in the SDK's
  moduleinfo tool. TICK 0.6.0 returns `kResultFalse` there.

The installed Pro-Q 4 controller does not expose `IDataExchangeReceiver` in the
offline check. A live editor check observed 412 off-thread connection messages
previously dropped by the proxy. The user's 20:47:55 recording now confirms
moving spectrum and active native meters. Separate 30-second checks at 32 and
64 frames passed without audio deadline misses or recovery. All 1,812/1,941
queued messages were accepted respectively; each run recorded seven queue-full
message drops. This does not certify arbitrary plug-ins or sustained timing.

Focused tests (no audio device or native window):

```sh
cargo test --manifest-path third-party/vst3-host/Cargo.toml \
  --no-default-features --lib internal::data_exchange::tests
cargo test --manifest-path third-party/vst3-host/Cargo.toml \
  --no-default-features --lib connection_proxy_tests
cargo test --manifest-path third-party/vst3-host/Cargo.toml \
  --no-default-features --lib main_thread_editor_tests
```

Replace this local patch with an upstream release when an equivalent safe
main-thread service API is available. Keep the runtime and its license together.
