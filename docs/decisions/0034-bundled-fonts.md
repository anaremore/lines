# 0034: Bundled fonts: IBM Plex Sans for the UI, IBM Plex Mono for numbers

**Status:** Accepted (owner accepted 2026-11-10)

**Context:** Lines draws with whatever fonts the OS provides. `docs/design.md` §5 calls for "one UI family and one monospace family for numbers", but which families those are depends on the machine. As a result:
- text width, and so layout, differs between Windows, macOS and Linux;
- screenshots differ across machines (a listed known gap), which weakens decision 0009's reproducible evidence;
- prices and sizes rely on the OS monospace having tabular figures.

Zed, the reference GPUI app, ships its own fonts and loads them at startup with `text_system().add_fonts`.

**Decision:**
1. **Fonts:**
   - **IBM Plex Sans** for UI text.
   - **IBM Plex Mono** for prices, sizes, times and every other number column.
   - Both are SIL Open Font License 1.1.
2. **Bundled in the binary:** the regular, medium and semibold weights only, taken from IBM's upstream release (not from another project's copy). They're loaded before the first window opens. The OS fonts remain only as a fallback, for glyphs the bundled fonts lack.
3. **Licensing:** the OFL text ships with the release, in `THIRD_PARTY_LICENSES.txt` and the source tree, and the font names are not changed. The fonts are only embedded, never sold on their own, as the OFL allows.
4. **Everything uses them:** screenshots, tests, the gallery and the layout lint. Screenshots then match across machines, apart from DPI and rasterization.
5. **Size budget:** only the weights the type scale uses. The added binary size is stated in the PR.

**Alternatives considered:**
- **Keep the OS fonts.** Rejected: layouts and evidence differ by machine, and tabular figures aren't guaranteed.
- **Inter or JetBrains Mono.** Both are fine under the OFL. Plex was chosen because its Sans and Mono are one family, so the UI and the numbers match, and because Zed already validates it with GPUI.
- **A user font setting.** Deferred. It may come later, behind the same loader.

**Consequences:**
- **The implementation is tier B:** new bundled assets, every screenshot changes, and the layout tests' text widths change. Expect a one-time update of the stored baselines, explained in the PR.
- `docs/design.md` §5 names the two families. The known gap about the OS UI font is closed.
- The text and number painters (`text.rs`) measure with the bundled faces.
