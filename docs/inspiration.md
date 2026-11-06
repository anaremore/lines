# Inspiration from open-source projects

> Behaviours and design ideas to adopt, by area. **Licence rule:** Lines is FSL-1.1-ALv2; code from GPL/AGPL/LGPL projects must never be copied (behaviour only). MIT/Apache code may be reused with its notices (and Lightweight Charts requires its NOTICE attribution). Researched 2026-11-06; a few items are from general knowledge rather than a fetched page and are marked in the text.


Tag key: **[project · licence · code reuse]**. "Code: no" means use the behaviour only and write it again yourself.

## Projects checked (state as of 2026-11-06)

| Project | Licence | Activity |
|---|---|---|
| Zed (github.com/zed-industries/zed) | **GPUI crate Apache-2.0**. `workspace`, `settings_ui` and the editor crates are GPL-3.0-or-later. Server: AGPL | pushed today, v1.27.0 (2026-11-04) |
| uPlot (github.com/leeoniya/uPlot) | MIT | pushed 2026-10-30. Last tagged release 1.6.32 (2025-03) |
| Lightweight Charts (github.com/tradingview/lightweight-charts) | Apache-2.0. The NOTICE requires a TradingView attribution link | pushed today, v5.2.1 |
| Flowsurface (now github.com/flowsurface-rs/flowsurface) | **GPL-3.0** | v0.9.0 (2026-10-27), iced |
| NautilusTrader (github.com/nautechsystems/nautilus_trader) | **LGPL-3.0** | pushed today |
| Hummingbot (github.com/hummingbot/hummingbot) | Apache-2.0 | v2.17.0 (2026-09) |
| Perspective (now github.com/perspective-dev/perspective) | Apache-2.0 | v5.8.0 (today) |
| barter-rs (github.com/barter-rs/barter-rs) | MIT | pushed 2026-08 |
| edgedepth-terminal (github.com/edgedepthhq/edgedepth-terminal) | **AGPL-3.0**. Its gateway is MIT | pushed 2026-10 |

## Charts, axes, crosshair

- **Pick ticks by minimum pixel spacing over a fixed list of increments.** Use the smallest increment from an increments table (`incrs`) whose spacing in pixels is at least `space`. Measure `space` from the widest label, as a function of the axis length. Keep a separate table for time (1 ms … 1 y) and for price. For price, the table is multiples of the tick (1, 2, 5, 10, 25, …), so labels are always on the grid. [uPlot · MIT · code: yes] https://github.com/leeoniya/uPlot/blob/master/docs/README.md
- **Time labels by rollover.** Each increment row has a default format plus formats used when a larger unit changes: the date appears only on the first tick of a new day, the year only at the turn of a year. Realign multi-hour ticks to local midnight across DST changes (`nice.dst`). [uPlot · MIT · code: yes]
- **Weighted time tick marks.** Each tick gets a weight (year > month > day > hour > minute). When labels collide the higher weight wins, and with `uniformDistribution` ticks of one weight show all together or not at all, so spacing never looks ragged. Labels are capped at 8 characters (`tickMarkMaxCharacterLength`). [Lightweight Charts · Apache-2.0 · code: yes with notice; behaviour preferred] https://tradingview.github.io/lightweight-charts/docs/api/interfaces/TimeScaleOptions
- **Shift the view on a new bar only if the live edge is visible** (`shiftVisibleRangeOnNewBar`). Keep a `rightOffset` measured in bars, not pixels. Bar spacing is clamped between `minBarSpacing` (0.5) and `maxBarSpacing`. `fixLeftEdge`/`fixRightEdge` are optional. `lockVisibleTimeRangeOnResize` keeps the time range on resize instead of the bar width. [Lightweight Charts · Apache-2.0]
- **Price-axis label collision.** `alignLabels` pushes overlapping axis labels (last price, crosshair, VWAP tags, order prices) apart along the axis instead of drawing them over each other. `entireTextOnly` hides edge labels that would be clipped. Use `minimumWidth` to give stacked charts equal axis widths. [Lightweight Charts · Apache-2.0] https://tradingview.github.io/lightweight-charts/docs/api/interfaces/PriceScaleOptions
- **Interacting with the axes.** Dragging an axis rescales it, double-clicking an axis resets that axis only (`axisPressedMouseMove`, `axisDoubleClickReset`), and the wheel zooms. Kinetic scroll is on for touch but **off for mouse** by default, which is the right default for a trading tool. [Lightweight Charts · Apache-2.0]
- **Magnet crosshair.** Modes are Normal, Magnet (snap Y to close) and Magnet-OHLC (snap to the nearest O/H/L/C). Make it a per-chart command. [Lightweight Charts · Apache-2.0]
- **Cursor sync keyed by a sync key, with match functions.** `cursor.sync = { key, scales: [x, y], match: [fn, fn], filters: { pub, sub } }`. Each chart publishes the cursor position in *data* units on a named bus. `match` decides whether another chart's scale is the same (for example the same contract and timeframe → sync Y as well, otherwise X only). Pub/sub filters let a chart opt out. Lines already syncs time. Adding a Y sync for charts of the same contract, and a per-chart publish/subscribe toggle, fits this model. [uPlot · MIT · code: yes]
- **Drag-to-zoom with a uni-directional threshold.** A drag zooms X only unless the pointer moves more than `uni` px (50) in Y. `setRange` hooks snap the drag bounds to whole bars. Double-click on the plot resets to auto. [uPlot · MIT]
- **Batch updates.** `batch()` merges several scale or size changes into one layout pass, which fits "one frame per input". [uPlot · MIT]

