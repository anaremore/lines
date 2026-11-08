# Lines

Desktop futures charting, DOM and paper-trading terminal. Rust 1.98.1 + GPUI (pinned Zed rev).
Public repo, FSL-1.1-ALv2. Nothing is deployed: GitHub pre-releases are built by
`.github/workflows/release.yml` from `v*` tags (`docs/release.md`).

## Read before changing an area
- `docs/invariants.md`: per-crate invariants. Read the sections for every crate you touch.
- `docs/architecture.md`: crate layout, allowed dependencies, every CLI flag, pinned versions,
  the public/private boundary (decisions 0014, 0020).
- `docs/design.md`: binding for every UI change.
- `docs/decisions/`: accepted records are binding; supersede with a new record, never edit.
- `docs/release.md`: release steps and owner-only actions.
- `ROADMAP.md`: update status in the change that finishes a milestone.
- `docs/known-gaps.md`: limits to state honestly; update it when you add or close one.

## Hit every surface
The most common defect here is a change that works where you tested it and is missing elsewhere.
Before calling UI work done, say which of these applied:
- Entry points: key binding (`commands::BINDINGS`), palette, pane menu, context menu, CLI flag
  for screenshots. Fixing one is not fixing the feature.
- Panes and windows: chart, DOM, T&S; main window, popped-out pane windows, hidden tabs (must do
  no paint work); narrow widths.
- Themes: dark and light, standard and colorblind palettes; DPI 1.0 and 2.0.
- Sources: mock, replay, Coinbase, Kraken, an extension source; "unavailable" panes; stale feeds.
- Saved state: workspace / settings fields (optional, migrated), restore after restart.
- Reverse states: a way in needs a way out and a way to see it (hide/show, arm/disarm, add/remove).
- Trading: paper and replay clocks, DOM and (later) chart, `pnl_display` off.

## Machine
- cargo may not be on PATH: `~/.cargo/bin/cargo.exe`.
- Build with `CARGO_TARGET_DIR=Z:\cargo-target\<branch>` (C: is small), one per worktree.
- `git submodule update --init` once per checkout (cqg-proto needs `third_party/cqg-webapi`).
- Several threads build at once here: no wall-clock speed assertions outside `#[ignore]` perf tests.

## Run
- `cargo run -p lines-app -- --source mock`: CQG-protocol mock (ES only); paper trading any hour.
- `cargo run -p lines-app -- --source replay --replay recordings/x.lfr --speed 10`
- `cargo run -p lines-app -- --source coinbase --instrument BTC/USD`: live public data.
- Headless check of any source: `cargo run -p lines-app -- --source mock --headless 5`
- Synthetic evidence data: `cargo run -p lines-feed --example synth_session -- recordings/synth-es-2026-11-05.lfr`
- Screenshot (decision 0009): `cargo run -p lines-app --features screenshot -- --replay <file>
  --screenshot out.png --at <ISO time> --size 1600x900 --theme dark`
- Everything else: `cargo run -p lines-app -- --help` and `docs/architecture.md`.

## Gate: the definition of done
Run on the exact HEAD you push. Report the SHA and each result.
1. `cargo fmt --all --check`
2. `cargo clippy --workspace --all-targets -- -D warnings`
3. `cargo clippy --workspace --all-targets --all-features -- -D warnings`
4. `cargo test --workspace --no-fail-fast`: sum the `test result:` lines (passed N, failed 0).
5. `cargo test --workspace --no-fail-fast --features lines-app/vendor-cqg,lines-feed/vendor-cqg`
6. UI or screenshot code touched: `cargo test -p lines-app --features screenshot --test screenshot_exit`
   and the quick gallery, `cargo test -p lines-app --features screenshot --test gallery`,
   plus before/after PNGs from one screenshot command (give the command and the paths).
7. Cargo.toml / Cargo.lock / deny.toml touched: `cargo deny --locked check` (if it isn't
   installed locally, say that CI runs it).
8. `.claude/` touched: `python .claude/hooks/test_guard.py` (`python3` on macOS; needs Git Bash on Windows).
- Never skip, `#[ignore]`, or weaken a failing test to pass. A failure that looks load-related:
  rerun that test alone once and report both runs.
- The passed count must not drop versus origin/main unless the PR lists removed tests and why.
- CI re-runs 1, 2 and 4 on Windows/macOS/Linux, 3 on Windows, 8, and cargo-deny. 5 and 6 are local only.

