# Brief: Sierra Chart importer (roadmap 3b)

> **Status: done** (roadmap 3b, `a40523c`). Kept for reference; findings that differ from this brief are recorded in `docs/known-gaps.md` and decision 0015.

## Goal
Convert local Sierra Chart tick (`.scid`) and market depth (`.depth`) files into Lines' replay format, so the DOM, paper broker, and chart trading (roadmap 5–8) can be built and tested on real CME data without any vendor contract.

## Source data (owner's machine; never commit: decision 0010)
`C:\SierraChart\SierraChart - Apex\Data\` (path in `DataFilesFolder.txt`), ~137 GB.
- **Depth:** `Data\MarketDepthData\<SYMBOL>.<YYYY-MM-DD>.depth`, one file per symbol per day. 291 days of NQ (NQH5–NQH6, 2025-03-09 → 2026-03-24). No MNQ/MES depth yet.
- **Ticks:** `Data\<SYMBOL>.scid`, one file per contract. MNQ/MES M5→Z6 (live, still growing), NQ H5→H6, ES H5/M5. Files are 1–6 GB. Some are empty (56-byte header only).
- **Owner's real fills:** `TradeActivityLogs\TradeActivityLog_<date>_UTC.<account>.data` (6,036 files). Contains account numbers. For the fill-calibration test only, read locally, never copied into recordings.

## Scope and licence caution
Sierra Chart has **permanently removed** its public "Market Depth Data File Format" page and points readers to its licence agreement. Treat this importer as a **personal development tool**: local use on the owner's own data only. It is not a product feature and must not ship to users until the Sierra licence has been reviewed. Keep it behind a dev-only Cargo feature or a separate binary.

## Formats (header and record sizes verified against files on disk; `.scid` is documented under Sierra Chart's "Intraday Data File Format"; the `.depth` layout below is from the formerly public spec, so verify it empirically)
All little-endian. Timestamps are `SCDateTimeMS`: `i64` microseconds since 1899-12-30 00:00:00 UTC.

**`.scid`**: header 56 bytes: magic `SCID`, `u32 header_size` (=56), `u32 record_size` (=40), `u16 version`, rest reserved. Record 40 bytes:
`i64 datetime, f32 open, f32 high, f32 low, f32 close, u32 num_trades, u32 total_volume, u32 bid_volume, u32 ask_volume`.
For tick records: `close` = trade price, `high`/`low` = ask/bid at the time of the trade, `bid_volume`/`ask_volume` give aggressor side. `open` may be 0 or a sentinel. Check the docs for the tick-record conventions, and note that Sierra makes timestamps unique within a millisecond by incrementing microseconds.

**`.depth`**: header 64 bytes: magic `SCDD`, `u32 header_size` (=64), `u32 record_size` (=24), `u32 version`, rest reserved. Record 24 bytes:
`i64 datetime, u8 command, u8 flags, u16 num_orders, f32 price, u32 quantity, u32 reserved`.
Commands: 1 clear book, 2 add bid level, 3 add ask level, 4 modify bid, 5 modify ask, 6 delete bid, 7 delete ask. `flags & 1` = end of batch (apply the batch atomically).

## Requirements
1. **Stream, never load whole files.** Buffered reader, bounded memory regardless of file size. Allow seeking to a start time (binary search on record timestamps for `.scid`; per-day files for `.depth`).
2. **Exact prices (decision 0002).** Convert `f32` prices to integer ticks using the instrument's tick size. Round to nearest and **reject/count** values that aren't within a tiny epsilon of a tick. Never store the float.
3. **Instrument mapping (decision 0003).** Map Sierra symbols (`NQZ5.CME`, `MNQZ6.CME`, legacy `NQH25-CME`) to internal instrument IDs; Sierra names never leave the importer.
4. **Merge ticks + depth by timestamp** into one replay stream for a symbol/day, preserving batch boundaries.
5. **Validate and report:** magic, header/record sizes, monotonic time, book never crossed after a batch, counts of rejected records. Skip header-only files cleanly.
6. **Output** the existing replay format (or extend it with depth events if it lacks them, documenting the change). Output goes to `recordings/` (gitignored).
7. **CLI:** e.g. `lines import-sierra --symbol NQZ5.CME --date 2025-09-11 --out recordings/nqz5-2025-09-11.lfr`.

## Tests
- Unit tests on **synthetic** byte buffers built in the test (no real data in git): header parsing, every depth command, batch flags, price-to-tick rejection, timestamp conversion.
- An `#[ignore]` integration test that runs only when `LINES_SIERRA_DATA` points at a real folder: import one NQ day, replay it, assert the book is never crossed and trade prices fall within the bid/ask.

## Done when
An NQ day with depth replays in Lines with a correct book and trades, the import is reproducible, and `ROADMAP.md` 3b is marked done.

## Follow-on (not this milestone)
- **Fill calibration:** parse TradeActivityLogs, replay the same day, submit the owner's orders through the PaperBroker, and compare simulated vs real fills to tune the default fill model (decision 0005).
- MNQ/MES depth: the owner is enabling Sierra depth recording for current micros; same importer.
