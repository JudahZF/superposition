# Phase 0 foundation qualification record

- **Baseline revision:** `6c3a616e631641aa971b4a36560c441f20d40ccd`
- **Source state while observed:** dirty working tree on `claude/complete-plan`
- **Static-gate result:** Passed
- **Hardware-certification result:** Not evaluated; Phase 1 remains pending
- **Raw evidence:** `target/phase0/foundation-report.json` and its referenced `target/phase0/logs/` files

This record covers only the Phase 0 static workspace, contract, documentation, and dependency-boundary gate. It does not certify device timing, plug-in compatibility, loopback, MIDI, fault containment, or soak behavior.

## Source provenance

The foundation report records a deterministic SHA-256 source manifest alongside the baseline revision. It hashes the complete binary tracked patch against the baseline plus every untracked source input; qualification documents are excluded from that source manifest because they are derived evidence rather than executable/source inputs.

| Manifest item | Observed value |
| --- | --- |
| Tracked patch SHA-256 | `49ca1793aad5a2fb7403b0aa24f9309cfb995e70a3f5bc0f36b61ae002ca6263` |
| Aggregate source-manifest SHA-256 | `9e41c2a62e018586014a24280f819d1d99fffeaf6af1acc358cd9c1feceb2e0a` |
| Manifest algorithm | SHA-256 |
| Excluded evidence paths | `docs/qualification/**` |

| Untracked source input | SHA-256 |
| --- | --- |
| `docs/adr/0010-direct-auhal-backend.md` | `4fae770e7d85f2e9f5a64bc2f92ed43befcb4e915efc8ff4adee32fdc54b1c4f` |
| `tools/llvm-tools/llvm-cov` | `a853ec775794e7552fa6b263824d50e8275d8b702795942b640b5866b5879053` |
| `tools/llvm-tools/llvm-profdata` | `1bd0289a25e54550cc8a855274d6bd563740cacab92fc8c0401e1ceff056eb5c` |
| `tools/llvm-tools/llvm-tool` | `9538eafdee33d436ad37b56c47ee6d95eb2e1a08becb56d04689919bd72351d8` |
| `tools/llvm-tools/rustc` | `84efa9afc115a95b90ef5e9e405fa40208074d44b7d6177fd2ea56dc5fc918eb` |
| `tools/xtask/src/phase0.rs` | `cb3d1d1fe192b2265de734dbe65ee718ab16023a81ffe981401394187a13b681` |

## Observed environment

| Item | Observed value |
| --- | --- |
| Platform | macOS/aarch64 |
| macOS | 27.0 (build `26A5368g`) |
| Reference hardware | `Mac16,8`, 25,769,803,776 bytes memory |
| Rust | `rustc 1.95.0 (59807616e 2026-04-14) (built from a source tarball)` |
| Cargo | `cargo 1.95.0 (f2d3ce0bd 2026-03-21)` |
| Xcode | 27.0 (build `27A5194q`) |
| CMake | 4.1.2 |
| cargo-nextest | 0.9.136 |
| cargo-deny | 0.20.2 |
| cargo-audit | 0.22.1 |
| cargo-llvm-cov | 0.8.7; the repository's `build.rustc` wrapper resolved the pinned Rustup sysroot and its `llvm-tools-preview` binaries |
| `VST3_SDK_DIR` | Not configured; not required for this static gate |

## Observed static evidence

`cargo xtask phase0-report` ran the static checks, retained stdout/stderr logs, and atomically wrote the JSON report. It returns failure if any listed command or the Cargo-metadata dependency boundary fails.

| Evidence | Observed command / inspection | Result | Retained output hashes |
| --- | --- | --- | --- |
| Workspace readiness | `cargo xtask doctor` | Passed; implementation readiness passed and hardware certification remained pending | stdout `9129b0a97ad0ac55172cd979a1de986dca92d72c6f89597db4d31de4ea683a48`; stderr `1e66729317afda6dd2efaced0a7f23ecf547e0df053f03422404ed38cbf85f55` |
| Formatting | `cargo fmt --all -- --check` | Passed | stdout/stderr `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855` |
| Lint | `cargo clippy --workspace --all-targets --all-features -- -D warnings` | Passed | stdout `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855`; stderr `9d892c5778d0c975f64a671e66ea5a753502353da3addd1dfdf3825225e406a2` |
| Tests | `cargo nextest run --workspace --all-features` | Passed | stdout `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855`; stderr `fa5eb3656daefdcfe034dc1330f58f77c7f6a4f74f03e91fdae4bbb0f0a8b2b8` |
| Dependency policy | `cargo deny check` | Passed | stdout `f1a0fca39d4280363937aabd77783990ea6480bd9ca257816de3b68fc8efa845`; stderr `a76812ab864c43b077e3bec28a57c8afed92837d347e52becc7963c216446629` |
| Dependency audit | `cargo audit` | Passed; allowed unmaintained warnings were retained | stdout `48833645b967969df917693b0256fb67407b0f0fbfeea8c4f8644b84bdb6f940`; stderr `35c154de1a62d0c6312d6f97bcf4ca84478d1acc3d97539e5fedccd253d28ced` |
| Static coverage | `cargo llvm-cov nextest --workspace --all-features` | Passed | stdout `22611188b11d068e6ca3894da2eb3b26dd166b1bbc3c0a304ab020f276668fea`; stderr `cdf583dec936836ec233009710bca658bed0d5d7eca497b91c4b1a64de83a02c` |
| Required bare coverage invocation | `cargo llvm-cov nextest --workspace` | Passed through the portable pinned-Rustup compiler/tool wrapper | Direct command run; its uninstrumented-scope result is not substituted for the all-feature retained evidence above |
| VST3 boundary | Cargo metadata inspection in `cargo xtask doctor` and the foundation report | Passed: VST3 loading is helper-only; the app and engine do not reach `sp-vst3`, `vst3`, or `vst3-host` | Structured result in `target/phase0/foundation-report.json` |
| Unsafe/FFI review | Direct review of the implemented AUHAL, callback ownership, mapped-memory, clock, `Send`, and teardown sections in [real-time safety](../realtime-safety.md) against their current code boundaries | Passed for the documented Phase 0 foundation invariants | Documentation/source review; no generated artifact |

The all-feature coverage build excludes the deadline-sensitive real-process Phase 1 fault test because instrumentation changes scheduler timing; the ordinary `cargo nextest run --workspace` gate executed that behavioral test successfully.

## Hardware certification boundary

No device-attached qualification was run for this Phase 0 record. `cargo xtask doctor` and the foundation report intentionally leave `phase1_hard_gate_certified` false. The required later command shape is:

```bash
cargo xtask device-feasibility --racks <1|2|4|8> --frames <128|256> --duration-seconds <seconds>
```

Phase 1 requires the controlled 48 kHz stereo AUHAL matrix and dedicated long-run evidence. Hosted CI, unit tests, synthetic IPC runs, and this static record do not meet that hardware gate.

## Known current limitations

At the time this Phase 0 record was captured, the direct-AUHAL foundation was output-only. Later implementation added the duplex product route, worker/supervisor composition, and multi-slot lifecycle; this historical record still makes no hardware-timing or broad VST3-compatibility claim. VST3 loading remains confined to scanner and worker helpers.
