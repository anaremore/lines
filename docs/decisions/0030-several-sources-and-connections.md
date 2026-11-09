# 0030: Several data sources at once, set up in Settings › Connections

**Status:** Accepted (owner accepted 2026-11-09; Connections mock approved the same day)

**Context:** Lines runs one data source per process, chosen with `--source`. The owner wants public crypto data (Coinbase, Kraken) without a flag, the Databento key entered in the UI, and charts from different sources side by side. Several more sources are planned: Databento (now), Schwab (0028), Rithmic and CQG (0014, 0020). They share the same needs: a key or a login, entitlements that vary by plan, and status. Connectors stay private (0014, 0020), so the public app can't name them.

**Decision:**

1. **Several sources run at once.** Each source is started lazily, when a pane first needs one of its symbols, and stopped when no pane needs it. A source failing, refusing or going stale affects only its own panes. The local store is already keyed by source (0019).
2. **Public crypto sources are on by default.** Coinbase and Kraken need no account and connect only when a pane shows one of their symbols. Each can be turned off in Settings. When the machine is offline, their symbols show as unavailable, with no dialogs.
3. **Settings › Connections** lists every registered source: its state (off, not set up, connecting, connected, refused with the provider's reason), what the account's plan allows, and its settings.
   - A connector **describes its own fields** (text, choice, switch, secret) through the extension point (0020), and the public app renders them. Public code never names a private connector.
   - The page also has **Test** for each source.
4. **Secrets are entered in the UI and stored in the OS keychain** (Windows Credential Manager, macOS Keychain, Secret Service on Linux), never in the settings file, workspaces, logs or diagnostics.
   - A saved secret shows masked with its last 4 characters, with Show, Replace and Remove.
   - Environment variables still override for tests and headless runs.
5. **Entitlements are part of a source's status** (for example "top of book only", "delayed", "no options"). The picker and DOM show them, so a thin book is never mistaken for a quiet market (extends #97).
6. **A pane saves its source** in the workspace (a new optional field; no migration). After a restart, a pane whose source isn't set up says so, with the way into Connections. Other panes load normally.
7. **One product, several sources.** A product (ES) keeps one identity across sources, so studies, levels, drawings and history follow it. The picker searches every enabled source and tags each result. When a product is available from more than one source, a preferred source can be set per product.
8. **History that costs money is fetched once.** Downloaded history goes into the local store and is never requested again. A source that charges for history says so before a large pull.
9. **Mock and replay stay developer and testing tools,** behind their flags, plus an "Open recording…" palette item. `--source` remains and, when given, limits the run to that source (tests, screenshots, headless checks).
10. **The connector interface is versioned.** It covers settings fields, status, entitlements and lifecycle. A private connector built against an older version is refused with a clear message, never loaded wrong.
11. **Data and execution stay separate.** Paper trading works on any source. A live broker account (later, owner-enabled per 0014) states which source its orders' prices come from.

**Alternatives considered:**
- **Keep one source per run and add a source picker at startup.** Rejected: the owner wants mixed sources side by side, and a restart to switch is the friction being removed.
- **Store secrets in the settings file, encrypted with an app key.** Rejected: an app key shipped in a public binary protects nothing. The OS keychain is per user and already used by hand today (`cmdkey`).
- **Make the public app know each connector's settings.** Rejected by 0020: public code stays vendor-neutral.

**Consequences:**
- Startup no longer chooses a source. Panes ask the feed registry for a contract, and the registry starts its source on demand.
- `docs/architecture.md` (CLI flags, registry) and `docs/invariants.md` (feeds, panes, settings) need updates in the PRs that implement this.
- **Recordings, diagnostics and screenshots made from licensed sources are marked** with their source and never attached anywhere automatically (extends 0010 to user output).
- **Build order:** several sources at once, with crypto on by default; then Connections with secrets (Databento first); then picker tags and preferred source; then entitlements in the DOM and picker.
