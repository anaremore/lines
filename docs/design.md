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
- The window footer's left zone is for safety: account kind, trading on (with the armed DOM count), feeds that aren't live, recording, a pending restart and notices that need action, in a fixed order, never hidden or folded (narrow, they shorten their words). The middle is one line of background work; the right holds links, which fold into ⋯ (`footer.rs`). No key-hint text: keys are keycaps where the action is.
- Errors appear inline where they happened, never as modal dialogs that steal focus during trading. Modals only for destructive confirmations the user opted into.
- Flatten and Cancel All are always reachable with one action.

## 4. Readable at a glance
- **Numbers:** tabular (fixed-width) digits everywhere prices or sizes align, right-aligned, using the instrument's display format (decision 0002). Never truncate a price.
- **Hierarchy:** last price and position P&L are the loudest elements; axis labels and grid the quietest.
- **Contrast:** text meets WCAG AA (4.5:1) against its background in both themes. Grid lines are visible but sit behind data.
- **Colour independence:** up/down never relies on red/green alone (shape, fill, or position also differs). A colourblind-safe preset ships by default (see Cross-cutting requirements in `ROADMAP.md`).

## 5. Consistent system
- All colours come from `crates/lines-app/src/theme.rs` tokens; no literal colours in views. Dark is the primary theme; light must be equally complete.
- **State tokens.** Hover, selection and "on" use the theme's named states, never a token faded by hand (`tests/state_tokens.rs`): `ghost_hover` and `ghost_selected` for frameless rows (a menu's or list's highlighted row also gets an `accent` left edge), `element_hover` and `element_selected` for framed buttons and chips. Each is opaque and chosen per theme, so a light-mode fix is one token edit, tested for AA text and 3:1 edges on it. Focus and selected edges are `accent` (a chip that is on is outlined in it); dimmed is a dashed `border_disabled` edge (or the side's color) and dim text (§6), never faded; text over the plot sits on `label_bg`.
- User-chosen colors for drawings and studies are user data, not code (decision 0026): the style editor warns when one is close to a color with a meaning or too low in contrast, but never blocks.
- **Spacing scale:** 2, 4, 8, 12, 16, 24 px. **Type scale:** 11, 12, 13, 15 px, one UI family and one monospace family for numbers (decision 0034): **IBM Plex Sans** for UI text and **IBM Plex Mono** for prices, sizes, times and every other number column, both built in (regular, medium and semibold). The UI family is a setting (Settings › Appearance › UI font: Plex Sans, the OS's UI font, or an installed family; decision 0036); numbers stay in Plex Mono whatever it is. Design and check layouts in Plex Sans: screenshots use it unless `--ui-font` is given.
- Edges snap to device pixels (as the crosshair work established), so lines are crisp at every DPI.
- Brand palette (from the icon): ink `#11141B`, signal violet `#8C7BFF`, paper `#ECEAF4`. Violet is the UI accent (focus, selection, active state). It is never used for up/down, which keeps market colours meaning only market direction.
- **Language: US English (en-US)** for every word the user sees: UI text, menus, command names, notices, CLI help, README and release notes (color, center, gray, canceled, favorites, behavior). User-facing strings stay simple literals for now; when localization comes, they move to a string table with en-US as the default and fallback. Identifiers, comments and internal docs follow US spelling in new or touched code; existing names are renamed opportunistically, never in a sweep that conflicts with open branches. Saved-file field names and CLI flags never change spelling without a migration or alias.
- Lines has its own look. Don't copy another platform's palette or layout; match behaviour traders rely on, not appearance.

## 6. Keyboard-first, mouse-excellent
- Every action is reachable from the keyboard and the command palette; hotkeys are remappable.
- A shortcut shows as keycaps, one per key (`Ctrl` `Shift` `F`; `⇧` `⌘` `F` on macOS), beside the action's name in tooltips, menus, lists, the palette and Settings › Keys (`keycap.rs`, from `commands::keys`, sized with the UI zoom). Inside a sentence it stays text ("Ctrl+Z undoes").
- Direct manipulation for spatial things: drag to pan or scale, drag orders to modify. Hover reveals detail (crosshair readout) without clicks.
- Destructive or financial actions (send, cancel all, flatten) have clear hotkeys and can't fire from an accidental hover or scroll.

### Menus
Context menus follow these rules (`crates/lines-app/src/menu.rs`, `pane_menu.rs`):
1. About 12 top-level rows at most. Submenus go one level deep, never deeper.
2. A set is a submenu, and its row shows the current value on the right: Timeframe › 5m, Link › A.
3. Never one row per instance (per level type, per source): one row opens the panel that lists them.
4. Show/hide pairs are switches (under Show ›), drawn as a box that is checked when on, never "Hide X / Show X" labels that flip. One of a set (a timeframe, a link group) has a plain check on the one in force. Section names inside a menu are small capitals.
5. Right-clicking an object (an order's tag, a drawing) gives that object's menu, its name and value as a non-clickable header above a divider: "Stop 2 @ 7562.00", then what can be done to it. It opens with nothing highlighted, so a stray Enter never acts on an order.
6. Delete, cancel and close go last, in `Theme::destructive`, and say what they do. Bulk deletes confirm first.
7. Shortcuts show right-aligned, from `commands::BINDINGS`. "All commands…" is the last row of a pane menu, with "Settings…" just above it, opening Settings at that pane's section (`pane_menu::settings_section`; its hint is Settings' key, which opens the window where it last was), and every row's action is also in the palette (tested), except an object's own rows, which act on the thing under the pointer and have direct controls too, and a chart's order rows, which place an order at the price right-clicked (no price without the pointer). A menu that can place an order opens with nothing highlighted, like an object's.

### Controls
- A **field** names what it holds and shows its value ("Qty 10 ▾", "Bracket 8/16 t ▾"): a quiet box on `panel` with an `axis_line` edge (the accent while its list is open). Its list opens under it in the menus' look: a check on the one in force, a box for a mode, shortcuts on the right, an editor ("Edit presets…") last.
- **Primary** actions are solid in their meaning's color with text in `bg` (Buy in `up`, Sell in `down`); **secondary** ones are outlined in it on `bg` (Join bid, Join ask). The words always say the side.
- **Dimmed** (nothing it would do now: Buy and Sell while entries are locked, BE while flat, Reverse while flat or entries are locked) is a 2 px dashed edge with dim text and no fill, never faded (the chart's order strip keeps a 1 px dash: its buttons are a fixed 96 px and a five-digit size needs the room): the words stay readable (a locked Buy or Sell says "Locked" under its size wherever that fits, in place of its price), and only a live primary action is solid.
- **Exits** sit at the bottom under a divider, outlined in `axis_line`; **Flatten** is outlined in `warn` (2 px) and bold. Cancel all and Flatten never dim; BE and Reverse dim while there is nothing to act on.
- Narrower, a control drops its least needed part first and is never cut: a price line before a size, a field's name before its value. A row goes whole.

### Trade panel
The DOM's trading panel (`trade_bar.rs`), top to bottom: PAPER, the account ▾ and the Trading switch (its words and a track, amber when on; below 240 the track alone); the position card (▲/▼ size @ average, open P&L large; SL in `down` or "No stop", TP in `order_target`, ticks under the open P&L, the day's P&L on the last line; or "Flat"); one status line; rule meters; Qty ▾ and Bracket ▾; Buy and Sell ("Buy 10" over "Market · 7572.00"); Join bid and Join ask (Settings › DOM hides them); BE, Reverse, Cancel all, Flatten. Its height depends only on the pane's size (§7). It lays out in 100 % units through `crate::zoom`, so it is one panel at every zoom, and fits from 165 px: narrowing, Buy and Sell lose the price line (never the size), the joins lose their prices and then hide (below 300) before anything else gives way, and below 240 BE and Reverse take a row of their own. The position card has two lines under its first (four below 240), so the stop (or "No stop"), the target and the working count are never left out for a shorter panel.

### Settings window
`settings_view.rs`, its rows from `settings_schema.rs` (decision 0021).
- **Sidebar:** sections under small-capital group headers: App (General, Appearance, Keys), Panes (Charts, DOM), Trading (Order entry, Accounts), Data (Connections, Data folder), then About on its own. Diagnostics live under About. The selected section has the accent edge on `panel`. A ● in the accent marks a section with a setting changed from its default. While searching, each section shows its count of matches instead.
- **Pages:** each page has a title and groups in small capitals ("LIMITS", "TURN TRADING OFF WHEN"). Accounts holds everything about one account, with one account picker beside its title, its kind as the main window's PAPER/SIM/LIVE badge and its size muted ("$50,000 start"), and the tighter/looser rule said once under them. Its groups are Limits, News blackout, Prop rules (the overrides folded behind Override…) and Account (the account's identity and rules on one row, then reset). Per-product rows show their product picker only where they are: Order entry's Brackets group carries the account and product on its title, then the account's badge.
- **Quiet rows:** one line per setting: the label (with a quiet scope word in `text_dim` when the value isn't app-wide: "New panes", "This workspace"), then the control, then the ● and ↺ in one fixed column at the right (the Keys page's keycaps end where the controls do). Max position reads "2 per contract": a cap in each contract month, as the core checks it, never an account or product total. The description shows only on the selected line. Units live in the value ("$1,000", "15 min", "5 contracts"; tabular digits, right-aligned). An unset limit reads "Off", dimmed. Related settings share a line ("Cooldown after 3 losses, for 15 min"; the news window "2 min before, 2 min after"); while the losing streak is off the Cooldown line is just "Off", and while the blackout is off the window shows the minutes it starts with, dimmed. The selected option of a choice is bold on the accent fill. No per-row chips: trading rows are the Trading group's pages, and per-account scope is the page's picker.
- **●/↺:** ● means the setting differs from its default, as a shape and not by color alone. ↺ shows only then, as a 24 px hit in `text_main` (the glyph at 16 px, which reads as 13 px text), with a tooltip (on a line of two it names the setting it resets), and Ctrl+R is its key. Both come from one pure function (`settings_schema::marks`). The prop overrides' summary line has neither: each override has its own. A row that a flag rules for this run (a Data folder flag, a look flag) has neither, and Ctrl+R does nothing there.
- **Search** runs across every page and keeps each match under its section's header (and its group's), so no match is read out of context. A match shows as its page's line, with the same label, value and buttons. The old section names (risk, trading, diagnostics, data) still find their rows. The count is of lines, as the page shows them.
- **Overflow:** a page longer than the window fades into the bottom bar (16 px, `bg`), so a row cut there reads as more below.
- **Bottom bar:** only "Ctrl+Z undo · Ctrl+R reset". Esc still clears the search, then closes the window, without a hint.

