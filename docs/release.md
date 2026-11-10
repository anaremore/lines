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

## Update signing key (decision 0031; owner only)
In-app updates run only what the project signed. Until the steps below are done, every build reports "this build doesn't update itself" and makes no update request.
1. **Generate** the key pair on a machine you control: `minisign -G -p lines-update.pub -s lines-update.key` (choose a strong password). Never commit, paste or upload the secret key file.
2. **CI secret:** add the secret key file's contents and its password as repository secrets (`UPDATE_SIGNING_KEY`, `UPDATE_SIGNING_PASSWORD`). The release workflow's signing step (a separate tier C change, drafted for your approval) signs `lines.exe` with the trusted comment `lines <version> public pre-release x86_64-pc-windows-msvc` (exactly; the app refuses a signature naming another release) and writes `update.json` (version, channel, edition, and per target the file's URL, SHA-256 and the `.minisig` text).
3. **Public key:** commit the public key line (the second line of `lines-update.pub`) as `COMMITTED_KEY` in `crates/lines-app/src/update/verify.rs`. Builds from then on verify against it.
4. **Custody:** keep the secret key and its password offline in two places (e.g. a password manager and an encrypted backup). Anyone holding both can ship code that every installed Lines will run.
5. **Rotation** (a suspected leak, or a planned change):
   1. Generate a new pair and replace the CI secrets.
   2. Commit the new public key and release a version signed with the **old** key, so installed builds update to one that trusts the new key.
   3. From the next release on, sign with the new key only, and delete the old secret everywhere.
   4. After a leak, also say so in the next release's notes: builds older than the rotation release still trust the old key until they update.
6. **Official build:** its feed (a private release feed or Settings-enabled connectors) is still to be chosen; until then only public builds update themselves.
