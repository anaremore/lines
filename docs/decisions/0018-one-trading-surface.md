# 0018: One trading surface: commands, trading core, connectors, accounts

**Status:** Accepted

**Context:** Lines should feel like one elegant platform: flexible charts, a first-class DOM, and execution triggered from either, while data sources and execution venues stay pluggable and invisible to the user.

**Decision:**

*Commands*
- Every user action is a typed **command** (`PlaceOrder`, `ModifyOrder`, `CancelOrder`, `Flatten`, `CancelAll`, `SetSymbol`, `SplitPane`, …). Chart gestures, DOM clicks, hotkeys and the command palette all emit commands; later, alerts and plugins can too, subject to permission (0008).
- Commands are the single path into the trading core, so chart and DOM orders are identical by construction. Hotkeys are just bindings from keys to commands. Guardrails (8b) and the journal (8c) observe every command. Command sequences are replayable in tests.

*Trading core*
- One order lifecycle, risk layer (0006), prop rules, positions and P&L, above all connectors and isolated from the UI (0012).

*Connectors*
- Data providers and brokers are adapters that **declare capabilities** (depth, history, server-side brackets, OCO, modify, fractional quantity, …). The UI enables controls from capabilities, never from vendor identity. A missing capability shows as a disabled control with a reason.
- Adding a venue means adding a connector, without changes to chart, DOM or trading core.

*Accounts*
- Users choose an **account** (e.g. a prop evaluation account or Paper). The account determines the broker, the default data source and the prop rules. A single **Connections** screen handles logins. No other UI mentions vendors.
- **Live data/venue consistency:** a live account defaults to its broker's own market data. Using a different data source for a live account requires an explicit override and shows a persistent warning badge. Paper accounts may use any data source.

**Consequences:**
- Roadmap 6 includes the command layer and the Connections/accounts screen. 7 (DOM trading) and 8 (chart trading) emit commands only.
- The trading core's public surface is the command API and an event stream (order/position updates), which also suits a future automation or front-end-for-an-engine use (`docs/positioning.md`).
