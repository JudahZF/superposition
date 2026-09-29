# Superposition UI design: film strip

Status: approved design, 2026-09-25; implemented the same day. This document is the
implementation brief for the egui show screen. It is self-contained; the interactive
mock and the exploration documents are references, not requirements.

Where the build differs from this brief, and why:

- Columns are a fixed 240 px instead of the mock's stretching `minmax(196px, 1fr)`
  grid, so a column looks the same with two racks or eight.
- LED segments are 3 px, not 4 px. Sixteen 4 px segments with 2 px gaps need 94 px,
  which does not fit the 80 px meter.
- Preview height adapts to the column: 84 px at 1920×1080, shrinking to at least
  60 px on the laptop preset or while the fault line shows, so nothing scrolls
  vertically.
- Preview caption bars are opaque base, not 85 %, so a bright plug-in picture never
  lowers caption contrast below AA.
- The scene counter shows the model's limit (`n of 256 parameters`), not 64.
- The recovery modal gives ages ("from 12 min ago") rather than clock times.
- Hovered list rows draw their secondary text in text colour; dim on the hover
  fill is below 4.5:1.
- A rack-sourced sidechain carries whatever that rack sends to the device, so a
  rack in dry fallback feeds its dry audio; a muted rack feeds silence.
- Only meters animate: egui's own hover and popover fades are turned off.

References:

- Interactive mock (authoritative for look and behaviour): `plans/2026-09-25-film-strip-full-mock.html`,
  published at https://pl8admm8jpr4.postplan.dev
- Variation study with the preview capture design: `plans/2026-09-25-bench-variations.html`
- Five-direction exploration (history only): `plans/2026-09-25-ui-design-directions.html`

## 1. Decisions

1. The app is a plug-in host, not a mixer. There is no master bus, no master strip,
   no master meter. Each rack's output pair goes straight to the device. Remove
   `main_output_meter` and the master strip from the app.
2. All parameter editing happens in the plug-in's own editor window. The generic
   parameter editor, the parameter drawer and the bottom rack detail pane are
   removed from the product UI. Keep the worker parameter protocol; scenes and MIDI
   mappings still read and write normalized values through it.
3. Every rack has an audio input, mono or stereo. The UI never offers a "no input"
   or "return" rack. The model keeps `RackChannelRoute.input: Option<..>` for
   compatibility with saved sessions, but the UI always writes `Some`. If
   instrument racks are wanted later, add them as a distinct input mode, not as
   "no input".
4. Plug-ins can take a sidechain from a physical input pair or from another rack's
   post-fader output. This is new model, protocol and data-plane work (section 8).
5. Each plug-in slot shows a small picture of its editor as it last looked. The
   worker captures it; the host stores it in the session package (section 7).
6. Style is the "Bench" instrument-panel direction: one monospace face on a 16 px
   line grid, hairline dividers, no cards, no corner radii, no shadows,
   inverted-video selection, segmented meters. Judah has explicitly rejected: neon
   or saturated LED colours, uppercase labels, ASCII widgets such as `[====|--]`
   faders and unicode sparklines, and run-on status lines.
7. Layout target is a 1920×1080 display at 1× scale: eight rack columns across,
   eight preview slots down, nothing scrolls vertically. A laptop preset exists for
   1512×982.
8. A session holds up to 64 racks. More racks than fit scroll horizontally, and
   user-defined pages choose which racks show (section 3a).

## 2. Tokens

Colours (the only ones in the UI chrome; plug-in pictures supply everything else):

| Name | Value | Use |
| --- | --- | --- |
| base | `#121315` | window background |
| text | `#D9DAD6` | primary text, lit meter segments, inverted-selection fill |
| dim | `#7C8088` | labels, secondary text, tick marks |
| faint | `#3A3E45` | empty slots, unlit borders, disabled text |
| hairline | `#26292E` | column dividers, head and foot rules |
| segment off | `#24272C` | unlit meter segment |
| hover fill | `#1B1D21` | row hover in lists |
| warn | `#D9A21B` | LDG and REC tokens, mute lit, route warnings |
| fault | `#E5484D` | MIS and FLT tokens, top two meter segments, fault line |
| info | `#7FB0FF` | bypass lit, sidechain marker, mapped CC |
| fault tint | `#1A1214` | fault line background |

Type: IBM Plex Mono (OFL, embedded from `crates/sp-ui/assets/fonts/`) at 12 px
on a 16 px line. Readout labels are 10 px. Weight
500 for the wordmark, rack names and popover titles; 400 everywhere else.
Sentence case for all labels. The only uppercase strings are the three-letter
state tokens.

State tokens, always paired with their colour and never colour alone:

