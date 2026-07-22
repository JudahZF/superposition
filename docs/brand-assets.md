# Brand assets and design system

## Source and status

The sole supplied reference is `/Users/judahfuller/Code/quanta/brandpack.png`, a raster brand board reviewed for Phase 0. It depicts the Quanta Sound wordmark/logo, palette, type guidance, icon style, UI direction, imagery, voice, and texture examples. It is **reference material only**: it is outside this repository, has no recorded author/license/assignment metadata, and is not a releasable source asset.

No brand image has been copied into the project. Do not extract, trace, recolor, or publish the mark from the PNG until provenance and an approved source package are recorded.

## Approved interface tokens

The executable design-token module is the sole canonical source for color values and font-family names. Use these source-derived mappings for UI implementation and maintenance rather than copying raw brand values:

| Planning token | Runtime token | Intended use |
| --- | --- | --- |
| `color-cyan` | `CYAN` | primary emphasis / online |
| `color-blue` | `BLUE` | active / secondary emphasis |
| `color-lime` | `LIME` | signal / success accent |
| `color-deep-navy` | `CANVAS` | base canvas |
| `color-charcoal` | `PANEL` | persistent panels |
| `color-slate` | `RAISED` | raised cards and dialogs |
| `color-steel` | `STEEL` | borders / disabled structure |
| `color-light` | `PRIMARY_TEXT` | primary text |
| `color-text-dim` | `SECONDARY_TEXT` | secondary text |

The approved gradients progress from cyan to blue and from blue to lime. Use the semantic interface/display and monospace font-family roles from the token module, subject to font licensing. The style is dark, precise, minimal, high-contrast, and technical; accessibility contrast and non-color status cues remain mandatory.

## Application-UI fonts

The app UI embeds Sora (Regular/Medium/SemiBold/Bold) and Space Mono (Regular) from `crates/sp-ui/assets/fonts/`, downloaded from Google Fonts. Both families are licensed under the SIL Open Font License 1.1; the license texts are stored alongside the payloads (`OFL-Sora.txt`, `OFL-SpaceMono.txt`) and permit app embedding and redistribution. These embedded UI fonts are separate from the release asset manifest below: the manifest's font entries (with hashes and approval records) still gate `cargo xtask bundle --profile release` for production payloads.

## Release assets and provenance

`packaging/resources/release.json` is the release asset manifest. It is deliberately unresolved: it names the expected production icon, vector logo, and Sora/Space Mono font payloads without providing substitute files, hashes, or approvals. `cargo xtask bundle --profile release` refuses to proceed until this manifest includes all of the following:

1. Approved production logo and `.icns` icon exports.
2. Licensed app-embedding font files and their exact SHA-256 hashes.
3. An approved fault-red semantic token value.
4. Per-resource source, license, rights holder, approval owner, and approval reference.
5. Named approval owner and reference for the logo, icon, fonts, fault-red token, and provenance set.

The manifest is staged by hash into the app bundle; it is not a place to copy a raster board, invent a logo, or claim an unverified license. Until these records are available, use text-only `Superposition`/`Quanta Sound` labels in developer material and system fonts or properly licensed substitutions. The displayed imagery, patterns, icon set, screenshot components, font files, source color profiles, SVG/vector marks, clearspace measurements, and copyright/trademark permissions remain unverified.
