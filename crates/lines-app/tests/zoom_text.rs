//! UI zoom (`crate::zoom`): no pane renders text at a size the zoom doesn't scale. Element
//! trees size text through the window's rem or `zoom::ui`; painted text through `Glyphs::new`,
//! `zoom::f` or a `*_px()` helper. Counted as unscaled: a literal `text_size(px(..))`, a
//! `Glyphs::fixed`, and a `shape_line`/`shape_sized` given a `px(..)` constant or a `*_100`
//! size. That is the bug the owner found in the Positions pane (#114): text that stays at
//! 100 % while the rest zooms. A size wrapped in `zoom::f`/`zoom::ui`/`zoom::get` counts as
//! scaled. Not caught here (review by hand): a fixed-height or fixed-width container around
//! zoomed text, and a size passed in through a local variable.
//!
//! The exceptions are surfaces that don't zoom yet (docs/known-gaps.md, UI zoom), each listed
//! with its file and reason; a new one fails this test until it scales or is listed.

/// (file under `src/`, what may stay at 100 % and why, how many such sizes the file has).
const NOT_YET: &[(&str, &str, usize)] = &[
    (
        "chain_view.rs",
        "the option chain's header, row strip, ticket dock, armed rows and footer (their 100 % \
         width rules: NARROW_HEADER, EXITS_IN_HEADER, strip_orders_fit) and its ladder (cells \
         are the pointer's)",
        8,
    ),
    (
        "chart_view.rs",
        "a note being typed (drawings don't zoom)",
        1,
    ),
    ("order_group_view.rs", "the group tag's order list", 1),
    (
        "footprint.rs",
        "footprint cell text (its cells are sized to it)",
        1,
    ),
    ("econ_layer.rs", "the economic calendar's chart tags", 1),
];

/// Unscaled text sizes in a source file.
fn unscaled(src: &str) -> usize {
    let mut n = src.matches("text_size(px(").count() + src.matches("text_size(gpui::px(").count();
    n += src.matches("Glyphs::fixed(").count();
    for call in ["shape_line(", "shape_sized("] {
        for (i, _) in src.match_indices(call) {
            // The call's arguments (enough to reach the size).
            let args: String = src[i + call.len()..].chars().take(160).collect();
            let args = args.split(";").next().unwrap_or("");
            // Scaled where the size goes through the zoom.
            let zoomed = ["zoom::f(", "zoom::ui(", "zoom::get()"]
                .iter()
                .any(|z| args.contains(z));
            let fixed = !zoomed
                && (args.contains("_100")
                    || args.match_indices("px(").any(|(j, _)| {
                        args[j + 3..]
                            .starts_with(|c: char| c.is_ascii_digit() || c.is_ascii_uppercase())
                    }));
            if fixed {
                n += 1;
            }
        }
    }
    n
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
fn no_pane_renders_text_the_zoom_misses() {
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
        let n = unscaled(&std::fs::read_to_string(p).unwrap());
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
    // Every exception names a file that exists (a renamed one is never silently skipped).
    for (f, _, _) in NOT_YET {
        assert!(
            dir.join(f).is_file(),
            "NOT_YET names {f}, which doesn't exist"
        );
    }
    assert!(
        found.is_empty(),
        "text sizes the UI zoom doesn't scale (use `crate::zoom::ui`, `Glyphs::new` or \
         `zoom::f`):\n{}",
        found.join("\n")
    );
}

/// The scanner sees each pattern it is meant to.
#[test]
fn the_scan_counts_each_unscaled_pattern() {
    assert_eq!(unscaled(".text_size(px(12.0))"), 1);
    assert_eq!(unscaled(".text_size(crate::zoom::ui(12.0))"), 0);
    assert_eq!(unscaled("Glyphs::fixed(12.0)"), 1);
    assert_eq!(unscaled("Glyphs::new(12.0)"), 0);
    assert_eq!(unscaled("x.shape_line(s, px(TAG_PX), &[run], None);"), 1);
    assert_eq!(
        unscaled("x.shape_line(s, px(label_px()), &[run], None);"),
        0
    );
    assert_eq!(
        unscaled("self.shape_sized(&s, c, LABEL_PX_100, window);"),
        1
    );
    assert_eq!(unscaled("self.shape_sized(&s, c, label_px(), window);"), 0);
    assert_eq!(
        unscaled("self.shape_sized(&s, c, crate::zoom::f(LABEL_PX_100), window);"),
        0
    );
    assert_eq!(
        unscaled("x.shape_line(s, px(TAG_PX * crate::zoom::get()), &[r], None);"),
        0
    );
}
