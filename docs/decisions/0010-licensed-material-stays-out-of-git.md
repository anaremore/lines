# 0010: Licensed data and vendor files never enter the repo

**Status:** Accepted

**Decision:** Feed recordings, Sierra Chart `.scid`/`.depth` files, and any vendor-proprietary protos or SDKs stay out of git (enforced in `.gitignore`). Public vendor material is referenced by submodule, not copied. Test fixtures committed to the repo must be synthetic.
