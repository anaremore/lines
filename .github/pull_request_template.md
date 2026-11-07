## What & why

## Risk tier

- [ ] A  - [ ] B (add a Risk section: what could go wrong, which tests cover it)  - [ ] C (owner approved: link the message)  (CLAUDE.md "Risk tiers")

## Testing (CLAUDE.md gate, on HEAD `<sha>`)

- [ ] `cargo fmt --all --check`
- [ ] clippy `-D warnings`, default and `--all-features`
- [ ] `cargo test --workspace --no-fail-fast`: passed N, failed 0
- [ ] same with `--features lines-app/vendor-cqg,lines-feed/vendor-cqg`
- [ ] `screenshot_exit` (UI changes) · `cargo deny --locked check` (dependency changes) · `test_guard.py` (`.claude/` changes)
- [ ] review-loop: rounds used, final verdict 0 blocking

## Visual evidence (decision 0009)

- [ ] Before/after screenshots from the same replay + screenshot command, **or**
- [ ] GIF/video for interaction changes, **or**
- [ ] N/A: no UI impact

Command used:

## Checklist

- [ ] No licensed data or vendor-proprietary files added (decision 0010)
- [ ] No allocation added in the paint path
- [ ] UI changes follow `docs/design.md`: theme tokens only, both themes, readable at DPI 1.0 and 2.0, states unambiguous
- [ ] New decision record if this changes architecture
- [ ] `ROADMAP.md` status updated if this completes a milestone
