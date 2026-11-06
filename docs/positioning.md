# Positioning

## Who Lines is for
Discretionary futures traders, prop-firm traders first: people who trade from the DOM and the chart by hand, often across several evaluation or funded accounts, and who live under drawdown and consistency rules.

**Not for (now):** quant developers building automated strategies (NautilusTrader serves them well), crypto-first traders, equities investors.

## The competition
Closed-source desktop platforms: Sierra Chart, NinjaTrader, Quantower, ATAS, Bookmap, Jigsaw, R | Trader Pro, TopstepX. Most are Windows-only and visually dated. In open source, the closest is Flowsurface (Rust, crypto order flow). NautilusTrader is an engine for algorithmic trading, not a discretionary front end.

## How Lines wins: narrow and much better
1. **The fastest, most trustworthy DOM and chart trading.** Everything else is secondary to this.
2. **Native on macOS, Windows and Linux.** Most incumbents are Windows-only; Mac traders run them in VMs. Lines must stay genuinely native on all three, which CI enforces.
3. **Prop-firm workflow in one place:** rule tracking, discipline guardrails and news lockouts, an automatic journal, and multi-account copying. Traders currently stitch these from several paid tools.
4. **Verifiable trust:** public source (FSL, decision 0014), deterministic replay tests for every bug, and published performance numbers.
5. **A modern experience:** keyboard-first, command palette, instant startup, a clean look (`docs/design.md`).
6. **A safe ecosystem:** sandboxed WASM plugins with permissions (decision 0008), unlike add-ons that run with full machine access.

## Out of scope
- Breadth for its own sake: dozens of exchanges, a backtesting engine, Python strategy frameworks.
- Hundreds of studies. Ship the ~15 discretionary futures traders use (VWAP, volume profile, footprint, delta, session levels, moving averages); plugins cover the rest.
- Copying another platform's look. Match behaviour traders rely on, not appearance.

## Possible later
Lines as the discretionary front end beside an algorithmic engine such as NautilusTrader: showing its orders and positions on the DOM with manual override, connected over an interface boundary (NautilusTrader is LGPL; don't link it in).

## Published benchmarks
Measured the same way on every release, on stated reference hardware, from replay files so runs are repeatable. Published in the release notes and README.

| Metric | Definition | Target |
|---|---|---|
| Tick → pixel | Feed frame received → frame presented containing it (p50/p99) | ≤ 1 frame |
| Click → order sent | Input event → order message written to the socket (p50/p99) | < 1 ms |
| Startup | Process start → trading-ready with workspace restored | ~2 s |
| Replay stress | Worst frame time during a market-open replay at max speed | < 16 ms |
| Memory | Resident memory with 6 charts + 2 DOMs open | Published, tracked for regressions |

Benchmarks run in CI on every platform where hardware allows; regressions beyond a threshold fail the build.
