# 0003: Internal instrument IDs, symbol mapping, capability-based traits

**Status:** Accepted

**Context:** Futures first, but equities/crypto may come later, and multiple vendors name the same instrument differently.

**Decision:** Every instrument has a stable internal ID plus a per-provider symbol mapping. Provider and broker traits declare capabilities (depth, history, server-side brackets, fractional quantity, shorting, …); the UI enables features by capability, never by `if asset_class == …`. Ledger amounts carry a currency. Timestamps are UTC internally; exchange/local time is display only.

**Consequences:** No vendor symbol appears above the provider layer.
