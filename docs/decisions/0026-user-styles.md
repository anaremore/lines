# 0026: User styles for drawings and studies

**Status:** Accepted (owner, 2026-11-07)

**Context:** Traders want to set the color, line type, width and opacity of drawings and of every study (VWAP lines and bands, volume profile, footprint, cumulative delta, external levels). `docs/design.md` says colors come only from theme tokens, a rule about *our code*: no color literals in views. Decision 0022 already allowed external level sources to supply their own colors.

**Decision:**
- **One shared style model.** `LineStyle` (color, line type solid/dashed/dotted/dash-dot, width 1–4 px, opacity), `FillStyle` (color, opacity) and `TextStyle` (size S/M/L, color, background on/off). Drawings add extend left/right and an axis price tag on/off.
- **A color is either a theme token or a custom color.**
  - Theme tokens are the default, and follow dark/light and the colorblind palette.
  - Custom colors are user data saved with the drawing or study. They are not literals in code, so the theme-token rule for code stands.
- **One style editor**, used by drawings (right-click or double-click, then Style) and by every study's editor in the Studies menu, with tabs per part. "Save as default for <tool>" and study templates store styles.
- **Readability guard, never a block.** The editor warns when a chosen color is perceptually close (CIEDE2000) to a color with a meaning: up, down, the violet last price/accent, the order target/stop roles, warn. It also warns when contrast against the chart background is below 3:1 for lines or 4.5:1 for text.
- **Saved as optional fields** in `drawings.json` and the chart's study set. No format version bump; unknown fields from newer builds are kept.

**Consequences:** `docs/design.md` §5 gains a line on user-chosen colors. The existing perceptual-distance helper from the drawing palette is reused for the warnings.
