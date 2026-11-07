# 0027: Strategy host heartbeat on the wall clock; the daily loss closes the trade

**Status:** Proposed (amends 0025; the daily-loss rule is the owner's, 2026-11-07)

**Context:** Building the strategy host (0025, phase (c)) turned up two points where 0025's text doesn't fit the system.

- **Heartbeat.** 0025 has the core pause armed strategies when the host's heartbeat is missing "for 5 s of core time". On a replay the core runs on the market clock. A sped-up replay moves that clock minutes ahead in a moment, while the host beats once a wall-clock second. So every fast replay paused its strategies at once, with a host that was alive. Host liveness is a wall-clock matter, and the pure core can't read the wall clock (0012).
- **Daily loss.** 0025 lists the daily loss among the pauses, and a pause keeps the instance's protective exits. The owner decided otherwise: reaching the daily loss limit should close the strategy's trade, not leave it to its stop.

**Decision:**
- **Heartbeat:**
  - The trading service, which runs the core and can read the wall clock, watches the host's heartbeat. The host sends `TradingHandle::strategy_heartbeat` every second.
  - Once a host has sent one, a gap of more than `HEARTBEAT_TIMEOUT` (5 s) of **wall-clock** time pauses every armed instance (`TradingCore::strategy_host_silent`). It acts once per gap.
  - The core keeps no heartbeat state. A backtest, with no host thread, has no heartbeat.
- **Daily loss:** when an instance's daily loss (realised plus unrealised on its own fills, per trade date) reaches its `max_daily_loss`:
  - the core flattens its account and contract, which cancels every order there;
  - an armed instance is paused;
  - arming is refused for the rest of the trade date.
- The close happens once per trade date. It also applies when the host had already paused the instance with a trade open.
- The close never applies after Stop, or when a person's own order paused the instance, because the trade is the person's then.
- The flatten is an internal command (`Source::Rule`, an id from the core's internal range).

**Consequences:**
- Fast replays and `--run-strategy` on a sped-up replay run strategies correctly.
- A day can't lose more than the limit plus the slippage of one flatten. Before this, it could lose up to one more stop.
- 0025's text on these two points is superseded by this record once accepted. The rest of 0025 stands.
