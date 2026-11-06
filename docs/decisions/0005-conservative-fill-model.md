# 0005: Default paper fill model is trade-through

**Status:** Accepted

**Decision:** Fill models are pluggable: touch (optimistic, opt-in), trade-through (default), queue-position estimate (requires depth). Market orders walk the book. Stops trigger on trades. Model partial fills, tick-size validation, fees, and sessions/halts.

**Consequences:** Paper results err pessimistic, protecting traders from false confidence.
