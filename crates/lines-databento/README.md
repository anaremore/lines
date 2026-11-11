# lines-databento

Databento's CME Globex data (`GLBX.MDP3`) in Lines (decision 0033), with **your own Databento
account and API key**: Lines has no Databento identity of its own and carries no Databento data.

- **Live market data:** built into the `lines` app as the `databento` source (`src/live/`,
  feature `live`): trades with aggressor, the inside bid and ask, and an intraday replay of trades
  as back-fill. Billed by Databento.
- **History:** "Download days…" in the app (decision 0030 §8) fetches whole past trade dates
  into the local store. The `lines-databento` command line (`cost`, `download`, `contracts`,
  `import`, `days`) does the same from a terminal and turns Databento's DBN files into Lines
  replays (`.lfr`), exact and deterministic.

| Command | What it does |
|---|---|
| `cost` | Prices a request with Databento's free cost check. It downloads and bills nothing. |
| `download` | Runs the cost check first and downloads only if the total is within `--max-usd`. Files are DBN + zstd. |
| `contracts` | Lists the contracts and days covered by the `definition` records in some files. |
| `import` | Converts DBN files (`definition`, `trades`, `mbp-1`, `mbp-10`) into one `.lfr` replay for one contract. The output is exact and deterministic. |
| `days` | Whole past trade dates into the local store, as the app's "Download days…" does: the free estimate per day, and with `--download` the days, only if their total estimate is within `--max-usd` (default 0: nothing that costs money). |

Data from Databento is licensed (decision 0010): downloads, imported recordings and
screenshots made from them stay on your machine and never enter this repository.

## Products

Every CME Group future in the public catalog (its `docs/catalog-sources.md` has the specs and
sources), mapped by `PRODUCTS` in `src/live/mod.rs`:

| Group | Roots |
|---|---|
| Equity index | ES, NQ, YM, RTY, MES, MNQ, MYM, M2K |
| Rates | ZT, ZF, ZN, ZB |
| Energy | CL, MCL, NG |
| Metals | GC, MGC, SI, HG |
| FX | 6E, 6J, 6B |
| Agriculture | ZC, ZS, ZW |

Raw symbols are root + month code + year: one year digit (`CLZ6`, `ESZ0` for 2030) for every
root but NG, which always uses two (`NGZ26`), and CL from 2030 on (`CLZ9`, then `CLF30`; its
listings run more than ten years out). MCL stays at one digit (`MCLF0` for 2030). Checked on
2026-11-09 against Databento's historical `definition` files for trade date 2026-11-06, every
listed contract of every root (definitions only, no market data; the cost check said $0.00),
on top of the live gateway's front-month definitions pulled the same day (the evidence file).

Known gap: the CL rule (two digits from 2030) is a snapshot of one day's symbols. When the
decade turns, CME may write 2030s contracts differently (or start two digits at a later
year); re-check the definitions before 2030. A wrong guess is refused as `BadMetadata`
(nothing charts), never silently mapped. Contracts outside the
catalog's months (gold's serial months, FX monthlies) are skipped, not charted. A dynamic
catalog built from the definitions themselves is later work.

## Setting up

### 1. Your key

Create an account at databento.com and an API key in its portal (it starts with `db-`). Lines
reads the key from, in order:

1. `DATABENTO_API_KEY` (Databento's own convention): an override for one run.
2. The OS keychain, entry `lines-databento` (the key as its password):
   - Windows: `cmdkey /generic:lines-databento /user:databento /pass` (it prompts for the
     key, so it never lands in your shell history).
   - macOS: `security add-generic-password -s lines-databento -a databento -w` (it prompts).
   - Linux (Secret Service): `secret-tool store --label="Lines Databento" service lines-databento
     username databento`.
3. A file named by `LINES_DATABENTO_KEY_FILE` (the key alone, outside any repository).

With no key, the `databento` source is "not set up": Lines makes no Databento requests and
leaves it out of the run's sources. The key is used only in the historical API's
`Authorization` header and to answer the live gateway's challenge (SHA-256). It is never
printed, logged, recorded, put in a URL or written to any file. The simplest way in is the app:
Settings › Connections takes the key and keeps it in this same keychain entry (decision 0030).

### 2. Cost check (free)

Pick the days. Databento's range is in UTC, and the end is exclusive. A CME session starts the
evening before its trade date (17:00 CT = 23:00 UTC in winter). To cover the trade dates
Tue 10 to Thu 12 November in full, use 2026-11-09 to 2026-11-13. The front month until the
14 December roll is Z6.

```sh
lines-databento cost --symbols ESZ6,NQZ6 --start 2026-11-09 --end 2026-11-13
```

It prints the uncompressed size (what Databento bills) and the dollar cost per schema
(`definition`, `trades`, `mbp-10`), plus a total. mbp-10 dominates. A third-party estimate is
about 1.2 GB per day for ES mbp-10 uncompressed; your figure is the one that counts.

- mbp-10 already contains every trade. To save the `trades` cost, use
  `--schemas definition,mbp-10`, and the importer takes the trades from mbp-10.
- Don't use `--stype-in parent --symbols ES.FUT`. It selects every ES contract and spread and
  costs much more.

### 3. Download

Set `--max-usd` a little above the cost check. Nothing downloads if the total is higher.

```sh
lines-databento download --symbols ESZ6,NQZ6 --start 2026-11-09 --end 2026-11-13 --max-usd 10
```

The files go to `data/databento/`, which is gitignored. Never commit them: the data is licensed
(decision 0010).

### 4. Import into a Lines replay

Make one recording per contract. Pass all three files; the importer picks the contract's
records out of each.

```sh
lines-databento import --contract ESZ6 --out recordings/esz6-2026-11-09.lfr data/databento/GLBX_MDP3-*-2026-11-09-2026-11-13.dbn.zst
lines-databento import --contract NQZ6 --out recordings/nqz6-2026-11-09.lfr data/databento/GLBX_MDP3-*-2026-11-09-2026-11-13.dbn.zst
```

The import prints a report. What to expect:

- `trades rejected 0` and `off-grid levels 0`. Anything else means a price wasn't exact. It is
  counted and skipped, never rounded; please report it.
- `unmapped 0`. A non-zero count means a day's `definition` file is missing.
- `crossed batches` and `maybe-bad-book records` near zero. They come from Databento's own data
  quality flags.

If a definition disagrees with Lines' catalog (tick size, multiplier, currency, exchange or
symbol), the import stops with `BadMetadata` and writes nothing.

