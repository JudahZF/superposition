# macOS distribution

## Product shape

Live workers require Apple Silicon macOS 14.4 or later for public process-shared
request wait/wake support. The app bundle declares this minimum version.

The product is a signed, notarized macOS app bundle with narrowly scoped helper executables. The host app launches the worker/scanner through a versioned, authenticated control contract and verifies that an installed helper belongs to the same release. Helpers are not a generic plug-in execution service and do not expose a public IPC endpoint.

## Bundle profiles and resource manifests

`cargo xtask bundle` has two deliberately separate profiles:

- `local-dev` (the default) creates `target/bundle/local-dev/Superposition.app`. It is non-distributable and can remain unsigned or use `--sign` for ad-hoc signing. It must never be repackaged as a release.
- `release` creates `target/bundle/release/Superposition.app`. It is blocked before compilation until `packaging/resources/release.json` records every required brand approval, source hash, and provenance field.

Both profiles use a JSON resource manifest. Only entries declared in that manifest are copied to `Contents/Resources`; every staged entry is SHA-256 checked. The build additionally writes:

- `superposition-helper-manifest.json`, with the installed app/helper hashes and shared-memory/control protocol versions.
- `superposition-build-provenance.json`, with version, CFBundle build number, Git commit/dirty state, Rust compiler, and Xcode information.

`packaging/resources/release.json` intentionally contains unresolved entries for the production icon, logo, and licensed font files. It is a blocker, not a request to fabricate substitutes.

## Release procedure

After the release resource manifest has been completed and independently reviewed, run:

```bash
cargo xtask bundle \
  --profile release \
  --build-number 42 \
  --identity "Developer ID Application: Legal Organization (TEAMID)" \
  --notary-profile superposition-notary
```

The `--notary-profile` value names a preconfigured Keychain profile for `xcrun notarytool`; no Apple ID, app-specific password, API key, private key, or signing certificate is stored in this repository. The command signs worker/scanner helpers first, then the outer app, with the Developer ID identity, hardened runtime, and timestamp. It then verifies every embedded entitlement, performs `codesign --verify --deep --strict`, archives the app with `ditto`, submits to `notarytool --wait`, staples and validates the ticket, and performs `spctl --assess`.

A release invocation fails if any signing, entitlement, verification, notarization, stapling, or Gatekeeper assessment step fails. Do not replace those checks with `--deep` signing, an ad-hoc identity, a missing timestamp, or an unsigned archive.

## Entitlements and plug-in validation

Checked-in templates live under `packaging/entitlements/`:

- `app.entitlements` applies only to `Contents/MacOS/superposition` and explicitly leaves library validation enabled.
- `helper.entitlements` applies only to `Contents/Helpers/sp-plugin-worker` and `Contents/Helpers/sp-plugin-scanner` and explicitly enables `com.apple.security.cs.disable-library-validation`.

The release command checks the signed entitlement payload of each of those three binaries; it does not infer correctness from templates alone. Network, microphone, camera, accessibility, automation, and broad filesystem entitlements are not implied.

Plug-in discovery and loading are untrusted operations: scanning occurs in the scanner helper; host decisions rely on recorded discovery facts and user selection. The worker boundary supports recovery but does not make arbitrary native code safe.

## CI runner limitation

The CI workflow targets GitHub-hosted `macos-14`, which is Apple Silicon where that runner class is available to the repository. Availability varies by GitHub plan, visibility, and organization policy. If it is unavailable, use a managed or self-hosted Apple Silicon runner with the same toolchain; do not silently replace release checks with Intel. CI alone cannot prove real-time behavior: timing checks run on physical Apple Silicon hardware. See [ADR 0008](adr/0008-helper-library-validation-entitlement.md).
