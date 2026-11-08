# Architecture

<!-- Moved from CLAUDE.md (agent workflow, 2026-11-06). Agents: read the sections for every crate you touch. -->

## Crate layout

| Crate | Purpose | May depend on |
|---|---|---|
| `crates/lines-core` | Vendor-neutral types and math: `Price`/`PriceScale`/`UnitConverter`, `Qty`/`QtyFormat`, `Product`/`ProductId`/`ContractKey` (futures, spot, equities, listed options: decision 0028)/`Capabilities` (`instrument.rs`), `calendar` (civil dates, expiry/roll calendars), `latency` (roadmap 6b: allocation-free 60 s latency histograms, the clock-offset bound, the process-wide HUD switch), `columns` (resizable table columns: layout, dividers, drag, auto-fit; fixed arrays, no allocation), `roll` (front-month resolution), `continuous` (display-only continuous series), `sessions` (trading-session calendars: exchange time zone, ETH/RTH, holidays; holiday-table parser), `stitch` (history + live), `chain` (option chains: quotes by expiry and strike, decision 0028), bars, book, `ladder` (DOM rows, grouping, centring), `tape` (T&S ring, `Aggressor`), `vap` (volume at price), `profile` (volume profile: per-session and per-bar volume by price and aggressor, rows/POC/value area), `footprint` (footprint chart cells: per-bar merge over a timeframe bucket, diagonal imbalances, POC; allocation-free), `layout_lint` (UI checks: pure rules over the rectangles a frame drew), `levels` (external level maps: model, validation, VWAP-relative levels, allocation-free placement, `price_at`), `trail` (DOM price trail: decaying buy/sell volume per row on the market clock, fixed ring), `timeframe` (chart timeframes: `Timeframe`, session-aligned `Bucketer`, incremental `Derived` bars, `BarsView`), `ticks` (tick bars: incremental `TickBars` from counted trades, session resets, per-bar volume by price), `drawings` (chart drawings in data space: `Drawing`/`Tool`/`Style`, `TimeAxis`, projection, hit-testing, snapping, measure, `DrawingSet` with undo), `style` (user styles, decision 0026: `LineStyle`/`FillStyle`/`TextStyle`, `Color` token or custom, dash patterns, WCAG contrast, CIEDE2000, the readability guard), viewport, allocation-free geometry. | jiff (time zones only) |
| `crates/lines-indicators` | Indicator engine (decision 0017), pure: `graph` (series nodes keyed by (feed, `Spec`), shared, acyclic, incremental; the `Indicator` trait and its WIT mapping), `series` (`MarketSeries`: stitched bars + exact `i128` trade sums per bucket, exact vs approximate), `vwap` (ETH/RTH/weekly/monthly/anchored, ±1/2/3σ), `delta` (cumulative aggressor delta candles: ETH/RTH/no reset), `render` (allocation-free VWAP columns), `IndicatorConfig` (serde, saved in the chart pane's `ChartState`; its optional `style` is a `PartStyles` JSON map, hence `serde_json`). | lines-core |
| `crates/cqg-proto` | Rust types generated at build time from CQG `.proto` files (`protox` + `prost-build`, no `protoc`). | prost |
| `crates/lines-feed` | `ticks` (per-store tick-bar builders shared by charts, fed the same counted trades as the tape (stored and live), rebuilt from a replay or the local store), `catalog` (products with permanent ids, exchange names, session calendars + the embedded holiday table `data/exchange-holidays.txt`, crate-private provider root symbols, `Target` = product or contract), `availability` (what a source can serve, with a reason for the UI; never looser than the feed's own validation), `provider` (`Provider`, I/O-free `ProviderSession`, `MarketEvent`, `Frame`), `cqg/` (session, mock, symbol map), `coinbase/` (session, symbol map), `kraken/` (spot WebSocket v2 + Futures v1 sessions, own book copy, CRC-32 checksum, exact number parsing, symbol map), `native` (`lines-events-1` encoding of normalized events incl. depth; replay-only provider `lines`), `connection` (generic driver + reconnect backoff), `extension` (decision 0020: `SourceExtension` = CLI name/help + `Provider` + availability rules + a connector returning a `DynTransport`; `Source::Extension`; process-wide registry so replays of an extension's recordings find their decoder), `transport` (WSS + HTTP, replay, recording tee), `recording` (`.lfr` v3, v4 for equity and option contracts), `occ` (OCC option symbols from a `ContractKey`, and back), `recorded` (`RecordedEvents`: a recording or stored session decoded synchronously by its own provider session, for backtests), `store` (`ChartStore`: bars, book, tape, volume at price), `levels/` (decision 0022: `LevelSource` trait (registered through `lines_app::Extensions::with_level_source` or `levels::register`), levels runtime with record/replay, `PriceAtTime`/`price_at`, `demo` synthetic source behind `demo-levels`), `registry` (`FeedRegistry`: one feed per source + contract, shared by refcounted `Subscription`s), `disk/` (the local market-data store, decision 0019: segments, per-contract writer, coverage, bar index, retention, stored-session replay, `orderflow`: the stored trades replayed into the order-flow consumers at startup). Own thread + Tokio runtime per feed. `indicators`: the indicator runtime (one indicator thread running the graph, an `EngineTap` on every feed, `IndicatorHandle`s with lock-free output cells). `synth`: two synthetic ES sessions (optionally with a book) for screenshots and the gallery. `synth_chain`: a synthetic SPY session with a six-expiry option chain (decision 0028) for the chain pane's screenshots. `listen`: a channel of every feed batch (`Arc<[MarketEvent]>`) for other consumers (the trading core). | lines-core, lines-indicators, cqg-proto |
| `crates/lines-app` | Library + `lines` binary. `lib.rs`: CLI and the entry point `run(Extensions)` (decision 0020; `main.rs` passes none, official builds pass their sources and brokers: `with_source`, `with_broker`; broker names are validated before anything runs and only account *counts* are logged), `workspace` (decision 0016: `Workspace` global + one `WindowView` per OS window: tabs, the active tab's panes placed from its `layout`, focus, link groups, crosshair sync, context menu, autosave), `layout` (pure split tree `Layout<T>`: place (only shown leaves), split/close, divider drag, focus neighbours, presets), `tabstate` (pane states shown/hidden/chart-only/detached and the pure decisions for hide, toggle, chart only, pop out, tab close/reopen), `pane_window` (a detached pane's own window), `pane_columns` (DOM/T&S columns: kinds, specs, commands, saved form; header-row painting and divider cursors), `menu` (context menus), `wsdoc` (the workspace document + file format v3; optional per-pane fields such as indicators need no version bump, built-ins `pair`/`6c2d`/presets, pure window-placement and tab-drop rules), `versioned` (versioned JSON: forward migrations, backups, atomic saves), `persist` (config dir, background `Writer`, named workspaces), `commands` (typed commands, one binding table for keymap + menu hints + palette), panes `chart_view` / `dom_view` / `tape_view` (`pane`: each given a `feeds::PaneFeed` + theme, owns its view state; a feed may be *unavailable*: greyed with the reason), `profile` (chart volume profile layer, serialisable `ProfileConfig`), `levels_layer` (external levels on charts, `LevelsConfig`), `chart_draw` (a chart's drawings: drawing mode, selection, drags, notes, measure, styled paint, the open style editor), `drawings_store` (per-product drawings files and the `Drawings` global), `style_doc` (saved form of styles: drawing style fields, per-tool defaults, the color encoding), `style_editor` (the one style editor, GPUI-free: parts as tabs, keys, readability warnings) + `style_editor_view` (its popover), `study_style` (each study's style parts, their saved form and the style editor a study opens from the studies menu), `picker` (symbol search/ranking/settings, GPUI-free) + `picker_view` (its UI, shared by all panes; `AppSettings` global), `text` (glyph-cached number painter), `theme`, `bench` (`--bench`), `logs` (rotating log files, panic hook), `redact` (secrets out of logs and bundles), `diagnostics` (stats sampler, "Export diagnostics" bundle), `zipw` (minimal zip writer), `screenshot` (feature-gated), `layout_rec` (records what panes draw for the layout lint; screenshot builds only), `gallery` (`--gallery`: the scene set). `build.rs` embeds the Windows icon (`assets/icon/lines.ico` as resource 1, which GPUI uses as the window/taskbar icon) and version info, via `embed-resource` pinned to GPUI's version; no-op on other targets. `studies`: a chart's indicators (engine handles, smooth VWAP lines and bands, cumulative delta candles in the sub-pane, legend, Ctrl+I menu). `footprint` (the footprint chart layer, `FootprintConfig`), `subpane` (the sub-pane below the plot: layout, divider drag, value scale). Trading (roadmap 7): `trading` (the `Trading` global: paper core on its own thread, event pump into a `TradeState` cache with per-account revisions, `--paper` scripts), `trade_bar` (a pane's account/qty/bracket/trading-switch state + the DOM's fixed-height trading panel; `TradeDoc` saved on `PaneDoc.trade`), `brackets` (bracket presets per account and product, ticks/points, exact conversion, the inline preset editor; GPUI-free), `tooltip` (hover tooltip with the action's key, plus the accessible role and name), `settings_schema` (decision 0021: the settings table that drives the Settings window, its search, the palette and storage; GPUI-free), `settings_view` (the Settings window, Ctrl+,), `icons` (pane-header and tab icons drawn in code as stroked paths, snapped to device pixels; `icons::button`), `dom_trade` (GPUI-free: click → command, arming, order marks, drag ghosts, rule levels), `dom_trail` (the DOM's price trail column: config, paint, frame requests), `dom_orders` (the orders column, ghosts, position marker, row rejections; allocation-free paint). `journal_view` (the Journal pane: stats strip, trades table on `lines_core::columns`, inline tags/note editor), `journal_shots` (a saved journal's per-trade trade snapshots: SVG drawn on the `lines-shots` thread from a followed store's bars, in `journal/shots/`). `chain_view` (the option chain pane, decision 0028: calls | strike | puts around the underlying's price row, expiry chips, keyboard navigation; reads `ChartData::chains`). `latency_hud` (roadmap 6b: the latency HUD's text, sparklines and clock status in chart headers; per-chart receive → paint). | lines-core, lines-feed, lines-trading |
| `crates/lines-trading` | Trading core, headless (roadmap 6; decisions 0004, 0005, 0006, 0012, 0018). `command` (typed `Command`s in an `Envelope`: id, `Source`, click `Instant`), `event` (`Event` stream out), `order` (lifecycle `OrderState`), `position` (FIFO lots, exact P&L), `risk` (`RiskLimits`), `rules` (prop presets as data, `RuleTracker`), `guardrails` (roadmap 8b: the trader's own limits, `GuardrailTracker`), `broker` (`Broker` trait + `BrokerCaps`), `paper` (`PaperBroker`: latency, fill models, book walk, stops, brackets/OCO, fees, session checks), `core` (`TradingCore`, pure, `now` passed in), `service` (`TradingService` thread with a `Clock`: wall, market (replays) or manual (tests, screenshots; `advance_to` + `sync`); `TradingHandle`; `FeedBridge::follow_store` forwards a running pane feed, seeded from its book), `persist` (versioned trading file), `journal` (roadmap 8c: round trips from fills with exact P&L, MAE/MFE, tags/notes; `stats`; `store`: append-only JSON Lines with a background writer; `JournalObserver` on the service's `Observer` hook), `extension` (decision 0020: `BrokerExtension` = name, label, `BrokerCaps`, an `AccountSource` (accounts with `AccountKind` Simulated/Live/Unknown and a tradable-or-reason), and `open()` returning the `Box<dyn Broker>` the core drives; `validate`: well-formed unique names, `paper`/`replay` reserved), `strategy` (decision 0025: `StrategyId`, `StrategyConfig`, `StrategyLimits`, `StrategyState`; the gate itself is `core/strategy_gate.rs`). No GPUI. | lines-core, lines-feed (market data only), jiff (strategy hours) |
| `crates/lines-strategy` | Strategies (decision 0025), headless. `lib` (the pure `Strategy` trait: `Input`s in, `Decision`s with a reason and at most one `Command` out; `to_tick`), `extension` (`StrategyExtension`: name, label, version, products, required level sources, settings, factory; `validate`), `host` (`StrategyHost`: instances on their own thread, fed by their feed, level sources and core events; pauses on a stale feed or levels, a roll or a panic; a snapshot for the panel), `backtest` (one instance over a recording or stored session plus recorded levels, `TradingCore` + `PaperBroker` + journal on the recorded clock, single-threaded: a deterministic `Report`; `Settings` is the `--backtest-config` file), `example` (`level-touch`: synthetic, no edge). No GPUI. | lines-core, lines-feed, lines-trading |
| `crates/lines-sierra` | **Personal dev tool, never ships.** `lines-import-sierra` binary: Sierra Chart `.scid` + `.depth` → `lines-events` replay file. Workspace member (tested) but not a default member; nothing depends on it. | lines-core, lines-feed |
| `crates/lines-fuzz` | Fuzz harnesses for every decoder of untrusted bytes (`cqg_decode`, `cqg_session`, `coinbase_session`, `native_session`, `kraken_session`, `recording`, `sierra_import`, `disk_store`): each runs the real session + store pipeline. Stable smoke tests over the same harnesses (`tests/smoke.rs`) run in `cargo test --workspace`. Seeds are synthetic, generated by `examples/gen_corpus.rs`, never committed. Workspace member, not a default member. | lines-core, lines-feed, lines-sierra |
| `fuzz` | cargo-fuzz targets (`fuzz_targets/`, one line each, calling `lines-fuzz`). Its **own workspace**, excluded from the root, so `libfuzzer-sys` (NCSA) never enters the product graph or `cargo deny`. Nightly only. | lines-fuzz, libfuzzer-sys |

`third_party/cqg-webapi` is a git submodule (github.com/cqg/WebAPIPythonSamples); only `proto/` is used. Protos are not vendored (decision 0010).

## Build / test / run

```sh
git submodule update --init          # required: cqg-proto build fails without it
cargo build                          # first build ~6 min (GPUI)
cargo test --workspace               # offline
cargo test -p lines-feed --test history -- --ignored   # live Coinbase: stitch vs Coinbase's candles, level2 depth, aggressor (~1.5 min)
cargo test -p lines-feed --test kraken -- --ignored --nocapture   # live Kraken BTC/USD: 60 s, zero checksum mismatches, replay determinism
cargo test -p lines-feed --test depth_stress -- --nocapture   # deep book at max speed: DOM work per frame bounded
cargo test -p lines-feed --test feed -- --ignored --nocapture   # perf: the CQG mock sustains 5k updates/s (quiet machine only)
cargo test -p lines-feed --test history store   # local store: torn tail, concurrent read, gaps, retention, cached startup, stale
cargo test --release -p lines-indicators --test bench -- --ignored --nocapture   # indicator CPU per tick, 6 charts x 2 VWAPs
cargo test --release -p lines-trading --test session -- --ignored --nocapture   # perf: click->order-sent p50/p99 budget (< 1 ms), run alone
cargo test --release -p lines-app --lib -- --ignored dom_click_latency_budget --nocapture   # perf: DOM click->order sent, run alone
# Drawings (screenshots): --drawings FILE (a drawings file naming its product; screenshots never read saved drawings);
#   --style-editor ID[:TAB[:COL[:ROW]]] (the first chart's style editor open on drawing ID of that file, on tab TAB, 0 = first, with the keyboard focus on control COL of row ROW, both 0 = first, ROW defaulting to the first row: `3`, `4:1`, `3:0:1`, `3:0:1:4`).
#   In the app: Alt+H/T/Z/L/N/U, Alt+Shift+H/T, Esc, Delete, Enter (pin measure), Ctrl+Z / Ctrl+Y; palette "Drawings: …".
# Timeframe: --timeframe 5m (1m 2m 3m 5m 10m 15m 30m 1h 2h 4h 1d 1w; or bars from counted trades: tick 500t (presets 100t 233t 500t
#   1000t 2000t, any 1t-100000t), volume 500v (whole contracts, 1v-1000000v), range 10r (price ticks, 1r-10000r), seconds
#   30s (dividing the day, 1s-3600s); first chart; a saved one wins). In the app: type 5 / 1h / 500t / 500v / 10r / 30s +
#   Enter on a chart, or the header chip.
#   --tick-continuous: tick, volume, range and seconds bars run across sessions (no reset at the session open; a saved mode wins).
# VWAP: --vwap eth|rth|week|month|anchor@TIME, optional bands after a slash ("eth/1,2,3", quoted in PowerShell); repeatable; first chart; also in screenshots. Ctrl+I in the app.
LINES_SIERRA_DATA="C:\SierraChart\SierraChart - Apex\Data" cargo test -p lines-sierra -- --ignored   # real Sierra data, ~1 min
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy --workspace --all-targets --all-features -- -D warnings   # includes screenshot + vendor-cqg
cargo test --workspace --features lines-app/vendor-cqg,lines-feed/vendor-cqg   # tests must pass with and without it
cargo fmt --all --check
cargo test -p lines-app --features screenshot --test screenshot_exit   # failures exit non-zero
# Fuzzing (nightly + cargo-fuzz, Linux/WSL; scheduled in .github/workflows/fuzz.yml, never on PRs):
cargo run -p lines-fuzz --example gen_corpus -- fuzz/corpus     # synthetic seeds (gitignored)
cargo +nightly fuzz run cqg_session fuzz/corpus/cqg_session -- -max_total_time=300
LINES_FUZZ_SMOKE_ITERS=20000 cargo test -p lines-fuzz --test smoke   # stable, more mutations
# macOS: install Xcode, or build with `--features runtime-shaders` (GPUI compiles Metal shaders
# at startup instead of with `xcrun metal`, which only full Xcode has). Build-time is the default.

cargo run -p lines-app -- --source mock                          # CQG-protocol mock (ES only), 5k updates/s
# The app restores the autosaved workspace (<config dir>/workspace.json); first run: `pair`
# (NQ | ES, each chart + DOM + T&S; on the mock NQ shows greyed "mock serves ES only").
cargo run -p lines-app -- --workspace 2x2                        # built-ins: pair trading chart 1x2 2x2 1+2 6c2d, or a saved name
cargo run -p lines-app -- --source mock --disconnect-every 10    # reconnect + back-fill
cargo run -p lines-app -- --source coinbase --instrument BTC/USD # live; --instrument/--layout: this run only, not autosaved
cargo run --release -p lines-app -- --bench 20                   # 6 charts + 2 DOMs: frame time + memory; exit 2 if a hidden tab painted
cargo run --release -p lines-app -- --bench 20 --bench-hide-doms # same with the DOMs hidden: exit 2 if they painted
# The bench draws demand-driven (only when a view notified) and prints frames/s, paints/s per pane kind and process CPU.
lines --source mock --headless 3 --record recordings/idle.lfr   # then: nothing may repaint once the replay has finished (exit 3)
cargo run --release -p lines-app -- --source replay --replay recordings/idle.lfr --speed 0 --layout trading --bench 10 --bench-expect-idle
# Compare benchmarks A/B by alternating binaries in one session: other threads' builds load the CPU.
cargo run -p lines-app -- --source kraken --instrument BTC/USD.KR                      # live BTC/USD.KR (or ETH/USD.KR, BTC-PERP.KR, ETH-PERP.KR)
cargo run -p lines-app -- --source kraken --headless 60          # prints book checksums ok / mismatches / resyncs
cargo run -p lines-app -- --source mock --headless 5 --record recordings/x.lfr
cargo run -p lines-app -- --source replay --replay recordings/x.lfr --speed 10   # 0 = max speed
cargo run -p lines-app -- --source replay --replay recordings/x.lfr --at 2026-11-06T14:05:00Z
# Real CQG (never run: no credentials). Only with the off-by-default `vendor-cqg` feature;
# public builds have no `--source cqg` at all. Defaults: demo server, label CQG, app id WebAPITest.
LINES_CQG_USER=... LINES_CQG_PASSWORD=... cargo run -p lines-app --features vendor-cqg -- --source cqg --instrument ES

# Sierra Chart import (personal dev tool; output stays in gitignored recordings/).
cargo run --release -p lines-sierra -- --data "C:\SierraChart\SierraChart - Apex\Data"   --symbol NQZ5.CME --date 2025-09-11 --out recordings/nqz5-2025-09-11.lfr
cargo run --release -p lines-sierra --example scid_markers -- <file.scid> <YYYY-MM-DD>   # marker diagnostics

# Screenshot mode (decision 0009). Replays at full speed to --at, renders off-screen, writes PNG, exits.
cargo run -p lines-app --features screenshot -- --replay recordings/x.lfr \
  --screenshot out.png --at 2026-11-06T14:05:00Z --size 1280x800 --theme dark --scale 1
# --drag-position Y previews the position line dragged to plot-local Y (the stop or target it would set, with its P&L; nothing is placed).
# --order-size N starts every symbol at that order size; --max-order-qty N sets the paper accounts' largest allowed order (a larger size shows "max N").
# --order-strip shows the order strip on the first chart; --strip-pick buy|sell shows it with that limit's price pick armed.
# --cursor X,Y adds a fixed pointer (logical px from the plot's top-left) to show the crosshair; --magnet snaps it to OHLC.
# README images (docs/img/): synthetic data only (decision 0010); workspaces in docs/img/*.json:
#   cargo run -p lines-feed --example synth_session -- recordings/synth.lfr --depth
#   P="buy 2 @ 6021.00 bracket 8/16; ack; buy 1 @ 6017.00; ack"
#   (`lines` here is a `--features screenshot` build; release binaries can't take screenshots)
#   lines --replay recordings/synth.lfr --workspace docs/img/footprint-trading.json --size 1600x900 --theme dark --paper "$P" --enable-trading --screenshot hero.png
#   lines --replay recordings/synth.lfr --workspace docs/img/trading-5m.json --size 1600x900 --theme light --paper "$P" --enable-trading --screenshot light.png
#   lines --replay recordings/synth.lfr --layout chart --timeframe 500t --vwap "eth/1,2" --size 1200x700 --theme dark --screenshot tick.png
#   then quantize each (hero → chart-orders-footprint.png, light → light.png, tick → tick-chart.png):
#   magick hero.png -strip +dither -colors 256 PNG8:docs/img/chart-orders-footprint.png
# Colours: --theme dark|light, --palette standard|cb (colourblind: blue up, orange down),
# --hollow up|down (which direction's candle bodies are hollow; default up). Also for the live app, where
# they win over Settings › Appearance for that run.
#   --settings SECTION (also render the Settings window into <out stem>-settings.png), --settings-search TEXT,
#   --settings-set KEY=VALUE (repeatable; e.g. guard.max_daily_loss=1500 shows a loosening's confirmation).
#   --econ-calendar FILE (an economic calendar in the bundled file's format, read instead of the bundled one by
#   the app and its trading cores: synthetic releases on synthetic data). Trading scripts: `guard news warn|block|flatten`.
# --price-zoom F zooms the auto-fitted price range by F (as Ctrl+wheel), i.e. manual price scale.
# Layout: --layout trading (default: chart | DOM over time & sales) or chart, or --workspace NAME
# (a built-in, e.g. `pair`, or a workspace .json file; screenshots never read saved workspaces).
# The first window is drawn; panes the recording can't serve show greyed with the reason. Its
# popped-out panes are drawn into <out stem>-pane-<n>.png at their saved window size.
# --dom-grouping N (ticks per row; G / Shift+G cycle 1 2 5 10 25 in the app), --large-print SIZE.
# --dom-trail: DOMs start with the price trail on (a DOM's saved `trail` wins). Evidence from synthetic data:
#   lines --replay recordings/synth-es-2026-11-05.lfr --screenshot out.png --at 2026-11-05T14:52:00Z --size 1400x800 --dom-trail
# Local market-data store (decision 0019): on by default for coinbase, kraken, cqg and extensions with a local
# store, never for mock, replay or screenshots unless --store-dir names one. Location: LINES_DATA_DIR, else %LOCALAPPDATA%\Lines (macOS ~/Library/Application Support/Lines,
# Linux $XDG_DATA_HOME/lines). --no-store, --store-depth ES,NQ (depth off by default), --store-budget-gb 4.
# --orderflow-sessions N: at startup replay the stored trades of the newest N stored sessions into the profile,
# footprint, delta, VWAP, DOM volume and T&S (default 2, 0 = off). --store-dir DIR: use the store under DIR for any
# source (mock, replays and screenshots included; the run writes into DIR, so give each screenshot a fresh
# copy; evidence with a synthetic stored session):
#   cargo run -p lines-feed --example synth_session -- recordings/synth-day1.lfr --day 1   # (and --day 2)
#   lines --source replay --replay recordings/synth-day1.lfr --speed 0 --headless 20 --store-dir recordings/of-store
#   lines --replay recordings/synth-day2.lfr --store-dir <a copy of of-store> --screenshot out.png --at 2026-11-04T23:05:00Z
# A stored contract (directory) or day (.lseg) replays like a recording:
#   lines --source replay --replay "%LOCALAPPDATA%\Lines\store-v1\coinbase\BTC-USD" --speed 0
# Startup timing: lines --source coinbase --headless 20 prints first/newest bars and history-complete times.
# DOM/T&S evidence from synthetic data: record the mock (it has depth), then shoot the replay:
#   lines --source mock --headless 6 --record recordings/dom-mock.lfr
#   lines --replay recordings/dom-mock.lfr --screenshot out.png --size 1600x900 --dom-grouping 2 --large-print 6
# --picker "nq" renders the symbol picker open on that query (screenshots never read or write user settings).
# Backtest a strategy (decision 0025), headless: --backtest NAME --replay FILE|DIR [--replay-levels DIR]
#   --backtest-config settings.json [--report out.json]. The public build has only the example `level-touch`.
# Run a strategy through the host on this run's feed (live or replay), paper only, headless:
#   --run-strategy NAME --backtest-config settings.json --headless SECONDS [--source replay --replay FILE --speed N]
# Run a strategy in the app, paper only, on the feed's contract (it starts stopped: the strategy panel's Start
# arms it; palette "Open strategies"; Stop all = Ctrl+Shift+R): --strategy NAME --backtest-config settings.json
# Screenshots of the strategy panel on synthetic instances: --strategies-demo.
# External levels (decision 0022): --record-levels DIR / --replay-levels DIR (with --replay); a synthetic source
# for screenshots: build with --features screenshot,demo-levels and pass --demo-levels (ES/MES):
#   lines --replay recordings/synth-es-2026-11-05.lfr --at 2026-11-05T19:00:00Z --layout chart --price-zoom 0.3 --demo-levels --screenshot out.png
# UI gallery (docs/design.md "Checking a UI change"): --gallery DIR [--gallery-full] renders the scene set from synthetic
#   data into DIR and runs the layout lint on each (exit 1 on a finding); --layout-report (with --screenshot) writes
#   the lint's findings beside the PNG as <stem>.lint.txt.
# Header dropdowns open (screenshots): --order-group-menu (the first chart's first group tag's list of orders), --timeframe-menu (the first chart's timeframe list), --account-menu (every
#   chart's and DOM's account list; needs an account chip, i.e. trading running, as in screenshot mode).
# Studies menu (screenshots): --studies-menu open | edit:STUDY | style:STUDY[:TAB] (the style editor on its parts), STUDY = vwap:N | delta:N | profile | footprint | levels:ID; --study-set FILE (a StudySet JSON).
# The chain's ticket (screenshots): --chain-ticket "buy|sell STRIKE call|put [at PRICE] [close] [one-click]", with
# --paper "option 2026-11-09 682 call; buy 2 @ 1.89; ack" for orders on a series.
# Paper trading evidence (in-memory core, manual clock): --paper "account paper-eval; buy 2 mkt; ack; buy 1 @ 5807.00;
#   buy 1 @ 5808.00 bracket 4/8; ack; buy 1 @ 5806.50" (orders after the last `ack` stay pending), --enable-trading (DOMs start with trading enabled; `--arm` is the old name), --enable-hotkeys (DOMs start with order hotkeys on). --pnl off|ticks|currency|both.
#   --bracket-units ticks|points (every account), --bracket-editor (DOMs open their bracket-preset editor), --hover-header (the focused pane's header controls show as if hovered).
#   --bracket-position on|ask: one bracket for the whole position (on: saved for every account on the recording's product; ask: DOMs switch to it as the trader would, after --paper, showing the confirmation when exits work).
#   `wait SECONDS` and `print PRICE [QTY]` (a market trade only the trading core sees) make round trips on a finished
#   replay, e.g. for the journal: --workspace FILE with a `journal` pane, --paper "buy 2 mkt; ack; print 6011.50; wait 20;
#   sell 1 @ 6013.50; ack; print 6013.75; wait 25; sell 1 mkt; ack"
#   --journal-dir DIR: keep that journal in DIR/screenshot.jsonl, so the script's trades get trade snapshots in DIR/shots/ (evidence; in memory without it).
# Study visibility (screenshots): --hide-study-labels, --hide-studies, --hide-hints (both of the first and last = Clean chart).
# Volume profile (Alt+V/R/B/G in the app): --profile off|session|visible|composite, --profile-session eth|rth,
# --profile-sessions N, --profile-va PCT, --profile-split, --profile-grouping N. Evidence from synthetic sessions:
#   cargo run -p lines-feed --example synth_session -- recordings/synth-es-2026-11-05.lfr   # 2 ES sessions, deterministic
#   ... -- recordings/synth-es-transit.lfr --transit   # the same trades received 2-40 ms late (latency HUD shots)
#   (`lines_feed::synth`; add --depth for a 10-level book around every trade, as the gallery uses)
#   lines --replay recordings/synth-es-2026-11-05.lfr --screenshot out.png --at 2026-11-05T19:00:00Z --size 1600x900 #     --layout chart --price-zoom 0.45 --profile session --profile-session rth
```

Catalog products (ids are permanent): `ES` 1, `NQ` 2, `ZN` 3, `MES` 4, `MNQ` 5, `BTC/USD` 1001, `ETH/USD` 1002 (Coinbase), `BTC/USD.KR` 1003, `ETH/USD.KR` 1004 (Kraken spot), `BTC-PERP.KR` 1101, `ETH-PERP.KR` 1102 (Kraken Futures perpetuals). A crypto pair is its exchange's own market, so each exchange's pair is its own product. Contracts are computed, never listed: `ContractKey { product, delivery }`, with `Delivery::Monthly { year (4-digit), month }` for futures and `Delivery::Spot` for spot products and perpetual swaps (one contract that never expires or rolls; no separate perpetual variant is needed). Canonical label: `ESZ26` (2-digit year); `--instrument` takes a product (`ES`, followed to its front month) or a contract (`ESZ26`, `ESZ2026`). Provider symbols are derived by rule: CQG root + month code + last year digit (ES→`EP`, so `EPZ6`; NQ→`ENQ`, ZN→`TYA`; only `EP` verified, via the mock; MES/MNQ unmapped). Coinbase maps `BTC-USD`, `ETH-USD` (spot). Kraken maps spot `BTC/USD` (REST `XBTUSD`), `ETH/USD`, and Futures `PF_XBTUSD`, `PF_ETHUSD`. Sierra maps roots ES, NQ, MES, MNQ (`NQZ5.CME`, `NQH25-CME`, `NQH25_FUT_CME`). The mock resolves `EP` and any `EP` contract symbol.

`recordings/`, `*.lfr`, `*.scid`, `*.depth` are gitignored. Recordings may contain licensed data; committed fixtures must be synthetic (tests build them in code).

## Pinned versions

- Rust `1.98.1` (`rust-toolchain.toml`, matches the pinned Zed commit)
- `gpui` + `gpui_platform`: git `zed-industries/zed` rev `db43f05e398036f08e544c03cbc068216c7a0aaa`. Bump both together. crates.io `gpui 0.2.2` (Oct 2025) has a different API.
- CQG protos: submodule commit `6c92c8f80c9d0701468875bc083098d9ff21b185` = WebAPI protocol 2.230
- prost / prost-build / prost-types 0.14, protox 0.10, tokio 1.54, tokio-tungstenite 0.30, rustls 0.23 (`ring`), rustls-native-certs 0.8, ureq 3.5 (rustls/`ring`, webpki roots), serde_json 1 (`raw_value`), jiff 0.2 (`std` + `tzdb-bundle-always`: tz database compiled in, never the OS's), image 0.25 (png, screenshot feature only)
- No `aws-lc` or OpenSSL in the dependency tree.

## Public/private boundary and rules

- The repo is public under FSL-1.1-ALv2 (`LICENSE.md`, decision 0014). Never commit credentials, account IDs, vendor usernames, vendor protos/kits, or anything derived from licensed data, including in screenshots and docs.
- Live vendor connectors (Rithmic, CQG production) belong in a private module (decision 0014). Public code may depend only on the provider/broker traits. They plug in through `lines_feed::extension::SourceExtension`, `lines_trading::extension::BrokerExtension` and `lines_app::run(Extensions)` (decision 0020); no vendor name appears in public code beyond the extension API, and the public binary registers none (`crates/lines-app/tests/extension.rs` runs a dummy source headless, records and replays it; `tests/broker_extension.rs` registers a dummy broker; `lines-trading/tests/extension.rs` drives one through the core).
- Public builds ship only mock, replay, Coinbase and paper (`docs/release.md`). The real CQG connection (`Source::Cqg`, `--source cqg`, `--cqg-url`/`--private-label`/`--client-app-id`, the `LINES_CQG_*` env vars) exists only with the off-by-default `vendor-cqg` feature on `lines-feed`/`lines-app`. The CQG mock, `CqgSession` and `cqg-proto` stay public (mock and CQG-format replays need them). Never add `vendor-cqg` to default features or release builds.
- Never drive the real mouse or keyboard or capture the screen. Verify UI with screenshot mode and unit tests; the owner checks live behaviour.
- `cargo` may not be on PATH: use `~/.cargo/bin/cargo.exe`.
