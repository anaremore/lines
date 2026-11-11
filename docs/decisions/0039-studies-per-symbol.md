# 0039: Studies belong to the symbol, shared by every chart showing it

**Status:** Accepted (owner, 2026-11-11).

**Context:** A chart's studies (VWAPs, the volume profile, Volume, level sources, external series and their settings, plus the study label placement) are saved on the chart pane (`ChartState`, `StudySet`). Switching a chart from ES to NQ carries ES's setup over. Two ES charts can drift apart, and a symbol's setup has to be rebuilt on every new chart. The owner wants studies and their settings unique per symbol.

**Decision:**
1. **Keyed by product.** Study sets are keyed by the catalog product (ES, NQ, MNQ, BTC/USD.KR…), never by contract, so a roll keeps them. Timeframe isn't part of the key.
2. **Shared by every chart of that product.** A change made on one ES chart applies to every ES chart in every window and tab.
3. **Switching symbols swaps the set.** A chart switched from ES to NQ shows NQ's set, and switching back restores ES's exactly.
4. **A new symbol starts from the defaults.** A product without a set starts from "Save as default for new charts" (`chart_defaults`), as a new chart does today.
5. **Per-chart override.** "Use separate studies on this chart" (Studies menu, pane menu, palette) gives one chart its own set, copied from the symbol's at that moment. The chart keeps it until turned off, which returns it to the symbol's set. It's saved on the chart pane as before.
6. **Not per link group.** A link group decides which market panes follow, never what they show. The per-chart override covers a differently studied chart in another group.
7. **App-wide.** The per-symbol sets live in `settings.json` (versioned, optional field), not in the workspace, so a symbol looks the same in every workspace. Per-chart overrides stay in the workspace.
8. **Migration, nothing lost.** On first load:
   - the first chart of each product (in window, tab and layout order) gives that product its set;
   - a later chart of the same product whose set differs becomes a per-chart override;
   - the workspace file's version is bumped, with a tested migration and backup as usual.
9. **Visibility is still per chart:** hide studies, hide study labels and Clean chart stay a chart's view state (`StudyVisibility`), not part of the symbol's set.

**Consequences:**
- Editing studies on one chart can change another visible chart of the same symbol. The Studies menu says "ES charts" in its header, so that isn't a surprise.
- Templates (`study_templates`) still apply to the chart's set, whether that's the symbol's or the override.

**Alternatives:**
- Per symbol per chart (each chart remembers a set per symbol it has shown): rejected, because two ES charts still drift apart.
- Per symbol per link group: rejected, see §6.
- Per workspace: rejected, because the owner wants a symbol to look the same everywhere.
