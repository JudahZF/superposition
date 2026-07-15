# Session format

## Goals

A session will be a versioned, inspectable project container that preserves host-owned topology and makes unresolved external dependencies explicit. It is not a promise of byte-identical plug-in recall across versions, architectures, vendors, or machines.

## Host-owned structure

The durable model records a format version, project metadata, rack order and graph edges, plug-in identity and discovery facts, rack bypass/gate policy, routing, parameter metadata/values, **named parameter scenes**, MIDI mappings, and references to opaque plug-in state. `sp-session::SessionDocument` embeds `sp_model::Session` under `model` inside atomic session packages. IDs are stable opaque identifiers, not display names. Unknown fields must be retained where practical so newer writers do not cause needless loss.

Each migration is directional, versioned, and tested with fixtures. Parsers must cap sizes, nesting, strings, and collection lengths before allocating unbounded memory. Corrupt or unknown data must fail a rack or component with a useful diagnostic rather than execute a plug-in during load.

## Scenes versus opaque state

Parameter scenes are host-owned snapshots of normalized, addressable parameters. They are the supported target for automation, comparison, selective restore, and user-visible recall. A scene records the plug-in identifier, parameter identity, normalized value, optional display context, and capture schema.

Opaque plug-in state is a bounded byte artifact owned by the plug-in adapter. The host preserves it only for whole-plug-in restore and never interprets, diffs, merges, or assumes portability of it. Restoration applies compatible parameter scenes first where possible, then offers opaque state only to the matching adapter/plugin in the worker. A failed opaque restore keeps the rack gated and reports the reason.

## Compatibility policy

Sessions declare their writer and format versions. Readers accept the supported compatibility window, preserve unavailable racks as inert placeholders, and surface substitutions as user decisions. Automatic replacement never silently maps a plug-in merely because its display name matches. See [ADR 0006](adr/0006-parameter-scenes-and-opaque-state.md).
