# Decision records

One file per decision: `NNNN-title.md`. Never rewrite an accepted decision; supersede it with a new one and mark the old one `Superseded by NNNN`.

Format: Status · Context · Decision · Consequences.

| # | Decision |
|---|----------|
| 0001 | Network runs on a dedicated Tokio runtime, not GPUI's executor |
| 0002 | Prices and quantities are exact; f64 only at the pixel boundary |
| 0003 | Internal instrument IDs, symbol mapping, capability-based traits |
| 0004 | Paper trading is a broker behind the same trait as live |
| 0005 | Default paper fill model is trade-through |
| 0006 | Risk checks live in the OMS; prop-firm rules are data |
| 0007 | Indicators are pure; network I/O only through data-source plugins |
| 0008 | User plugins run as WebAssembly components in Wasmtime |
| 0009 | UI pull requests include reproducible screenshots |
| 0010 | Licensed data and vendor files never enter the repo |
| 0011 | Providers are I/O-free sessions; every inbound byte flows through recorded frames |
| 0012 | The OMS runs independently of the UI, and orders bypass the frame loop |
| 0013 | Products, contracts, and rolls |
| 0014 | Public repo under FSL; live vendor connectors in a private module |
| 0015 | Sierra contract-year resolution, amends 0013 |
| 0016 | Workspaces, panes, layouts and link groups |
| 0017 | Indicator engine: composable series graph; VWAP first |
| 0018 | One trading surface: commands, trading core, connectors, accounts |
| 0019 | Local market-data store |
| 0020 | Official builds wrap the public app; connectors register through an extension point |
| 0021 | Settings: in context first, one searchable Settings window, settings as data (Accepted) |
| 0022 | External level sources: a public levels layer, private adapters |
| 0023 | Trade execution via Sierra Chart's DTC server |
| 0024 | Contract alignment for live orders, amends 0023 |
| 0025 | Strategies: pure state machines behind the trading core, backtested on replays (Accepted) |
| 0026 | User styles for drawings and studies: one style model and editor, custom colors as user data (Accepted) |
| 0027 | Strategy host heartbeat on the wall clock; the daily loss closes the trade, amends 0025 (Proposed) |
