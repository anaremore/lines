# 0031: In-app updates, signed, never during a trade

**Status:** Accepted (owner accepted 2026-11-09; the owner-only choices under "Open" are still to be made)

**Context:** Lines ships often: three pre-releases on 2026-11-09 alone. Updating today means downloading a zip and replacing files by hand. The owner wants updates applied inside the app with no reinstall. Lines is a trading terminal, so an update must never interrupt a position or working orders, and must never run code that didn't come from this project. Public builds are released from `.github/workflows/release.yml`. The official build, with private connectors, is built from `anaremore/lines-connectors` (0020).

**Decision:**

1. **Check, download, then ask.**
   - Lines checks its release feed at startup and every 6 hours.
   - It downloads a newer release in the background and shows "Update ready: vX.Y.Z" in the window header, with **Restart to update**.
   - Nothing restarts on its own.
2. **Never during a trade.** Restart to update is held while any account has a position, a working order or an armed trigger. The header says why ("Update waits: 1 position open"). Strategies running on paper count too.
3. **Every update is verified before it's applied:**
   - The SHA-256 must match the release's published checksum.
   - An **Ed25519 signature** must verify against a public key compiled into the app (minisign format or equivalent).
   - An update that fails either check is discarded and reported, and never run. Releases are signed in CI, with the private key held as a CI secret (owner-only, below).
4. **Atomic swap, with rollback.**
   - On Windows, the running executable is renamed aside and the new one written in its place. On macOS, the app bundle is replaced the same way.
   - The previous version is kept, and **Settings › About › Roll back to vX** restores it.
   - If the new version fails to start twice in a row, the next launch offers the rollback.
5. **Saved files only move forward.**
   - Before an update, Lines copies settings, workspaces and the trading file to `<data dir>/backups/<old version>/`. Migrations run as today (`versioned`).
   - A rollback restores that copy, so an older version never reads a newer format.
   - The local store and recordings aren't copied, since their formats are versioned per file.
6. **The user chooses.** Settings › General › Updates: **Automatic** (download and ask; the default), **Notify only**, or **Off**. Turning updates off makes no network request. `--no-update-check` exists for tests, screenshots and headless runs, which never check.
7. **Channels.** Pre-release (current) and, later, stable. A build updates only within its own channel and its own edition: public updates public, official updates official.
8. **Privacy.** The check is one HTTPS request for the release feed, sending no identifiers and no telemetry.
9. **Implementation.** Either Velopack (delta updates, rollback, Windows and macOS) or `self-replace` plus our own verification. Choosing between them is an implementation detail for the PR, provided it meets points 1–8. Points 2 and 3 are tested with synthetic feeds, including a tampered download and a wrong signature.

**Open (owner-only, from `docs/release.md`):**
- **The signing key:** the owner generates it, holds the private key, and adds it as a CI secret. The public key is committed.
- **The official build's feed:** a private release feed reached with a read-only token, or, later, one app with connectors enabled through Settings (the owner's stated long-term direction). Until this is chosen, only public builds update themselves.
- **macOS notarization:** not required for updates to work, but Gatekeeper is smoother with it. It needs the owner's Apple developer account.

**Alternatives considered:**
- **An installer per release (MSI or pkg).** Rejected: the reinstall is what the owner wants gone.
- **A checksum only, no signature.** Rejected: a compromised release page could publish a matching checksum.
- **Silent automatic restart.** Rejected: a restart mid-trade is unacceptable in a trading app.

**Consequences:**
- `release.yml` gains a signing step and publishes a small update manifest (version, channel, URLs, checksums, signature).
- `docs/release.md` gains key custody and rotation steps.
- `docs/architecture.md` lists `--no-update-check`.
- Known gaps: no delta updates if `self-replace` is chosen; no Linux auto-update until there are Linux builds.
