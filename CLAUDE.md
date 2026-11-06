# Lines

Desktop futures charting terminal. Rust, GPUI UI, CQG WebAPI (protobuf over WSS) market data.

## Crate layout

| Crate | Purpose | May depend on |
|---|---|---|
| `crates/lines-core` | Prices (exact `i64` + rational scale), bars, viewport math (pan/zoom, index/price ↔ px), per-frame geometry. | nothing (no GPUI, Tokio, protobuf) |
| `crates/cqg-proto` | Rust types generated at build time from CQG `.proto` files via `protox` + `prost-build` (no `protoc` needed). | prost |
| `crates/lines-feed` | Session state machine (`session.rs`, I/O-free), connection + reconnect loop, transports (WSS, mock, replay, recording tee), shared `ChartStore`. Runs on its own thread + Tokio runtime. | lines-core, cqg-proto |
| `crates/lines-app` | `lines` binary: CLI, GPUI window, chart view (`chart_view.rs`). | everything |

`third_party/cqg-webapi` is a **git submodule** (github.com/cqg/WebAPIPythonSamples). The protos are not vendored because their licence doesn't clearly allow redistribution. Only `proto/` is used.

## Build / test / run

```sh
git submodule update --init          # required: cqg-proto build fails without it
cargo build                          # first build ~6 min (GPUI)
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check

cargo run -p lines-app -- --source mock                       # 5k quotes/s mock
cargo run -p lines-app -- --source mock --disconnect-every 10 # exercise reconnect/back-fill
cargo run -p lines-app -- --source mock --record recordings/x.lfr
cargo run -p lines-app -- --source replay --replay recordings/x.lfr --speed 10   # 0 = max speed
cargo run -p lines-app -- --source mock --headless 5          # no window, prints stats
# Real CQG (untested: no credentials yet). Defaults to the demo server, label CQG, app id WebAPITest.
LINES_CQG_USER=... LINES_CQG_PASSWORD=... cargo run -p lines-app -- --source cqg --symbol EP
```

`recordings/` and `*.lfr` are gitignored. Recordings may contain licensed exchange data, so never commit them.

## Pinned versions

- Rust toolchain `1.98.1` (`rust-toolchain.toml`, matches the pinned Zed commit)
- `gpui` + `gpui_platform`: git `zed-industries/zed` rev `db43f05e398036f08e544c03cbc068216c7a0aaa` (2026-11-06). The crates.io `gpui 0.2.2` is from Oct 2025 and its API differs. Bump both revs together, deliberately.
- CQG protos: submodule commit `6c92c8f80c9d0701468875bc083098d9ff21b185` = WebAPI protocol **2.230** (`cqg_proto::PROTOCOL_VERSION_*`)
- prost / prost-build / prost-types 0.14, protox 0.10, tokio 1.54, tokio-tungstenite 0.30, rustls 0.23 (`ring` provider, not aws-lc; OS roots via rustls-native-certs 0.8)

## Invariants (enforced by tests)

- Prices are `lines_core::Price(i64)` in CQG scaled units. CQG's `correct_price_scale` (a double) is converted once to an exact `PriceScale` rational (bit-exact match required). `f64` is used only for pixel mapping. Display follows CQG `display_price_scale` (decimals, 1/32nds, quarter-32nds...).
- All CQG `sint64` times are ms offsets from `LogonResult.base_time` (`Session::from_offset`).
- No heap allocation in our paint path: `lines-core/tests/no_alloc.rs` uses a counting global allocator. `Scene` buffers are sized on resize (`ensure_capacity`), and labels are formatted into `FixedStr`. In the app, shaped text is cached per label value; a miss allocates (GPUI `SharedString` + shaping). GPUI's own element tree also allocates per frame (bump arena), outside our control.
- The UI never blocks on the feed. The feed decodes outside the lock and applies a frame under a short `parking_lot` lock. The UI polls the atomic `ChartStore::revision()` every 8 ms, notifies only on change, and uses `try_lock` in paint (if the lock is busy, it redraws the previous scene). Every tick is folded into bars on the feed thread, so a slow UI loses nothing.
- Transports' `recv` must be cancel-safe (raced against timers in `connection.rs`).
- Contract ids are session-scoped: re-resolved on every connection. If the contract symbol or price format changes (e.g. a roll), bars are cleared.
- Replay drives the same `Session` code as live. `record_then_replay_reproduces_bars_exactly` asserts identical bars.

## Known gaps

- Real CQG connection never exercised (demo credentials requested from apihelp@cqg.com). Session restore (`RestoreOrJoinSession`), variable tick tables (`tick_sizes_by_price`), quote corrections (counted, not applied), trading sessions/time zones (axis is UTC), and `display_price_scale` 206 are not implemented.
- Verified on Windows 11 only. macOS/Linux builds are untested.
