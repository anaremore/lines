# Catalog sources: CME Group futures

Where each futures spec in `crates/lines-feed/src/catalog.rs` comes from. Checked 2026-11-09.

**How it was checked.** cmegroup.com refuses automated access (HTTP 403), so CME's pages were
read through search-engine excerpts of CME's own product pages, fact cards and rulebook
chapters (the URLs below), not opened directly. Tick sizes, multipliers, exchanges and listed
months were then cross-checked against the instrument definitions CME publishes through a
market-data vendor's `GLBX.MDP3` feed (minimum price increment, unit-of-measure quantity,
exchange, listed maturities, fraction display; pulled 2026-11-09). Every tick and multiplier
below agrees with both. Where a rule wasn't confirmed by a CME excerpt, the table says so.

Prices are exact integers in each product's unit (`Price(i64)`, decision 0002); the tick value
is `tick × unit × point value` exactly (`catalog::tests::cme_tick_values_are_exact`).

| Product | Id | Exchange | Unit (price) | Tick | Point value | Tick value | Display | Months (catalog) | Last trade (calendar rule) | Calendar roll | RTH (CT) | Holidays |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| ES | 1 | CME | 0.01 | 0.25 | $50 | $12.50 | decimal | HMUZ | 3rd Friday | 8 days before | 08:30–15:00 | cme-equity-index |
| NQ | 2 | CME | 0.01 | 0.25 | $20 | $5.00 | decimal | HMUZ | 3rd Friday | 8 days before | 08:30–15:00 | cme-equity-index |
| YM | 6 | CBOT | 1 | 1 | $5 | $5.00 | decimal | HMUZ | 3rd Friday | 8 days before | 08:30–15:00 | cme-equity-index |
| RTY | 7 | CME | 0.01 | 0.10 | $50 | $5.00 | decimal | HMUZ | 3rd Friday | 8 days before | 08:30–15:00 | cme-equity-index |
| MES | 4 | CME | 0.01 | 0.25 | $5 | $1.25 | decimal | HMUZ | 3rd Friday | 8 days before | 08:30–15:00 | cme-equity-index |
| MNQ | 5 | CME | 0.01 | 0.25 | $2 | $0.50 | decimal | HMUZ | 3rd Friday | 8 days before | 08:30–15:00 | cme-equity-index |
| MYM | 8 | CBOT | 1 | 1 | $0.50 | $0.50 | decimal | HMUZ | 3rd Friday | 8 days before | 08:30–15:00 | cme-equity-index |
| M2K | 9 | CME | 0.01 | 0.10 | $5 | $0.50 | decimal | HMUZ | 3rd Friday | 8 days before | 08:30–15:00 | cme-equity-index |
| ZT | 12 | CBOT | 1/256 | 1/8 of 1/32 | $2,000 | $7.8125 | 32nds + eighths (`110'033`) | HMUZ | last business day of the month | 2 days before first notice | 07:20–14:00 | cbot-treasury |
| ZF | 11 | CBOT | 1/256 | 1/4 of 1/32 | $1,000 | $7.8125 | 32nds + quarters (`108'077`) | HMUZ | last business day of the month | 2 days before first notice | 07:20–14:00 | cbot-treasury |
| ZN | 3 | CBOT | 1/128 | 1/2 of 1/32 | $1,000 | $15.625 | 32nds + quarters (`112'165`) | HMUZ | 7th business day before the last | 2 days before first notice | 07:20–14:00 | cbot-treasury |
| ZB | 10 | CBOT | 1/128 | 1/32 | $1,000 | $31.25 | 32nds (`117'31`) | HMUZ | 7th business day before the last | 2 days before first notice | 07:20–14:00 | cbot-treasury |
| CL | 13 | NYMEX | 0.01 | 0.01 | $1,000 | $10.00 | decimal | monthly | 3 business days before the 25th of the prior month* | 5 days before last trade | 08:00–13:30 | cme-energy-metals |
| MCL | 14 | NYMEX | 0.01 | 0.01 | $100 | $1.00 | decimal | monthly | 1 business day before CL's | 5 days before last trade | 08:00–13:30 | cme-energy-metals |
| NG | 15 | NYMEX | 0.001 | 0.001 | $10,000 | $10.00 | decimal | monthly | 3rd last business day of the prior month | 5 days before last trade | 08:00–13:30 | cme-energy-metals |
| GC | 16 | COMEX | 0.1 | 0.10 | $100 | $10.00 | decimal | GJMQVZ (active) | 3rd last business day of the month | 2 days before first notice | 07:20–12:30 | cme-energy-metals |
| MGC | 17 | COMEX | 0.1 | 0.10 | $10 | $1.00 | decimal | GJMQVZ | 3rd last business day of the month | 2 days before first notice | 07:20–12:30 | cme-energy-metals |
| SI | 18 | COMEX | 0.001 | 0.005 | $5,000 | $25.00 | decimal | HKNUZ (active) | 3rd last business day of the month (unconfirmed) | 2 days before first notice | 07:20–12:30 | cme-energy-metals |
| HG | 19 | COMEX | 0.0001 | 0.0005 | $25,000 | $12.50 | decimal | HKNUZ (active) | 3rd last business day of the month | 2 days before first notice | 07:20–12:30 | cme-energy-metals |
| 6E | 20 | CME | 0.00001 | 0.00005 | $125,000 | $6.25 | decimal | HMUZ | 2 business days before the 3rd Wednesday | 7 days before last trade | 07:20–14:00 | cme-fx |
| 6J | 21 | CME | 0.0000001 | 0.0000005 | $12,500,000 | $6.25 | decimal | HMUZ | 2 business days before the 3rd Wednesday (unconfirmed for 6J) | 7 days before last trade | 07:20–14:00 | cme-fx |
| 6B | 22 | CME | 0.0001 | 0.0001 | $62,500 | $6.25 | decimal | HMUZ | 2 business days before the 3rd Wednesday (unconfirmed for 6B) | 7 days before last trade | 07:20–14:00 | cme-fx |
| ZC | 23 | CBOT | 1/8 cent | 1/4 cent | $50 per cent | $12.50 | cents + eighths (`432'2`) | HKNUZ | business day before the 15th | 2 days before first notice | 08:30–13:20 | cbot-grains |
| ZS | 24 | CBOT | 1/8 cent | 1/4 cent | $50 per cent | $12.50 | cents + eighths | FHKNQUX | business day before the 15th | 2 days before first notice | 08:30–13:20 | cbot-grains |
| ZW | 25 | CBOT | 1/8 cent | 1/4 cent | $50 per cent | $12.50 | cents + eighths | HKNUZ | business day before the 15th | 2 days before first notice | 08:30–13:20 | cbot-grains |

