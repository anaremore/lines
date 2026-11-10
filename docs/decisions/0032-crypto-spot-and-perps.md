# 0032: Crypto, spot and perpetuals: venues, data and the trading screens

**Status:** Draft (planning notes from 2026-11-09; research findings added under Open questions; not proposed for acceptance yet)

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

**Open questions, with research findings (2026-11-09):**

Sources are official exchange docs unless marked "(news)". Items marked **unverified** could not be confirmed from a primary source. Only unauthenticated public endpoints were sampled, lightly, from a US network on 2026-11-09.

*Recommendation (not a decision):*
- **First: Coinbase.** Spot and US perpetual-style futures go through one REST API.
- **Second: Kraken.** Spot runs over WebSocket. Its US perps need FIX and SBE onboarding.
- **CME crypto futures** go through the futures connectors already planned.
- **Perp data:** Kraken Futures (already connected), Coinbase's public US futures data, and Hyperliquid.
- **Binance and Bybit** can't be used from the US, even for data.

| | Coinbase | Kraken | CME crypto (via futures broker) | Hyperliquid / Binance / Bybit |
| :- | :- | :- | :- | :- |
| US spot | Yes, Advanced Trade | Yes, Kraken Pro (not NY; WA reported excluded, unverified) | n/a | No |
| US perps | Perpetual-style futures through Coinbase Financial Markets (FCM): about 25 crypto contracts, 5-year expiry, hourly funding, 24/7 | Perps listed on Bitnomial, cleared by Kraken Derivatives US (NinjaTrader Clearing, FCM): 16 contracts, USD collateral, funding settled daily 3:00 pm CT | Dated futures only (BTC, MBT, ETH, MET and others), 24/7 on CME since June 2026 | No: US persons excluded by terms and geoblocked |
| Account | Coinbase spot account, plus a futures account application (eligibility and suitability checks) | Kraken Pro account with US futures unlocked; US perps API credentials are issued at onboarding | Futures account at a retail FCM | n/a |
| Trading API | One REST API (`/api/v3/brokerage`) for spot and US futures; order entry over REST only | Spot: REST and WebSocket v2 (order entry over WebSocket). US perps: FIX 4.4 orders, SBE binary market data, FIX drop copy | Rithmic or CQG (planned) | n/a |
| Order book | Public `level2` (aggregated, full depth); no public L3 | Spot: `book` depth 10 to 1000 with CRC32 checksum; `level3` with a token. US perps: L3 over SBE | Through the data vendor (Databento) | Hyperliquid `l2Book` is public |
| Sandbox | Static responses, accounts and orders only | Spot UAT on request; the self-service derivatives demo covers the global platform, not US perps (unverified) | Broker demo accounts | Hyperliquid testnet |
| Low-volume fees | Spot, US entry tier: 0.50% maker / 0.90% taker (unverified, from a 2026-09-16 Coinbase post). Futures: a percentage fee plus about $0.15 per contract for exchange, clearing and NFA (unverified) | Spot: 0.40% maker / 0.80% taker at $0+. US perps: $0.0113 per contract per side all-in; $10 liquidation fee | Per-contract commission plus exchange fees | n/a |

*1. US eligibility*
- **Coinbase:** US perpetual-style futures launched 2025-07-21 through Coinbase Financial Markets (CFM), a CFTC-registered FCM.
  - Futures cash is held at CFM, separate from spot cash at Coinbase Inc. Cash moves to futures automatically and comes back by a scheduled "sweep".
  - Coinbase's international perps (Bermuda) ended 2026-10-01 and were never open to US persons.
  - A 2026-05-29 CFTC no-action letter lets CFM route US institutions to Deribit's global perps (news). Institutions only, so it doesn't apply to the owner.
