//! UI zoom (`crate::zoom`): no pane renders text at a size the zoom doesn't scale. Element
//! trees size text through the window's rem or `zoom::ui`; painted text through `Glyphs::new`
//! or `zoom::f`. A literal `text_size(px(..))` or a `Glyphs::fixed` is the bug the owner found
//! in the Positions pane (#114): text that stays at 100 % while the rest zooms.
//!
//! The exceptions are the surfaces whose geometry is also the pointer's hit test and doesn't
//! zoom yet (docs/known-gaps.md, UI zoom): each is listed here with its file, and a new one
//! fails this test until it scales or is listed with a reason.

/// (file, what may stay at 100 %, how many such sizes the file has).
const NOT_YET: &[(&str, &str, usize)] = &[
    (
        "chain_view.rs",
        "the option chain's ladder and action row (their cells are the pointer's)",
        8,
    ),
    (
        "chart_view.rs",
        "the order strip (`chart_trade::strip_box`) and the position tag's glyphs",
        2,
    ),
    ("order_group_view.rs", "the group tag's order list", 1),
    (
        "footprint.rs",
        "footprint cell text (its cells are sized to it)",
        1,
    ),
];

fn unscaled(src: &str) -> usize {
    src.matches("text_size(px(").count() + src.matches("Glyphs::fixed(").count()
}

#[test]
fn no_pane_renders_text_the_zoom_misses() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut found = Vec::new();
    for e in std::fs::read_dir(&dir).unwrap().flatten() {
        let p = e.path();
        if p.extension().is_none_or(|x| x != "rs") {
            continue;
        }
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        let n = unscaled(&std::fs::read_to_string(&p).unwrap());
        let allowed = NOT_YET
            .iter()
            .find(|(f, _, _)| *f == name)
            .map_or(0, |e| e.2);
        if n != allowed {
            found.push(format!(
                "{name}: {n} unscaled text sizes, {allowed} allowed"
            ));
        }
    }
    assert!(
        found.is_empty(),
        "text sizes the UI zoom doesn't scale (use `crate::zoom::ui` or `Glyphs::new`):\n{}",
        found.join("\n")
    );
}