\* When the 25th isn't a business day, the count starts from the business day before it.

ETH (Globex) is Sunday–Friday 17:00–16:00 CT with the daily 16:00–17:00 pause for every product
above except the grains: Sunday–Friday 19:00–07:45 and Monday–Friday 08:30–13:20 CT, modelled as
one session from 19:00 to 13:20 (the 07:45–08:30 pause isn't modelled; `docs/known-gaps.md`).
RTH is the former floor session (energy 08:00–13:30, metals the gold floor's 07:20–12:30, FX the
IMM floor's 07:20–14:00) or the grains' day session. RTH windows are a charting convention, not
a CME specification.

First notice day, for the rolls, is the last business day of the month before the contract
month: confirmed for Treasuries (CME's delivery-process guide); assumed for metals and grains,
where delivery starts in the contract month. Calendar rules ignore exchange holidays (business
day = Monday–Friday) and are the fallback only: a provider's expiry wins, and the front month
follows volume when it is known (decision 0013).

Listed months: the catalog lists each product's main cycle. Gold, silver, copper and the FX
products also list serial months (CME lists gold monthly for the nearest months, FX monthly
contracts beside the quarterlies); those aren't in the catalog, so `GCX26` is not a contract
here. Gold's active months now include October (SER-9637, December 2025).

Not modelled: variable ticks. No outright here has one (the vendor definitions carry no tick
rule for any of them); calendar spreads trade finer than outrights for M2K, ZF and SI, and
spreads aren't charted.

## Sources

Equity index:
- https://www.cmegroup.com/articles/faqs/frequently-asked-questions-micro-e-mini-equity-index-futures.html (MES, MNQ, MYM, M2K units, ticks, 3rd Friday)
- https://www.cmegroup.com/trading/equity-index/files/cme-micro-e-mini-futures-fact-card.pdf
- https://www.cmegroup.com/markets/equities/dow-jones/e-mini-dow.contractSpecs.html and https://www.cmegroup.com/rulebook/CBOT/III/27.pdf (YM)
- https://www.cmegroup.com/markets/equities/russell/e-mini-russell-2000.contractSpecs.html (RTY)
- https://www.cmegroup.com/markets/equities/russell/micro-e-mini-russell-2000.contractSpecs.html (M2K)
- https://www.cmegroup.com/notices/electronic-trading/2021/06/20210621.html (the 15:15–15:30 CT equity halt removed, June 2021)

Rates:
- https://www.cmegroup.com/markets/interest-rates/us-treasury/10-year-us-treasury-note.contractSpecs.html (ZN)
- https://www.cmegroup.com/content/dam/cmegroup/rulebook/CBOT/II/18.pdf (ZB, chapter 18)
- https://www.cmegroup.com/rulebook/CBOT/II/20.pdf (ZF, chapter 20)
- https://www.cmegroup.com/rulebook/CBOT/II/21.pdf (ZT, chapter 21)
- https://www.cmegroup.com/trading/interest-rates/files/us-treasury-futures-delivery-process.pdf (first position and notice days)

Energy:
- https://www.cmegroup.com/markets/energy/crude-oil/light-sweet-crude.contractSpecs.html, https://www.cmegroup.com/trading/files/WTI-Fact-Card.pdf, https://www.cmegroup.com/rulebook/NYMEX/2/200.pdf (CL, rule 200102)
- https://www.cmegroup.com/market-regulation/rule-filings/2021/6/21-154.pdf, https://www.cmegroup.com/trading/energy/files/micro-wti-crude-oil-futures-fact-card.pdf (MCL, chapter 309)
- https://www.cmegroup.com/rulebook/NYMEX/2/220.pdf (NG, chapter 220)

Metals:
- https://www.cmegroup.com/rulebook/COMEX/1a/113.pdf, https://www.cmegroup.com/trading/metals/files/fact-card-gold-futures-options.pdf, https://www.cmegroup.com/content/dam/cmegroup/notices/ser/2025/12/ser-9637.pdf (GC)
- https://www.cmegroup.com/rulebook/COMEX/1a/120.pdf (MGC, chapter 120)
- https://www.cmegroup.com/rulebook/COMEX/1a/112.pdf (SI, chapter 112)
- https://www.cmegroup.com/rulebook/COMEX/1a/111.pdf (HG, chapter 111)

FX:
- https://www.cmegroup.com/rulebook/CME/III/250/261/261.pdf (6E, chapter 261)
- https://www.cmegroup.com/rulebook/CME/III/250/253/253.pdf (6J, chapter 253)
- https://www.cmegroup.com/education/lessons/british-pound-product-overview (6B)
- https://www.cmegroup.com/markets/fx/fx-product-guide.html

Agriculture:
- https://www.cmegroup.com/rulebook/CBOT/I/10.pdf (ZC, chapter 10)
- https://www.cmegroup.com/rulebook/CBOT/II/11/11.pdf (ZS, chapter 11)
- https://www.cmegroup.com/rulebook/CBOT/II/14/14.pdf (ZW, chapter 14)
- https://www.cmegroup.com/trading/agricultural/files/grain-and-oilseed-futures-options-fact-card.pdf (grain hours)

Hours and holidays: https://www.cmegroup.com/trading-hours.html and the sources in
`crates/lines-feed/data/exchange-holidays.txt` (every calendar there is unverified).