## DOM, T&S, order flow

- **Chase tracker on the ladder.** It counts consecutive best bid/ask moves in one direction, draws a segment of strength `1 − 1/(1+n)`, and fades it over updates where nothing changes. It encodes data (momentum of the touch), so it isn't decoration. [Flowsurface · GPL-3.0 · code: no] https://github.com/flowsurface-rs/flowsurface (`data/src/panel/ladder.rs`)
- **Trade volume in the ladder with a retention window.** The ladder shows buy/sell volume per row over the last N minutes (`trade_retention`). It keeps raw trades and *regroups* from them when the grouping changes, so changing the grouping is exact. Lines' `vap` counts since data start. A rolling-window column would be a cheap addition. [Flowsurface · GPL-3.0 · code: no]
- **T&S filters and options.** A size filter hides prints below a threshold. Tapes of several contracts can be merged, with a symbol prefix. A stacked buy/sell ratio bar sits above the tape, by count or volume. Time format is selectable. [Flowsurface · GPL-3.0 · code: no]
- **Footprint binning rules.** Footprint/OHLC trades bin to the *nearest* step. Ladder/quote binning is side-biased (floor sells, ceil buys), and the source warns never to mix the two. Each bar keeps `first/last_time`, counts and buy/sell quantity per price plus a summary, which serves both indicators and rendering. The visible-range maximum is computed only over the rows on screen. [Flowsurface · GPL-3.0 · code: no]
- **Heatmap (historical DOM).** Store depth as *runs* `(start, until, qty, side)` per price, not snapshots. Merge adjacent similar runs above a threshold. Filter orders and trades by size. Purge price levels older than the visible window past a fixed count (4800). Seeking replays depth with an interruptible replay, so the UI isn't blocked. [Flowsurface · GPL-3.0 · code: no]
- **Pulled and stacked liquidity flags and imbalances.** Mark levels pulled before trading and walls on the DOM. On the footprint, show diagonal bid/ask imbalances and closed-bar delta. [edgedepth · AGPL-3.0 · code: no] https://github.com/edgedepthhq/edgedepth-terminal
- **Label a profile approximate when it comes from candles.** Profiles built from candles (volume spread over high–low; the side taken from close ≥ open) are a different source from trade-based ones, and the UI shows which. This matches Lines' exact/approximate VWAP flag; extend it to any future candle-based profile. [Flowsurface · GPL-3.0 · code: no]

