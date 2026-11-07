# Release process

Lines ships continuous **pre-alpha** builds. Releases are produced by the planning thread (orchestrator) as milestones land.

## Versioning
- `0.MINOR.PATCH`. MINOR bumps when a roadmap milestone is marked done; PATCH for fixes between milestones.
- Tag `vX.Y.Z` on `main`; a GitHub Release with notes and builds for Windows (`lines-vX.Y.Z-windows-x64.zip`: `lines.exe`, `LICENSE.md`, a short README), macOS (`lines-vX.Y.Z-macos-arm64.zip`) and Linux (`lines-vX.Y.Z-linux-x64.tar.gz`), plus `SHA256SUMS.txt` and `THIRD_PARTY_LICENSES.txt`.
- Releases are marked **pre-release** until 1.0.

## A release requires
1. `main` builds, `cargo test --workspace` passes, clippy `-D warnings` and `cargo fmt --check` clean (CI once roadmap 1b lands, locally until then).
2. Release notes: what changed, known gaps, and the published benchmarks once they exist (`docs/positioning.md`).
3. No vendor connector, proto, kit, credential, account ID, or licensed data in the tree or the artifact (decisions 0010, 0014).
4. **Live trading cannot be enabled in any public build.** Only mock, replay, Coinbase public data and paper trading ship.

## Always stops for the owner (never done autonomously)
- Anything involving money, contracts, vendor agreements, or legal terms (including license changes).
- Enabling live order routing, or connecting to any production trading system.
- Publishing outside GitHub (package registries, websites, app stores), code signing, notarization.
- Deleting branches, tags, releases or history; force-pushing `main`.
- Changing accepted decisions (proposals are fine; acceptance is the owner's).
- Manual UI checks are the owner's: UI-heavy milestones ship with screenshot evidence and are listed as "awaiting owner check" in the notes.
