# ADR 0002: One worker per rack with a gate

- **Status:** Accepted
- **Date:** 2026-07-13

## Context

A shared helper could let one faulty plug-in delay or crash unrelated work. The audio callback also needs a fixed response when a worker is late or unavailable.

## Decision

Launch one plug-in worker per rack. Place a supervisor-owned gate between the rack worker and the engine. The gate validates liveness, protocol generation, and block sequence, and closes for a deadline miss, malformed result, or worker loss.

## Consequences

Faults and restarts stay rack-local; process overhead and lifecycle complexity increase. The gate returns a deterministic fallback rather than waiting, so the design does not provide a transparent in-process path.