| Token | Meaning | Colour |
| --- | --- | --- |
| RDY | ready and processing | text |
| LDG | loading, or worker recovering | warn |
| BYP | bypassed (slot or rack) | dim |
| MIS | plug-in missing on this machine | fault |
| FLT | worker recovery failed, rack passes dry | fault |
| REC | rack worker recovering | warn |
| UNL | engine offline, worker not loaded | dim |

Spacing: 4 px unit. Column padding 12 px. Head 56 px, fault line 32 px, foot 40 px.

Motion: only meters animate. Meter repaint keeps the existing 30 Hz cadence and
ballistics from `sp-ui::components::meter`; no other ambient motion. Respect the
reduced-motion preference by holding meters still.

## 3. Show screen layout (1920×1080)

```
┌ head 56 ───────────────────────────────────────────────────────────────────┐
│ Superposition  Engine  Device  Rate  Buffer  Latency  Load  Misses          │
│                Restarts  Session            status message   Stop engine   │
│                                                              Save ⌘S  Setup ⌘, │
├ fault line 32, only when a fault is showing ───────────────────────────────┤
├ columns ───────────────────────────────────────────────────────────────────┤
│ 01 Vox   │ 02 Keys  │ 03 Gtr   │ …  │ 08 Click │ + add rack (only if < 8)  │
│ in 1 out 1-2                                                                 │
│ [preview] ×8 slots                                                           │
│ Gain -3.0 dB  track                                                          │
│ LED meters in / L / R                                                        │
│ Mute Byp                                                                     │
│ RDY 4/4                                                                      │
├ foot 40 ───────────────────────────────────────────────────────────────────┤
│ Scene 1 Intro 2 Verse [3 Chorus] 4 Bridge 5 Outro   Fade − 250 ms +        │
│ Capture ⌘⇧C  Update  Edit                       Callback ▁▂▃ load bars      │
└────────────────────────────────────────────────────────────────────────────┘
```

Head readouts are cells: a 10 px dim label above a 12 px value. Cells: Engine
(Online / Connecting / Offline), Device (output device name), Rate, Buffer,
Latency (buffer ÷ 48 in ms), Load (callback %), Misses (deadline misses),
Restarts (sum of worker restarts, warn colour when non-zero), Session (name, with
` *` when dirty). Then the latest status message in dim, right-aligned, then the
actions: Start/Stop engine, Save ⌘S, Setup ⌘, (inverted while the setup page is
open). Actions are plain text; hover underlines them.

Columns are a fixed 240 px, whatever the rack count, plus a 120 px "+ add rack"
column right after the last rack while fewer than 64 racks exist. Spare width stays
empty; a narrow window scrolls horizontally. Columns are separated by hairlines. Each column, top to
bottom:

1. Header: `0n` index in dim, rack name, gain in dB at the right. The selected
   rack's header is inverted (text fill, base text). Click selects, double-click
   renames inline, right-click opens the rack menu.
2. Route line in dim: `in 1   out 1-2`. Warn colour if any channel is not on the
   current device. Click opens the route popover.
3. Eight preview slots of 216×84 (column width less padding × 84 px), 4 px apart. Filled slots
   show the preview; the first empty slot is a dashed "+ add plug-in"; later empty
   slots are dashed and blank so all columns share one rhythm.
4. Gain: label row (`Gain` dim, value in text), a 2 px track with a 1 px tick at
   0 dB and a 2×12 px marker, tick labels -60, 0, +12 in faint. Range -60 to +12
   dB. Click or drag sets; double-click resets to 0 dB.
5. Meters: three LED columns (in, L, R), 12 px wide, 16 segments of 4 px with 2 px
   gaps, 80 px tall. Lit segments are text colour, the top two are fault colour.
   Offline: lit segments drop to faint.
6. Mute and Byp: bordered text toggles. Mute lit = warn fill; Byp lit = info fill.
7. Status: rack token, then `ready/total` slot count, then `muted` in dim.

Laptop preset (1512×982): previews drop to 60 px tall, six 240 px columns fit, the
rest scroll horizontally. Nothing else changes.

Empty session: the column area shows one centred message, "No racks yet. Add a
rack, then choose a plug-in after scanning." with the add-rack column.

## 4. Preview tile

```
┌──────────────────────────────┐
│ (editor picture, cropped)    │
│                              │
│ Pro-C 2        SC Drums RDY 2 min │  ← caption bar, 14 px, 85 % base
└──────────────────────────────┘
```

- Picture: the 320×200 capture scaled to the tile width and cropped to its middle
  band. Hover shows the full capture at 322×202 in a popover beside the tile.
- Caption: name at the left (truncates with an ellipsis); at the right the
  sidechain marker in info colour if set, the slot token, then the capture age
  (`2 min`, `1 h`, `Sep 18`) or `live` with a 6 px fault-colour dot while the
  editor is open.
