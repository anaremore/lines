# 0038: Lucide icons, from its releases, at a thinner stroke

**Status:** Accepted (owner accepted 2026-11-11). It writes up the owner's call of 2026-11-10 ("Header icons: Lucide (ISC), the thinner stroke. Never copy from Zed's GPL repo") as built in `crates/lines-icons` (#194, then the drawing toolbar).

**Context:** Lines' icons were drawn in code: hand-plotted polylines with square corners and flat ends, rough next to the bundled fonts (0034), and every new icon meant plotting another shape. Lucide is an open icon set on a 24-unit grid with round caps and joins, under the ISC licence (MIT for the icons it derived from Feather). GPUI draws SVGs, colored by the theme: painted from bytes (`paint_svg`) or as an `svg()` element served by the app's `AssetSource`.

**Decision:**
1. **Source:** SVG files from Lucide's own GitHub releases, never from another project's copy and never from Zed's repository (GPL). Each icon's release is named in the crate's README with the release asset's SHA-256 (1.61.0 and 1.62.0 so far; their licence texts are the same).
2. **Two edits, documented:** every file's `stroke-width="2"` becomes `1.6` (the owner's thinner stroke at 16 px); `play`, `pause` and `ellipsis` get `fill="currentColor"` so they read solid. Shapes, caps and joins are Lucide's.
3. **One crate, one mechanism:** `crates/lines-icons` embeds the files the app uses (`ICONS`, by Lucide name; no dependencies, no GPUI). `lines-app/src/icons.rs` hands them to GPUI: `icons::paint` and `icons::button` for painted icons, `icons::Assets` (registered at every `Application`: the app, screenshots, the bench) and `icons::lucide` for `svg()` elements. Colors are theme tokens only. There is no second way of drawing vendored icons.
4. **Licensing:** the crate's licence is `ISC AND MIT`, with the release's `LICENSE` split into `LICENSE-ISC.txt`, `LICENSE-MIT.txt` and `NOTICE-feather.txt` so cargo-about matches each and ships its copyright lines; it is not `publish = false` (as `lines-fonts`), so the notices ship in every release's `THIRD_PARTY_LICENSES.txt`. The README and `Cargo.toml` name the Feather-derived icons in use.
5. **Only what is used:** the change that first uses an icon adds it (README's "Adding an icon"); every SVG is tested to parse as GPUI renders it.

**Alternatives considered:**
- **Keep drawing icons in code.** Rejected: the look the owner asked to replace, and slow to extend.
- **Lucide unmodified (stroke 2).** Rejected by the owner: too heavy at 16 px.
- **An icon font.** Rejected: one more font to load and shape; SVGs need nothing new in GPUI.
- **Zed's icons.** Rejected: GPL.

**Consequences:**
- The release notice carries Lucide's and Feather's licence texts; the binary grows by a few kilobytes per icon.
- `docs/architecture.md` lists the crate and the pinned releases.
