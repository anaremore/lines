# Lines Roadmap

Direction only. Tasks and bugs live in GitHub Issues/Milestones. Rationale lives in `docs/decisions/`.
Update the status markers in the PR that finishes a milestone.

**Audience:** futures traders first, prop-firm traders core. US first; international possible later.
**Cost posture:** no paid data or routing contracts until there is user demand. Milestones 1–11 must be buildable with $0 of vendor fees.

Status: `done` · `current` · `next` · `later`

| # | Milestone | Status | Done when |
|---|-----------|--------|-----------|
| 1 | Skeleton: mock feed, candles, pan/zoom, record/replay, screenshot mode | done | Runs on Windows; deterministic `--screenshot` output from a replay file |
| 2 | Axes & crosshair | current | Price/time labels, DPI-correct, crosshair hit-testing via viewport math |
| 3 | Provider trait + CQG feed shape + Coinbase public feed (dev tool) | done | Two providers behind one trait; Coinbase gives free 24/7 live data |
| 3b | Sierra Chart importer (`.scid` ticks, `.depth` depth) → replay format | next | Local futures data replays with depth. Never committed (licensed data) |
| 4 | Historical bars + live stitch | next | Backfill then live with no gap or double-count |
| 5 | Depth model + read-only DOM, price grouping | later | Tick-indexed ladder, per-frame coalescing, fixed rows, no allocs in paint |
| 6 | OMS + Broker trait + PaperBroker + prop-firm rules | later | Order lifecycle, positions, P&L, risk layer, account groups, rule presets as data |
| 7 | DOM trading | later | Click/drag/hotkey orders, brackets, pending vs working states, flatten/cancel-all |
| 8 | Chart trading | later | Draggable order/position lines, bracket P&L, same intents as DOM |
| 9 | Multi-chart / multi-symbol | later | One feed fanned out to many panes |
| 10 | Indicators: 10a built-in trait · 10b WASM host · 10c data-source plugins · 10d plugin dev tooling | later | See decisions 0007–0008 |
| 11 | Drawing tools + saved workspaces | later | Data-space drawings, persisted layouts |
| 12 | First live connection (likely Rithmic; CQG WebAPI is enterprise-priced) | later | Vendor conformance passed; live routing gated behind a Cargo feature until then |
| 13 | Second live connection (whichever of CQG/Rithmic wasn't first) | later | |
| 14 | Trade copier across account groups | later | Rule-aware per account/firm |
| — | Not planned: Tradovate, equities, crypto trading, international | later | Kept possible by decisions 0002–0003; nothing built |

## Vendor access status

- **CQG:** WebAPI is enterprise-only (sales process, likely fees). Public protos via `third_party/cqg-webapi` let us build against real message shapes now. Information request form not yet submitted. Demo API credentials requested from apihelp@cqg.com (2026-11-06).
- **Rithmic:** primary live target (prop firms; owner trades on it). Not yet contacted.
- **Open question for both:** can prop-firm accounts log into a conformed third-party platform, or must each firm approve it?