- States: live (editor open); captured (age shown); recovering or unloaded
  (grayscale at 35 % opacity, token LDG or UNL); missing (grayscale, caption in
  fault colour, MIS); bypassed (picture at 40 % opacity, BYP); never opened
  (dashed tile with the name and "not opened yet", sidechain marker if set).
- Selected slot: text-colour border. Hover: dim border.
- Click opens the editor. Right-click opens the slot menu. Enter opens the
  editor of the selected slot, B toggles its bypass, arrows move the selection.

## 5. Overlays

All popovers and modals are bordered boxes (1 px dim border, base fill, 10×12 px
padding, drop shadow is the one permitted shadow), positioned near what opened
them. A transparent scrim closes popovers on outside click; modals ignore it.
Esc closes the topmost overlay, in this order: menu, route popover, sidechain
popover, plug-in picker, modal, editor window, fault line, setup page.

Rack menu: Rename (double-click), Route…, Move left ⌘←, Move right ⌘→, Remove
rack…. Move items are disabled at the ends.

Slot menu: Open editor ↩ (disabled when MIS), Bypass/Enable B, Sidechain… (or
"No sidechain input", disabled, when the plug-in has no aux bus), Move up, Move
down, Remove plug-in ⌫.

Route popover: title `Route for <rack>` with the output device in dim. `Input`
with Mono and Stereo text toggles (selected one inverted), then a jack grid of
the device's input channels (mono: 16 singles; stereo: 8 pairs), then `Output,
stereo pair` and the output pair grid. Channels beyond the device are faint and
disabled. A warn line appears when the route uses channels the device lacks.
Done Esc. Changing a route re-routes at the next block; no restart.

Sidechain popover: title `Sidechain for <plug-in>` with rack and slot in dim.
Options: None ("plug-in uses its own input"); `From a physical input, stereo
pair` with an 8-pair grid; `From another rack, post fader` listing every other
rack with its output pair. One dim line states: "Rack sources arrive one buffer
late, 1.3 ms at 64 samples. Physical inputs are sample aligned." Changing the
sidechain reloads the rack transactionally like a topology edit.

Plug-in picker (centred, 560 px wide): title `Add plug-in to <rack>` with `slot n
of 8`, a search field, the catalog grouped by vendor with hover and keyboard
cursor (↑↓ ↩ Esc). Quarantined and non-loadable entries are faint with a reason.
Choosing adds the slot with no preview and reloads the rack. Empty catalog shows
"No plug-ins yet. Rescan from Setup, Plug-ins."

Scene capture and edit (centred, 640 px): name field, filter field, the list of
automatable parameters grouped by `rack, plug-in` with checkboxes and current
values, a counter `n of 64 parameters`, and one dim line: "Scenes recall only the
parameters selected here, over the fade time. Opaque plug-in state is never part
of a scene." Capture defaults to the first two automatable parameters of every
slot; Edit opens with the scene's current set.

Modals: Stop the audio engine? (Cancel Esc / Stop engine ↩); Remove <rack>? with
"Its plug-ins and their settings are removed. Other racks keep playing."; Recover
the last session? on boot after an unclean shutdown, with Discard / Restore.

Fault line: `Fault` in fault colour, the message in text, Dismiss Esc at the
right. Replaces the current fault banner card.

Editor window: the worker-owned native `NSWindow`, unchanged. The host positions
a newly opened editor beside its column (x ≈ 520 + rack × 40, y ≈ 180 + slot ×
20 in window coordinates) and brings an already open one to front.

## 6. Foot line

`Scene` in dim, then each scene as `n Name` (number in dim); the active scene is
inverted. Scenes are disabled (faint) while the engine is offline. Then `Fade − 250
ms +` stepping 50 ms from 0 to 10 000. Then Capture ⌘⇧C, Update (rewrites the
active scene from current values), Edit (opens the picker for the active scene).
At the right, `Callback` with twelve 3 px bars of recent callback load; a bar
over 90 % is warn colour. Number keys 1–8 recall scenes.

## 7. Setup page

Replaces the settings drawer. Opened with Setup or ⌘,, it takes the column area;
head and foot stay. A 200 px tab rail (Audio, MIDI, Plug-ins, Diagnostics; the
active tab inverted) and a page limited to 900 px.

- Audio: Output device list, Input device list (including "No audio input"),
  Buffer at 48 kHz (32/64/128/256 with ms), Refresh devices. All locked with a
  warn note while the engine is online. Warn when input and output share a device.
- MIDI: MIDI input list, Refresh MIDI, mappings table (CC, Ch, Target, Remove),
  "Learn new mapping" which arms learn; the next CC maps to the parameter last
  touched in any plug-in editor. Note: "Armed mappings apply normalized values
  only."
- Plug-ins: Rescan plug-ins, last scan time, catalog and quarantine counts, the
  catalog table (Plug-in, Vendor, State, Allow retry for quarantined entries) and
  the note that Allow retry clears only that plug-in's failure history.
- Diagnostics: per-rack table (Worker state, Restarts, Deadline misses,
  Rejections, State captured, Retry restart or Preload action) and engine rows
  (Callback load, Deadline misses, Aggregate device).

## 8. Model, protocol and data-plane changes

Sidechain (new):

- `sp-model`: add to `PluginSlot` a `#[serde(default)] sidechain: Option<SlotSidechain>`
  with `enum SlotSidechain { PhysicalInput(PhysicalChannels), RackOutput(RackId) }`.
  Validation: a physical pair must be stereo; a rack source must exist and must not
  be the slot's own rack; no cycles are needed to check because rack sources are
  one block delayed (see below). Bump the document schema and add the migration.
