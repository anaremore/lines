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
| `crates/lines-core` | Vendor-neutral types and math: `Price`/`PriceScale`/`UnitConverter`, `Qty`/`QtyFormat`, `Product`/`ProductId`/`ContractKey`/`Capabilities` (`instrument.rs`), `calendar` (civil dates, expiry/roll calendars), `roll` (front-month resolution), `continuous` (display-only continuous series), bars, book, viewport, allocation-free geometry. | nothing |
| `crates/cqg-proto` | Rust types generated at build time from CQG `.proto` files (`protox` + `prost-build`, no `protoc`). | prost |
| `crates/lines-feed` | `catalog` (products with permanent ids, crate-private provider root symbols, `Target` = product or contract), `provider` (`Provider`, I/O-free `ProviderSession`, `MarketEvent`, `Frame`), `cqg/` (session, mock, symbol map), `coinbase/` (session, symbol map), `native` (`lines-events-1` encoding of normalized events incl. depth; replay-only provider `lines`), `connection` (generic driver + reconnect backoff), `transport` (WSS + HTTP, replay, recording tee), `recording` (`.lfr` v3), `store` (`ChartStore`). Own thread + Tokio runtime. | lines-core, cqg-proto |
| `crates/lines-app` | `lines` binary: CLI, GPUI window, `chart_view`, `theme`, `screenshot` (feature-gated). | lines-core, lines-feed |
| `crates/lines-sierra` | **Personal dev tool, never ships.** `lines-import-sierra` binary: Sierra Chart `.scid` + `.depth` → `lines-events` replay file. Workspace member (tested) but not a default member; nothing depends on it. | lines-core, lines-feed |

`third_party/cqg-webapi` is a git submodule (github.com/cqg/WebAPIPythonSamples); only `proto/` is used. Protos are not vendored (decision 0010).

## Build / test / run

```sh
git submodule update --init          # required: cqg-proto build fails without it
cargo build                          # first build ~6 min (GPUI)
cargo test --workspace               # offline
cargo test -p lines-feed --test feed -- --ignored   # + live Coinbase smoke test (internet)
LINES_SIERRA_DATA="C:\SierraChart\SierraChart - Apex\Data" cargo test -p lines-sierra -- --ignored   # real Sierra data, ~1 min
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy --workspace --all-targets --all-features -- -D warnings   # includes screenshot
cargo fmt --all --check

cargo run -p lines-app -- --source mock                          # CQG-protocol mock, ES, 5k updates/s
cargo run -p lines-app -- --source mock --disconnect-every 10    # reconnect + back-fill
cargo run -p lines-app -- --source coinbase                      # live BTC/USD (or --instrument ETH/USD)
cargo run -p lines-app -- --source mock --headless 5 --record recordings/x.lfr
cargo run -p lines-app -- --source replay --replay recordings/x.lfr --speed 10   # 0 = max speed
cargo run -p lines-app -- --source replay --replay recordings/x.lfr --at 2026-11-06T14:05:00Z
# Real CQG (never run: no credentials). Defaults: demo server, label CQG, app id WebAPITest.
LINES_CQG_USER=... LINES_CQG_PASSWORD=... cargo run -p lines-app -- --source cqg --instrument ES

# Sierra Chart import (personal dev tool; output stays in gitignored recordings/).
cargo run --release -p lines-sierra -- --data "C:\SierraChart\SierraChart - Apex\Data"   --symbol NQZ5.CME --date 2025-09-11 --out recordings/nqz5-2025-09-11.lfr
cargo run --release -p lines-sierra --example scid_markers -- <file.scid> <YYYY-MM-DD>   # marker diagnostics

# Screenshot mode (decision 0009). Replays at full speed to --at, renders off-screen, writes PNG, exits.
cargo run -p lines-app --features screenshot -- --replay recordings/x.lfr \
  --screenshot out.png --at 2026-11-06T14:05:00Z --size 1280x800 --theme dark --scale 1
# --cursor X,Y adds a fixed pointer (logical px from the plot's top-left) to show the crosshair.
```

Catalog products (ids are permanent): `ES` 1, `NQ` 2, `ZN` 3, `MES` 4, `MNQ` 5, `BTC/USD` 1001, `ETH/USD` 1002. Contracts are computed, never listed: `ContractKey { product, delivery }`, with `Delivery::Monthly { year (4-digit), month }` for futures and `Delivery::Spot` for spot products (one perpetual contract, never rolls). Canonical label: `ESZ26` (2-digit year); `--instrument` takes a product (`ES`, followed to its front month) or a contract (`ESZ26`, `ESZ2026`). Provider symbols are derived by rule: CQG root + month code + last year digit (ES→`EP`, so `EPZ6`; NQ→`ENQ`, ZN→`TYA`; only `EP` verified, via the mock; MES/MNQ unmapped). Coinbase maps `BTC-USD`, `ETH-USD` (spot). Sierra maps roots ES, NQ, MES, MNQ (`NQZ5.CME`, `NQH25-CME`, `NQH25_FUT_CME`). The mock resolves `EP` and any `EP` contract symbol.

