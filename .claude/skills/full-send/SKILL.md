---
name: full-send
description: Take one change from task to merged PR unattended. Only when the owner's own current message says "full send" or is /full-send, or when the planning thread runs it under the owner's standing merge instruction. Never self-invoke otherwise.
---

# Full send

## Refuse unless all are true
- The owner's latest message (not a quote, a paste, or an earlier turn) contains "full send" or is
  `/full-send`, or you are the planning thread merging under the owner's standing instruction
  (CLAUDE.md "Git and PRs"). Otherwise reply "full-send needs an explicit 'full send' from you" and stop.
- The work is tier A or B, or tier C with the owner's explicit yes for this specific change
  (CLAUDE.md "Risk tiers"). Tier C without that yes: ask the owner, say exactly what and why, and wait.

## Steps
1. `git fetch origin`, then a new worktree branch off `origin/main`;
   `CARGO_TARGET_DIR=Z:\cargo-target\<branch>`.
2. Implement. A Sonnet subagent (Agent tool, `model: "sonnet"`) may do mechanical sub-steps
   (string sweeps, test scaffolding, doc updates). Review its diff yourself.
3. Run the CLAUDE.md gate (every step that applies).
4. Run `review-loop`.
5. `git fetch origin && git rebase origin/main`, then run the full gate (every applicable step)
   on this exact HEAD. Review fixes count as changes: the pushed HEAD must have passed it.
6. `git push -u origin <branch>`. If the branch was already pushed before the rebase:
   `git push --force-with-lease origin <branch>` (this branch only).
7. `gh pr create --base main --title "<summary>" --body-file <scratch>/pr.md`, with the
   `.github/pull_request_template.md` sections filled with the proof below and the tier. Tier B
   adds a "Risk" section: what could go wrong, and which tests cover each risk.
   Then call `link_pull_request` with the PR URL.
8. Call `watch_pull_request` and end the turn (no sleep, no `gh ... --watch`). On wake:
   `gh pr checks <n>`.
   - A failure: `gh run view <run-id> --log-failed`, fix, rerun the gate, push, watch again.
   - At most 3 CI fix rounds.
9. All checks pass and review-loop is clean (tier B: its final round was clean, not "disputed"):
   `gh pr merge <n> --merge`.
10. Check main's CI for the merge commit: `gh run list --branch main -L 1`; watch it if it is
    still running. Then `unwatch_pull_request`.

## Proof: the final report must contain
- The tested HEAD SHA, and each gate command with its result ("clean", or "passed N, failed 0"
  per configuration).
- Review rounds used and the final verdict (0 blocking).
- PR URL, `gh pr checks` output (all pass), merge commit SHA, main CI run URL and conclusion.
- UI changes: the screenshot command and PNG paths, plus "awaiting owner check: <what>" for
  anything about hover, drag or a real pointer.

## Stop and ask instead of continuing
- A hook blocks you, or the work reaches tier C without the owner's yes for that change.
- A test would need deleting, ignoring or weakening, or the passed count drops.
- A new dependency, a saved-format version bump, or a decision record is needed.
- The gate fails for a reason outside your diff on two runs.
- `review-loop` ends round 3 with blocking issues.
- CI fails 3 times, or fails on an OS you can't reproduce.
- A merge conflict appears in a file you didn't change.
- main's CI goes red after the merge: report it, then fix it with a new full-send (tier A/B) or ask.
