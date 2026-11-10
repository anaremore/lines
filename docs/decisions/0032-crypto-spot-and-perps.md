# 0032: Crypto, spot and perpetuals: venues, data and the trading screens

**Status:** Draft (planning notes from 2026-11-09; a research brief is in progress; not proposed for acceptance yet)

**Context:** Lines already streams public crypto data from Coinbase and Kraken, including Kraken Futures perpetuals (`BTC-PERP.KR`, `ETH-PERP.KR`). The owner asked which crypto platforms to integrate with for trading, including perpetuals, and how that would change the trading screens. This is lower priority than the futures work. This draft records the current thinking so the research has a baseline. Venue eligibility changes often, so every venue claim below is to be checked by the research before this is proposed.

**Working direction (to be confirmed by research):**

1. **CME crypto futures first.** BTC, MBT, ETH and MET trade through the futures connectors Lines already plans (Rithmic or CQG for orders, Databento for data), with no new connector or trading-screen work.
2. **Native crypto trading:**
   - Coinbase is the leading candidate: Advanced Trade for spot, and its US-regulated derivatives for small BTC/ETH futures and perpetual-style futures.
   - Kraken is second: spot, plus its derivatives where the account is eligible.
   - Offshore venues (Binance, Bybit, OKX, Deribit, Hyperliquid, dYdX) are not trading targets for a US user, for regulatory reasons.
   - The rollout follows the 0028 path: data, then watching the real account, then paper, then live, which the owner enables.
3. **Perp data, even without trading on those venues.** Public market data from the largest perp venues:
   - Hyperliquid (fully public, no account) and the Kraken connector Lines already has; possibly Binance and Bybit public feeds.
   - It drives lower panes for funding rate and open interest (using the #118 lower-pane stack), liquidation markers on the chart, and mark price beside last.
   - Data use must respect each venue's terms.

**What changes in the trading screens:**
- **Size:** fractional sizes per product (e.g. 0.0035 BTC), with Qty presets in units, plus sizing by dollar amount.
- **24/7 trading:** a defined day boundary for daily P&L and daily loss limits (default midnight ET, with a setting). No session close, so the paper market-order remainder needs an end rule.
- **Fees and funding:** maker and taker fees in fills and net P&L. Perp funding payments in P&L and on the position card.
- **Leverage and liquidation:** a liquidation price beside the stop on the position card, plus margin used. Isolated vs. cross margin is shown when the venue has it.
- **Trigger price:** mark, last or index. Show mark, and say which price each stop triggers on.
- **Order types:** post-only and IOC, plus the existing reduce-only and brackets.
- **Spot holdings:** a Holdings section in the Positions pane, with value and cost basis where known.
- **Price detail:** many decimals and fine price steps; DOM price grouping matters more.
- **Credentials:** the Connections page (0030) accepts trade-only API keys and warns when a key can also withdraw.

**Open questions for the research:**
- Which venues a US individual can actually trade spot and perps on today, and with what account requirements.
- API quality: WebSocket order book depth, rate limits, sandbox or testnet, reduce-only and post-only support, and documentation.
- Fees, funding schedules and liquidation rules, including how they're reported over the API.
- Data licensing for public feeds.
- Fit with Lines' connector model (0020, 0030) and the paper engine.
- A recommended first venue, with reasons.

**Consequences (once proposed and accepted):** new private connectors in `lines-connectors`, core changes for fractional quantity display, 24/7 sessions and funding in P&L, and screen changes in the trade panel, position card and Positions pane.
