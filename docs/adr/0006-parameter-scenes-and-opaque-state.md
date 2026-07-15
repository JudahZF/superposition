# ADR 0006: Parameter scenes and opaque state

- **Status:** Accepted (Phase 0 target)
- **Date:** 2026-07-13

## Context

Users need addressable recalls and automation, while plug-ins also expose proprietary whole-state blobs with uncertain portability.

## Decision

Persist host-owned, normalized parameter scenes separately from bounded opaque plug-in state. Scenes are the canonical host feature for automation, compare, selective recall, and user visibility. Opaque state is retained only for whole-plug-in restore through the matching adapter/worker.

## Consequences

The host does not parse, diff, merge, or promise portability of opaque data. Failed restoration keeps the rack gated with diagnostics; it does not silently substitute state.
