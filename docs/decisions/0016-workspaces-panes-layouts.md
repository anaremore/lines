# 0016: Workspaces, panes, layouts and link groups

**Status:** Proposed

**Context:** Traders watch several markets at once. The owner's everyday setup is NQ and ES side by side. Users need custom grids, tabs, and separate windows across monitors, and all of it must stay fast (decision 0001, `docs/design.md`).

**Decision:**

*Model*
- **Workspace** → **windows** → **tabs** → a **layout tree** → **panes**.
- A layout tree is nested horizontal/vertical splits with ratios. A "grid" is a split tree with equal ratios; presets (1×2, 2×2, 1+2…) are just generated trees.
- A **pane** is one component: chart, DOM, time & sales (later: watchlist, journal…). Panes are independent: each holds a contract (or product following its front month, 0013), its own view state, and a link group.
- Tabs can be dragged out into new OS windows and back. Windows remember their monitor and position.

*Data*
- Market data **fans out**: all panes on the same contract share one subscription and one store, held in a refcounted registry off the UI thread. Opening a second ES chart costs no extra connection.
- Each pane repaints only when its own data's revision changes. Hidden tabs and minimised windows do no paint work.

*Linking*
- **Link groups** (a few colours, plus "unlinked"): changing the symbol in one pane changes every pane in its group. Typical use: a chart + DOM + T&S trio follows one symbol.
- **Crosshair time sync** across chart panes (opt-in per workspace), so NQ and ES can be compared bar for bar.
- Interaction is keyboard-first: move focus between panes, change a pane's symbol with the picker, split and close panes, switch tabs.

*Persistence*
- The workspace (windows, tabs, layouts, panes, links, view state) saves automatically and restores on start (the ~2 s startup target). Named workspaces can be saved and switched. This replaces the "saved workspaces" half of roadmap 11.

**Consequences:**
- The DOM/T&S work (roadmap 5) builds panes and the shared data registry from the start.
- Order entry (roadmap 7–8) acts on the focused pane's contract, so focus must always be visible.
- Many panes multiply GPU and memory cost. Benchmark "6 charts + 2 DOMs" (`docs/positioning.md`) becomes a CI-tracked number.
