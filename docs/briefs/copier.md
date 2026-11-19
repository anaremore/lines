# Trade copier (roadmap 14): how it builds on account groups

Status: design note, nothing here is built. Written with group order entry (roadmap 6, paper),
which is the seam the copier extends.

## What exists after roadmap 6

- **Groups** (`lines_trading::group`, `core/groups.rs`; decision 0006): a name and member
  accounts in order, saved in the trading file. They carry no rules of their own.
- **Group orders** (`Command::GroupPlace { group, order }`): one `PlaceOrder` or
  `PlaceBracket` per member, each with its own internal command id, handled as if sent alone.
  Each member's risk limits, prop rules, guardrails, rate limits and bracket mode check its own
  order. A refused member is named in `Event::GroupCommand` and the rest proceed.
- **Member sizes** (`AccountGroup::sizing`, `MemberSize { num, den }`): an optional per-member
  ratio, 1:1 when absent. A member's quantity is the order's × num / den, exact. A size that
  isn't a whole number of contracts refuses that member, never rounds (decision 0002). Saved as
  an optional field; no UI or command sets it yet.
- **Origin** (`TradingCore::group_command_of(order)`): every member order, bracket exits
  included, maps to the group command that placed it, while the core holds the order.
  `Event::GroupCommand` carries the same mapping (each member's command id and order ids).

## What the copier adds

1. **Leader and followers.** A group gains an optional `leader: AccountId`. A copy group is
   then "whatever the leader does, the followers do", rather than "one click, every member".
   The trigger is the leader's own order events (`Event::Order`, `Event::Fill`), not a pane
   click. A `CopyGroup` setting decides whether a follower copies the leader's orders
   (working orders mirrored) or only its fills (market orders after each leader fill).
2. **Follower ratios** use `MemberSize` as is: 1:2 is half size, 3:1 is triple. The copier
   UI is where the "Copy ratio" column from the approved groups mock lands.
3. **Contract mapping** (NQ → MNQ, ES → MES): a per-follower `ContractMap` from the leader's
   product to the follower's, with the size scaled by the products' micro relation
   (`Product::micro_units`). The ratio and the mapping compose: NQ 1 → MNQ 10 at 1:1 exposure.
   An unmapped product refuses that follower, never guesses.
4. **Per-follower rule checks** come free: each follower's copy is its own command through the
   core, so its prop rules, risk limits and guardrails refuse it alone, exactly as group orders
   do now. A follower past its daily loss stops copying; the leader never notices.
5. **Drift detection.** For each copy group, compare the leader's position × ratio (mapped)
   with each follower's position per contract. Any difference is drift. Show it on the group's
   row ("Eval 2: flat, leader long 2") with a one-press "Match leader" that sends the follower
   the order closing the gap, through the same per-member checks. `group_command_of` ties each
   follower order to the leader action that caused it, so the drift view can say which copy
   failed (refused, rejected, partly filled). Persisting that map (today it lives only in
   memory) comes with the drift view.
6. **Cross-venue groups.** Today group commands go to the paper core only, so a group holds
   paper accounts. The copier's main use is one leader account at one venue and followers at
   others (prop firms on different brokers). That needs:
   - group commands dispatched per member to that member's core: `Trading::submit` splits a
     group command by venue;
   - `VenueIds` translating `Event::GroupCommand`'s member command and order ids;
   - a group's saved members that aren't listed yet (a venue connecting later) kept through
     `SetGroupMembers`.

   The per-venue split is the main work; each venue core already checks its own accounts'
   rules.
7. **Safety.** Copying to a live account is live order routing (owner only, decision 0023).
   The copier ships paper → paper first, then Sim followers, then live behind the same owner
   gate. Flatten and Cancel all on a copy group stay ungated, as they are for every group now.

## Open questions for the owner (when roadmap 14 starts)

- Does a follower copy the leader's working orders (limits rest at every follower) or only
  fills (followers get market orders after the leader fills: simpler, but they get worse
  prices)?
- What should "Match leader" do with a follower whose rules refuse the gap order: show the
  refusal, or offer a smaller size that fits?
