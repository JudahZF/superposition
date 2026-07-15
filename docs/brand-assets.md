# Brand assets and design system

## Source and status

The sole supplied reference is `/Users/judahfuller/Code/quanta/brandpack.png`, a raster brand board reviewed for Phase 0. It depicts the Quanta Sound wordmark/logo, palette, type guidance, icon style, UI direction, imagery, voice, and texture examples. It is **reference material only**: it is outside this repository, has no recorded author/license/assignment metadata, and is not a releasable source asset.

No brand image has been copied into the project. Do not extract, trace, recolor, or publish the mark from the PNG until provenance and an approved source package are recorded.

## Approved interface tokens

The executable design-token module is the sole canonical source for color values and font-family names. Use these source-derived mappings for internal planning and future UI implementation rather than copying raw brand values:

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

## Gaps, fallbacks, and release blockers

Until source files arrive, use text-only `Superposition`/`Quanta Sound` labels in developer material and system fonts or properly licensed substitutions; do not invent a logo fallback. The displayed imagery, patterns, icon set, screenshot components, font files, source color profiles, SVG/vector marks, clearspace measurements, and copyright/trademark permissions are all unverified.

A public release is blocked on: written ownership/license and trademark permission; approved vector/raster logo exports and usage rules; font licenses and web/app embedding terms; provenance and usage rights for imagery/icons/textures; accessibility review of the final token implementation; and an asset manifest with hashes, source, license, and approval owner. See [ADR 0009](adr/0009-brand-design-system.md).
