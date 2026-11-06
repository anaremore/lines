# Lines UI design principles

Minimal, modern, maximally useful, beautiful, and never at the cost of speed or reliability. When these conflict, the order is: **correct → fast → clear → beautiful**. A trader must never misread a price, wait on the UI, or lose an order because of a visual choice.

## 1. Every pixel earns its place
- Show data, not chrome. No decorative borders, gradients, shadows, or icons that don't carry meaning.
- Default to the fewest visible controls. Advanced options live in the command palette (Ctrl+K), context menus, and settings, not in toolbars.
- One accent colour per screen region. Colour signals state (up/down, working/pending, alert); it is never decoration.
- Empty space is a feature: consistent padding beats extra separators.

## 2. Speed is a design requirement
- **Budgets:** input → visible response within one frame (≤ 8 ms at 120 Hz). Startup → trading-ready ~2 s. No frame over 16 ms during a market-open replay at max speed.
- No allocation in paint paths (enforced by `lines-core/tests/no_alloc.rs`).
- **Animation:** only to explain a change (a level appearing, a panel opening), ≤ 120 ms, never blocking input, never on price data. Prices and order states update instantly, never tweened. A global "reduce motion" setting disables all of it.
- No spinners on the trading path. If something is slow, show its state (pending, reconnecting), not a wait.

## 3. Trust: state is always unambiguous
- Order states look distinct and never borrow each other's look: **pending** (outlined), **working** (filled), **partially filled**, **rejected** (with the reason inline).
- Connection, data delay, and paper vs live are always visible, never only in a menu. Paper vs live differ by more than colour (decision 0006).
- Errors appear inline where they happened, never as modal dialogs that steal focus during trading. Modals only for destructive confirmations the user opted into.
- Flatten and Cancel All are always reachable with one action.

## 4. Readable at a glance
- **Numbers:** tabular (fixed-width) digits everywhere prices or sizes align, right-aligned, using the instrument's display format (decision 0002). Never truncate a price.
- **Hierarchy:** last price and position P&L are the loudest elements; axis labels and grid the quietest.
- **Contrast:** text meets WCAG AA (4.5:1) against its background in both themes. Grid lines are visible but sit behind data.
- **Colour independence:** up/down never relies on red/green alone (shape, fill, or position also differs). A colourblind-safe preset ships by default (see Cross-cutting requirements in `ROADMAP.md`).

## 5. Consistent system
- All colours come from `crates/lines-app/src/theme.rs` tokens; no literal colours in views. Dark is the primary theme; light must be equally complete.
- **Spacing scale:** 2, 4, 8, 12, 16, 24 px. **Type scale:** 11, 12, 13, 15 px, one UI family and one monospace family for numbers.
- Edges snap to device pixels (as the crosshair work established), so lines are crisp at every DPI.
- Brand palette (from the icon): ink `#11141B`, signal violet `#8C7BFF`, paper `#ECEAF4`. Violet is the UI accent (focus, selection, active state). It is never used for up/down, which keeps market colours meaning only market direction.
- **Language: US English (en-US)** for every word the user sees: UI text, menus, command names, notices, CLI help, README and release notes (color, center, gray, canceled, favorites, behavior). User-facing strings stay simple literals for now; when localization comes, they move to a string table with en-US as the default and fallback. Identifiers, comments and internal docs follow US spelling in new or touched code; existing names are renamed opportunistically, never in a sweep that conflicts with open branches. Saved-file field names and CLI flags never change spelling without a migration or alias.
- Lines has its own look. Don't copy another platform's palette or layout; match behaviour traders rely on, not appearance.

## 6. Keyboard-first, mouse-excellent
- Every action is reachable from the keyboard and the command palette; hotkeys are remappable.
- Direct manipulation for spatial things: drag to pan or scale, drag orders to modify. Hover reveals detail (crosshair readout) without clicks.
- Destructive or financial actions (send, cancel all, flatten) have clear hotkeys and can't fire from an accidental hover or scroll.

## 7. Reliability over cleverness
- Prefer the simple, predictable layout over the adaptive one that moves things. Nothing the trader aims at should shift under the cursor (e.g. the DOM never re-centres while hovered).
- UI degrades gracefully: missing data shows as missing (greyed, labelled), never as zero or a stale value presented as live.
- **Stale is a state.** When the market is open (per the sessions calendar) and a feed delivers nothing for longer than its expected cadence, every pane on it shows a clear **stale** marker until data resumes. A frozen DOM must never look live.

## Checking a UI change
Every UI PR (template checklist) confirms: tokens only, no paint-path allocation, both themes, readable at DPI 1.0 and 2.0, states unambiguous, and before/after screenshots (decision 0009).
