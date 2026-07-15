# ADR 0008: Helper-only library-validation exception

- **Status:** Accepted (Phase 0 target; implementation evidence required)
- **Date:** 2026-07-13

## Context

Third-party plug-in binaries may require a hardened-runtime library-validation exception, but applying that exception to the host would unnecessarily enlarge its attack surface.

## Decision

If testing proves it necessary, grant the library-validation entitlement only to the plug-in worker/helper that loads the plug-in. The host app does not receive it. The helper remains signed, version-matched, least-privilege, and limited to the rack protocol; the scanner remains a separate helper.

## Consequences

Loading may work for the targeted native code without weakening the host process to the same degree. The exception is not a general sandbox bypass and needs a release-time entitlement audit, exact-path evidence, and Apple Silicon feasibility results before shipping.