## Git and PRs
- Branch in a worktree off origin/main. Never commit on main; pushing to main is hook-blocked (PRs only).
- Rebase on origin/main before review and before merge; `--force-with-lease` to your own branch is fine.
- PR body: `.github/pull_request_template.md`, filled with gate output, screenshots and the risk tier.
- Merge only with `gh pr merge <n> --merge`. Never `--admin` or `--delete-branch`.
- Parallel threads: name the shared files you touch (`chart_view.rs`, `dom_view.rs`, `commands.rs`,
  `workspace.rs`, `trading.rs`, `ROADMAP.md`, `docs/invariants.md`) and keep edits there small.
- Skills: `review-loop` (fresh-context review), `full-send` (to merged), `full-stop` (to a green PR,
  for the owner to merge). Who merges tier A/B: a thread the owner told "full send", or the planning
  thread (owner's standing instruction, 2026-11-06). Other threads stop at a green PR and report.
- Keep moving: never end a turn waiting for a decision that isn't owner-only (tier C). Take your
  recommended option, say so in the report or PR ("Decided: X, because Y; alternative Z"), and
  continue; the planning thread can still redirect. With queued work, start the next item.
  Never end a turn while your own background work runs (gate, build, reviewer): call
  `t3_background_wait` on it, or keep working; nothing else will wake you when it finishes.

## Risk tiers (state the tier in the PR)
- A: docs; tests; pane UI with screenshot evidence; chart layers, studies, indicators; picker;
  theme tokens; feed parsing covered by fuzz smoke tests; benches. Agents merge when green.
- B: `crates/lines-trading/**`; DOM/chart click-to-order paths (`dom_trade.rs`, `trade_bar.rs`,
  `trading.rs`); any saved format or migration (workspace, settings, trading file, `.lfr`, store
  segments); dependencies, `deny.toml`, toolchain, GPUI pin; anything needing a decision record.
  Agents merge when green, with extra proof: a "Risk" section in the PR (what could go wrong,
  which tests cover it) and a clean final review round.
- C: everything under "Owner only", plus any change to the agent rules and guardrails:
  `CLAUDE.md`, `.claude/**` (hooks, settings, skills, agents) and `.github/**` (the hook asks).
  Ask the owner first and say exactly what and why. Proceed (including merging) only on an explicit
  yes for that specific change; a yes never carries over to another change.

## Docs
- `docs/invariants.md` holds constraints and traps the code can't show, not feature narratives
  or file catalogs. When a rule changes, rewrite the affected text; never append a second account.
- If a rule here fights the task in front of you, say so plainly and get the owner's yes first.

## Conventions
- `Price(i64)`, `Qty(i64)`, `Money(i128)` are exact. No floats for order prices or money.
  Unrepresentable values are counted, never rounded.
- Vendor symbols live only in `lines-feed` (crate-private) and `lines-sierra`.
- `ProviderSession`s do no I/O. No heap allocation in paint paths (`lines-core/tests/no_alloc.rs`).
  The UI never blocks on a feed or the trading core.
- Colors only from `crates/lines-app/src/theme.rs`. Text meets WCAG AA in all theme/palette
  combinations. Direction is never shown by color alone.
- User-facing text is US English (`docs/design.md` §5).
- Saved files go through `versioned`. A format change needs a version bump and a tested
  migration; new optional fields don't.
- Keys go through `commands::BINDINGS`; pane letter keys stay unbound.
- Test fixtures are built in code and synthetic. Tests use manual or market clocks, not sleeps.

## Never
- Commit or print credentials, account IDs, vendor usernames or API keys, or anything from
  `recordings/`, `*.lfr`, `*.scid`, `*.depth`, the Sierra Chart folder, or vendor kits and protos
  (decisions 0010, 0014).
- Edit `third_party/`, `LICENSE.md`, or an accepted decision record (except adding
  "Superseded by NNNN" with the owner's yes, per `docs/decisions/README.md`).
- Name the owner's private level services or strategies in this repo (code, docs, commits, PRs):
  say "a private level source" / "a private strategy"; names live only in the private repos.
- Add `vendor-cqg` to default features or release builds, or put live connectors in this repo
  (they belong in private `lines-connectors`, decision 0020).
- Drive the real mouse or keyboard, or capture the screen. Verify UI with screenshot mode and tests.
- Touch Sierra Chart settings or files, or send any order to Sierra (Sim included): the owner runs
  Sim tests (`sim-once` builds are hook-blocked).
- Force-push main, delete branches/tags/releases, rewrite history, `git clean -x`, `git add -f`,
  `--no-verify`.

## Owner only: ask first (tier C)
- Money, contracts, vendor agreements, legal or license terms.
- Enabling live order routing or connecting to a production trading system; Sim order tests.
- Publishing outside GitHub, code signing, notarization.
- Accepting or changing decisions (drafting proposals is fine).
- Live UI checks (hover, drag, real pointer): ship screenshots and list "awaiting owner check".
