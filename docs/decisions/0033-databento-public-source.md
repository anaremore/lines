# 0033: Databento is a public data source

**Status:** Proposed (draft for the owner)

**Context:** Decisions 0014 and 0020 put live vendor connectors in the private `lines-connectors` repo. Databento's connector (`lines-databento`: history import, cost check, live market data) went there by default. As a result:
- the public, self-updating release (0031) can't use Databento;
- the owner runs a separately built official binary, which has no update feed yet;
- every Databento fix needs a private PR plus a re-pin.

0014's reasons don't apply to Databento:
- **No certified app identity.** Rithmic certifies an app name, so a public connector would let a fork log in as Lines. Databento authenticates the user's own API key and nothing else.
- **No proprietary material.** Its wire format (DBN) and protocol are published, and the decoder (`dbn`, Apache-2.0) is public. There are no kits, protos or certificates.
- **Data only.** It routes no orders.

**Decision:**
1. **Databento moves into the public repo,** as a public crate (`lines-databento`), and is registered as a built-in source in the public `lines` binary. This covers the history import, the cost check, Download days, and live data (top of book on the Standard plan).
2. **The key is the user's.** It is entered in Settings › Connections and stored in the OS keychain (0030). It is never in the repo, the settings file, logs, recordings or diagnostics. With no key, the source shows "not set up" and makes no requests.
3. **The rule for what can be public:** a connector may be public when (a) it uses only the user's own credentials, with no app identity to protect; (b) its protocol and libraries are public and permissively licensed; and (c) it carries market data only. Order-routing connectors, and anything with vendor kits or a certified app identity (Rithmic, CQG production, Sierra DTC trading), stay private under 0014 and 0020. Private level sources (0022) stay private.
4. **Data from it is licensed data** (0010). Recordings, imported `.lfr` files and screenshots made from Databento never enter the repo. Tests use synthetic DBN built in code, as the private crate does today.
5. **Vendor names in public code:** Databento's symbols and schema names may live in the public `lines-databento` crate, next to `lines-feed` and `lines-sierra` (amends the CLAUDE.md convention).

**Alternatives considered:**
- **Keep it private and build an update feed for official builds** (0031's open item). This is still needed for the owner's private parts (levels, Rithmic). But it would leave every other user without Databento, and keep a private PR plus a re-pin on each fix. Rejected *for Databento*; the official update feed stays on the list for the private parts.
- **Ship Databento as a downloadable plugin.** There is no plugin loader for native connectors yet (0008 covers WebAssembly indicators). Too much for one source.

**Consequences:**
- **The move:** copy the crate, with its tests and docs, into the public workspace; register it; then remove it from `lines-connectors` and point `lines-official` at the public crate.
- **Tier B:** new dependencies (`dbn`, `ring` directly); `cargo deny`; `docs/architecture.md`, `docs/invariants.md` and CLAUDE.md conventions updated (CLAUDE.md is tier C: it needs the owner's yes in the same change).
- Public release notes and the README list Databento as a source that needs the user's own Databento account.
- Once it lands, the owner's daily Databento use can run on the public, self-updating build. Levels and Rithmic still need the official build until it gets its own update feed.
- Before release: confirm that Databento's terms allow a public client to use a user's key (they publish client libraries for exactly this).
