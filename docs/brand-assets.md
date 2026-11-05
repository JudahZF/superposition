# Brand assets and design system

## Source and status

The sole supplied reference is a raster Quanta Sound brand board kept outside this repository. It depicts the Quanta Sound wordmark/logo, palette, type guidance, icon style, UI direction, imagery, voice, and texture examples. It is **reference material only**: it has no recorded author/license/assignment metadata, and is not a releasable source asset.

No brand image has been copied into the project. Do not extract, trace, recolor, or publish the mark from the PNG until provenance and an approved source package are recorded.

## Approved interface tokens

The application chrome follows the rack UI brief in [ui-design.md](ui-design.md), not the brand board. Its palette is T3 Code's dark theme. `crates/sp-ui/src/design/tokens.rs` is the sole canonical source for colour values, spacing, radii, and fixed dimensions; the renderer resolves every colour through one token bridge, and a source test fails if a palette literal appears anywhere else.

| Token | Value | Use |
| --- | --- | --- |
| `Base` | `#0A0A0A` | window background |
| `Surface` | `#111111` | cards, rails, popovers, the scene bar |
| `Text` | `#F5F5F5` | primary text, lit meter segments, fader thumbs |
| `Dim` | `#818181` | labels, secondary text, tick marks |
| `Faint` | `#4D4D4D` | disabled text, empty-slot outlines |
| `Hairline` | `#232323` | card borders and dividers |
| `SegmentOff` | `#262626` | unlit meter segments, tracks, neutral chips |
| `HoverFill` | `#1A1A1A` | row and button hover |
| `Accent` | `#346BF1` | selection rings, the active tab, scene, and page, primary buttons |
| `AccentText` | `#FFFFFF` | text on an accent fill |
| `Success` | `#10B981` | the ready dot |
| `Warn` | `#FE9A00` | loading and recovering states, mute lit, route warnings |
| `WarnTint` | `#312108` | behind lit warn controls and chips |
| `Fault` | `#FB414A` | missing and failed states, top meter segments, the fault banner |
| `FaultTint` | `#301214` | fault banner background |
| `Info` | `#3B82F6` | bypass lit, sidechain chips, mapped CCs |
| `InfoTint` | `#111C2D` | behind lit info controls and sidechain chips |

States always show as a word beside a coloured dot (Ready, Loading, Bypassed, Missing, Failed, Recovering, Not loaded); colour never carries meaning alone. `crates/sp-ui/src/design/theme.rs` lists every foreground/background pairing the renderer draws and tests each against WCAG AA (4.5:1 for text, 3:1 for boundaries). Faint text is reserved for disabled and decorative content, and hovered rows promote secondary text to `Text` because `Dim` on `HoverFill` falls below 4.5:1. `Hairline` card borders are decorative; selection uses the accent ring, which meets 3:1.

## Application-UI fonts

The app UI embeds Inter Regular and SemiBold and IBM Plex Mono Regular from `crates/sp-ui/assets/fonts/`. The Inter files are static 400 and 600 instances of Inter 4.1's variable font (from the nixpkgs `inter` package, cut with fontTools' `varLib.instancer`); Plex Mono comes from the IBM Plex repository. Both are licensed under the SIL Open Font License 1.1; the license texts are stored alongside the payloads (`OFL-Inter.txt`, `OFL-IBMPlexMono.txt`) and permit app embedding and redistribution. Inter sets interface text; Plex Mono sets numbers, routes, and key hints. egui's bundled Hack and icon fonts supply any symbol both lack. The embedded UI fonts are separate from the release asset manifest below: the manifest's font entries (with hashes and approval records) still gate `cargo xtask bundle --profile release` for production payloads.

## Release assets and provenance

`packaging/resources/release.json` is the release asset manifest. It is deliberately unresolved: it names the expected production icon, vector logo, and Inter and IBM Plex Mono font payloads without providing substitute files, hashes, or approvals. `cargo xtask bundle --profile release` refuses to proceed until this manifest includes all of the following:

1. Approved production logo and `.icns` icon exports.
2. Licensed app-embedding font files and their exact SHA-256 hashes.
3. An approved fault-red semantic token value.
4. Per-resource source, license, rights holder, approval owner, and approval reference.
5. Named approval owner and reference for the logo, icon, fonts, fault-red token, and provenance set.

The manifest is staged by hash into the app bundle; it is not a place to copy a raster board, invent a logo, or claim an unverified license. Until these records are available, use text-only `Superposition`/`Quanta Sound` labels in developer material and system fonts or properly licensed substitutions. The displayed imagery, patterns, icon set, screenshot components, font files, source color profiles, SVG/vector marks, clearspace measurements, and copyright/trademark permissions remain unverified.
