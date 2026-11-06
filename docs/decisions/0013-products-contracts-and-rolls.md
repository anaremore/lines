# 0013: Products, contracts, and rolls

**Status:** Accepted

**Context:** Today `InstrumentId` names a product (ES), but orders, positions, fills, the DOM and recorded data belong to a specific contract (ESZ6). The importer already loses the contract month. The symbol picker (roadmap 4b), prop-firm contract caps (decision 0006) and the order system (roadmap 6) all need the distinction before they're built.

**Decision:**

*Identity*
- **Product:** a stable, permanent `ProductId` in the catalog. It holds everything shared across months: description, price unit and tick, quantity format, currency, point value, trading hours, the listed-month cycle (e.g. H M U Z), each provider's root symbol, and **micro/mini equivalence** (e.g. MNQ = 0.1 × NQ), which prop-firm contract caps need.
- **Contract:** a `ContractKey { product, year (4-digit), month }`, computed rather than assigned, so every machine and recording agrees on it without a table. Each provider derives its own symbol from the key with a rule (CQG `EP`+`Z6`, Sierra `ESZ6.CME`, Rithmic `ESZ6`), never a hand-kept per-contract list.

*Where each is used*
- **Always a contract:** orders, positions, fills, the DOM, depth, stored bars, recordings.
- **Product (with a resolution rule):** symbol picker results, chart defaults, prop-firm caps, watchlists.

*Front month*
- **Default rule: volume crossover.** The front month switches when the next contract's prior-session volume exceeds the current one's. This follows where traders actually are.
- **Fallback: a per-product calendar rule** (e.g. equity index futures roll 8 days before expiry) when volume isn't available, such as offline or Sierra-only data.
- The rule is per chart and per watchlist, overridable, and the active contract is always shown.

*Continuous charts*
- **Default: unadjusted.** Each bar shows the contract that was front month on that date, at actually traded prices, with a visible marker at each roll.
- **Option: back-adjusted by price difference**, which stays exact on the tick grid. Ratio adjustment is not offered, because it produces off-tick prices (decision 0002).
- Adjustment is display-only. Stored bars are never adjusted.

*Expiry and roll data*
- Use provider metadata when available (CQG and Rithmic supply expiry dates). Otherwise use a static per-product calendar rule. If they disagree, the provider wins and a warning is logged.

*Positions*
- Positions never roll automatically. Rolling is an explicit pair of orders (close the expiring contract, open the next). The UI warns N days before a held contract's roll date and expiry.

**Consequences:**
- `InstrumentId` splits into `ProductId` + `ContractKey`. The store is keyed by contract. `ContractChanged` carries the new contract.
- Recordings must store the contract key: an `.lfr` format version bump, with old files read as "contract unknown".
- The Sierra importer derives the contract from the file name (`NQZ5.CME` → NQ, 2025-12). Single-digit years resolve to the nearest decade at or before the file's data dates.
- Prop-firm caps are evaluated in micro-equivalent units through the product's equivalence ratio.
