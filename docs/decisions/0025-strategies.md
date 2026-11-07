# 0025: Strategies: pure state machines behind the trading core, backtested on replays

**Status:** Accepted (owner, 2026-11-07; requested 2026-11-06; implements 0004, 0006, 0011, 0012, 0018, 0020, 0022, 0023 for automation). Amended by 0027 (heartbeat, daily loss)

**Context:** The owner wants Lines to run rule-based automation of their own trading method, starting with a private strategy that trades from external levels Lines already draws (0022). The owner's previous attempt (the private `rithmic-trades` project) mixed strategy logic, I/O, persistence and order routing in one long-running server. That made it hard to replay a day and get the same decisions, and hard to say why an order was sent. Lines already has the parts a safer design needs. Provider sessions do no I/O, so replays are exact (0011). Every order is a typed command through one trading core that runs risk, prop rules and guardrails before any broker (0006, 0018). The paper broker is deterministic (0004, 0005). The journal turns fills into round trips with exact P&L and MAE/MFE. Recorded levels replay with the market (0022). `docs/positioning.md` says Lines is not for quant developers and that a backtesting engine is breadth for its own sake. This decision narrows that: Lines automates the trader's own discretionary method under the same guardrails as their clicks. It does not become a research platform. There is no optimizer, no parameter sweeps and no Python.

**Decision:**

*What a strategy is*
- A **strategy** is a pure, deterministic state machine, like a `ProviderSession` (0011). It does no I/O, reads no clock, uses no randomness and starts no threads. It gets `now` with every input.
  - **Inputs:** market batches (`MarketEvent`s for its contract: trades with aggressor, book, bid/ask, contract changes); level snapshots (each required source's `LevelMap`, its freshness and as-of time, and the source's **named values**: scalars such as an indicator reading, each with its as-of time. Named values are an optional addition to the level map (0022) that sources fill when a strategy needs them; display adapters stay display-only, and a strategy only reads what Lines publishes); the trading core's events for its own account and contract (command results, orders, fills, positions, guardrail and rule state, its own strategy state); and timer ticks, so time-based rules fire in a quiet market.
  - **Outputs:** **decisions.** Each decision has a rule id, a reason in plain US English, the levels it used (source, kind, label, price) and, optionally, one `Command`. Decisions with no command ("skipped: target crosses the zone boundary") are logged too.
- An **instance** is one strategy with its parameters, bound to one **account** and one **contract**. At most one instance trades a given account and contract, so its position is that account's position there. It has a stable `StrategyId(u32)`, assigned when it is created and saved with its configuration, like order ids. Parameters are data: the strategy declares a settings schema (0021) and checks the values into its own typed parameters. Bad values refuse the instance; they are never defaulted silently.
- Level values are exact or explicitly converted. `Tick` values are already on the grid. A strategy that uses a `Raw` value names its rounding (toward or away from the market) through one framework helper. Order prices are `Price`; floats never reach a `Command` (0002).
- The same strategy code runs live, on replays and in backtests. Only the host around it changes.