## Workspaces, panes, commands, settings

- **Context predicates for key bindings.** Contexts form a tree (Workspace > Pane > Chart…). A predicate supports `&&`, `||`, `!` and `>` (ancestor > descendant). A deeper context beats a shallower one; at equal depth the later binding wins. Binding a key to `null` unbinds it in that context. GPUI's `KeyContext`/`DispatchTree`/`actions!` machinery is in the Apache crate, so Lines can use it directly instead of its own capture-phase table, where that helps. [GPUI · Apache-2.0 · code: yes] https://zed.dev/docs/key-bindings
- **Chords with a pending-key indicator.** When one binding is a prefix of another, wait about 1 s. The status bar shows the pending keys and possible completions, and the countdown pauses on hover. A which-key popup lists the continuations. [Zed · GPL (workspace) · code: no]
- **Keymap editor as searchable data.** A table lists every action, its bindings and its context, and editing it writes back to the user's keymap JSON. Base keymaps are presets. A "key context view" debug command shows the active contexts. [Zed · GPL · code: no]
- **Command palette.** Fuzzy match on action names (`namespace: action`), recently used commands ranked first, each binding shown beside its command, and a filter that hides commands invalid in the current state rather than greying them. [Zed · GPL · code: no]
- **Settings: one JSON schema drives everything.** Each setting has a dotted path (`dom.grouping`). The settings editor is a search box over all paths, and the JSON file stays the source of truth. Profiles are overlays on top of user settings or the defaults. List values merge with `"..."` instead of replacing. Settings that need a restart say so. [Zed · GPL (settings_ui) · code: no] https://zed.dev/docs/configuring-zed
- **Tabs and panes.** `activate_on_close: history | neighbour | left_neighbour` (default history). Toggle-zoom a pane to the full window, then restore it. Pin tabs. Per-axis split direction. [Zed · GPL · code: no]
- **Focus.** `FocusHandle` plus `track_focus` and focus-in/out events. Actions dispatch from the focused node up through its ancestors, so a pane handles its own keys without a global table. [GPUI · Apache-2.0 · code: yes]

## Trading core, OMS, risk: test checklist against `lines-trading`

Behaviour only for Nautilus (LGPL). Sources: https://nautilustrader.io/docs/latest/concepts/orders/, …/execution/, …/backtesting/fill-models/

1. Keep *denied* (local risk, never sent) separate from *rejected* (by the broker). Lines' `Reject` should keep that provenance.
2. "Not open" doesn't mean finished: Submitted is neither open nor closed. Test that `CancelAll` covers Submitted and Acked orders.
3. **Pending cancel/modify**: the order is still working while a cancel or modify is in flight. A fill that arrives during a pending cancel must apply, and the order must then become Filled or Cancelled with the filled quantity.
4. A fill after expiry (GTD/DAY) is applied without reopening the order.
5. Reduce-only: reject any order that would increase or flip the position. In the simulator the order's quantity shrinks as the position shrinks, and it is cancelled when the position goes flat.
6. Trading states **ACTIVE / REDUCING / HALTED**. REDUCING allows only single reduce-only orders that oppose the position and are no larger than it; HALTED allows only cancels. This maps onto prop breaches: a breach → HALTED or REDUCING instead of a single "blocked" flag.
7. Rate limits on submits and on modifies (separate limits). Maximum notional per order. GTD already expired → deny.
8. Deduplicate fills by a stable `trade_id` across every path (stream, poll, rebuild), whatever their price or quantity. Synthetic fills from reconciliation use **deterministic ids**, so a restart doesn't double-count them.
9. Overfill: a fill beyond the order's quantity is either refused or applied and recorded in `overfill_qty`, never silently clamped.
10. Fill models to compare against Lines' paper broker: `prob_fill_on_limit` (touch), `liquidity_consumption` (taken size isn't reused until the level updates; Lines already does this), one-tick slippage on L1 books, and tiered synthetic books. Test that several orders never fill more than a trade printed.
11. Contingent orders: OTO, OCO, OUO (*one-updates-other*: a partial fill shrinks the sibling). Lines has brackets/OCO. Add OUO tests.
12. Shutdown and restart: any submit without a broker outcome is listed as unresolved, never assumed working.

