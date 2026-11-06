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
