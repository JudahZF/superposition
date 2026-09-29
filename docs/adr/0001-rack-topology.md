# ADR 0001: Rack topology

- **Status:** Accepted
- **Date:** 2026-07-13

## Context

Third-party plug-ins must be contained without requiring arbitrary process-to-process graph semantics at the outset.

## Decision

Model a project as ordered racks connected by declared engine graph edges. A rack owns its plug-in chain, routing configuration, parameter scenes, and recovery state. It is the unit of isolation, restart, and fallback.

## Consequences

The topology is understandable and limits fault scope. Arbitrary plug-in graph rewiring, cross-rack memory access, and universal routing are deferred. See [architecture](../architecture.md).
