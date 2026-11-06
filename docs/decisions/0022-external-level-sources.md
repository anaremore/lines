# 0022: External level sources: a public levels layer, private adapters

**Status:** Accepted (implements 0007 and 0020)

**Context:** The owner wants levels from external services on charts: RocketScooter levels and VARIS zones via the local RS Levels API (`127.0.0.1:8765`, display-only plugin contract), and OptionIdea's SPY/QQQ option-structure levels via its authenticated v2 platform API (translated onto ES/MES and NQ/MNQ by its documented anchor rule). Both adapters are private; the display machinery should be reusable for any future level source.

**Decision:**

*Public (Lines repo)*
- A **`LevelSource` data source** (decision 0007), registered through the extension point (0020). Each source declares the products it serves, its refresh mode (poll interval, SSE, or one-shot), its stale threshold, and its settings schema (0021).
- It publishes **level maps**: horizontal **lines**, **zones/bands** (lower–upper) and **labels**, each with a kind, a source-defined style (colour and weight), optional metadata (as-of time, source session, provenance), and the map's own as-of time.
- **Fetching runs off the UI thread** on the feed/runtime side. HTTP and SSE responses go through recorded frames (0011), so replays reproduce what was shown.
- A **levels chart layer** draws maps on matching chart panes, with no paint-path allocation.
  - **Source-defined colours are allowed** for external levels when the source's contract fixes them (OptionIdea requires this). This is the one exception to "theme tokens only"; labels still use theme text tokens and the legend box (from the clean-chart work).
- **Per-pane controls:** per-source and per-kind visibility, label visibility, clean-chart and hide-all-studies integration, and persistence as optional workspace fields.
- **Missing or stale maps** show as missing or stale (`docs/design.md` §7), never as current.
- A **price-at-time helper** returns the chart's price at a given instant from the store or history (open of a start-stamped bar, within a tolerance), so adapters can do anchor translations. If history doesn't cover that instant, translation is withheld.

*Private adapters (one repo each)*
- **`anaremore/lines-rs-levels`:** follows the RS Levels display-plugin contract (`/status`, `/levels/:symbol/rows`, `/stats/:symbol/rows`, plus SSE if suitable). It covers ES/MES and NQ/MNQ, VARIS zones from the `RI` stat, and the contract's required settings: service URL, symbol, refresh interval, stale threshold, line kinds and labels. Localhost only by default.
- **`anaremore/lines-optionidea`:** follows OptionIdea platform contract v2 (`POST /api/v2/levels/query`; rejects unknown versions, malformed rows, more than 16 levels; no silent historical fallback). It applies the futures translation rule with `referencePrice` / `referencePriceAsOf` against the chart's own price and rounds to tick. The API key comes from the OS keychain (or `OPTIONIDEA_API_KEY`), sent only as `Authorization: Bearer`, never logged or displayed. HTTPS only.
- Both are display-only: no order entry, account or broker access.
- The official/personal build (`lines-connectors`) includes them as optional git dependencies, which is fine because all three repos are private.

**Consequences:**
- The public levels layer lands after the volume-profile and clean-chart work (same chart-layer seam).
- The adapters' parsers, translation maths and contract validation are pure, testable code, and can be built in parallel against fixtures modelled on each contract's published fixtures. They are never captured from paid or licensed live data.