### 5. Open it in Lines

```sh
lines --source replay --replay recordings/esz6-2026-11-09.lfr --speed 10
```

`--speed 0` plays as fast as possible; `--at 2026-11-10T14:30:00Z` stops at a moment.

## Live market data

With a key, `lines` has the `databento` source next to Coinbase and Kraken: a chart or DOM of a
product in the table above opens on it (Settings › Connections turns it off and takes its key). `--source databento`
limits a run to it. Live data is billed by Databento; what it asks for per contract shown:
- `definition` replayed from the start of the weekly session (one contract: small);
- `trades` replayed from up to 24 hours back (the chart's back-fill), then live;
- `mbp-1` live only (no replay): the inside bid and ask. The DOM shows those two levels, says
  "Top of book only", and leaves the other rows blank.

Settings (environment, for now): `LINES_DATABENTO_REPLAY_HOURS` caps the trades replay (for
example `1`; `0` = none). Unset, it is the gateway's whole window (about 23 h 40 min), which
covers a normal session from its open, so its VWAP starts there; a longer session (after a
holiday, say) starts its VWAP late and says so ("from HH:MM"). A cap you set stands, even when it
cuts the session trading now (its VWAP then says "from HH:MM"). `LINES_DATABENTO_GATEWAY` overrides the gateway;
`LINES_DATABENTO_DEPTH=mbp-10` asks for top-10 depth, which real-time needs Databento's CME Plus
plan for. If the gateway refuses it (it says so and closes the connection), Lines reconnects with
`mbp-1` and doesn't ask again this run. A refused `trades` or `definition` stops the feed with a
clear error. Each contract shown is one connection (Databento allows at most 8).

```sh
# Headless, recorded: prints the feed's stats after 120 s.
LINES_DATABENTO_REPLAY_HOURS=1 lines --source databento --instrument MNQ --headless 120 --record recordings/databento-mnq.lfr
# Replay the recording (no key, no cost):
lines --source replay --replay recordings/databento-mnq.lfr --speed 0
```

The header says "history incomplete" when the app asks for more history than the replay window
holds. Errors from the gateway (a rejected key, a symbol it can't resolve, a DBN version this
build doesn't decode) stop the feed with the gateway's words. The design: [LIVE-DESIGN.md](LIVE-DESIGN.md).

## Download days (the app's "Download days…")

With a key, the `lines` app offers "Download days…" for Databento in the palette (decision
0030 §8). It fetches whole past trade dates for one contract into the local store, under
`store-v1/databento/<contract>/`, where each day replays and warm-starts like a day stored live.
The same engine runs from the command line:

```sh
# The estimate only (free): one line per day, "Included in your plan" or "Estimated $X.XX".
lines-databento days --contract ESZ6 --from 2026-11-02 --to 2026-11-06
# Download them if the total is $0 (--max-usd defaults to 0; the total is printed first).
lines-databento days --contract ESZ6 --from 2026-11-02 --to 2026-11-06 --download
```

- **Contents.** "Trades + top of book" fetches `trades` and `mbp-1` (included in the Standard
  plan for the last 12 months). "Full depth (top 10)" fetches `mbp-10` (included for the last
  month; its trades come from its own records). Every day also pulls the contract's
  `definition` records (free) for the definition check. `mbo` is later work.
- **The day.** A trade date is its CME session: 17:00 CT the evening before to 16:00 CT.
  Definitions are fetched for the whole UTC days around it, because instrument ids are mapped
  per UTC day.
- **Cost.** The estimate is `metadata.get_cost` and `metadata.get_billable_size` per schema,
  which are free. $0 shows as "Included in your plan". A day is fetched only at or below the
  price you accepted: the price is checked again just before the day is fetched.
- **Refusals.** An entitlement or symbol error shows Databento's own message.
- **Definition first.** The contract's definition (free) is fetched and checked before any
  paid schema is requested; a disagreeing definition stops the day there.
- **Contract life.** A date after the contract's expiry, or more than 5 years (quarterly
  products; 9 for other one-digit-year roots, 12 for two-digit) before it, is refused before
  any cost check: a one-digit raw symbol a decade off names another contract.
- **Conversion.** The importer's (`import::convert`): exact prices, the definition check
  (tick, point value, currency, exchange, symbol; a disagreeing definition stops the day), and
  the best bid and ask written only when a price changes.
- **Data hygiene.** The DBN files are written to a scratch folder inside the store and deleted
  with the day. Nothing is written outside the data directory, and the data must never be
  committed or attached anywhere (decisions 0010, 0030).
- **Never twice.** A day already in the store is never estimated or fetched again. A contract
  a live feed is storing is refused until its panes close.

## How the import works

- **Contracts.** Instrument ids map to contracts through that UTC day's `definition` records.
  Databento ids are unique only within a day and may be reused. A record with no definition
  for its day is counted as unmapped and dropped, never guessed.
- **Checks.** Every definition is checked against the catalog (`BadMetadata` on a mismatch):
  the tick in 1e-9 units must equal the catalog's exactly; the multiplier
  (`unit_of_measure_qty` by unit: index points, barrels, MMBtu, troy ounces, pounds and FX
  units as they are, Treasury face value and bushels per 100) must equal the catalog's point
  value exactly; the currency and the exchange (XCME, XCBT, XNYM, XCEC) must match; and the raw
  symbol must match the maturity. The `display_factor` isn't checked: it varies by product (YM 1, GC 0.1, SI 0.001,
  6J 0.000001) and never scales a price, which is always in 1e-9 units. Spreads, options and
  serial months are skipped.
