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
│   ├── metadata.json
│   └── preview.png          # optional editor picture
└── recovery/
    ├── latest.json
    └── explicit.json        # document of the last explicit save
```

`session.json` is a `SessionDocument`: document schema version plus the host-owned `sp_model::Session` (topology, rack gain/mute/bypass, ordered plug-in identities/fingerprints and bypass states, normalized parameters, scenes, MIDI mappings, audio device settings, per-rack channel routes, and state references). `manifest.json` has the package schema version, a generated revision ID, optional writer metadata, and one plugin-state declaration per instance. `metadata.json` records the instance ID, captured fingerprint, byte counts, state-capture schema, last known activation result, and an optional `preview` declaration (`bytes`, `captured_at_unix_ms`) for `preview.png`. Component and controller VST3 streams are separate opaque byte files. Rack controls, `audio_settings`, and `rack_routes` use serde defaults, so version-1 packages written before those fields existed remain readable without a schema bump.

`audio_settings` saves optional input and required output device IDs and names,
plus the preferred 32-, 64-, 128-, or 256-frame block size. On load, the app
matches ID and name, or a unique matching name; an unavailable or ambiguous
device must be selected again. `rack_routes` assigns each rack an optional mono
or stereo input and a mono or stereo output. Channel indices are zero-based in
JSON (0–63, shown as physical channels 1–64 in the UI), and a stereo pair must
use distinct channels. Routes are checked against the opened devices before
audio starts. The model and backend accept these choices at 48 kHz. BlackHole
operation has passed live checks at 32 and 64 frames; this does not qualify every
hardware device. See [live validation](live-validation.md).

Document schema 2 adds plug-in sidechains. A plug-in slot may have `sidechain`;
it is left out when unset. `{"physical_input": {"kind": "stereo", "left": 2,
"right": 3}}` feeds the slot's stereo aux input from two zero-based input-device
channels in the same audio block. `{"rack_output": "<rack id>"}` feeds it another
rack's post-fader output, one block late. A physical source must be a stereo
pair of distinct channels 0–63. A rack source must name an existing rack other
than the slot's own rack; removing a rack clears the sidechains that name it.
Routes are checked before audio starts, and a missing sidechain input channel is
an error. The UI never writes a rack route without an input.

`pages` (left out when empty) holds up to 16 named rack views:
`{"id": "page-1", "name": "Drums", "racks": ["rack-1", "rack-4"]}`. A page lists
existing rack IDs, each at most once; it only chooses which racks the show screen
shows and never affects audio. A session holds up to 64 racks, with one source and
one endpoint declaration per rack.

The default `vst3-host` adapter stores its versioned complete-state envelope in
`component.bin`. That envelope contains component state and any controller state
supplied by the plug-in; the outer `controller.bin` is empty. The adapter restores
the envelope through the library's matching decoder and also accepts legacy raw
component state. Separate nonempty controller streams require an adapter that
supports that representation.

## Editor pictures

`preview.png` is a 320×200 PNG of the instance's native editor as it last looked:
the editor's content area scaled to cover and centre-cropped, without the title
bar. The worker takes it on its `AppKit` thread, never the audio thread: about
0.5 s after the editor opens, then at most every 2 s while the plug-in reports
changed parameter values (editor edits or its own output), and once as the
editor closes (red button, close request, slot unload, or shutdown). Parameter
changes made by the host (scenes, MIDI) do not trigger a picture by themselves.
The worker uses the window server's image of its own window
(`CGWindowListCreateImage`, looked up at run time), which includes Metal and
OpenGL layers. On macOS 27 this needs no screen-recording permission and shows
no prompt: the window server only preflights the check. A drawn view cache is
the fallback if that call is unavailable, but it misses layer-backed content.

An explicit save stores the newest picture per instance, declared in that
instance's `metadata.json` and committed with the manifest like the state
streams. A picture is stored only for an instance with saved state, and only
when it is a PNG of at most 1 MiB; other pictures are skipped. Instances
without a new picture keep their stored one; instances no longer in the saved
document drop theirs. Fresh opaque state keeps the stored picture. Autosave and
other document-only saves write no pictures and keep every stored one.

Loading a picture reads only the package, so it works before any worker starts
and for missing plug-ins. A missing, oversized, mis-sized (byte count differs
from the declaration), or non-PNG `preview.png` reads as no picture. It never
fails session load.

Instance IDs and revision IDs are bounded, single safe path components (`A–Z`, `a–z`, digits, `.`, `_`, `-`); display names are never paths. Instance IDs must be unique across the entire package because opaque state is stored by instance ID. The host does not interpret, merge, diff, or use opaque bytes for normal scene recall.

## Publication and recovery

Every save first builds and syncs a complete staged revision: `session.json`, copied/replaced opaque state, every state metadata file, and `manifest.json`. It retains the last complete package under the private recovery rollback area, replaces payload files, then atomically renames `manifest.json` last. The manifest is the commit record for the whole revision.

If a save is interrupted before that commit, its transaction marker makes the reader use the complete rollback package; partial new payloads are never mixed into the loaded session. A failed save attempts cleanup and leaves the previously published package recoverable. `recovery/latest.json` is the clean-shutdown marker: autosave writes `clean_shutdown: false`, a successful explicit save or orderly exit writes `true`. On boot, `false` triggers a recovery offer.

After each explicit save commits, the app atomically replaces
`recovery/explicit.json` with a copy of the saved `session.json`. Autosave never
touches it. The recovery offer reports when the recoverable document was written
(the unclean marker's modification time) and when the last explicit save was
written (the copy's modification time, if a copy exists). Restore keeps the
loaded, autosaved document. Discard publishes the explicit copy as the current
document through the normal save transaction and marks a clean shutdown. Opaque
state and editor pictures on disk stay, because autosave carries them forward
unchanged. Without an explicit copy, Discard keeps the recovered document and
reports an error.

Dirty host edits autosave every 30 seconds, including while the engine is stopped.
Autosave updates the host model and host-known parameter values but carries forward
the last successfully captured opaque-state declaration and bytes. It runs on the
UI thread and does not pause audio. Native-editor parameter changes reported by
the worker update the host model and can autosave while audio runs. If feedback
overflows or a plug-in changes state without reporting parameter values, the
live model may be incomplete; explicitly Save to read current values and
capture opaque state. Failed autosaves retain dirty state and retry at the
next interval. Explicit state capture publishes new streams only as part of the
same full-package transaction.

Explicit Save reads worker parameters and captures opaque state while audio
keeps running. Native-editor
parameter changes are included in the saved model. A package with orphaned saved
content and no recoverable manifest reports an error rather than becoming a new
empty session.

## Decode bounds and migrations

Before JSON decoding, each JSON file is limited to 8 MiB, nesting to 64 levels, an individual JSON string to 64 KiB, and JSON collection separators to 16,384. Opaque component and controller files are each limited to 32 MiB. File size is checked from metadata before reading; JSON shape is checked before serde constructs application objects. Size, metadata, ID, or model validation failures are reported without launching a plug-in.

Worker control protocol version 6 transfers each state stream in chunks of at
most 256 KiB, up to the same 32 MiB per-stream package limit. One transfer is
active per worker and is bound to its slot, generation, and transfer ID. Capture
reads one immutable snapshot. Restore stages both complete streams before
calling the plug-in; an incomplete or rejected transfer does not change its
state. A plug-in's own restore failure is not a transactional rollback guarantee.
The slot remains inactive during transfer, and the desktop pauses audio for
session saving. Capture and restore commit have finite 30-second deadlines;
a timed-out connection cannot be reused for later commands. Version-6 apps and
helpers must be deployed together. Version 6 accepts rack indices up to 63 (plug-in
slots stay at 8). Version 5 added `CaptureEditorPreview`: the
host sends the picture sequence it holds. The worker answers from its stored
pictures with a 29-byte descriptor: sequence, capture time, PNG length, whether
the editor is open, and a live-capture transfer ID when its picture differs. The
host reads the PNG with `ReadStateChunk` from stream 0 and releases the transfer.
The worker refuses a poll at once while its `AppKit` thread has been held for
250 ms, for example by a plug-in's modal dialog, so polls never wait on plug-in
UI code. `OpenNativeEditor` may carry an 8-byte top-left window position in
points, measured from the top-left of the primary screen; the worker applies it
only to a newly created window. Parameter-value synchronization uses ordered
batches of 1–128 IDs; replies must match the requested IDs and contain finite
normalized values. This avoids one socket round trip per parameter before an
engine restart or save. The on-disk session format is unchanged.

Package and document schemas migrate independently through explicit, directional one-version steps. Readers may migrate an older supported version forward; they never attempt to read or rewrite a newer version. The document schema is version 2 and the package schema is version 1. Document migrations `0 -> 1` and `1 -> 2` are explicit; `1 -> 2` only rewrites the version, because version-1 slots have no sidechain. Readers that know only version 1 reject version-2 documents, so they never silently drop sidechains. The package keeps its explicit `0 -> 1` hook for legacy input. Unsupported forward versions fail plainly and do not fall back to an older package merely because it exists.

## Activation and placeholders

At restore, the app/supervisor compares the session fingerprint and `metadata.json` capture fingerprint with scanner results and quarantine state. Missing, changed, quarantined, or restore-failed plug-ins remain inert and bypassed; the host retains their slots, parameters, state files, and diagnostics for a user decision. No automatic replacement is selected from a display name. A compatible worker restores only while stopped/muted, in component state → controller synchronization → controller state order; failures keep the rack gated and report the recorded activation state.

Parameter scenes remain host-owned normalized snapshots. They may change rack gain,
mute, rack bypass, plug-in-slot bypass, and selected parameter values. Each parameter
stores `ramp` (the default, including for older saved scenes) or `step` recall.
Ramps interpolate over the scene transition; stepped values are applied once at
its end. Capture reads only selected parameters while audio is stopped and uses
worker metadata to mark discrete controls as steps. Scene rack-bypass entries are
optional in older packages. Scenes never restore opaque plug-in state. See
[ADR 0006](adr/0006-parameter-scenes-and-opaque-state.md).
