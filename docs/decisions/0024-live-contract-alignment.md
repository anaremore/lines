# 0024: Contract alignment for live orders (amends 0023)

**Status:** Accepted (owner decision, 2026-11-06)

**Context:** Decision 0023 requires "fresh contract alignment" before a live order. The owner's `rithmic-trades` bridge compares the routed contract with Sierra's 1-minute "authority chart". Lines has its own charts, contracts (0013) and front-month resolver, and needs its own definition of that check.

**Decision:** A live order is allowed only when **all three agree** on the exact contract (product, year and month):
1. **The pane's contract:** the contract shown on the Lines pane (DOM or chart) the order is issued from.
2. **Lines' front-month resolver** (0013), when the pane follows a product rather than a fixed contract. If the pane is pinned to a specific contract, the order carries an explicit "pinned contract" flag, and the resolver check becomes a visible warning rather than a block.
3. **Sierra's own definition:** the DTC security definition for the routed symbol (e.g. `MNQZ6.CME`) resolves to the same contract, with exact price-scale agreement.

Any mismatch blocks Live with a clear reason in the DOM header. Sim routing is not blocked. Checks are evaluated at order time, never cached across a roll.

**Consequences:** No dependency on Sierra charts or the levels server for alignment. Roll days are handled by the resolver and an explicit pin, not by a stale authority feed.
