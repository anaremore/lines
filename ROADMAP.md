# Lines Roadmap

Direction only. Tasks and bugs live in GitHub Issues/Milestones. Rationale lives in `docs/decisions/`.
Update the status markers in the PR that finishes a milestone.

**Audience:** futures traders first, prop-firm traders core. US first; international possible later.
**Cost posture:** no paid data or routing contracts until there is user demand. Milestones 1–11 must be buildable with $0 of vendor fees.

Status: `done` · `current` · `next` · `later`

| # | Milestone | Status | Done when |
|---|-----------|--------|-----------|
| 1 | Skeleton: mock feed, candles, pan/zoom, record/replay, screenshot mode | done | Runs on Windows; deterministic `--screenshot` output from a replay file |
| 1b | CI on Windows, macOS, Linux | done | GitHub Actions builds, tests, clippy and fmt on all three OSes on every push/PR, public build only (no vendor features); macOS and Linux run headless tests, screenshot mode where the runner allows |
| 2 | Axes, crosshair & axis scaling | done | Price/time labels, DPI-correct, crosshair hit-testing via viewport math; scroll pan, Ctrl/axis-drag scaling, auto-scale indicator |
| 3 | Provider trait + CQG feed shape + Coinbase public feed (dev tool) | done | Two providers behind one trait; Coinbase gives free 24/7 live data |
| 3b | Sierra Chart importer (`.scid` ticks, `.depth` depth) → replay format | done | Local futures data replays with depth. Never committed (licensed data) |
| 4 | Historical bars + live stitch | done | Backfill then live with no gap or double-count |
| 4b | Symbol search & picker | done | Keyboard-first fuzzy search over products → contracts (decision 0013), recents/favourites, micro↔mini toggle, results <16 ms locally, provider lookups async |
| 5 | Depth model + read-only DOM, price grouping, time & sales, volume profile | done | Tick-indexed ladder, per-frame coalescing, fixed rows, no allocs in paint; T&S with aggressor side; session/visible-range volume profile; visible **stale** state when a feed goes quiet during market hours |
| 5a | Local market-data store | done | Per-contract append-only segments in `lines-events-1`, partitioned by session trade date; CRC blocks, torn-tail truncation, gap tracking, retention budget; instant cached startup then stitch-fill (decision 0019) |
| 5b | Workspaces: grid, tabs, windows, link groups | done | Panes (chart/DOM/T&S) in custom grids and presets, tabs, drag-out windows across monitors; link groups; opt-in crosshair time sync; shared data per contract; autosave/restore (decision 0016). Owner's default: NQ and ES side by side. DOM and T&S hide/show in one action (chart reflows, size restored) and detach into their own window per pane; per-chart "chart only" toggle. Settings and workspace files are versioned with forward migrations and a backup of the previous file; user data is never silently discarded |
| 5c | Sessions + indicator engine + VWAP | done | Product trading-hours calendars (TZ/DST, ETH/RTH, holidays); composable series graph with shared nodes (decision 0017); VWAP (session/weekly/monthly/anchored resets, σ bands, exact accumulators, approximate flag on bar-only history) on charts and as an input to other indicators |
| 5d | Kraken public data provider | done | Spot + public perps; L2 book verified against Kraken's CRC32 checksum on every update (mismatch → resync, counted); history via REST; exact decimal parsing; free, no account |
| 5e | External levels layer + private adapters (private level services, mapped onto ES/NQ) | next | Public `LevelSource` + levels chart layer + price-at-time helper (decision 0022); private adapter repos |
| 6 | Command layer + OMS + Broker trait + PaperBroker + prop-firm rules + Connections/accounts (decision 0018) | current | Order lifecycle, positions, P&L, risk layer, account groups, rule presets as data. OMS isolated from UI (decision 0012); state reconciled from broker on reconnect, no orphaned brackets; click/hotkey → order-sent latency measured and tracked |
| 6b | Latency HUD | done | Exchange→receive, input→order-sent, gateway RTT visible; clock-sync status shown. Chart headers (palette / pane menu "Latency HUD", off by default): feed, paint, order and ack p50/p99 over 60 s with sparklines; clock offset bound with a warning beyond 250 ms |
| 6c | Settings window + settings schema | done (phase 1) | Ctrl+, window with search and sections; every setting also a palette command; explicit scope (pane / workspace / defaults); instant apply with undo and reset; risk-loosening changes confirmed; one typed schema drives UI, palette and persistence (decision 0021) |
| 7 | DOM trading | done (paper) | Click/drag/hotkey orders, brackets, pending vs working states, flatten/cancel-all; orders dispatch from the input handler, not the next frame |
| 8 | Chart trading | current (phase 1 done) | Phase 1 done: order lines on the chart: see, drag and cancel orders; add a stop or target (one-cancels-other, reduce-only, never opens a position). Phase 2: drag the position line to set brackets, bracket P&L, an order strip at the crosshair |
| 8b | Discipline guardrails + economic calendar | current (core done) | Loss/streak lockouts with cooldown, max trades/day, news blackout (warn/flatten/block) tied to prop-rule presets; overrides deliberately hard. Calendar from official sources only (BLS, BEA, Federal Reserve schedules; FRED API), as a data-source plugin (0007); no scraping |
| 8c | Automatic trade journal | current | Phase 1 done: fills → round trips (the guardrails' definition), exact P&L/fees, MAE/MFE from the paper broker's market data, fill sources, tags and notes, stats by date/product/hour/tag, append-only `journal/<source>.jsonl`, Journal pane, CSV export. Phase 2: auto entry/exit chart screenshots (`Trade::screenshots`), P&L by account/setup views, consistency-rule progress |
| 10 | Indicators (10a engine + VWAP moved to 5c): 10a more built-ins · 10b WASM host · 10c data-source plugins · 10d plugin dev tooling | later | See decisions 0007–0008 |
| 9 | Chart timeframes | done | Minute/hour/day/week candles derived exactly from 1-minute bars, aligned to sessions; per-chart, saved; VWAP/profile/crosshair sync follow |
| 9b | Tick, volume, range and seconds charts | done | One counted-trade bar builder (`lines_core::ticks`, kinds `BarKind`): `500t` tick, `500v` volume (whole contracts), `10r` range (exact in ticks) and `30s` seconds bars from counted trades (live, replay, local store), reset at the session open or continuous, per chart and saved; a late start is labelled; footprint, profile, VWAP, drawings and crosshair sync work on them |
| 10e | Footprint + cumulative delta | done | Footprint chart mode (bid × ask / delta / volume cells per bar, diagonal imbalances, POC) from counted trades, exact on every timeframe; cumulative delta as an engine indicator (ETH/RTH/no reset) drawn as candles in the first chart sub-pane |
| 11 | Drawing tools (workspaces moved to 5b) | done | Data-space drawings per product, persisted and shared by every chart of it |
| 12a | Rithmic Test market data (private connector) | current | Provider extension point in public app (decision 0020); private `lines-connectors` repo builds the official binary; ticker + history plants against Rithmic Test; protos read locally, never committed |
| 12b | Sierra Chart DTC execution (private) | next | Observe mode → Sierra Sim → owner-enabled Live per decision 0023 safety rules; restart reconciliation; audit log |
| 12 | First live connection (likely Rithmic; CQG WebAPI is enterprise-priced) | later | Vendor conformance passed; live routing gated behind a Cargo feature until then |
| 13 | Second live connection (whichever of CQG/Rithmic wasn't first) | later | |
| 14 | Trade copier across account groups | later | Rule-aware per account/firm |
| 15 | Strategies (decision 0025) | current | (a) the core's strategy gate, `lines-strategy` and deterministic backtests (`--backtest`); (b) private strategies backtested on recorded market data and levels; (c) the strategy panel, Stop all, chart markers and the in-app host; (d) Live, owner-gated |
| — | Not planned: Tradovate, equities, crypto trading, international | later | Kept possible by decisions 0002–0003; nothing built |

## Before the first outside users

| # | Milestone | Status | Done when |
|---|-----------|--------|-----------|
| U1 | Diagnostics | done | Rotating bounded local logs; one command exports a support bundle (logs, version, settings with credentials stripped, recent feed stats); opt-in crash reports after |
| U2 | Auto-update | later | Signature-verified updates, never applied during market hours, one-click rollback; depends on owner code signing |
| U3 | Legal | later | Owner completes `docs/legal-checklist.md` (required before 12) |
| — | Parser fuzzing | done | `cargo-fuzz` targets for every network decoder (CQG protobuf, Coinbase/Kraken JSON, recordings), run in CI on a schedule |

## Cross-cutting requirements (apply to every milestone)

- **Startup:** launch → trading-ready in ~2 s (workspace restore, cached contract metadata and recent bars, parallel connect).
- **Keyboard-first:** command palette, fully remappable hotkeys.
- **Colours:** configurable up/down colours with a colourblind-safe preset; never rely on red/green alone.
- **Updates:** never apply during market hours; one-click rollback.
- **Telemetry:** opt-in crash reports and performance metrics only.

## Backlog (unscheduled)

Multi-monitor layouts with symbol link groups · alerts (sound, desktop, phone push) · contract roll/expiry and holiday-hours warnings · import hotkeys/bracket presets from Sierra Chart and NinjaTrader · AI journal review and plain-language alerts.

## Strategy questions (owner)

- Prop firms as distribution partners (bundling, platform approval).
- Pricing model: licence, subscription, or freemium. Affects build order.

## Vendor access status

- **CQG:** WebAPI is enterprise-only (sales process, likely fees). Public protos via `third_party/cqg-webapi` let us build against real message shapes now. Information request form not yet submitted. Demo API credentials requested from apihelp@cqg.com (2026-11-06).
- **Rithmic:** primary live target (prop firms; owner trades on it). Rithmic Test developer user created 2026-11-06 (separate from the owner's trading login); agreements signed; R | Protocol kit v0.90.0.0 downloaded locally (not in any repo). Connector will be private (decision 0014); open-source acceptability is asked of Rithmic. Vendor questions to sales@rithmic.com pending. Path: R | Protocol API (WebSocket + protobuf); dev kit via rithmic.com/api-request; free "Rithmic Test" system for development; conformance on Rithmic Paper Trading required before production. Vendor/conformance fees unknown. Protos are proprietary: never vendor them (decision 0010). `pbeets/rithmic-rs` is a reference only (uses native-tls, which conflicts with our rustls rule). Risks to confirm: one market-data session per login (users can't run Lines alongside R | Trader Pro or Sierra Chart on the same login); whether users of a conformed platform pay the $100/mo API fee or only the $25 connection fee; per-firm API permission ("1011 permission denied").
- **Topstep:** new accounts are TopstepX-only since 2025-07-07, so it's not reachable via Rithmic.
- **Open question for both:** can prop-firm accounts log into a conformed third-party platform, or must each firm approve it?
