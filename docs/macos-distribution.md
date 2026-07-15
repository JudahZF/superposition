# macOS distribution

## Product shape

The planned product is a signed, notarized macOS app bundle plus narrowly scoped helper executables. The host app launches the worker/scanner through a versioned, authenticated control contract and verifies that the installed helper belongs to the same release. Helpers are not a generic plug-in execution service and do not expose a public IPC endpoint.

## Signing, notarization, and updates

Release builds will use Developer ID signing with hardened runtime, a reproducible archive procedure, notarization, and staple verification before distribution. The release pipeline must preserve bundle identifiers, team identity, entitlements, helper hashes, minimum macOS version, and toolchain provenance. Updates must be signed and reject downgrade or mismatched helper payloads.

Local debugging may use separate development identities and clearly marked unsigned artifacts; it must not normalize shipping development entitlements.

## Entitlements and plug-in validation

Checked-in entitlement templates live under `packaging/entitlements/`:
- `app.entitlements` — host app; library validation remains enabled
- `helper.entitlements` — worker/scanner helpers; library-validation exception is opt-in

`cargo xtask bundle` builds `target/phase9/Superposition.app` with nested `Contents/Helpers/{sp-plugin-worker,sp-plugin-scanner}`, copies `packaging/macos/Info.plist`, and verifies entitlement templates. Pass `--sign` for local ad-hoc codesign (helpers first, then the outer app). Apple notarization and stapling remain a manual follow-up: they require Developer ID credentials that are intentionally not stored in this repository.

The only entitlement exception under consideration is a **helper-only library-validation entitlement** when required to load third-party native plug-in code. It is not granted to the host app, is not a blanket sandbox escape, and is justified only after the feasibility prototype documents the exact loading path. Network, microphone, camera, accessibility, automation, and broad filesystem entitlements are not implied.

Plug-in discovery and loading are untrusted operations: scanning occurs in the scanner helper; host decisions rely on recorded discovery facts and user selection. The worker boundary supports recovery but does not make arbitrary native code safe.

## CI runner limitation

The CI workflow targets GitHub-hosted `macos-14`, which is Apple Silicon where that runner class is available to the repository. Availability varies by GitHub plan/visibility and organization policy. If it is unavailable, use a managed or self-hosted Apple Silicon runner with the same toolchain; do not silently replace feasibility or release checks with Intel. CI alone cannot prove real-time behavior: Phase 1 timing gates run on specified physical Apple Silicon hardware. See [ADR 0008](adr/0008-helper-library-validation-entitlement.md).
