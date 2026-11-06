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
| 5 | Depth model + read-only DOM, price grouping, time & sales, volume profile | later | Tick-indexed ladder, per-frame coalescing, fixed rows, no allocs in paint; T&S with aggressor side; session/visible-range volume profile |
| 6 | OMS + Broker trait + PaperBroker + prop-firm rules | later | Order lifecycle, positions, P&L, risk layer, account groups, rule presets as data. OMS isolated from UI (decision 0012); state reconciled from broker on reconnect, no orphaned brackets; click/hotkey → order-sent latency measured and tracked |
| 6b | Latency HUD | later | Exchange→receive, input→order-sent, gateway RTT visible; clock-sync status shown |
| 7 | DOM trading | later | Click/drag/hotkey orders, brackets, pending vs working states, flatten/cancel-all; orders dispatch from the input handler, not the next frame |
| 8 | Chart trading | later | Draggable order/position lines, bracket P&L, same intents as DOM |
| 8b | Discipline guardrails + economic calendar | later | Loss/streak lockouts with cooldown, max trades/day, news blackout (warn/flatten/block) tied to prop-rule presets; overrides deliberately hard |
| 8c | Automatic trade journal | later | Fills → trades with auto entry/exit screenshots, tags, P&L by time/setup/account, consistency-rule progress |
| 9 | Multi-chart / multi-symbol | later | One feed fanned out to many panes |
| 10 | Indicators: 10a built-in trait · 10b WASM host · 10c data-source plugins · 10d plugin dev tooling | later | See decisions 0007–0008 |
| 10e | Footprint + cumulative delta | later | Built on 10a; per-price bid/ask volume per bar from aggressor-tagged ticks |
| 11 | Drawing tools + saved workspaces | later | Data-space drawings, persisted layouts |
| 12 | First live connection (likely Rithmic; CQG WebAPI is enterprise-priced) | later | Vendor conformance passed; live routing gated behind a Cargo feature until then |
| 13 | Second live connection (whichever of CQG/Rithmic wasn't first) | later | |
| 14 | Trade copier across account groups | later | Rule-aware per account/firm |
| — | Not planned: Tradovate, equities, crypto trading, international | later | Kept possible by decisions 0002–0003; nothing built |

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
- **Rithmic:** primary live target (prop firms; owner trades on it). Rithmic Test developer user created 2026-11-06 (separate from the owner's trading login). Connector will be private (decision 0014); open-source acceptability is asked of Rithmic. Vendor questions to sales@rithmic.com pending. Path: R | Protocol API (WebSocket + protobuf); dev kit via rithmic.com/api-request; free "Rithmic Test" system for development; conformance on Rithmic Paper Trading required before production. Vendor/conformance fees unknown. Protos are proprietary: never vendor them (decision 0010). `pbeets/rithmic-rs` is a reference only (uses native-tls, which conflicts with our rustls rule). Risks to confirm: one market-data session per login (users can't run Lines alongside R | Trader Pro or Sierra Chart on the same login); whether users of a conformed platform pay the $100/mo API fee or only the $25 connection fee; per-firm API permission ("1011 permission denied").
- **Topstep:** new accounts are TopstepX-only since 2025-07-07, so it's not reachable via Rithmic.
- **Open question for both:** can prop-firm accounts log into a conformed third-party platform, or must each firm approve it?
