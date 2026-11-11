//! Interaction states use the theme's named state tokens (`ghost_hover`, `ghost_selected`,
//! `element_hover`, `element_selected`; docs/design.md §5, "State tokens"), never an ad hoc
//! fade: a faded accent reads differently on paper than on ink, so every light-mode fix to a
//! hand-faded state is a hunt. Counted: every `.opacity(..)`, `.alpha(..)` or `fade_out(..)` given
//! an argument outside
//! `theme.rs` and test modules (`*tests.rs`), whatever it fades: a token (`t.accent.opacity(`),
//! a local (`accent.opacity(`), a whole element (`el.opacity(0.4)`, `d.opacity(..)` in a
//! `.when`, `s.opacity(..)` in a `.hover`). `.opacity()` with no argument reads a style's
//! opacity back (`Look::opacity`) and isn't counted.
//!
//! The fades that remain are of data, not UI states (candles, studies, levels, order lines, a
//! stale series or pane), each file listed with its reason and count; a new fade anywhere fails
//! this test until it uses a state token, or is data and is listed. Not caught (review by
//! hand): an alpha set through `with_alpha`, an `Hsla { a, .. }` or a color's own alpha.

/// (file under `src/`, what it fades and why that is data, how many fades the file has).
const ALLOWED: &[(&str, &str, usize)] = &[
    (
        "levels_layer.rs",
        "a level label on the chart under the pointer, faded (box and text) so the bars under it show",
        2,
    ),
    (
        "pane.rs",
        "the stale veil: the chart's background over a stale pane's content",
        1,
    ),
    (
        "footprint.rs",
        "the bid × ask footprint's divider hairline, quieter than the numbers beside it",
        1,
    ),
    (
        "dom_trail.rs",
        "the price trail's neutral and direction shares, faded by the trail's decay",
        2,
    ),
    (
        "journal_shots.rs",
        "a trade snapshot's span behind the candles, in an exported SVG",
        1,
    ),
    (
        "study_menu_view.rs",
        "the studies menu's scroll cue: the panel, fully transparent, fading in over its last rows",
        1,
    ),
    (
        "chart_draw.rs",
        "the temporary measure's fill in its direction color",
        1,
    ),
    (
        "chart_view.rs",
        "order lines and a group's band in the order's color, and the last-price line \
         (`LAST_LINE_ALPHA`)",
        4,
    ),
    (
        "studies.rs",
        "overlay and VWAP band fills, the fill's hairline edges, approximate (bar-derived) delta \
         candles, and a test's heavy fill",
        5,
    ),
    ("study_style.rs", "a VWAP band's own fill translucency", 1),
    (
        "profile.rs",
        "the volume profile's split rows, dimmed outside the value area",
        2,
    ),
    (
        "series_pane.rs",
        "a stale external series' lines (`STALE_FADE`)",
        1,
    ),
];

/// Fades (`.opacity(..)` with an argument) in a source file.
fn faded(src: &str) -> usize {
    [".opacity(", ".alpha(", "fade_out("]
        .iter()
        .map(|pat| {
            src.match_indices(pat)
                .filter(|(i, m)| {
                    !src[i + m.len()..]
                        .trim_start_matches([' ', '\t', '\r', '\n'])
                        .starts_with(')')
                })
                .count()
        })
        .sum()
}

fn sources(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            sources(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

#[test]
fn ui_states_use_the_state_tokens() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    sources(&dir, &mut files);
    let mut found = Vec::new();
    for p in &files {
        let name = p
            .strip_prefix(&dir)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        // The tokens themselves are made here; test modules build fixtures, not UI.
        if name == "theme.rs" || name.ends_with("tests.rs") {
            continue;
        }
        let n = faded(&std::fs::read_to_string(p).unwrap());
        let allowed = ALLOWED
            .iter()
            .find(|(f, _, _)| *f == name)
            .map_or(0, |e| e.2);
        if n != allowed {
            found.push(format!("{name}: {n} fades, {allowed} allowed"));
        }
    }
    // Every exception names a file that exists (a renamed one is never silently skipped).
    for (f, _, _) in ALLOWED {
        assert!(
            dir.join(f).is_file(),
            "ALLOWED names {f}, which doesn't exist"
        );
    }
    assert!(
        found.is_empty(),
        "fades by hand (a UI state uses `Theme::ghost_hover`, `ghost_selected`, \
         `element_hover` or `element_selected`; a dimmed control is a dashed edge and dim \
         text; text over the plot sits on `label_bg`; a fade of data is listed in ALLOWED):\n{}",
        found.join("\n")
    );
}

/// The scanner sees each pattern it is meant to, and leaves a style's opacity read alone.
#[test]
fn the_scan_counts_each_fade() {
    assert_eq!(faded(".when(lit, |d| d.bg(t.accent.opacity(0.16)))"), 1);
    assert_eq!(faded(".hover(move |s| s.bg(accent.opacity(0.18)))"), 1);
    assert_eq!(faded(".bg(t.panel.opacity(0.9))"), 1);
    assert_eq!(faded("fill(b, theme.text_dim.opacity(0.35))"), 1);
    assert_eq!(
        faded(".when(!on, |d| d.text_color(t.text_dim).opacity(0.6))"),
        1
    );
    // The order strip's dimmed buttons, as this check's first version missed them.
    assert_eq!(faded("let el = if dim { el.opacity(0.4) } else { el };"), 1);
    assert_eq!(faded(".when(off, |d| d.opacity(0.5))"), 1);
    assert_eq!(faded(".hover(|s| s.opacity(0.8))"), 1);
    assert_eq!(faded("this.opacity(\n    0.5,\n)"), 1);
    // GPUI's other ways to fade a color.
    assert_eq!(faded(".bg(t.accent.alpha(0.16))"), 1);
    assert_eq!(faded("let mut c = t.panel; c.fade_out(0.1);"), 1);
    assert_eq!(faded(".when(lit, |d| d.bg(t.ghost_selected))"), 0);
    // A style's opacity read back.
    assert_eq!(faded("looks[i].opacity()"), 0);
    assert_eq!(faded("t.part_color(c.color(), t.up, fill.opacity())"), 0);
}
