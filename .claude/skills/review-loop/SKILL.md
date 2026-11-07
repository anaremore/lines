---
name: review-loop
description: Fresh-context Claude review of the current branch's diff, then fix and repeat until there are no blocking issues (at most 3 rounds). Use before opening any PR, or when asked for a "review loop".
---

# Review loop

Each round (at most 3):
1. `git fetch origin && git diff origin/main...HEAD > <scratch>/review-<round>.diff`.
   Write the task as given (Why / What / Constraints / Proof) to `<scratch>/task.md`.
2. Spawn a NEW `reviewer` agent (Agent tool, `subagent_type: "reviewer"`). Never continue an
   earlier reviewer, and never pass your reasoning, summaries or earlier findings. Prompt it
   with only: "Task: <scratch>/task.md. Diff: <scratch>/review-<round>.diff. Read CLAUDE.md and
   the docs/invariants.md sections for every crate in the diff, then review."
3. Blocking: a correctness bug with a concrete failure; a broken CLAUDE.md, `docs/invariants.md`,
   `docs/design.md` or accepted-decision rule; changed behavior with no test; data or secret rules.
   Everything else is non-blocking (fix it if it is cheap).
4. Fix the blocking issues, rerun gate steps 1, 2 and 4 (plus 6 for UI, 8 for `.claude/`), commit, and start
   the next round. A blocker you dispute stays open unless the next fresh reviewer agrees.

## Proof: report per round
- The number of findings by severity, and each blocker's outcome: "fixed in <sha>", or
  "disputed: <reason>". The last round must show 0 blocking.

## Stop and ask instead of continuing
- Round 3 ends with open blockers: report them and stop.
- The reviewer finds two documented rules that conflict.
- A fix would need tier B or C changes that weren't in the task.
