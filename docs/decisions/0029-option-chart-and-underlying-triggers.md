# 0029: Trading options from the underlying's chart, with triggers on the underlying's price

**Status:** Proposed (amends 0028)

**Context:** Decision 0028 gives options a chain pane and resting limits. A trader who reads levels on SPY wants to act on those levels: enter when SPY reaches a price, and exit on SPY's price rather than on the option's, whose quote is noisier and whose spread can be wide. The owner reviewed mocks on 2026-11-08 and chose a chain locked to the chart's price scale, with projected P&L as a toggle. Other platforms offer pieces of this (order pills on an option's chart, P&L curves, form-based conditional orders); the search found none locking the chain to the underlying's chart, though it wasn't exhaustive.

**Decision:**

*The surface (public)*
- An **option chart** view: the underlying's chart beside a chain whose rows share the chart's price scale, so a strike's row sits at that strike's price.
- Clicking an ask opens a buy on that contract, and a bid a sell, through the same order logic as the ticket in the chain pane. It's one trading path with two views, not a second one.
- **Entry, Stop and Target are lines on the underlying's price.** They can be dragged and run across the chart and the chain.
- Each line's tag shows an **estimated** option value and P&L at that level, always marked ≈. Estimates are display values from an option model (f64, per 0028), and are never used for an order's price, a risk check or P&L.
- **"P&L to close" is a toggle, off by default.** With it on, the chart continues past now to the close, shaded by the contract's estimated P&L at each time and price, with signed values and a breakeven line, so color isn't the only signal. The chart widens and the chain keeps only the traded side.

*Triggers (public, in the trading core)*
- A **trigger** is held by the trading core, not the UI, so it fires on the core's market data with the UI closed to the tray or busy. It watches the underlying's last trade.
- Triggers fire only during the option's regular hours, so a premarket print can't set one off. A long put's direction is flipped: its stop fires when the underlying rises.
- **Entry trigger:** when it fires, sends a limit at the ask, capped at a max price the trader set. It doesn't chase. If the ask is above the cap, the entry is refused with its reason, and the trigger ends. The far-from-mid guard, guardrails and all new-order checks apply when it fires, not only when armed.
- **Exit triggers:** Stop and Target are linked (one-cancels-other) and reduce-only. They can never open, grow or flip a position, and they shrink with it.
- **A fired exit chases:** it sends a limit at the bid and re-prices to the current bid every few seconds (default 3 s) until filled, with no floor. The chase stops if the position is gone or the trader cancels it.
- A reduce-only exit at the current bid is exempt from the far-from-mid guard, because a wide quote must never trap an exit. An exit with no usable bid keeps waiting, and says so loudly.
- Market orders stay refused by default (0028).

*When data or the app is missing*
- **A stale underlying feed never fires a trigger.** It shows a loud warning on the line and the armed row. When the feed returns already past a level, an exit fires at once and an entry ends with its reason.
- **After a restart,** entry triggers don't come back. Exit triggers on a position that's still open return armed, with a visible notice.
- A halted or locked-out account blocks entries, never exits, as elsewhere.
- **The ticket and every armed row state the limit:** a trigger only works while Lines is running with a fresh feed.

*Live (phase 4 of 0028)*
- This decision covers paper. Live option orders stay a separate owner decision.
- Before that decision, find out what Schwab can hold on its side as a backup stop, because a local trigger can't protect a position while Lines is closed.

**Consequences:**
- The trading core gains trigger state with tests for firing, staleness, restart, chasing and reduce-only limits, in its own tier B PR before any UI.
- The far-from-mid guard gains the reduce-only exemption above. This closes the known gap where a wide quote can refuse a limit at its own bid.
- A new pane kind or mode needs a workspace version bump and a tested migration.
- Build order: the chain ticket, then core triggers, then the locked chart and chain with its lines and tags, then the toggle.
- Single-leg only. Spreads remain later.
- Strategies using triggers on options come later, through the same core gate.
