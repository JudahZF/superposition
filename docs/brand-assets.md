# Brand assets and design system

## Source and status

The sole supplied reference is a raster Quanta Sound brand board kept outside this repository. It depicts the Quanta Sound wordmark/logo, palette, type guidance, icon style, UI direction, imagery, voice, and texture examples. It is **reference material only**: it has no recorded author/license/assignment metadata, and is not a releasable source asset.

No brand image has been copied into the project. Do not extract, trace, recolor, or publish the mark from the PNG until provenance and an approved source package are recorded.

## Approved interface tokens

The application chrome follows the film-strip brief in [ui-design.md](ui-design.md), not the brand board. `crates/sp-ui/src/design/tokens.rs` is the sole canonical source for colour values, spacing, and fixed dimensions; the renderer resolves every colour through one token bridge, and a source test fails if a palette literal appears anywhere else.

| Token | Value | Use |
| --- | --- | --- |
| `Base` | `#15181C` | window and rack-column background |
| `Surface` | `#1A1E23` | head and foot bars, selected rack column, scene pads, popovers, and modals |
| `Well` | `#101215` | preview slot fill, opaque caption bar, unlit meter, and text-field fill |
| `Raised` | `#1D2126` | button and toggle fill |
| `HoverFill` | `#252A31` | hovered button, pad, or list row |
| `Strong` | `#E7EAEE` | rack names, readout values, caption names, gain marker, and meter peak hold |
| `Text` | `#C9CED5` | body text |
| `Dim` | `#868E99` | labels, secondary text, tick labels, key hints, and gain track fill |
| `Faint` | `#4D5560` | disabled text, hovered preview ring, and hovered empty-slot border |
| `Hairline` | `#252A30` | column dividers, head and foot rules, and readout-cell dividers |
| `Groove` | `#2E353E` | empty-slot dashes, gain track, and scene pad border |
| `Outline` | `#353C45` | button, toggle, text-field, and popover borders |
| `Accent` | `#2BB3A3` | selection and focus only: selected rack bar, selected slot ring, active scene border, focus outline |
| `AccentFill` | `#1B4A46` | active scene pad fill, selected jack fill, and text selection background |
| `MeterLit` | `#AAB2BC` | meter fill below −6 dBFS |
| `Warn` | `#D9A21B` | LDG and REC tokens, lit Mute, route warnings, meter −6 to −3 dBFS, hot load bars |
| `Fault` | `#E6494D` | MIS and FLT tokens, the "Fault" label, the live dot, meter above −3 dBFS |
| `Info` | `#7FB0FF` | lit Bypass, sidechain markers, mapped CCs |
| `FaultTint` | `#24161A` | fault line background |

Warn, fault, and info always accompany a text state token (RDY, LDG, BYP, MIS, FLT, REC, UNL); colour never carries meaning alone. Accent marks selection and focus only and never means a state. Text on lit Mute (`Warn`) and lit Bypass (`Info`) is `Base`, and text on `AccentFill` is `Strong`. `crates/sp-ui/src/design/theme.rs` lists every foreground/background pairing the renderer draws and tests each against WCAG AA (4.5:1 for text, 3:1 for boundaries). Faint text is reserved for disabled content, and hovered list rows promote secondary text to `Text` because `Dim` on `HoverFill` falls below 4.5:1.

## Application-UI fonts

The app UI embeds Inter Regular, Medium, and SemiBold (rsms/inter v4.1) from `crates/sp-ui/assets/fonts/` for all chrome, and IBM Plex Mono Regular and Medium, downloaded from the IBM Plex repository, for monospace text such as diagnostics. Both families are licensed under the SIL Open Font License 1.1; the license texts are stored alongside the payloads (`OFL-Inter.txt` and `OFL-IBMPlexMono.txt`) and permit app embedding and redistribution. Inter and Plex Mono lack a few symbols the UI uses (⌘, ⇧); egui's bundled Hack and icon font supply them as fallbacks. The embedded UI fonts are separate from the release asset manifest below: the manifest's font entries (with hashes and approval records) still gate `cargo xtask bundle --profile release` for production payloads.

## Release assets and provenance

`packaging/resources/release.json` is the release asset manifest. It is deliberately unresolved: it names the expected production icon, vector logo, and IBM Plex Mono font payloads without providing substitute files, hashes, or approvals. `cargo xtask bundle --profile release` refuses to proceed until this manifest includes all of the following:

1. Approved production logo and `.icns` icon exports.
2. Licensed app-embedding font files and their exact SHA-256 hashes.
3. An approved fault-red semantic token value.
4. Per-resource source, license, rights holder, approval owner, and approval reference.
5. Named approval owner and reference for the logo, icon, fonts, fault-red token, and provenance set.

The manifest is staged by hash into the app bundle; it is not a place to copy a raster board, invent a logo, or claim an unverified license. Until these records are available, use text-only `Superposition`/`Quanta Sound` labels in developer material and system fonts or properly licensed substitutions. The displayed imagery, patterns, icon set, screenshot components, font files, source color profiles, SVG/vector marks, clearspace measurements, and copyright/trademark permissions remain unverified.
