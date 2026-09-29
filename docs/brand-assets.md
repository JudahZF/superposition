# Brand assets and design system

## Source and status

The sole supplied reference is a raster Quanta Sound brand board kept outside this repository. It depicts the Quanta Sound wordmark/logo, palette, type guidance, icon style, UI direction, imagery, voice, and texture examples. It is **reference material only**: it has no recorded author/license/assignment metadata, and is not a releasable source asset.

No brand image has been copied into the project. Do not extract, trace, recolor, or publish the mark from the PNG until provenance and an approved source package are recorded.

## Approved interface tokens

The application chrome follows the film-strip brief in [ui-design.md](ui-design.md), not the brand board. `crates/sp-ui/src/design/tokens.rs` is the sole canonical source for colour values, spacing, and fixed dimensions; the renderer resolves every colour through one token bridge, and a source test fails if a palette literal appears anywhere else.

| Token | Value | Use |
| --- | --- | --- |
| `Base` | `#121315` | window background |
| `Text` | `#D9DAD6` | primary text, lit meter segments, inverted-selection fill |
| `Dim` | `#7C8088` | labels, secondary text, tick marks |
| `Faint` | `#3A3E45` | empty slots, unlit borders, disabled text, secondary text on an inverted fill |
| `Hairline` | `#26292E` | column dividers, head and foot rules |
| `SegmentOff` | `#24272C` | unlit meter segment |
| `HoverFill` | `#1B1D21` | row hover in lists |
| `Warn` | `#D9A21B` | LDG and REC tokens, mute lit, route warnings |
| `Fault` | `#E5484D` | MIS and FLT tokens, top two meter segments, fault line |
| `Info` | `#7FB0FF` | bypass lit, sidechain marker, mapped CCs |
| `FaultTint` | `#1A1214` | fault line background |

Warn, fault, and info always accompany a text state token (RDY, LDG, BYP, MIS, FLT, REC, UNL); colour never carries meaning alone. `crates/sp-ui/src/design/theme.rs` lists every foreground/background pairing the renderer draws and tests each against WCAG AA (4.5:1 for text, 3:1 for boundaries). Faint text is reserved for disabled and decorative content, and hovered list rows promote secondary text to `Text` because `Dim` on `HoverFill` falls below 4.5:1.

## Application-UI fonts

The app UI embeds IBM Plex Mono Regular and Medium from `crates/sp-ui/assets/fonts/`, downloaded from the IBM Plex repository. It is licensed under the SIL Open Font License 1.1; the license text is stored alongside the payloads (`OFL-IBMPlexMono.txt`) and permits app embedding and redistribution. Plex Mono lacks a few symbols the UI uses (⌘, ⇧); egui's bundled Hack and icon font supply them as fallbacks. The embedded UI fonts are separate from the release asset manifest below: the manifest's font entries (with hashes and approval records) still gate `cargo xtask bundle --profile release` for production payloads.

## Release assets and provenance

`packaging/resources/release.json` is the release asset manifest. It is deliberately unresolved: it names the expected production icon, vector logo, and IBM Plex Mono font payloads without providing substitute files, hashes, or approvals. `cargo xtask bundle --profile release` refuses to proceed until this manifest includes all of the following:

1. Approved production logo and `.icns` icon exports.
2. Licensed app-embedding font files and their exact SHA-256 hashes.
3. An approved fault-red semantic token value.
4. Per-resource source, license, rights holder, approval owner, and approval reference.
5. Named approval owner and reference for the logo, icon, fonts, fault-red token, and provenance set.

The manifest is staged by hash into the app bundle; it is not a place to copy a raster board, invent a logo, or claim an unverified license. Until these records are available, use text-only `Superposition`/`Quanta Sound` labels in developer material and system fonts or properly licensed substitutions. The displayed imagery, patterns, icon set, screenshot components, font files, source color profiles, SVG/vector marks, clearspace measurements, and copyright/trademark permissions remain unverified.
