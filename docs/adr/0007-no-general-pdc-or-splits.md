# ADR 0007: No general PDC or arbitrary splits initially

- **Status:** Accepted (Phase 0 target)
- **Date:** 2026-07-13

## Context

General plug-in delay compensation and arbitrary split/merge routing multiply scheduling, buffering, alignment, and recovery cases across process boundaries.

## Decision

Phase 1 excludes general PDC and arbitrary graph splits/merges. The engine accepts only the declared rack topology and fixed-buffer contract. Any narrowly supported latency or routing behavior must be explicit rather than inferred as a general facility.

## Consequences

The first feasibility scope is testable and bounded, but projects needing broad DAW-style routing or automatic compensation are unsupported. Adding either feature requires a new ADR and timing/recovery evidence.