*Commands, and what the core enforces*
- Strategy commands carry **`Source::Strategy(StrategyId)`**, which is `Origin::Automated`. They go through `TradingHandle::submit` like any other command. A strategy has no other way to act and no access to a broker.
- The **trading core enforces the strategy rules**, not the host. A buggy or hung host can't get around them. Before prop rules, risk and guardrails, the core's strategy gate checks each strategy command against:
  - the instance's **state**: Disarmed, Armed, Paused (with a reason) or Stopped. Only Armed may enter. Paused, Stopped and Disarmed may only reduce: cancel their own orders, flatten their own contract, move their exits, and place orders that only close part of the position (bracket exits already working count as closing). Changing an entry order, its price included, is an entry. A strategy's Cancel All cancels only its own orders, never the trader's. Its Flatten closes the contract's position and, like any Flatten that closes a position, cancels every order on that contract, so no order is left to reopen it. Reducing acts on the account's position on that contract: after a Stop without flatten the host no longer runs the strategy, so in practice the position is the trader's. **Exits are never limited** by state, hours, size, loss or rate.
  - its **account and contract**: an instance trades only its own.
  - its **limits** (`StrategyLimits`, all required, no "unlimited"): max position (worst case with its open orders), max daily loss (realized + unrealized on its own fills, per trade date of the account's day product), max entries per minute (counted when accepted; exits are never limited), and allowed hours (windows in a named time zone, from the bundled tz database).
  - **paper only:** strategy orders go only to the built-in paper broker (live sources' paper accounts, replays, backtests), until Live is enabled under *Live* below.
- After that, strategy orders pass everything a click passes: validation, prop rules, risk limits and guardrails (0006, 8b). A denial is a `Reject` with `RejectCode::Strategy` (the gate) or the existing codes. The strategy sees it as an event and logs it.
- The core keeps strategy state in its own memory and emits `Event::Strategy { id, state, reason }` on every change. It also remembers which orders each instance placed, so pausing and stopping can act on them.

*Stopping and pausing*
- **Stop all strategies** (`Command::StopStrategies { flatten }`, manual source) is always allowed and never gated by arming, risk, rules or guardrails. It acts in the core at once: every instance goes Stopped, so a strategy command already queued is denied. Every instance's entry orders are cancelled. With `flatten`, each account and contract a strategy traded is flattened. Without it, protective exits stay while a position is open (as in 0023's cleanup rule) and the position becomes the trader's to manage. It goes straight to the core and doesn't depend on the strategy host, so it works even if the host has hung. In the UI it is always visible, has a key binding and a palette entry, and asks "and flatten?" with flatten as the default.
- **Auto-pause:** any of these pauses an Armed instance. Pausing cancels its resting entries and keeps protective exits while a position is open:
  - In the core: manual order activity (any command with `Origin::Manual`) on the same account and contract (0023: automation pauses until re-enabled); a guardrail lockout or prop-rule breach on the account; the broker disconnected; the instance's own daily loss reached; and the host's heartbeat going missing (none for 5 s of core time).
  - In the host (sent as a pause command): the feed is stale or unavailable; a required level source is missing or stale, or its map's as-of time falls outside what the strategy accepts; the contract changed or the front month moved (a roll); the strategy panicked (caught per instance, which then counts as a fault); or the strategy itself asked to pause.
- **No automatic resume.** Only an explicit Arm with a manual source re-arms an instance. Strategies cannot arm themselves or each other. **Every instance starts Disarmed** after a restart; armed state is never saved (like DOM arming).

*Host (live and in-app replay)*
- `lines-strategy::host` runs every instance on one strategy thread, separate from the UI and from the trading thread (0012). It never takes a lock the UI or the core holds. It reads market batches with `lines_feed::listen` on the pane's feed and level cells by revision. It subscribes to core events and submits through `TradingHandle`. It sends a heartbeat on every tick.
- In-app replays run on the market clock. Their decisions may differ slightly from a backtest because thread timing differs. The **backtest is the reference result.**

*Backtest*
- `lines-strategy::backtest` runs one instance single-threaded and deterministically, with no threads and no wall clock:
  - **Market data:** a `.lfr` recording, or a stored contract or day (0019). Sierra and Databento imports are stored segments. Frames go through the recording's provider session exactly as replay does.
  - **Levels:** the recorded levels files (`<source>-<PRODUCT>.lfr`), decoded by the same `LevelSource::decode` at their recorded times.
  - **Trading:** a `TradingCore` with a `PaperBroker` (the configured fill model, trade-through by default (0005), latency and fees) on the manual market clock. The strategy gate, prop rules, risk and guardrails are all enforced, as live.
  - **Journal:** the same journal turns fills into trades, with exact P&L and MAE/MFE.
- The result is a **deterministic report**: identical inputs (files, parameters, strategy version, broker config) give a byte-identical report, and a test checks this. It lists inputs with content hashes; trades; P&L, fees, MAE/MFE and summary statistics; every decision with its reason; denials; pauses; and the position open at the end, if any (marked to the last price, never closed silently).
- Headless entry: `lines --backtest <strategy> --replay <file|dir> [--replay-levels <dir>] --backtest-config <settings.json> [--report <out.json>]` (settings: parameters, limits, balance, paper broker) (official builds list their strategies; the public build lists the example).
- Backtests never touch a live broker, a live feed or a level service.

*Audit*
- Every decision is logged with its reason, the rule, the levels used and the command result. The log is append-only JSON Lines, `<config dir>/strategies/<instance id>.jsonl`, written off the trading thread. Replays and backtests keep it in memory or write it into the report. It holds no credentials or account ids beyond the account's display label. The strategy panel shows it, and the journal shows the strategy as each fill's source.

*Plugging in (0020)*
- `lines_app::Extensions::with_strategy(Arc<dyn StrategyExtension>)`: a stable name, a label, a version, a settings schema, the level sources and products it needs, and a factory for instances. Official builds register private strategies. The public build ships the framework and one trivial example strategy (a synthetic, no-edge level-touch strategy for tests and documentation). No strategy logic derived from a vendor's or the owner's private methodology goes in the public repo.

*Live (owner-gated)*
- Strategy orders to a non-paper broker need every 0023 Live condition plus:
  - a per-instance **"Enable LIVE automation"** confirmation in the app (type the strategy name and `LIVE`);
  - a live-strategy allowlist of (strategy, account) pairs;
  - backtest and paper history on record for that strategy version.
- Changing the strategy version, parameters, account or contract drops Live for that instance.
- Enabling it is an owner action in the app. **No agent ever enables Live, edits the allowlist, or runs a strategy against a real or Sim broker.**

*UI (sketch; built in its own phase, under `docs/design.md`)*
- A **Strategies** pane per account and contract:
  - each instance's state as a word chip (never color alone), Arm, Pause and Stop;
  - parameters (read-only while armed);
  - limits and how much of each is used;
  - the latest decisions with reasons;
  - day P&L following `pnl_display`.
- **Stop all strategies** sits in every trading header whenever any instance exists.
- Chart markers for a strategy's orders and fills, tagged with the instance.

**Consequences:**
- New public crate `lines-strategy` (depends on lines-core, lines-feed, lines-trading; lines-app depends on it). `lines-trading` gains `Source::Strategy`, the strategy gate, its commands (`AddStrategy` (also how limits change, while not armed), `RemoveStrategy`, `ArmStrategy`, `PauseStrategy`, `StopStrategies`), `Event::Strategy` and the host heartbeat (`TradingCore::strategy_heartbeat`, a handle message, not a command, so it never floods the event stream). This is tier B.
- `Source` gains a variant, and the journal's saved form now carries it. An older build reading a journal with strategy fills skips those trades with a warning (it can't parse the source); it doesn't damage the file. That is acceptable for a pre-release, and no version bump is needed: new builds read old files unchanged. A test pins the saved form. The paper trading file does not save strategy state, because armed state is never saved and limits live with the instance configuration.
- Strategy limits are separate from, and in addition to, account risk, prop rules and guardrails. The tightest one wins.
- One instance is one account and one contract. Coordination across instances is later work: combined MES/MNQ exposure in micro-equivalents, and staggered multi-account batches. Until then, the account's own max position and risk limits cap the total.
- An arm reconciles the instance's own books with the account: flat adopts flat, any other mismatch must be flattened first (a person may have traded the position while it was paused).
- The roadmap gains a strategies milestone. On acceptance, `docs/positioning.md` changes to say Lines automates the trader's own method, while staying out of quant research. Phases: (a) the public framework, the core gate and the backtest harness; (b) private strategies and backtests on recorded data; (c) the UI; (d) Live, owner-gated.
