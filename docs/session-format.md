# Session format

A session is a versioned directory package. It preserves host-owned topology and makes external dependencies explicit; it does not promise byte-identical plug-in recall across hosts, architectures, plug-in versions, or vendors.

## Wire layout

```text
My Set.superposition/
├── manifest.json
├── session.json
├── plugin-state/<instance-id>/
│   ├── component.bin
│   ├── controller.bin
│   └── metadata.json
└── recovery/
    └── latest.json
```

`session.json` is a `SessionDocument`: document schema version plus the host-owned `sp_model::Session` (topology, rack gain/mute/bypass, ordered plug-in identities/fingerprints and bypass states, normalized parameters, scenes, MIDI mappings, and state references). `manifest.json` has the package schema version, a generated revision ID, optional writer metadata, and one plugin-state declaration per instance. `metadata.json` records the instance ID, captured fingerprint, byte counts, state-capture schema, and last known activation result. Component and controller VST3 streams are separate opaque byte files. Newly added rack and slot controls use serde defaults, so version-1 packages written before those fields existed migrate in place without a schema bump.

Instance IDs and revision IDs are bounded, single safe path components (`A–Z`, `a–z`, digits, `.`, `_`, `-`); display names are never paths. The host does not interpret, merge, diff, or use opaque bytes for normal scene recall.

## Publication and recovery

Every save first builds and syncs a complete staged revision: `session.json`, copied/replaced opaque state, every state metadata file, and `manifest.json`. It retains the last complete package under the private recovery rollback area, replaces payload files, then atomically renames `manifest.json` last. The manifest is the commit record for the whole revision.

If a save is interrupted before that commit, its transaction marker makes the reader use the complete rollback package; partial new payloads are never mixed into the loaded session. A failed save attempts cleanup and leaves the previously published package recoverable. `recovery/latest.json` is the clean-shutdown marker: autosave writes `clean_shutdown: false`, a successful explicit save or orderly exit writes `true`. On boot, `false` triggers a recovery offer.

Autosave updates host model and parameter snapshots but carries forward the last successfully captured opaque-state declaration and bytes. Explicit state capture publishes new streams only as part of the same full-package transaction.

## Decode bounds and migrations

Before JSON decoding, each JSON file is limited to 8 MiB, nesting to 64 levels, an individual JSON string to 64 KiB, and JSON collection separators to 16,384. Opaque component and controller files are each limited to 32 MiB. File size is checked from metadata before reading; JSON shape is checked before serde constructs application objects. Size, metadata, ID, or model validation failures are reported without launching a plug-in.

Package and document schemas migrate independently through explicit, directional one-version steps. Readers may migrate an older supported version forward; they never attempt to read or rewrite a newer version. The current schemas are both version 1, with explicit `0 -> 1` migration hooks retained for legacy input. Unsupported forward versions fail plainly and do not fall back to an older package merely because it exists.

## Activation and placeholders

At restore, the app/supervisor compares the session fingerprint and `metadata.json` capture fingerprint with scanner results and quarantine state. Missing, changed, quarantined, or restore-failed plug-ins remain inert and bypassed; the host retains their slots, parameters, state files, and diagnostics for a user decision. No automatic replacement is selected from a display name. A compatible worker restores only while stopped/muted, in component state → controller synchronization → controller state order; failures keep the rack gated and report the recorded activation state.

Parameter scenes remain host-owned normalized snapshots and may change rack gain/mute/bypass, parameters, and transition time. They never restore opaque plug-in state. See [ADR 0006](adr/0006-parameter-scenes-and-opaque-state.md).