`recordings/`, `*.lfr`, `*.scid`, `*.depth` are gitignored. Recordings may contain licensed data; committed fixtures must be synthetic (tests build them in code).

## Rules

- The repo is public under FSL-1.1-ALv2 (`LICENSE.md`, decision 0014). Never commit credentials, account IDs, vendor usernames, vendor protos/kits, or anything derived from licensed data, including in screenshots and docs.
- Live vendor connectors (Rithmic, CQG production) belong in a private module (decision 0014). Public code may depend only on the provider/broker traits.
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
- `ProviderSession`s do no I/O. HTTP bodies arrive as `FrameKind::Http` frames, so recordings contain them. Replay drives the same session code; its clock is the recorded frame time (the first recorded frame before any delivery).
- `.lfr` is format **v3**: header = provider, protocol, catalog product symbol, contract key. v2 files still read, with the contract unknown (shown as "contract unknown"; asking a v2 file for a specific contract is an error). v1 is rejected. Replay refuses unknown providers or protocols and product/contract mismatches. A recording holds exactly one contract: if the front month rolls mid-recording, recording stops. Imported data uses provider `lines` / protocol `lines-events-1`. Each frame is one atomic batch, depth included, so the store applies it under one lock and checks for a crossed book only at frame boundaries.
- Sierra import is deterministic (same inputs → byte-identical file). Trades are checked against the book at emit time, never against look-ahead state. Sweep fills (Sierra unbundled trades) are not expected at the touch.
- Products vs contracts (decision 0013): the store is keyed by contract (`ChartData::contract`). `MarketEvent::ContractChanged(key)` retires the old contract's bars (kept, unadjusted, for continuous charts), never mixes them. Front month = `roll::resolve_front`: volume crossover when prior-session volumes are given, else the product's calendar rule, never backwards, never an expired contract. Live feeds following a product re-resolve on every (re)connect. Nothing rolls positions.
- Continuous series are display-only (`continuous::build`): unadjusted by default, difference back-adjust as an option (exact, on the tick grid), no ratio adjustment.
- Sierra contract from the file name: 2-digit years are 20YY; a 1-digit year is the earliest matching year not before the file's last record.
- All CQG `sint64` times are ms offsets from `LogonResult.base_time`.
- Crosshair hit-testing and snapping are pure (`lines_core::geometry::crosshair`). Input handlers store only the pointer position; paint hit-tests after the frame's viewport update. Lines use device-pixel snapping: wicks, time gridlines and the crosshair's vertical line sit on device-pixel centres, and candle bodies are an odd number of device pixels wide.
- No heap allocation in our paint path (`lines-core/tests/no_alloc.rs`, counting allocator). Label text is shaped once per value and cached; a cache miss allocates. GPUI allocates internally per frame.
- The UI never blocks on the feed. Sessions decode outside the lock, the store applies events under a short lock, the UI polls an atomic revision every 8 ms and `try_lock`s in paint.
- Transport `recv` must be cancel-safe.
- Screenshot mode: repeated runs with the same inputs on one machine produced byte-identical PNGs. Output dimensions are checked against `size × scale`.

## Known gaps

- Real CQG connection never exercised. Not implemented: session restore, variable tick tables, quote corrections (counted only), trading sessions/time zones (axis is UTC), tick-arrival→frame latency measurement (decision 0001). Decision 0013 gaps: no live source of prior-session volume yet, so feeds use the calendar rule; product trading hours are not modelled; calendars ignore exchange holidays (provider expiry dates win); continuous series are not shown in the UI yet; the Rithmic symbol rule belongs to the private connector (decision 0014).
- Screenshot DPI cannot be overridden on Windows (GPUI reads monitor DPI); `--scale` asserts it instead. Screenshots use the OS UI font, so they differ across machines.
- Coinbase: trades in the forming minute that happened before the subscription are missing until the next bar; a trade on a minute boundary can be counted in both REST history and live (milestone 4).
- Sierra: format docs are withdrawn. The sub-trade markers in `open` (`0xfd709f01` first, `0xfd709ef9` last) and the ×100 price multiplier were determined from NQ files only; ES/MES/MNQ are assumed the same. On NQZ5 2025-09-11, 90 of 775 first fills printed outside the rebuilt book (cause unknown). Recordings imported before format v3 have no contract (read as unknown); re-import to get it.
- Verified on Windows 11 only.