## Connectors

- **Lost orders.** An order the venue reports "not found" 3 times (`lost_order_count_limit = 3`) moves to a *lost* set. Lost orders still match incoming fills and are retried, never dropped. [Hummingbot · Apache-2.0 · code: yes] https://github.com/hummingbot/hummingbot/blob/master/hummingbot/connector/client_order_tracker.py
- **Two polling intervals.** Poll order status every 5 s while the user stream is silent, otherwise every 120 s. Refresh trading rules every 30 min. [Hummingbot · Apache-2.0]
- **Client order id first.** Create the id locally before the request (state PENDING_CREATE), so a fill that arrives before the ack still matches. [Hummingbot · Apache-2.0]
- **Order book sequencing.** Apply a diff only if its id is greater than the last applied id, otherwise resnapshot. Lines' Kraken Futures handling already matches this. [Hummingbot · Apache-2.0] https://hummingbot.org/connectors/connectors/architecture/
- **Linked rate limits.** A throttler with named limits, where one call consumes weight from several linked buckets. Needed for Rithmic/CQG request budgets. [Hummingbot · Apache-2.0]
- **Fee schema as data.** Maker/taker, percent and fixed, deducted or added. [Hummingbot · Apache-2.0]
- **Audit stream and state replica.** The engine emits an audit stream. A replica rebuilt from it serves the UI and monitoring off the hot path, and instruments are indexed up front (lookup by index, no hashing). This mirrors Lines' `Event` stream: test that a replica built from events equals the core's state. [barter-rs · MIT · code: yes] https://github.com/barter-rs/barter-rs

## Tables and journal

- **Upsert by key, ring limit.** A table has an `index` column (`update` replaces the row with the same key) or a `limit` (rolling ring of rows). Use `index` for orders and positions keyed by order id, and `limit` for fills and the tape. [Perspective · Apache-2.0 · code: yes] https://github.com/perspective-dev/perspective
- **A view is a query stored as data.** `group_by` / `split_by` / `sort` / `filter` / expressions / aggregates. `save()` returns JSON and `restore()` takes it, so a journal view is a named setting, searchable like the rest. For example, P&L by trade date per product. [Perspective · Apache-2.0]
- **Number cell styles that encode data.** Bar (size relative to the column), gradient (by sign or magnitude), and pulse (a brief flash on change, coloured by direction). [Perspective · Apache-2.0]
- **Virtualised grid.** Lay out only the visible cells (regular-table); the column header carries the sort and filter state. [Perspective · Apache-2.0]

## Avoid

- Drawing-tool palettes and floating toolbars always on screen (TradingView-style left rail). Lines must not look like TradingView anyway.
- Animated transitions and spline smoothing of price data. uPlot deliberately leaves both out.
- Sound effects on every trade and decorative "bubbles" with no scale legend (Flowsurface/edgedepth). Keep any audio optional and tied to a threshold.
- An indicator catalog that fetches from a remote server inside the app (Flowsurface): it is a supply-chain and privacy surface.
- Server-only panels that sit empty when self-hosted (edgedepth's VPIN). Hide an unavailable panel or give its reason, as Lines already does.
- Big preference dialogs with tabs (Zed is moving to a search-first editor over JSON).
- Dashboards that are monolithic web components (Perspective's viewer chrome). Take the data model, not the toolbar.

## Licence summary

You may reuse **code** (with notices) from: GPUI, uPlot, Lightweight Charts (keep the attribution NOTICE), Hummingbot, Perspective, barter-rs. **Behaviour only** from: the Zed workspace and settings crates (GPL), Flowsurface (GPL), NautilusTrader (LGPL) and edgedepth (AGPL).