- Shared memory: each rack bank gains an aux input region of two channels per
  slot that declares a sidechain. Fixed layout, sized at rack build time like the
  main channels.
- Engine: physical-input sidechains are copied from the callback input buffer in
  the same block (sample aligned). Rack-output sidechains are copied from the
  source rack's previous-block output (one buffer of latency). Document this in
  `docs/realtime-safety.md`.
- Worker / `sp-vst3`: activate the plug-in's aux input bus when a sidechain is
  set and feed it from the aux region. Report whether a plug-in has an aux bus in
  the scan descriptor so the UI can disable the menu item.
- This removes "no sidechains" from the original alpha scope.

Editor preview (new):

- Control protocol: add `CaptureEditorPreview` (request) returning a PNG payload,
  chunked with the existing state-stream mechanism. The worker captures its own
  editor window with a window-list image call (own-process windows do not trigger
  the screen-recording prompt; verify on the target macOS versions), downscales to
  320×200 and encodes PNG on its main thread between event pumps. Never on the
  audio thread.
- Cadence: while an editor is open, capture every 2 s if any parameter changed;
  once more when the editor closes. Explicit Save writes the newest picture per
  instance into the package. Autosave does not.
- Package: `plugin-state/<instance-id>/preview.png`, declared in `metadata.json`
  with byte count and capture time, committed with the manifest like the state
  streams. Load shows stored pictures before any worker starts, including for
  missing plug-ins.
- Host: keep the newest picture per instance in memory as an egui texture;
  re-upload only when a new capture arrives.

Removed:

- `main_output_meter`, the master strip and the aggregate output meter path.
- The generic parameter editor UI, `parameters` grid, `selected_slot` editor
  state, the bottom rack detail pane and the settings drawer in `apps/superposition`.
- The component gallery screen (⌘G) from the product build; its fixtures may stay
  for tests.

Unchanged and reused: session package format for state, recovery markers and the
recovery offer, scene model and recall, MIDI learn controller, worker health and
recovery states, rack gain/mute/bypass control queue, meter ballistics.

## 3a. Pages

A page is a named set of racks, saved in the session. It only chooses which columns
the show screen shows; routing and audio never change. A rack may be on several
pages, and the columns keep session rack order. Up to 16 pages.

The page line (32 px, under the head and any fault line) appears once a page exists
or the session has more than eight racks: `Page` in dim, `All racks`, each page as
`n Name`, then `+ page`. The shown page is inverted. Right-click a page for Rename…
and Remove page (its racks stay in the session).

The rack menu gains Pages…: a popover with a check row per page and "New page with
this rack". On a page, a new rack joins that page, and Move left/right step past the
racks the page hides. Removing a rack takes it off every page.

## 9. Keyboard

| Keys | Action |
| --- | --- |
| 1–8 | recall scene |
| ⌘S | save |
| ⌘, | toggle setup page |
| ⌘⇧C | capture scene |
| ⌘← ⌘→ | move selected rack past its visible neighbour |
| ⌘1–⌘9 | show page 1–9 |
| ⌘0 | show all racks |
| ↑ ↓ | move slot selection within the selected rack |
| ↩ | open the selected slot's editor |
| B | bypass the selected slot |
| Esc | close the topmost overlay |

## 10. Suggested order of work

1. Theme: new tokens and IBM Plex Mono in `sp-ui`; drop the multi-accent palette.
   Keep the AA contrast tests and add the new pairings to them.
2. Show screen without previews: head readouts, columns with text slots, gain
   track, LED meters, toggles, foot line, fault line. Delete the detail pane,
   settings drawer, master strip and generic parameter editor.
3. Overlays: menus, route popover, plug-in picker, scene picker, modals.
4. Setup page.
5. Preview capture: protocol operation, worker capture, package file, host
   texture, tile states, hover popover.
6. Sidechain: model, shared-memory aux region, engine copy paths, worker aux bus,
   popover and caption marker.
