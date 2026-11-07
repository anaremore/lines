# 0021: Settings: in context first, one searchable Settings window, settings as data

**Status:** Accepted (owner, 2026-11-07)

**Context:** Options are accumulating: themes and palettes, hint bar, column widths, study visibility, grouping, VWAP resets, profile modes, hotkeys, and soon order defaults, risk rules and connections. Incumbent platforms bury hundreds of options in modal dialogs, which traders dislike. Lines needs a settings model that stays clear (`docs/design.md`), safe for trading, and consistent as features grow.

**Decision:**

*Three ways in, one source of truth*
1. **In context (primary):** right-click the thing you want to change (a chart, DOM column, study, tab). Most settings live where they act, scoped to that pane.
2. **Command palette:** every setting is a searchable command ("Palette: colourblind", "DOM: grouping 5"). Nothing is reachable only through a dialog.
3. **Settings window (Ctrl+,):** for app-wide preferences and defaults, with a search box at the top and a short left-hand list of sections:
   - **General:** startup workspace, updates (U2), diagnostics/telemetry opt-in (U1).
   - **Appearance:** theme, palette (incl. colourblind), up/down shape cue, text size/density, hint bar, reduce motion.
   - **Charts & DOM defaults:** for *new* panes: bar period, grouping, columns, study templates.
   - **Trading:** default quantity, one-click arming, order confirmations, bracket templates (roadmap 7–8).
   - **Risk & rules:** prop-rule presets, guardrails (6, 8b).
   - **Hotkeys:** searchable list, conflicts flagged inline, reset per binding.
   - **Connections & accounts** (0018).
   - **Data:** store location, retention budget, depth recording per product (0019).
   - **Advanced:** logs, diagnostics bundle, reset/export/import.

*Scope is always explicit*
- Every setting shows its scope: **this pane**, **this workspace**, or **all new panes / app-wide**. In-context menus offer "Apply to this chart" vs "Make default for new charts". No hidden inheritance surprises.

*Behaviour*
- Changes apply **instantly** (no OK/Apply), with **undo** (Ctrl+Z in the window) and **reset section / reset to default** per item.
- **Trading-affecting settings are visually distinct** and safe. Changes that loosen risk (raise max size, disable confirmations, change a prop preset) need an explicit confirm. Guardrail lockouts (8b) can make some settings read-only until they expire.
- No setting change blocks the UI or interrupts live data.

*Settings are data*
- One typed settings schema (key, type, default, scope, description, safety class) drives the window, the palette entries, persistence and validation, so they can't drift apart.
- Stored in the existing versioned files (settings and workspace) with the same forward-migration rules. Unknown keys from newer versions are preserved, not dropped.
- Export/import as a file (later: import hotkeys and presets from Sierra Chart/NinjaTrader, backlog).

**Consequences:**
- New roadmap item 6c (Settings window + schema) before DOM trading (7), because trading defaults, hotkeys and risk settings need a proper home.
- Existing toggles (palette, hint bar, clean chart, columns, studies) migrate into the schema without changing behaviour.
