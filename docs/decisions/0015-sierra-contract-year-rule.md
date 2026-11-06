# 0015: Sierra contract-year resolution (amends 0013)

**Status:** Proposed (supersedes only the Sierra decade clause of 0013)

**Context:** 0013 says single-digit Sierra years resolve to "the nearest decade at or before the data's dates". That fails at a decade boundary: `NQH0` traded in December 2029 is the March 2030 contract, not 2020.

**Decision:** A single-digit year resolves to the earliest year ending in that digit that is not before the file's last record date, because a contract cannot trade after it expires. Implemented in `938ec7e` with a boundary test.

**Consequences:** Identical results on all current files; correct across decade boundaries.
