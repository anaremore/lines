---
name: full-stop
description: Take a change to a green, reviewed PR and hand it to the owner to merge. Use when the owner says "full stop" or wants to merge a change themselves.
---

# Full stop

Follow `full-send` steps 1-8 exactly, with these differences:
- Any tier; tier C still needs the owner's yes before you start (CLAUDE.md "Risk tiers").
- No "full send" phrase is needed.
- Never merge. When every check passes and `review-loop` is clean, report
  "PR #<n> is green and reviewed, ready for you to merge", then `unwatch_pull_request`.

## Proof: the final report must contain
- Everything in full-send's proof except the merge commit and main-CI lines.
- The tier, and the specific risk the owner should look at
  (e.g. "changes which shell commands the guard blocks").

## Stop and ask instead of continuing
- Everything in full-send's list.
