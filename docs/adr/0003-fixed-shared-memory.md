# ADR 0003: Fixed shared-memory audio exchange

- **Status:** Accepted (Phase 0 target)
- **Date:** 2026-07-13

## Context

Callback safety rules prohibit allocation and blocking IPC, while worker processing needs bounded audio, events, and control data.

## Decision

Use preallocated, fixed-layout shared-memory slots negotiated before activation. Capacity covers the maximum supported frames, channels, events, and records. Sequence counters and acquire/release publication define ownership; queues remain bounded.

## Consequences

The callback has predictable work and no resizing path. Configurations beyond negotiated limits are rejected or reconfigured off-path. Overflow drops according to policy and signals diagnostics; it never blocks or expands capacity.
