# Phase 8 qualification checklist

- Install the licensed mandatory VST3 fixture named by `compatibility/corpus.toml`; run `cargo xtask compatibility`. A missing fixture is a failure.
- Route a physical or virtual loopback, retain the emitted stimulus and captured f32le stream plus provenance, then run `cargo xtask loopback --stimulus <file> --capture <file>`.
- Trigger each fault transition on attached hardware, capture the resulting audio, and run `cargo xtask click-test --fault <name> --capture <file>`.
- Attach a real MIDI source, record at least 100 paired CoreMIDI timestamps, and run `cargo xtask midi-timing --events <trace.json>`.
- Keep a supervisor alive for the requested soak duration, collect every host/worker PID's RSS endpoints and fault counts, complete the operator acknowledgement, and run `cargo xtask soak --hours 8 --metrics <report.json>`.
- Complete separate visual and accessibility JSON records using the schema. Reviewers must attach the required screenshots/recordings and mark every check explicitly.

Any unavailable device, absent capture, missing operator review, malformed artifact, or failed check is a Phase 8 failure. A qualified subcommand is evidence validation only; it does not certify the product.
