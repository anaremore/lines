<p align="center">
  <img src="assets/icon/png/lines-128.png" width="96" height="96" alt="Lines icon">
</p>

<h1 align="center">Lines</h1>

<p align="center">A native, fast futures charting, DOM and paper-trading terminal for discretionary and prop-firm traders.</p>

[![CI](https://github.com/anaremore/lines/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/anaremore/lines/actions/workflows/ci.yml)

![Dark theme: a footprint chart with a long paper position, its stop and target lines and a working buy limit, beside the DOM and time & sales](docs/img/chart-orders-footprint.png)

<sub>All screenshots use synthetic data, rendered with Lines' screenshot mode.</sub>

## Status: pre-alpha

Lines trades on paper only: no live broker connections and no live order entry. Data comes from a mock futures feed, the Coinbase and Kraken public crypto feeds, and replays of recorded sessions. Live futures connectors (Rithmic, CQG) are planned and not in public builds.

## What works today

- **Data:** a simulated ES futures feed, Coinbase and Kraken public feeds, record and deterministic replay, and a local market-data store for live feeds, so charts and order flow open instantly on restart.
- **Charts:** minute to weekly candles, plus bars built from counted trades: tick (`500t`), volume (`500v`), range (`10r`) and seconds (`30s`; volume, range and seconds bars are on `main`, after v0.3.0). Footprint, cumulative delta, VWAP with bands, and a volume profile.
- **Drawings:** lines, rays, trend lines, zones, notes and a measure tool, with color, line style, width and opacity per drawing.
- **DOM:** a tick-indexed ladder with depth, volume at price and a price trail; click, drag and hotkey paper orders; brackets; Flatten and Cancel all always visible.
- **Chart trading:** working orders as lines you can drag or cancel, and stop/target handles on the position line.
- **Risk:** your own guardrails (daily loss, losing-streak cooldown, max trades, max position, no entries before the close) and prop-firm rule presets (trailing drawdown, daily loss, contract cap) that apply to paper accounts.
- **Journal:** every round trip with exact P&L and fees, MAE/MFE, tags and notes, stats, CSV export.
- **Workspaces:** grids, tabs, pop-out windows and link groups, saved and restored. Command palette, Settings window, symbol picker.
- **Look:** dark and light themes, a colorblind-safe palette, and direction never shown by color alone.

Prices and money are exact integers, never floating point.

| | |
|---|---|
| ![A 500-trade tick chart with session VWAP and 1σ/2σ bands](docs/img/tick-chart.png) | ![Light theme: a 5-minute chart with a long paper position, its stop and target, beside the DOM and time & sales](docs/img/light.png) |
| Tick chart (500 trades per bar) with VWAP bands | Light theme |

## Install

Download your platform's archive from the [latest release](https://github.com/anaremore/lines/releases): a zip for Windows x64 and macOS (Apple silicon), a tar.gz for Linux x64. Extract it and run `lines.exe` (Windows), `Lines.app` (macOS) or `lines` (Linux). SHA-256 checksums are attached. Builds are not code-signed yet, so Windows and macOS may warn on first launch.

## Quick start

```sh
lines --source mock                           # simulated ES futures, any hour; paper trading works
lines --source coinbase --instrument BTC/USD  # live public crypto data, 24/7
lines --source kraken --instrument BTC/USD.KR
lines --help                                  # every option
```

To trade on paper, enable trading in the DOM's header (or Ctrl+Shift+Space), then click the bid column to buy and the ask column to sell. Shift+click places a stop.

## Keyboard basics

"Ctrl" is Cmd on macOS.

| Key | Action |
|---|---|
| Ctrl+Shift+P | Command palette: every command, with its key |
| Ctrl+, | Settings |
| Ctrl+K, or type a letter on a chart | Symbol picker |
| `5` / `1h` / `500t` + Enter (`500v` / `10r` / `30s` after v0.3.0) | Change the chart's timeframe |
| Ctrl+I | Studies (VWAP, volume profile, footprint, cumulative delta) |
| Alt+H / Alt+T / Alt+Z / Alt+N / Alt+U | Horizontal line, trend line, zone, note, measure |
| Ctrl+Shift+Space | Enable or disable DOM trading |
| Ctrl+Alt+B / Ctrl+Alt+S | Buy / sell market |
| Ctrl+Shift+F / Ctrl+Shift+A | Flatten / cancel all orders |
| Ctrl+Shift+D / Ctrl+Shift+E | Hide or show the DOM / time & sales |
| Ctrl+T, Ctrl+PageDown | New tab, next tab |
| F1 | Hide or show the key-hint bar |

## Build from source

You need [Rust](https://rustup.rs); the pinned toolchain installs itself on the first build. On Windows, also the Visual Studio C++ build tools (MSVC and the Windows SDK).

```sh
git clone https://github.com/anaremore/lines.git
cd lines
git submodule update --init      # protocol definitions the build needs
cargo run --release -p lines-app -- --source mock
```

macOS: install Xcode, or add `--features runtime-shaders` (Metal shaders compile at startup instead of at build time, which needs full Xcode). The first build takes several minutes.

Architecture and every CLI flag: [`docs/architecture.md`](docs/architecture.md). Decisions: [`docs/decisions/`](docs/decisions/). UI principles: [`docs/design.md`](docs/design.md).

## Roadmap and known gaps

- [ROADMAP.md](ROADMAP.md): what is done and what comes next (chart trading phase 2, strategies, live connectors).
- [docs/known-gaps.md](docs/known-gaps.md): the honest list of current limits.

Troubleshooting: run "Export diagnostics" from the command palette and attach the zip. It stays on your computer until you share it, with credentials and account IDs removed.

## License

[FSL-1.1-ALv2](LICENSE.md): you can read, use and modify Lines for any purpose except building a competing commercial product, and each release becomes Apache 2.0 two years after it is published.