### Chart trading
Everything a chart shows for trading (`chart_view::draw_orders`, `chart_trade.rs`) is one system: working, held and pending order tags, the position tag, a staged order, the order strip, the refusal flash and the right-click order section.
- **One tag shape.** Square corners, `order_tags::TAG_H` high and `TEXT_PAD` padded at the UI zoom (`Metrics::for_ui_zoom`, never under 100 %: they are hit targets), the semibold label font at the zoom (`LABEL_PX` 12 at 100 %), on `label_bg` with a 1 px outline and a **2 px role bar** at the left edge. A target adds a notch on its top edge and a stop on its bottom edge, so a role never reads by color alone; the position tag's bar is its side and its words say LONG or SHORT.
- **Tokens only.** Buy `up`, sell `down`; target `order_target`, stop `order_stop`; a group of several kinds `text_dim`; the flash `warn` with `on_warn` text. P&L text takes its sign's tone (`up`/`down`) and always writes the sign.
- **One ticks and money format:** `pnl::write_pnl` (`+32.0 t  +$800.00`), so the P&L display (off, ticks, currency, both) rules every number on a tag; with it off no tick or money number appears anywhere, a staged entry keeps only its reward-to-risk ratio. (The measure tool's readout is a drawing, not trading, and keeps its own format with thousands separators.)
- **The same affordances.** × sits at a tag's right end behind a 1 px divider in the tag's color, and acts on release only if the pointer is still on it. The body drags, with no grip glyph, by its grab offset (the order moves by how far the pointer moved, snapped to ticks), and Esc during a drag puts it back. A group's tag ends in ▾ instead (a list, never one order). "+T" and "+S" handles add a missing leg from the position tag and from a staged entry alike.
- **Working, held, pending, staged.** Working: a solid line and a solid outline. Held (a bracket exit waiting on its entry): a short, faded dashed line and a dashed outline. Pending (not acknowledged): the held line, a solid outline and dim text. **Staged** (on the chart, not sent): long full-strength dashes in its roles' colors, a dashed outline, and on the entry a **Place ⏎** button filled in its side's color (text in `bg`) and **× Esc**. Nothing else on the chart has a filled button on a tag, so a staged order never reads as working or held. Staged tags are laid out with the working ones (never overlapping); working orders group into the room left.
- **The flash** is the chart's one refusal or notice at the plot's top left, in the tag shape filled `warn`. It has no role bar (its whole fill is the role) and no × (it goes by itself after 3 s, and a × appearing under the pointer at the plot's corner could take a press meant for the chart).
- **The order strip** is a control, not a tag: rounded corners and `control_edge` outlines like the DOM's buttons, so it never reads as an order on the plot. It zooms with the UI (its pointer geometry too, `chart_trade::strip_box`) in the tags' semibold label font. Market buttons are solid primaries; limit buttons are outlined until their price pick is armed.
- **The right-click order section** comes first in the chart's menu: a header naming the price, size and bracket, then each order as "▲ Buy limit 2 @ 6010.00" (the glyph and the word both carry the side), sent on choosing. An order that may carry a bracket (it opens or adds) has a second row, "… with bracket", that stages it on the chart with its bracket's legs (or the "+T"/"+S" handles with no preset) and sends nothing until Place or Enter; Esc or × discards it.

## 7. Reliability over cleverness
- Prefer the simple, predictable layout over the adaptive one that moves things. Nothing the trader aims at should shift under the cursor (e.g. the DOM never re-centres while hovered).
- UI degrades gracefully: missing data shows as missing (greyed, labelled), never as zero or a stale value presented as live.
- **Stale is a state.** When the market is open (per the sessions calendar) and a feed delivers nothing for longer than its expected cadence, every pane on it shows a clear **stale** marker until data resumes. A frozen DOM must never look live.

## Checking a UI change
Every UI PR (template checklist) confirms: tokens only, no paint-path allocation, both themes, readable at DPI 1.0 and 2.0, states unambiguous, and before/after screenshots (decision 0009).

Three layers check every UI change, cheapest first:
1. **Layout checks in tests** (deterministic): no overlapping or clipped text, minimum hit targets, spacing on the scale, AA contrast, across the scene set.
2. **Screenshot evidence** from the gallery scenes the change touches, before and after.
3. **Visual review** (`review-loop`): three fresh `ui-reviewer` agents (scalper, prop evaluator, accessibility) score the screenshots against this document and must point at a region for every finding.

The owner's live check stays the final word on feel: hover, drag, latency and anything a still image can't show.

The commands, in order:
```sh
# 1. Layout checks: the quick scene set (run it with gate step 6 for UI changes; CI doesn't build the
#    screenshot feature), then all scenes.
cargo test -p lines-app --features screenshot --test gallery
cargo test -p lines-app --features screenshot --test gallery -- --ignored   # the full set
# The gallery itself: PNGs + <scene>.lint.txt + gallery.txt in DIR (synthetic data only).
cargo run -p lines-app --features screenshot -- --gallery target/gallery --gallery-full
# 2. Evidence for one scene, with the lint's findings beside the PNG (<stem>.lint.txt):
cargo run -p lines-app --features screenshot -- --replay <file> --at <ISO time> --screenshot out.png --layout-report
```
The layout lint (`lines_core::layout_lint`) fails on:
- text, chips or buttons that overlap in one layer;
- text clipped without "…", or a control cut by its clip;
- a hit target under 20×20 px;
- sibling gaps off the spacing scale;
- anything drawn over the DOM's price column or the chart's last-price tag;
- a popup that doesn't touch its trigger.

Panes record what they draw only in screenshot builds while a frame is being recorded (`lines-app/src/layout_rec.rs`), so the shipped app pays nothing. A known, documented finding goes in `gallery::KNOWN` with its reason in `docs/known-gaps.md`; it fails again once it no longer occurs.
