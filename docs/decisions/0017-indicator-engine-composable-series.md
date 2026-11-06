# 0017: Indicator engine: composable series graph; VWAP first

**Status:** Proposed

**Context:** VWAP is an early, critical indicator. Users must be able to add it to charts **and** use it as an input when building their own indicators (built-in or WASM plugins, decision 0008). That needs a real indicator system, not one-off chart code. Decision 0007 already makes indicators pure, incremental and I/O-free.

**Decision:**

*Series graph*
- Everything an indicator consumes or produces is a **series** keyed by `(contract, source, parameters)`: trades, bars (any period), depth-derived series, data-source series (0007), and **other indicators' outputs**.
- Indicators declare typed inputs and outputs. The engine builds a **dependency graph (DAG)** and evaluates it incrementally per new trade/bar, in dependency order, on the indicator threads (not UI, not feed).
- **Identical nodes are shared:** a VWAP with the same contract and parameters is computed once, whether it feeds a chart line, three custom indicators, or a WASM plugin. Cycles are rejected at build time.
- **Forming vs final:** every output carries whether its bar is still forming. History back-fill and late corrections (decision 0013, stitch) trigger a bounded recompute of affected nodes only.
- Deterministic: replaying the same recording yields identical outputs (testable like bars today).

*Built-ins and plugins share one interface*
- The Rust `Indicator` trait (roadmap 10a) is designed to map 1:1 onto the future WIT world (10b): a plugin requests built-in series such as `vwap(session=rth, bands=[1,2])` from the host and receives the shared node's output. It never recomputes them.

*Sessions (prerequisite)*
- Products gain **trading-hours calendars** (exchange time zone with DST, ETH and RTH windows, holidays, early closes). VWAP resets, volume profile (roadmap 5) and daily bars all depend on them. This closes the gap 0013 left ("product trading hours").

*VWAP specifically*
- **Resets:** session (ETH or RTH, per product calendar), weekly, monthly, and **anchored** (from a user-chosen bar, very common).
- **Bands:** ±1/2/3 standard deviations (volume-weighted), optional.
- **Source:** computed from **trades** when available (exact). When only bars exist (e.g. older history), it uses bar typical price × volume and **marks the output as approximate** until trade data takes over. Never silently mixed.
- **Exactness (extends 0002):** accumulators Σ(price×volume) and Σvolume are exact integers (`i128`). The VWAP value itself is a derived analytic, not a tradable price: it's carried as `f64` (bands need a square root) and is never snapped to the tick grid or used as an order price without explicit rounding by the order layer.

**Consequences:**
- New roadmap order: sessions + indicator engine + VWAP come right after workspaces (5b), before order entry.
- Volume profile (5) waits for session calendars, so its "session profile" is correct from the start.
- A time-zone database dependency is needed (e.g. `chrono-tz` or `jiff`).
- Indicator compute cost becomes a tracked benchmark (indicator work per tick with N charts open).
