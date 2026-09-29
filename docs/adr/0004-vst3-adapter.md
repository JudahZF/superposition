# ADR 0004: VST3 adapter boundary

- **Status:** Accepted
- **Date:** 2026-07-13

## Context

The initial implementation needs one constrained plug-in format without allowing SDK-specific types and rules to leak into the engine or session model.

## Decision

Start with VST3 behind `sp-vst3`. The adapter owns SDK translation for processing, buses, events/MIDI, parameters, state, and editor requests, and runs in the rack worker.

## Consequences

The core stays format-neutral and failures remain worker-local. AU/AAX and universal plug-in compatibility are not alpha commitments. This ADR does not claim a working adapter today.
