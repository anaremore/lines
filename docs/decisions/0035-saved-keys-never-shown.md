# 0035: A saved key is never shown again

**Status:** Accepted (owner accepted 2026-11-10). It supersedes one clause of decision 0030 §4; the rest of 0030 stands.

**Context:** Decision 0030 §4 says a saved secret "shows masked with its last 4 characters, with Show, Replace and Remove". While building Settings › Connections (#155), the planning thread looked at Zed's settings UI. Zed never displays a saved API key again: it says the key is configured, says where it comes from, and offers to reset it. Showing a stored key puts it on screen, where screenshots, screen shares and diagnostics bundles can capture it. It also means reading the secret back out of the OS keychain only to display it.

**Decision:**
1. **A saved secret is never shown again in full.** Its row shows "Configured", where the value comes from ("keychain", or "environment variable (overrides)"), and its last 4 characters.
2. **The actions are Replace and Remove.** There is no Show. To check a key, the user replaces it, or uses the source's **Test**.
3. **Everything else in 0030 §4 is unchanged:** secrets are entered in the UI and stored in the OS keychain; environment variables still override; nothing goes in the settings file, workspaces, logs or diagnostics.

**Consequences:**
- Settings › Connections (#155) implements this as built.
- 0030's status line notes that this clause is superseded by 0035.
- `docs/invariants.md` states it once: saved secrets are never displayed.
