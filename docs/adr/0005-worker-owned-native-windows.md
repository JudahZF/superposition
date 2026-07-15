# ADR 0005: Worker-owned native editor windows

- **Status:** Accepted (Phase 0 target)
- **Date:** 2026-07-13

## Context

A plug-in editor can carry framework-specific ownership, thread-affinity, and lifetime assumptions that are unsafe to impose on the host UI process.

## Decision

The rack worker owns creation, lifetime, focus, and destruction of its native editor window. The host sends lifecycle/geometry requests over the control plane and presents a proxy relationship; it does not reparent or render inside the plug-in's native hierarchy.

## Consequences

A worker exit removes the editor and leaves its rack unavailable rather than risking host UI corruption. Integration is less seamless and requires macOS-specific feasibility testing.