- **Prices.** Exact: 1e-9 units become the catalog's units by integer arithmetic. Off-grid and
  off-tick prices are counted, never rounded.
- **Book.** At every mbp-10 event end (`F_LAST`), Lines writes one atomic batch: the exact
  difference from the previous top 10. A level that leaves the top 10 is sent with quantity 0,
  so the replayed book always equals Databento's top 10. Book clears (action `R`) restart from
  an empty book.
- **Trades.** The aggressor comes from the record's side: `B` buy, `A` sell, `N` unknown.
- **Top of book.** From `mbp-1`, at each event end: the best bid and ask whose price changed,
  as one event. An empty side keeps its last price. Off-grid prices are counted, never rounded.
- **Order.** Records are merged by `(ts_recv, sequence)`, trades first at equal keys. Event
  times are `ts_event`, truncated to µs, since recordings hold µs.
- **Deterministic.** The same files give a byte-identical `.lfr`, whatever the argument order
  and whether the input is `.dbn` or `.dbn.zst` (tested).
- **Bounded memory.** One decoded record per input file at a time.

## Dependencies

- `dbn` (Apache-2.0) for decoding.
- `ureq` with rustls and `ring` for HTTP, and `ring` for the live gateway's challenge: the same
  stack as the rest of Lines (no aws-lc, OpenSSL or native-tls).
- Not the official `databento` client crate: it is built on reqwest, its TLS backend would need
  auditing for aws-lc, and its pre-1.0 API changes often. The HTTP calls needed here are three
  form POSTs.
- `cargo deny --locked check` (the workspace's policy) passes. One reviewed exception:
  `json-writer` (Unlicense, a public-domain dedication) via `dbn`.

## Tests

Unit tests cover prices, definitions, the book diff, the key, and the API forms and responses.
The integration tests (`tests/import.rs`) build synthetic DBN files in code, plain and zstd,
and import them. `tests/live.rs` (feature `live`) builds synthetic gateway traffic: the
handshake with the key kept out of every frame, the definition check, exact trades and books,
every chunk boundary, reconnects without double counts, the gateway's errors, a recording
replayed through the public feed, and the real TCP transport against a fake gateway on loopback.
None uses real data or Databento's network, and none needs a key:

```sh
cargo test -p lines-databento --features live
```

The `cost`, `download` and `days` commands are not run in tests: they need a key.
`tests/import.rs` covers the `mbp-1` conversion and the definition check `days` relies on;
`lines-feed/tests/download.rs` covers the store side (all or nothing, stored days never asked
again, cancel, the live-writer rule) with a synthetic source.
