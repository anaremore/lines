# Lines

Desktop futures charting terminal. Rust + GPUI. Market data through provider adapters: CQG WebAPI (protobuf/WSS; mock + untested live), the Coinbase Exchange public feed (JSON/WSS + REST, dev tool), and replays of imported Sierra Chart data (personal dev tool).

## Docs

- `ROADMAP.md`: milestones and status. Update the status in the change that finishes a milestone.
- `docs/decisions/`: decision records. Accepted ones are binding; supersede, never rewrite.
- `docs/briefs/`: task briefs for upcoming milestones (e.g. the Sierra Chart importer).
- `docs/design.md`: UI design principles and budgets. Binding for every UI change.
- `docs/positioning.md`: who Lines is for, how it differs, what's out of scope, published benchmarks.

## Crate layout

| Crate | Purpose | May depend on |
|---|---|---|
| `crates/lines-core` | Vendor-neutral types and math: `Price`/`PriceScale`/`UnitConverter`, `Qty`/`QtyFormat`, `Product`/`ProductId`/`ContractKey`/`Capabilities` (`instrument.rs`), `calendar` (civil dates, expiry/roll calendars), `roll` (front-month resolution), `continuous` (display-only continuous series), `stitch` (history + live), bars, book, `ladder` (DOM rows, grouping, centring), `tape` (T&S ring, `Aggressor`), `vap` (volume at price), viewport, allocation-free geometry. | nothing |
| `crates/cqg-proto` | Rust types generated at build time from CQG `.proto` files (`protox` + `prost-build`, no `protoc`). | prost |
| `crates/lines-feed` | `catalog` (products with permanent ids, exchange names, crate-private provider root symbols, `Target` = product or contract), `availability` (what a source can serve, with a reason for the UI; never looser than the feed's own validation), `provider` (`Provider`, I/O-free `ProviderSession`, `MarketEvent`, `Frame`), `cqg/` (session, mock, symbol map), `coinbase/` (session, symbol map), `native` (`lines-events-1` encoding of normalized events incl. depth; replay-only provider `lines`), `connection` (generic driver + reconnect backoff), `transport` (WSS + HTTP, replay, recording tee), `recording` (`.lfr` v3), `store` (`ChartStore`: bars, book, tape, volume at price), `registry` (`FeedRegistry`: one feed per source + contract, shared by refcounted `Subscription`s). Own thread + Tokio runtime per feed. | lines-core, cqg-proto |
| `crates/lines-app` | `lines` binary: CLI, GPUI window, `workspace` (renders a `layout` split tree of panes, forwards symbol changes within link groups), panes `chart_view` / `dom_view` / `tape_view` (`pane`: each given a `feeds::PaneFeed` + theme, owns its view state), `text` (glyph-cached number painter), `picker` (symbol search/ranking/settings, GPUI-free), `theme`, `screenshot` (feature-gated). `build.rs` embeds the Windows icon (`assets/icon/lines.ico` as resource 1, which GPUI uses as the window/taskbar icon) and version info, via `embed-resource` pinned to GPUI's version; no-op on other targets. | lines-core, lines-feed |
| `crates/lines-sierra` | **Personal dev tool, never ships.** `lines-import-sierra` binary: Sierra Chart `.scid` + `.depth` → `lines-events` replay file. Workspace member (tested) but not a default member; nothing depends on it. | lines-core, lines-feed |

`third_party/cqg-webapi` is a git submodule (github.com/cqg/WebAPIPythonSamples); only `proto/` is used. Protos are not vendored (decision 0010).

## Build / test / run

```sh
git submodule update --init          # required: cqg-proto build fails without it
cargo build                          # first build ~6 min (GPUI)
cargo test --workspace               # offline
cargo test -p lines-feed --test history -- --ignored   # live Coinbase: stitch vs Coinbase's candles, level2 depth, aggressor (~1.5 min)
cargo test -p lines-feed --test depth_stress -- --nocapture   # deep book at max speed: DOM work per frame bounded
LINES_SIERRA_DATA="C:\SierraChart\SierraChart - Apex\Data" cargo test -p lines-sierra -- --ignored   # real Sierra data, ~1 min
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy --workspace --all-targets --all-features -- -D warnings   # includes screenshot + vendor-cqg
cargo test --workspace --features lines-app/vendor-cqg,lines-feed/vendor-cqg   # tests must pass with and without it
cargo fmt --all --check
cargo test -p lines-app --features screenshot --test screenshot_exit   # failures exit non-zero
# macOS: install Xcode, or build with `--features runtime-shaders` (GPUI compiles Metal shaders
# at startup instead of with `xcrun metal`, which only full Xcode has). Build-time is the default.

cargo run -p lines-app -- --source mock                          # CQG-protocol mock, ES, 5k updates/s
cargo run -p lines-app -- --source mock --disconnect-every 10    # reconnect + back-fill
cargo run -p lines-app -- --source coinbase                      # live BTC/USD (or --instrument ETH/USD)
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
# --cursor X,Y adds a fixed pointer (logical px from the plot's top-left) to show the crosshair.
# README hero (assets/readme/lines-dark.png): synthetic mock data only (decision 0010):
#   lines --source mock --headless 5 --record recordings/readme-mock.lfr
#   lines --replay recordings/readme-mock.lfr --screenshot assets/readme/lines-dark.png --size 1600x900 --theme dark --cursor 700,420 --large-print 8
# Colours: --theme dark|light, --palette standard|cb (colourblind: blue up, orange down),
# --hollow up|down (which direction's candle bodies are hollow; default up). Also for the live app.
# --price-zoom F zooms the auto-fitted price range by F (as Ctrl+wheel), i.e. manual price scale.
# Layout: --layout trading (default: chart | DOM over time & sales) or chart. Also in screenshots:
# --dom-grouping N (ticks per row; G / Shift+G cycle 1 2 5 10 25 in the app), --large-print SIZE.
# DOM/T&S evidence from synthetic data: record the mock (it has depth), then shoot the replay:
#   lines --source mock --headless 6 --record recordings/dom-mock.lfr
#   lines --replay recordings/dom-mock.lfr --screenshot out.png --size 1600x900 --dom-grouping 2 --large-print 6
# --picker "nq" renders the symbol picker open on that query (screenshots never read or write user settings).
```

Catalog products (ids are permanent): `ES` 1, `NQ` 2, `ZN` 3, `MES` 4, `MNQ` 5, `BTC/USD` 1001, `ETH/USD` 1002. Contracts are computed, never listed: `ContractKey { product, delivery }`, with `Delivery::Monthly { year (4-digit), month }` for futures and `Delivery::Spot` for spot products (one perpetual contract, never rolls). Canonical label: `ESZ26` (2-digit year); `--instrument` takes a product (`ES`, followed to its front month) or a contract (`ESZ26`, `ESZ2026`). Provider symbols are derived by rule: CQG root + month code + last year digit (ES→`EP`, so `EPZ6`; NQ→`ENQ`, ZN→`TYA`; only `EP` verified, via the mock; MES/MNQ unmapped). Coinbase maps `BTC-USD`, `ETH-USD` (spot). Sierra maps roots ES, NQ, MES, MNQ (`NQZ5.CME`, `NQH25-CME`, `NQH25_FUT_CME`). The mock resolves `EP` and any `EP` contract symbol.

`recordings/`, `*.lfr`, `*.scid`, `*.depth` are gitignored. Recordings may contain licensed data; committed fixtures must be synthetic (tests build them in code).

## Rules

- The repo is public under FSL-1.1-ALv2 (`LICENSE.md`, decision 0014). Never commit credentials, account IDs, vendor usernames, vendor protos/kits, or anything derived from licensed data, including in screenshots and docs.
- Live vendor connectors (Rithmic, CQG production) belong in a private module (decision 0014). Public code may depend only on the provider/broker traits.
- Public builds ship only mock, replay, Coinbase and paper (`docs/release.md`). The real CQG connection (`Source::Cqg`, `--source cqg`, `--cqg-url`/`--private-label`/`--client-app-id`, the `LINES_CQG_*` env vars) exists only with the off-by-default `vendor-cqg` feature on `lines-feed`/`lines-app`. The CQG mock, `CqgSession` and `cqg-proto` stay public (mock and CQG-format replays need them). Never add `vendor-cqg` to default features or release builds.
- Never drive the real mouse or keyboard or capture the screen. Verify UI with screenshot mode and unit tests; the owner checks live behaviour.
- `cargo` may not be on PATH: use `~/.cargo/bin/cargo.exe`.

## Pinned versions

- Rust `1.98.1` (`rust-toolchain.toml`, matches the pinned Zed commit)
- `gpui` + `gpui_platform`: git `zed-industries/zed` rev `db43f05e398036f08e544c03cbc068216c7a0aaa`. Bump both together. crates.io `gpui 0.2.2` (Oct 2025) has a different API.
- CQG protos: submodule commit `6c92c8f80c9d0701468875bc083098d9ff21b185` = WebAPI protocol 2.230
- prost / prost-build / prost-types 0.14, protox 0.10, tokio 1.54, tokio-tungstenite 0.30, rustls 0.23 (`ring`), rustls-native-certs 0.8, ureq 3.5 (rustls/`ring`, webpki roots), serde_json 1 (`raw_value`), image 0.25 (png, screenshot feature only)
- No `aws-lc` or OpenSSL in the dependency tree.

## Invariants (enforced by tests)

- Prices are vendor-neutral: `Price(i64)` is an integer count of the *catalog product's* price unit, whose size is an exact rational `PriceScale` defined once in `catalog.rs` (ES/NQ/MES/MNQ: 1/100, tick 25 units). Quantities are `Qty(i64)`, a count of `10^-decimals` of the base unit (futures: decimals 0). No core type is in any vendor's units.
- Each provider converts its wire values exactly at its edge. CQG converts `scaled_price` with `UnitConverter`, from `correct_price_scale` turned into an exact rational. Coinbase parses decimal text, and REST numbers via `RawValue`. Sierra converts `f32 / multiplier` with an f32-relative tolerance, and the result must be on the tick grid. Values that aren't exactly representable are counted (`MarketEvent::Rejected`, import report), never rounded. CQG metadata whose tick disagrees with the catalog is a fatal `BadMetadata`.
- Vendor symbols exist only inside `lines-feed` (the crate-private `catalog::roots` table and `src/{cqg,coinbase}`) and the dev-only `lines-sierra`. The UI sees `Product`, `ContractKey`, `Capabilities` and the provider id. Bid/ask display is capability-gated.
- `ProviderSession`s do no I/O. HTTP responses arrive as `FrameKind::Http` frames carrying `URL
body` (pre-URL recordings: body only), so recordings contain them and sessions interpret a response by its URL, not their own request plan. Replay drives the same session code; its clock is the recorded frame time (the first recorded frame before any delivery).
- `.lfr` is format **v3**: header = provider, protocol, catalog product symbol, contract key. v2 files still read, with the contract unknown (shown as "contract unknown"; asking a v2 file for a specific contract is an error). v1 is rejected. Replay refuses unknown providers or protocols and product/contract mismatches. A recording holds exactly one contract: if the front month rolls mid-recording, recording stops. Imported data uses provider `lines` / protocol `lines-events-1`. Each frame is one atomic batch, depth included, so the store applies it under one lock and checks for a crossed book only at frame boundaries.
- Sierra import is deterministic (same inputs → byte-identical file). Trades are checked against the book at emit time, never against look-ahead state. Sweep fills (Sierra unbundled trades) are not expected at the touch.
- Products vs contracts (decision 0013): the store is keyed by contract (`ChartData::contract`). `MarketEvent::ContractChanged(key)` retires the old contract's bars (kept, unadjusted, for continuous charts), never mixes them. Front month = `roll::resolve_front`: volume crossover when prior-session volumes are given, else the product's calendar rule, never backwards, never an expired contract. Live feeds following a product re-resolve on every (re)connect. Nothing rolls positions.
- Continuous series are display-only (`continuous::build`): unadjusted by default, difference back-adjust as an option (exact, on the tick grid), no ratio adjustment.
- Sierra contract from the file name: 2-digit years are 20YY; a 1-digit year is the earliest matching year not before the file's last record.
- History + live stitch (`lines_core::stitch`, roadmap 4): a history snapshot is bars plus a *closed* `as_of` (includes every trade with `time <= as_of`, none after; CQG's `up_to_utc_time` definition). Live trades with `time <= as_of` are dropped as already counted; later ones are added; trades applied before a late snapshot overwrote their bar are re-applied from a bounded journal (500k trades while history is outstanding, then the newest 3 bars). Open/close follow trade time, not arrival order. Providers must supply a closed `as_of`: CQG `up_to_utc_time` (the mock never stamps a trade at or before its last snapshot); Coinbase candles closed at the bucket end, the forming bar rebuilt from REST trades de-duplicated against websocket trades by `trade_id`. REST lags the websocket by seconds, so id holes (REST newest .. `last_match`, and matches dropped mid-stream) are fetched by id (`trades?after=hi+1`), retried on the keepalive tick, and reported as a history gap after 6 attempts; history is `Complete` only once the REST-to-websocket hole is closed. Every (re)connect requests history from the newest stored bar; the window is `FeedConfig::history_ms` (default 3 days). `ChartData::history` (`Loading`/`Complete`/`Incomplete`/`Unsupported`) is shown as header text.
- All CQG `sint64` times are ms offsets from `LogonResult.base_time`.
- Depth (roadmap 5): `MarketEvent::BookClear`/`BookLevel` (new total per price, 0 removes); one frame is one atomic batch. CQG subscribes at `LEVEL_TRADES_BBA_DOM` and maps `TYPE_BID`/`TYPE_ASK` quotes; a snapshot clears the book. The CQG mock keeps a 10-level, one-tick-wide book that shifts with its price walk and is never crossed at a frame boundary. Coinbase uses the public `level2_batch` channel (`level2` needs authentication). Trades carry `aggressor: Option<Aggressor>`: CQG `sales_condition` (HIT/SELL_SIDE → sell, TAKE/BUY_SIDE → buy), Coinbase from the maker side (opposite), Sierra from the import.
- The store's `tape` (8192 prints) and `vap` (volume at price since data start for the contract) take only trades the stitch counts as new, so a re-delivered trade is never counted twice. Coinbase delivers the forming bar's REST trades as a closed snapshot (every REST trade before the newest REST timestamp `t_n`, `as_of = t_n - 1 ms`; trades at `t_n` follow as trades), so a reconnect inside the same minute replaces the bar instead of adding to it.
- DOM ladder (`lines_core::ladder`): rows are integer ticks × grouping (`row = floor(price / (grouping × tick))`); grouping only sums exact `Qty`, never rounds. `build_ladder` scans only the visible price range into a fixed-capacity buffer: per frame it visits at most `3 × rows × grouping` book/volume levels whatever the book depth (`tests/depth_stress.rs` asserts this at max replay speed, plus builds paced by the 8 ms UI tick, not the feed). Centring follows the inside market when it leaves the middle half of the rows, never while the pointer is over the ladder; C re-centres. Locked/crossed books are labelled and outlined, not only tinted.
- Panes (`lines-app/src/pane.rs`): chart, DOM and T&S are independent; each is given (`PaneFeed`, theme), owns its view state, polls only its own store's revision and is rendered as a cached view, so a tick on one market repaints only the panes showing it. Panes never reference each other; a symbol switch emits `FeedChanged` and the workspace forwards it to the pane's link group. Feeds are shared through `FeedRegistry` keyed by (source, product, contract, follows front month); the last `Subscription` dropped stops the feed on a background thread. The layout is a value (`layout::Layout` split tree; default `Layout::trading()`).
- Chart layers (`chart_view::ChartLayer`): `prepare` under the store lock after the viewport update, `paint` inside the plot after the candles; neither may allocate. The seam for the volume profile.
- Crosshair hit-testing and snapping are pure (`lines_core::geometry::crosshair`). Input handlers store only the pointer position; paint hit-tests after the frame's viewport update. Lines use device-pixel snapping: wicks, time gridlines and the crosshair's vertical line sit on device-pixel centres, and candle bodies are an odd number of device pixels wide.
- No heap allocation in our paint path (`lines-core/tests/no_alloc.rs`, counting allocator: chart, and DOM/T&S rows and formatting). Chart label text is shaped once per value and cached; a cache miss allocates. DOM and T&S numbers are painted from per-character glyphs shaped once (`text::Glyphs`), so new values don't allocate. GPUI allocates internally per frame.
- The UI never blocks on the feed. Sessions decode outside the lock, the store applies events under a short lock, the UI polls an atomic revision every 8 ms and `try_lock`s in paint.
- Transport `recv` must be cancel-safe.
- macOS always builds `gpui_platform` with `font-kit` (target-specific dependency in `lines-app`); without it GPUI's no-op text system renders no text. `lines-app/tests/text_system.rs` (no harness, main thread) asserts the platform text system lists fonts and shapes glyphs.
- Screenshot mode exits from inside the app with its own code (quitting the app exits 0 on macOS); every failure exits non-zero with no file.
- Screenshot mode: repeated runs with the same inputs on one machine produced byte-identical PNGs. Output dimensions are checked against `size × scale`.
- Colours are tokens in `crates/lines-app/src/theme.rs` only (a test rejects colour literals in views). Themes derive from the brand palette (ink `#11141B`, violet `#8C7BFF`, paper `#ECEAF4`); violet is the accent and last-price tag, never up/down. Every text token meets WCAG AA (4.5:1) on its backgrounds in all four theme/palette combinations (`theme::tests`). Direction is shown by shape too: one direction's bodies are hollow (bodies under 3 device px each way stay filled).

## Known gaps

- Real CQG connection never exercised. Not implemented: session restore, variable tick tables, quote corrections (counted only), trading sessions/time zones (axis is UTC), tick-arrival→frame latency measurement (decision 0001). Decision 0013 gaps: no live source of prior-session volume yet, so feeds use the calendar rule; product trading hours are not modelled; calendars ignore exchange holidays (provider expiry dates win); continuous series are not shown in the UI yet; the Rithmic symbol rule belongs to the private connector (decision 0014).
- Screenshot DPI cannot be overridden on Windows (GPUI reads monitor DPI); `--scale` asserts it instead. Screenshots use the OS UI font, so they differ across machines.
- DOM/T&S: volume at price counts since data start (trading sessions are not modelled), from live trades only (not history bars). No order entry yet; the DOM's left column is reserved for working orders (milestone 7). Splits have fixed ratios (no drag-resize, tabs or extra windows until the workspace milestone).
- Coinbase: if the local clock is ahead of Coinbase's, the connect-time bar boundary can be early and trades just before it are dropped as "covered". REST trades are paged back at most 10 × 1000 trades; beyond that the hole is reported as a history gap.
- Sierra: format docs are withdrawn. The sub-trade markers in `open` (`0xfd709f01` first, `0xfd709ef9` last) and the ×100 price multiplier were determined from NQ files only; ES/MES/MNQ are assumed the same. On NQZ5 2025-09-11, 90 of 775 first fills printed outside the rebuilt book (cause unknown), and in the DOM the rebuilt book looks thin (mostly 1–2 lots, alternate rows empty at 13:45Z): the importer's depth rebuild needs checking against Sierra. Recordings imported before format v3 have no contract (read as unknown); re-import to get it.
- Symbol picker: searches the built-in catalog only (no provider symbol lookup yet); front months use the calendar rule (no live volume before connecting). The CQG mock serves ES only. Recents/favourites live in `<config dir>/lines/settings.json` (`LINES_CONFIG_DIR` overrides).
- Verified on Windows 11, and by hand on macOS 27 (Apple silicon, Retina 2×; screenshots byte-identical across runs there too). Linux: CI only.
