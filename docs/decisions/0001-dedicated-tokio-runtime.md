# 0001: Network runs on a dedicated Tokio runtime, not GPUI's executor

**Status:** Accepted

**Context:** GPUI's `background_executor()` is not a Tokio runtime; tokio-tungstenite requires a Tokio reactor. The UI can only repaint once per frame.

**Decision:** Feed and broker I/O run on their own Tokio runtime thread(s), in crates with no GPUI dependency. The worker coalesces ticks into shared state; the UI is woken at most once per frame and reads a snapshot.

**Consequences:** Feed crates are testable headless. Never `cx.notify()` per tick. Measure tick-arrival → frame-presented latency rather than chasing "zero latency".
