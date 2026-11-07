# 0028: Equities and listed options, starting with SPY/QQQ through a broker extension

**Status:** Proposed (amends 0013; reverses the roadmap's "equities: not planned")

**Context:** The owner holds a Charles Schwab account and wants to see SPY and QQQ and their option chains in Lines, then trade them, by hand and from strategies (0025). Schwab's Trader API (Individual tier) provides quotes, price history, option chains, streaming level-one data (equities, options, futures, futures options), accounts and positions. Order placement is documented for stocks, ETFs and options only, not futures. Authorization is OAuth: the access token lasts 30 minutes and the refresh token 7 days, after which the user logs in again in a browser. Lines models futures contracts only today (0013). Options need a strike, an expiry and a right, plus multiplier, exercise style and Greeks. Equities have no contract month.

**Decision:**

*Instruments (public)*
- `ContractKey` gains two more kinds alongside the futures contract:
  - an **equity**: an underlying such as SPY, with tick 0.01, quantity in shares, and no expiry;
  - a **listed option**: `{ underlying, expiry date, strike, right (call/put) }`, with a multiplier (100 by default), exercise style, the settlement time from the product calendar, and the strike as an exact `Price`.
- Each provider derives its own symbol by rule (the OCC 21-character form for options), as 0013 does for futures.
- Prices, quantities and money stay exact (0002). Greeks and implied volatility are display values (f64), never used for order prices or P&L.
- The product catalog gains SPY and QQQ: their trading hours (RTH 09:30–16:00 ET plus extended hours for the equity, RTH only for options), the option tick rule (0.01, or 0.05 above $3 where it applies; the provider reports it per chain) and the expiry calendar (daily expiries).

*Data and UI (public)*
- An **option chain pane**:
  - choose the underlying and expiry;
  - calls on the left and puts on the right of a strike column, centered on the at-the-money strike;
  - columns: bid, ask, last, volume, open interest, IV and delta;
  - the underlying's last price marked between rows.
- It's keyboard-first: arrows move by strike, Tab switches calls and puts, Enter opens a ticket.
- Charts, DOM, and time & sales work for the equity, and for a single option where the provider streams it.
- Missing, stale or delayed data is shown as such (design.md §7). A delayed quote is labelled "delayed".

*Brokers*
- **Schwab is a private broker extension** in `lines-connectors` (0020). The public repo holds only the generic instrument model, the chain pane, and the paper broker's options support.
- **Phase 1:** data only.
- **Phase 2:** account observe mode, showing positions and orders read-only (the 0023 pattern).
- **Phase 3:** the paper broker supports equities and options:
  - fills at the option's own bid/ask (trade-through for limits);
  - P&L uses the multiplier;
  - expiry handling: options expire worthless or are cash-settled at intrinsic value against the underlying's close, with exercise and assignment marked approximate in known gaps;
  - strategies trade options on paper through the same core gate.
- **Phase 4: live orders through Schwab.** The owner enables this per account, under 0023's safety rules. That is a separate owner decision at the time, and it never happens by default.
- Credentials: the app key, secret and refresh token are kept in the OS keychain or a secret file, never committed, logged or exported in diagnostics. When the 7-day refresh token expires, the account chip shows "Schwab login expired" with a re-login action. It never fails silently.

*Risk*
- Prop rules don't apply to these accounts, but guardrails do: max loss, max trades and cooldowns.
- Options orders get one more guard: a limit price far from the mid (configurable) is refused, and market orders on options are refused by default.

**Consequences:**
- The roadmap gains a row: "Equities and options (SPY/QQQ), data → observe → paper → live".
- `ContractKey`'s saved form changes. That needs a versioned migration in every file that stores contracts: workspace, trading file, journal and store segments. Existing futures keys must read back unchanged.
- The chain pane is a new pane kind with screenshot evidence, like the DOM.
- The owner registers a developer app at developer.schwab.com and accepts Schwab's developer terms (owner-only: vendor agreement).
- Futures stay on the futures connectors (Rithmic, CQG, Sierra). Schwab is not used for futures orders.