- **Kraken:** US perps went live mid-June 2026 (news, consistent with Kraken's support article).
  - Kraken Derivatives US also offers CME futures, crypto included.
  - Kraken's developer docs describe US Perps as having "an initial focus on equity perpetuals" with ten contracts at launch, while the support article lists 16 crypto perps. That conflict is **unverified**.
- **CME crypto futures** (BTC 5 BTC, MBT 0.1 BTC, ETH, MET) are available through any retail futures broker, with no new venue. They are dated contracts, so there is no funding.
- **Offshore venues:**
  - Binance and Bybit refuse US IPs even for public market data. Sampled: Binance returned HTTP 451 citing its terms, Bybit HTTP 403.
  - OKX and Deribit exclude US persons by their terms (primary text **unverified**).
  - Hyperliquid's terms list US persons as restricted, and its front end geoblocks the US (news; primary terms text not retrieved, **unverified**). It reportedly discussed US access through Bitnomial in 2026-09 (news, not confirmed).
  - dYdX also geoblocks the US (news).
  - None of them are trading targets.

*2. API quality*
- **Coinbase Advanced Trade:**
  - Create Order requires a unique `client_order_id`.
  - Order types: market IOC and FOK; limit GTC, GTD and FOK; `post_only` on limit GTC and GTD; stop-limit; a native bracket (`trigger_bracket`); and TWAP. It has `leverage` and `margin_type` fields.
  - Cancel is `batch_cancel`. There is no cancel-all endpoint, and **no reduce-only flag** is documented.
  - Private REST rate limits aren't stated in the Advanced Trade docs (**unverified**).
  - The WebSocket allows 8 connections and 8 unauthenticated messages per second per IP. Channels close after 60 to 90 seconds without updates unless `heartbeats` is subscribed.
  - Docs are good but change often: the `/intx` endpoints were retired and the derivatives docs reorganized around Deribit.
- **Kraken spot:**
  - Order entry over WebSocket v2, with `cl_ord_id`, post-only, IOC, FOK and GTD, reduce-only for margin, cancel-all and a dead-man's switch.
  - Rate limits are per-pair counters with decay.
  - Docs are thorough and versioned, and there is an official Rust CLI.
- **Kraken US perps:**
  - FIX 4.4 order entry: market and limit; GTC, IOC and FOK; mass cancel; cancel-on-disconnect on by default.
  - Market data is SBE binary with L3 books.
  - Whether an individual retail account can get this access is **unverified**.
  - This is a much larger connector than a JSON WebSocket.
- **Kraken global derivatives API** (`futures.kraken.com`, which Lines already uses for data): REST order entry, a streaming-only WebSocket, a budget of 500 cost per 10 seconds, and a self-service demo. It isn't the US perps venue.

*3. Perp mechanics over the API*
- **Coinbase:**
  - The public `GET /api/v3/brokerage/market/products?product_type=FUTURE` endpoint was sampled with no auth.
  - Each perp-style product reports `funding_rate`, `funding_interval` (3600 s, hourly), the next `funding_time`, `open_interest`, `index_price`, and intraday and overnight margin rates.
  - BTC margin is about 10% intraday (10x) and 21% to 26% overnight. Contract sizes are 0.01 BTC and 0.1 ETH.
  - Funding accrues hourly and is applied at the midday and end-of-day margin cycles (help center).
  - Positions come from `GET /cfm/positions`, balances from `GET /cfm/balance_summary` and the `futures_balance_summary` WebSocket channel.
  - No funding-payment history endpoint was found, and no mark-price or liquidation feed (**unverified**).
  - `margin_type` (isolated) was an INTX field. US futures appear to be cross-margined by the FCM (**unverified**).
- **Kraken US perps:**
  - Funding nets daily at 3:00 pm CT. A shortfall is taken from position margin.
  - Liquidation happens below maintenance margin, with a $10 fee for perps-only accounts.
  - Mark price, margin mode, and how funding is reported over FIX are **unverified**: the consolidated PDF wasn't read.
- **Kraken Futures global** (public, sampled):
  - The ticker gives `markPrice`, `indexPrice`, `fundingRate`, `fundingRatePrediction` and `openInterest`.
  - Hourly historical funding comes from `/derivatives/api/v4/historicalfundingrates`.
  - Public trade history has a `type` field, and the docs list liquidation as a type. None appeared in a 100-trade sample.
- **Hyperliquid** (public `POST /info` `metaAndAssetCtxs`, sampled):
  - It returns `funding`, `openInterest`, `markPx`, `oraclePx`, `premium` and `impactPxs`. Funding is hourly.
  - The limit is 1200 weight per minute per IP.
  - The public `trades` feed doesn't flag liquidations (**unverified**), so liquidation markers would need another source or inference.
- **Binance and Bybit** have rich public funding, open interest and liquidation streams, but they can't be reached from US IPs.

*4. Data licensing*
- **Coinbase Market Data Terms:** personal or research use only. They bar redistributing or displaying the data, or works derived from it, to third parties.
- **Kraken:** commercial use of public market data beyond personal use needs permission (marketdata@kraken.com).
- **Hyperliquid:** no data-use clause found (**unverified**).
- **Binance:** its historical data portal (data.binance.vision) is non-commercial only.
- **Lines:** each user's own app connects to the venue, so Lines doesn't redistribute data, which matches the "personal use" grants.
  - Selling an official build, or hosting data for others, would need each venue's written permission.
  - Recordings and screenshots from these feeds follow 0030's marking rule.

*5. Fit with Lines*
- Fits 0020 and 0030: each venue is a private connector, with trade-only API keys in the OS keychain. Coinbase CDP keys are scoped to one portfolio and can be made trade-only.
- The paper engine needs:
  - fractional quantity by product increment;
  - hourly funding (Coinbase) and daily funding (Kraken US);
  - per-contract plus percentage fees;
  - Coinbase's overnight margin step-up at 4:00 pm ET.
- Reduce-only isn't available on Coinbase, so Lines would enforce it locally, as it does for brackets.

*Still open*
- Whether individuals can get API access to Kraken's US perps, and which contracts it lists (equity or crypto).
- Coinbase's US futures fees and private rate limits, and whether funding payments and liquidations are reported per position.
- Hyperliquid's current terms text, and a liquidation data source a US user can reach.

*Links*
- Coinbase:
  - [US derivatives API](https://docs.cdp.coinbase.com/coinbase-app/advanced-trade-apis/guides/futures)
  - [Create Order](https://docs.cdp.coinbase.com/api-reference/advanced-trade-api/rest-api/orders/create-order)
  - [WebSocket channels](https://docs.cdp.coinbase.com/coinbase-app/advanced-trade-apis/websocket/websocket-channels)
  - [WebSocket rate limits](https://docs.cdp.coinbase.com/coinbase-app/advanced-trade-apis/websocket/websocket-rate-limits)
  - [Sandbox](https://docs.cdp.coinbase.com/coinbase-app/advanced-trade-apis/sandbox)
  - [US perpetual-style futures overview](https://help.coinbase.com/en/coinbase/derivatives/us-perpetual-futures-overview)
  - [Market Data Terms](https://www.coinbase.com/legal/market_data)
- Kraken:
  - [US Perps APIs](https://docs.kraken.com/exchange/guides/us-perps/api)
  - [US perpetual futures](https://support.kraken.com/articles/us-perpetual-futures)
  - [US futures fees](https://support.kraken.com/articles/us-futures-fees)
  - [Kraken Pro fees](https://www.kraken.com/pro/fees)
  - [Spot WebSocket v2](https://docs.kraken.com/exchange/api-reference/spot-websocket-v2/ticker)
  - [Derivatives demo](https://docs.kraken.com/exchange/guides/futures/introduction)
- [CME 24/7 crypto trading (press release)](https://www.cmegroup.com/media-room/press-releases/2026/6/01/cme_group_announceslaunchof247cryptocurrencyfuturesandoptionstra.html)
- Hyperliquid:
  - [Rate limits](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/rate-limits-and-user-limits)
  - [Info endpoint](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/info-endpoint)
- [Binance terms](https://www.binance.com/en/terms)

**Consequences (once proposed and accepted):** new private connectors in `lines-connectors`, core changes for fractional quantity display, 24/7 sessions and funding in P&L, and screen changes in the trade panel, position card and Positions pane.
