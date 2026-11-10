# 0036: A UI font setting, amends 0034

**Status:** Accepted (owner accepted 2026-11-10)

**Context:** Decision 0034 bundles IBM Plex Sans (UI text) and IBM Plex Mono (numbers) so layouts and evidence match on every machine. It deferred "a user font setting" and kept the OS fonts "only as a fallback, for glyphs the bundled fonts lack". The bundled-fonts task then asked for a UI font picker: the built-in Plex Sans by default, the OS's UI font, or any installed family (Calibri on Windows), switched live. Shipping that picker changes 0034 §2 and its "deferred" alternative, so it needs its own record.

**Decision:**
1. **Settings › Appearance › UI font**, an optional saved field `ui_font` (no migration):
   - **IBM Plex Sans (built in)**, the default (absent).
   - **System**: the OS's UI font (GPUI's `.SystemUIFont`).
   - **Any installed family**, from a searchable list of the text system's families (the time zone's list pattern).
2. **Numbers never follow it.** Prices, sizes, times, P&L, axes and tags stay in IBM Plex Mono (0034 §1). Text with a letter (headers, messages, a contract, "3 ticks") follows the UI font.
3. **A family that isn't installed** shows Plex Sans and says so once; the saved value is kept, so the font comes back if it is installed again.
4. **Live:** a change reshapes every cached text; no restart.
5. **Evidence stays reproducible:** screenshots, the gallery and the layout lint use Plex Sans whatever is saved; `--ui-font` (`plex`, `system` or a family) overrides for one run.
6. **The OS fonts** are a primary font only when the user picks one; otherwise 0034 §2 stands (fallback only, for missing glyphs).

**Alternatives considered:**
- **No setting (0034 as written).** Rejected by the task: users who prefer their OS font (Segoe UI, SF) or need a different one for reading have no way to get it.
- **A short fixed list (Plex, System).** Simpler, but Calibri and other installed families were asked for, and the searchable list already exists for time zones.
- **Numbers follow the setting too.** Rejected: number columns need one width per digit (0034 §1), and a proportional family breaks alignment.

**Consequences:**
- 0034 §2's "only as a fallback" and its deferred "user font setting" are amended as above; the rest of 0034 is unchanged.
- A family with a small x-height (Calibri) reads smaller at the same size, and a symbol font (Wingdings) turns UI text into symbols; the way back is `--ui-font plex` (`docs/known-gaps.md`).
- 0034's status line notes that its font-setting clauses are superseded by 0036.
