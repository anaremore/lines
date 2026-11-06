# 0009: UI pull requests include reproducible screenshots

**Status:** Accepted

**Decision:** The app has a screenshot mode: replay file + fixed timestamp, window size, DPI scale, theme, and frozen clock → PNG, then exit. UI PRs include before/after images produced by the same command; interaction changes include a short GIF/video. Screenshots supplement, never replace, state-based replay tests.

**Consequences:** Pixels differ across GPUs/drivers, so any automated golden-image check must use tolerance or compare emitted geometry instead.
