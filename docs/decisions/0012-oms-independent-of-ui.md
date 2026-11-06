# 0012: The OMS runs independently of the UI, and orders bypass the frame loop

**Status:** Accepted (implementation at roadmap 6–7)

**Context:** A trader's worst case is a hung UI, crash, or disconnect while orders are working. Order entry latency should not be bounded by frame timing (a click waiting for the next repaint adds up to one frame, 7–16 ms).

**Decision:**
- The OMS (order state, positions, risk checks, broker connections) runs on its own thread(s) with its own Tokio runtime and never takes a lock the UI holds. The UI cannot block it.
- Input handlers (DOM click, hotkey, chart drag release) send order intents to the OMS immediately over a lock-free channel. The UI shows the resulting *pending* state on the next frame; it never waits for one.
- Flatten / Cancel All must work even if the UI thread is unresponsive: the OMS exposes them through a path that doesn't depend on the UI (a global hotkey, and later a separate watchdog process).
- On reconnect or restart, OMS state is rebuilt from the broker's authoritative orders and positions; local state is a cache, never the source of truth. Brackets are server-side where the broker supports it (decision 0006); client-side brackets are re-armed or flagged after any reconnect, never silently orphaned.
- Input→order-sent latency is measured as a `tracing` metric and surfaced in the latency HUD (roadmap 6b).

**Consequences:** OMS code has no GPUI dependency and is testable headless with the PaperBroker and replay. Moving the OMS into a separate process later (for crash isolation) stays a deployment change, not a redesign, because the UI already talks to it only through messages.
