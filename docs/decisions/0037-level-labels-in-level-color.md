# 0037: Level labels may use the level's color, amends 0022

**Status:** Accepted (owner approved the RS styling mocks, 2026-11-10). Supersedes 0022's clause "labels still use theme text tokens".

**Context:** Decision 0022 lets external level sources color their lines and zones, as the one exception to "theme tokens only", but keeps label text in theme text tokens. The owner's approved RS Levels styling draws each label's text in its level's color, so a label is read as belonging to its line at a glance, which plain text on a busy chart doesn't give. #185 implements it.

**Decision:**
1. **A level label's text may be drawn in its level's color** (the source's style, or the user's per-type style, decision 0026). It sits on `theme.label_bg`, as before.
2. **Readability wins over color.** The color is used only when it meets WCAG AA (4.5:1) against the label's actual backing, in the current theme and palette, including where the label sits over a zone fill. Otherwise the label falls back to the theme's text token. It never falls back silently to a low-contrast color.
3. **Never mistaken for the last price or for direction.** A label color too close to the last-price accent falls back as well, near the last price. Up and down stay shown by shape, never by a label's color alone (`docs/design.md`).
4. **The user can override it** per level type (`label-color`: the level's color or theme text). It's an optional saved field, written only when it differs from the type's default, so no version bump.
5. **The rest of 0022 stands:** sources still color only their own lines, zones and labels. Every other UI color stays a theme token.

**Consequences:** some light-theme labels whose colors measure below AA (for example DD, MHP, Yellow and Red Line) show in theme text. Darker light-theme variants for them are an open question for the owner, not a default. The AA check runs in tests across all four theme/palette combinations.

**Alternatives:** keep theme text for every label (0022 as written): rejected, because the owner wants labels tied visually to their levels. Always use the level color: rejected, because it's unreadable on some themes.
