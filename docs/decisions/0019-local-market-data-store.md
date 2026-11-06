# 0019: Local market-data store

**Status:** Accepted

**Context:** History is fetched from the provider on every launch. That can't meet the ~2 s startup target (`ROADMAP.md`), providers cap how far back data goes, and session-anchored analytics (VWAP, volume profile, decision 0017) need complete sessions. Recording and replay (0011) already define a normalized event encoding (`lines-events-1`).

**Decision:**
- **What's stored:** per contract, the normalized trade stream (and quotes/top of book) in the existing `lines-events-1` encoding, partitioned by **session trade date** (from the sessions calendar). Depth is **off by default** (large) and can be enabled per product. Bars are derived from trades and cached as a rebuildable index, never treated as the source of truth.
- **Format:** our own append-only segment files, versioned. Each block has a length and CRC. No external database: it's simpler, faster for sequential reads, and replay-compatible.
- **Writing:** one writer per contract on the feed side (never the UI thread, 0001), batched appends, and a bounded fsync policy. On startup, a torn tail (crash mid-write) is detected by CRC and truncated, never "repaired" with guesses.
- **Reading:** startup shows cached data immediately, then the provider fills from the last stored point using the existing stitch rule (`lines_core::stitch`). Readers are read-only and concurrent with the writer.
- **Gaps:** the store records which time ranges are known complete per contract. Missing ranges are fetched when possible and otherwise shown as missing (`docs/design.md` §7). Nothing is interpolated.
- **Location and licence:** the OS data directory, local only, never synced or uploaded. Stored data may be licensed (0010). A size budget with oldest-first retention is configurable, and the defaults are stated in the UI.
- **Replay unification:** a stored session is replayable like a recording, so the Sierra imports, live captures and the cache share one tool path.

**Consequences:**
- New roadmap 5a, before workspaces. Startup and indicator back-fill depend on it.
- Needs the sessions calendar (5c prerequisite, in progress) for trade-date partitioning.
- Store integrity tests (torn tail, concurrent read during write, gap tracking, retention) join the CI suite.
