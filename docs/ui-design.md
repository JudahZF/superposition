# Superposition UI design: overview and rack view

Status: implemented 2026-11-05. It replaces the film-strip design of 2026-09-25 (see
this file's history). The style follows T3 Code's dark theme; the layout follows Waves
SuperRack's Overview and Rack windows and its hot plug-ins panel.

The code is in `apps/superposition/src/ui/`: `show.rs` (top bar, fault banner, scene bar,
shared rack controls), `overview.rs`, `rack_view.rs`, `overlays.rs`, `setup.rs`,
`previews.rs`, and `streamed.rs` (streamed and docked editor windows). Tokens are in
`crates/sp-ui/src/design/`.

## 1. Decisions

1. The app is a plug-in host, not a mixer. There is no master bus. Each rack's output pair
   goes straight to the device.
2. All parameter editing happens in the plug-in's own editor. Scenes and MIDI mappings
   still read and write normalized values through the worker parameter protocol.
3. Every rack has an audio input, mono or stereo. The UI never offers a "no input" rack.
   The model keeps `RackChannelRoute.input: Option<..>` for older sessions, but the UI
   always writes `Some`.
4. Plug-ins can take a sidechain from a physical input pair or from another rack's
   post-fader output (section 10).
5. Each slot shows a small picture of its editor as it last looked (section 10).
6. Style: T3 Code's dark theme. Near-black surfaces, cards with hairline borders and 10 px
   corners, one blue accent for selection and primary actions, Inter for text and IBM Plex
   Mono for numbers, sentence case everywhere. A state always shows as a word beside a
   coloured dot, never colour alone. Still ruled out: neon or saturated meter colours,
   uppercase labels, ASCII widgets such as `[====|--]`, and run-on status lines.
7. Two views of the racks, as in SuperRack. The overview shows every rack as a narrow strip.
   The rack view shows one rack in detail, with its plug-in's editor in the centre pane.
8. Layout targets: 1920×1080 at 1× scale, and a 1512×982 laptop. Nothing scrolls
   vertically. The overview scrolls sideways when strips do not fit.
9. A session holds up to 64 racks, 16 pages, and 12 hot plug-ins.
10. Only meters animate. Hover and popover fades are off.

## 2. Tokens

`crates/sp-ui/src/design/tokens.rs` is the only source of colours, spacing, radii, and fixed
sizes. [brand-assets.md](brand-assets.md) lists the 17 colours. The renderer resolves every
value through `ui/style.rs`, and source tests fail on raw colours or literal sizes.

Radii: 4 px small marks and thumbnails, 6 px controls (buttons, chips, rows, toggles),
10 px cards and popovers, 14 px the scene bar, full for pills, dots, and scene pads.

Type (12 px text on a 16 px line):

| Role | Face | Size | Use |
| --- | --- | --- | --- |
| Body | Inter 400 | 12 | most text |
| Strong | Inter 600 | 12 | plug-in and rack names |
| Title | Inter 600 | 13 | pane and popover titles |
| Label | Inter 400 | 11 | secondary labels, captions |
| Wordmark | Inter 600 | 14 | the wordmark |
| Mono | Plex Mono 400 | 11 | gains, routes, timings, counts, key hints |
| MonoSmall | Plex Mono 400 | 10 | meter labels, key boxes |

States, each a word with a dot of its colour: Ready (success), Loading and Recovering
(warn), Bypassed and Not loaded (dim), Missing and Failed (fault). An open editor shows
"Editor open" with a fault-colour dot.

Spacing is a 4 px unit. The window gutter and card padding are 12 px; cards sit 6–8 px
apart.

## 3. Top bar (52 px)

Left to right: the wordmark; the session name with `· unsaved` while dirty; the rack
selector; the tabs Overview, Rack, Setup. At the right: the latest status message (dim,
truncated), an engine pill (dot and `Online · <device>`), mono readouts (rate, buffer,
latency, load, misses, and restarts in warn colour when any worker restarted), then
Start/Stop engine (outline) and Save ⌘S (primary).

The rack selector, `‹ 01 Vox ›`, steps through the racks the current page shows. Click the
name for the rack menu; double-click to rename (the rack view renames here, the overview in
the strip header).

## 4. Overview

A 184 px page rail, then one 124 px strip per shown rack, then a dashed "Add rack" strip
while fewer than 64 racks exist. Thirteen strips fit across 1920 px; more scroll sideways.

Page rail: "All racks", then each page as `n Name` with its rack count and one small level
bar per rack, then "+ New page". The shown page is accent-filled. Right-click a page for
Rename… and Remove page. The rail replaces the old page line.

A strip, top to bottom, follows its signal:

1. Header: `01` in mono dim and the rack name. Click selects, double-click renames,
   right-click opens the rack menu. The selected rack's card has the accent ring.
2. Route chips `In 1` and `Out 1-2`, warn-coloured if the device lacks a channel. Click
   opens the route popover.
3. Eight slot rows: thumbnail, name, state dot. Hover shows the full picture beside the
   row. Click opens the plug-in in the rack view; right-click opens the slot menu. The first
   empty row is a dashed "+ Add plug-in"; later empty rows are dashed and blank so every
   strip shares one rhythm.
4. Meters (in, L, R) beside a vertical gain fader, filling the remaining height, with the
   gain in mono below.
5. Mute (warn tint when lit) and Bypass (info tint when lit).
6. The rack state, `ready/total`, and `muted` in warn colour.

An empty session or page shows a centred message beside the "Add rack" strip.

## 5. Rack view

The selected rack, left to right: input strip (92 px), chain (260 px), editor pane (the
rest), output strip (116 px), and the hot plug-ins rail (176 px). At 1920 px the pane is
about 1200 px wide; at 1512 px about 800 px.

- Input strip: the `In` chip (route popover), a tall input meter, and rack Bypass.
- Chain: "Chain" and `n of 8`, then up to eight rows with a 72 px thumbnail, the name, a
  sidechain chip, and the state with the picture's age (`Ready · 2 min`) or "Editor open".
  Click opens the editor; right-click opens the slot menu; ↑↓ move the selection.
- Editor pane: a header with the plug-in name, `Slot n · Vendor`, and at the right the
  sidechain chip (opens the sidechain popover), Bypass, Pin/Unpin, and on Linux Pop out or
  Dock. The body shows the docked editor (section 6). Otherwise it shows the slot's last
  picture, fitted and at most twice its captured size, with a small card: "The editor is
  closed." and Open editor ↩; "Opening the editor…"; "The editor is open in its own
  window." and Dock here (Linux); or the missing-plug-in note. With no slot selected it
  says how to fill it, with Add plug-in for an empty rack.
- Output strip: the `Out` chip, L and R meters beside the gain fader, the gain, and Mute.
- Hot plug-ins: up to 12 pinned plug-ins from any rack, each a picture with its name and
  rack. Click opens it in the rack view; hover shows × to unpin. "+ Pin selected" pins the
  selected slot. The slot menu and the pane header also pin and unpin. Removing a slot or
  its rack unpins it. The pins are saved as `hot_plugins` (see
  [session-format.md](session-format.md)).

With no racks, the view says so and offers Add rack ⌘N.

## 6. Editors

Workers own their editor windows ([ADR 0005](adr/0005-worker-owned-native-windows.md)).

Linux: editors draw on the engine's Xvfb and stream into the app. Opening a slot from the
overview, the chain, a hot plug-in, or ↩ shows the rack view and docks that editor in the
pane: top-centred, shrunk to fit and never enlarged. One editor is docked at a time. It
closes when the selection leaves its slot or the rack view hides, and stays hidden for a
second while its window goes away. Pop out lets it float as its own window and keeps it
open; Dock brings it back. A docked editor takes pointer input only where nothing is drawn
over it, so popovers and menus above it work. Pointer positions map to editor pixels at the
docked scale.

macOS: editors are native windows. Opening a slot shows the rack view and opens or raises
the editor beside the app window; the pane shows its picture.

## 7. Fault banner and scene bar

Fault banner: under the top bar, a rounded fault-tinted card with "Fault", the message,
and Dismiss Esc.

Scene bar: a floating card at the bottom. "Scene", then each scene as a pill `n Name`
(accent while active, faint while the engine is offline); Fade `− 250 ms +` stepping 50 ms
from 0 to 10 000; Update and Edit. At the right: "Callback" with twelve bars of recent
callback load (warn over 90 %), then Capture ⌘⇧C.

## 8. Overlays

Popovers and modals are rounded surface cards with a soft shadow, positioned near what
opened them. A transparent scrim closes popovers on an outside click; modals ignore it. Esc
closes the topmost overlay, in this order: a menu, popover, picker, or modal; the scene
editor; an open editor; the fault banner; the setup page.

Rack menu: Rename, Route…, Pages…, Move left ⌘←, Move right ⌘→, Remove rack…. Move
items are disabled at the ends of the shown racks.

Slot menu: Open editor ↩ (disabled when missing), Pin to hot plug-ins or Unpin, Bypass or
Enable B, Sidechain… (or "No sidechain input" when the plug-in has no aux bus), Move up,
Move down, Remove plug-in.

Route popover: `Route for <rack>` with the output device. Input with Mono and Stereo, a jack
grid of the device's input channels, then the output pair grid. Selected jacks are accent;
channels beyond the device are faint. A warn line appears when the route uses channels the
device lacks. Changing a route re-routes at the next block, without a restart.

Sidechain popover: None; a physical stereo input pair; or another rack's post-fader
output. Rack sources arrive one buffer late; physical inputs are sample aligned. Changing
the sidechain reloads the rack like a topology edit.

Plug-in picker (560 px): `Add plug-in to <rack>` with `slot n of 8`, a search field, and
the catalog grouped by vendor with an accent keyboard cursor (↑↓ ↩ Esc). Quarantined and
non-loadable entries are faint with a reason.

Scene capture and edit (640 px): name, filter, the automatable parameters grouped by rack
and plug-in with check boxes and values, and `n of 256 parameters`.

Modals: Stop the audio engine?; Remove <rack>?; Recover the last session?

## 9. Setup page

Setup (⌘,) replaces the views; the top bar and scene bar stay. A 200 px tab rail (Audio,
MIDI, Plug-ins, Diagnostics) and a page limited to 900 px.

- Audio: output and input devices, buffer size, Refresh devices. Locked with a warn note
  while the engine is online.
- MIDI: MIDI input, mappings table, Learn new mapping.
- Plug-ins: Rescan plug-ins, scan time, catalog and quarantine, Allow retry.
- Diagnostics: per-rack worker state, restarts, deadline misses, and engine rows.

## 10. Editor pictures and sidechains

Editor pictures: the worker captures its editor at 320×200 PNG while it is open (every 2 s
if a parameter changed, and once as it closes). An explicit Save writes the newest picture
per instance to `plugin-state/<instance-id>/preview.png`; autosave does not. A loaded
session shows stored pictures before any worker starts, including for missing plug-ins.
Bypassed slots draw their picture at 40 % opacity; unavailable ones in grayscale at 35 %.

Sidechains: `PluginSlot.sidechain` is a physical stereo pair or another rack's ID. Physical
inputs are copied from the callback input in the same block; rack sources come from the
source rack's previous block. See [realtime-safety.md](realtime-safety.md).

## 11. Keyboard

| Keys | Action |
| --- | --- |
| 1–8 | recall scene |
| ⌘S | save |
| ⌘, | toggle the setup page |
| ⌘N | add a rack |
| ⌘⇧C | capture a scene |
| ⌘← ⌘→ | move the selected rack past its shown neighbour |
| ⌘1–⌘9 | show page 1–9 |
| ⌘0 | show all racks |
| ↑ ↓ | move the slot selection in the selected rack |
| ↩ | open the selected slot in the rack view |
| B | bypass the selected slot |
| Esc | close the topmost overlay |

Keys typed into a streamed editor go to that editor, not to these shortcuts.
