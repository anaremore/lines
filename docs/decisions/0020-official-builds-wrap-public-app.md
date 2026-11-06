# 0020: Official builds wrap the public app; connectors register through an extension point

**Status:** Accepted (implements 0014)

**Context:** Decision 0014 keeps live vendor connectors (Rithmic, CQG production) in a private module. Cargo resolves *all* dependencies listed in a manifest, optional ones included, when building the lockfile. A public `Cargo.toml` that names a private git repo therefore breaks every public build. The connector can't be an optional dependency of the public workspace.

**Decision:**
- The public `lines-app` exposes a small **provider extension point**: a library entry point (e.g. `lines_app::run(extensions)`) that accepts additional `Provider`/`Broker` registrations with their CLI/source names, capabilities and availability rules. The public `lines` binary calls it with no extensions.
- The private repo `anaremore/lines-connectors` is its own Cargo workspace. It depends on the public crates by pinned git revision (or a local path override for development), implements the connectors, and builds the **official binary**, which calls the same entry point with the vendor connectors registered.
- Vendor material (protos, kits, certificates, app names) never enters the public repo (0010). Until each vendor confirms its terms, vendor protos are not committed to the private repo either. They're read at build time from a local path (`RITHMIC_PROTO_DIR`, e.g. `Z:\vendor\rithmic\kit\0.90.0.0\proto`).
- Credentials come from the OS keychain or environment variables, and are never logged or written to recordings.
- Public CI never needs the private repo. Private CI (when added) builds the official binary against the public revision it pins.

**Consequences:** The public app must keep its provider/broker registration generic (no vendor names in public code beyond the extension API). Official releases are produced from the private repo. Public releases keep shipping mock/replay/Coinbase/Kraken/paper only.
