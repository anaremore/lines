# 0023: Trade execution via Sierra Chart's DTC server

**Status:** Accepted (owner-directed; implements 0004, 0012, 0018 for a first live-capable broker)

**Context:** Direct Rithmic/CQG order routing needs vendor conformance (weeks). The owner already trades through Sierra Chart connected to Rithmic, and Sierra Chart runs a local **DTC Protocol Server** (an openly specified protocol) that accepts order entry from local clients. The owner's `rithmic-trades` project (private) already defines a Sierra DTC execution adapter with explicit safety rules (`docs/sierra-dtc-execution-plan.md`, `live-safety-audit.md`, `restart-reconciliation.md`). Routing through Sierra means the conformed platform (Sierra Chart) remains the one talking to Rithmic.

**Decision:**
- A **`SierraDtcBroker`** implements the `Broker` trait (0004) behind the trading core (0012, 0018). It connects over WebSocket to Sierra's local DTC server: `127.0.0.1:11099` by default, JSON compact encoding, every message NUL-terminated, "Allowed Incoming IPs: Local Computer Only".
- It lives in the **private** `lines-connectors` repo and official build (0014, 0020). Public builds never contain live routing (`docs/release.md`).
- **Safety rules, adopted from the owner's plan, as non-negotiable acceptance criteria:**
  - **Observe mode first and by default:** account list, positions, orders and fills are read; no order-entry, cancel or flatten messages are ever sent in observe mode.
  - Discovered accounts are never automatically tradable. Unknown account mode is blocked.
  - **Sim routing** targets only connected Sierra Sim accounts.
  - **Live routing requires all of:**
    - a build with the live feature
    - a non-empty **live account allowlist**
    - a connected non-Sim account in that allowlist
    - an explicit in-app **"Enable LIVE entry"** confirmation (type `LIVE`)
    - bridge readiness
    - audit logging
    - risk checks passed (0006)
    - fresh **contract alignment**: the routed contract equals the contract Sierra's authority chart is currently trading
  - Switching routing from Sim to Live **locks order entry** until Live entry is explicitly enabled again.
  - Manual or unknown Sierra-side order activity is detected and surfaced. Automated features (later) pause until re-armed.
  - Stale managed-order cleanup never cancels attached stop/target children while exposure is unknown or a matching position is open.
  - Panic actions (flatten, cancel all) are scoped by routing mode and account, and always available.
- **Restart reconciliation:** on (re)connect, orders and positions are rebuilt from Sierra's reports (0012). Local state is a cache.
- **Market data via DTC** is a separate question, investigated and reported, not assumed. Sierra may restrict redistributing exchange data from some feeds through its DTC server.
- **Every order action is audited:** command, account, contract, routing mode, result and timestamps, in a local append-only log. Never synced.

**Consequences:**
- Depends on the trading core (roadmap 6 part 1, in progress). The DTC client, observe mode and reconciliation can be built in parallel.
- Enabling Live is an **owner action in the app**, after Sim testing. No agent ever enables it, toggles allowlists, or sends live orders. Agents test only in observe mode and against Sierra **Sim** accounts with the owner's go-ahead.
