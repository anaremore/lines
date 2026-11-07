use super::*;

fn trend() -> StyleEditor {
    StyleEditor::for_drawing(Tool::Trend, &Style::DEFAULT)
}

fn find(e: &StyleEditor, c: Control) -> (usize, usize) {
    for (r, row) in e.rows().iter().enumerate() {
        if let Some(k) = row.controls.iter().position(|(x, _)| *x == c) {
            return (r, k);
        }
    }
    panic!("{c:?} not offered");
}

fn click(e: &mut StyleEditor, c: Control) -> Outcome {
    let (r, k) = find(e, c);
    e.click(r, k)
}

#[test]
fn tabs_follow_the_tool() {
    let titles = |t: Tool| {
        StyleEditor::for_drawing(t, &Style::DEFAULT)
            .parts
            .iter()
            .map(|p| p.title)
            .collect::<Vec<_>>()
    };
    assert_eq!(titles(Tool::Trend), ["Line"]);
    assert_eq!(titles(Tool::Rect), ["Line", "Fill"]);
    assert_eq!(titles(Tool::Note), ["Text", "Marker"]);
    let e = trend();
    assert_eq!(e.title, "Trend line style");
    assert_eq!(
        e.default_label.as_deref(),
        Some("Save as default for trend lines")
    );
    // Options only where they apply.
    let rows = |t: Tool| {
        StyleEditor::for_drawing(t, &Style::DEFAULT)
            .rows()
            .iter()
            .map(|r| r.label)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        rows(Tool::Trend),
        ["Color", "Line", "Width", "Opacity", "Extend", ""]
    );
    assert_eq!(
        rows(Tool::HLine),
        ["Color", "Line", "Width", "Opacity", "Price tag", ""]
    );
    assert_eq!(rows(Tool::VLine), ["Color", "Line", "Width", "Opacity", ""]);
    assert_eq!(rows(Tool::Note), ["Size", "Color", "Box", ""]);
}

#[test]
fn every_control_changes_the_drawing_style() {
    let mut e = trend();
    assert_eq!(
        click(&mut e, Control::LineType(LineType::DashDot)),
        Outcome::Changed
    );
    assert_eq!(click(&mut e, Control::Width(4)), Outcome::Changed);
    assert_eq!(click(&mut e, Control::Opacity(50)), Outcome::Changed);
    assert_eq!(click(&mut e, Control::ExtendRight), Outcome::Changed);
    let red = Color::Custom(Rgb::hex(CUSTOM_SWATCHES[0].0));
    assert_eq!(click(&mut e, Control::Color(red)), Outcome::Changed);
    // Picking what is already set changes nothing.
    assert_eq!(click(&mut e, Control::Width(4)), Outcome::Nothing);
    let s = e.drawing_style(&Style::DEFAULT);
    assert_eq!(s.line.line, LineType::DashDot);
    assert_eq!((s.line.width, s.line.opacity), (4, 50));
    assert_eq!(s.line.color, red);
    assert!(s.extend_right && !s.extend_left);
    // The rows show the current values.
    let rows = e.rows();
    assert!(
        rows[2]
            .controls
            .iter()
            .any(|&(c, on)| c == Control::Width(4) && on)
    );
}

#[test]
fn a_zone_recolored_keeps_its_fill_in_step() {
    let mut e = StyleEditor::for_drawing(Tool::Rect, &Style::DEFAULT);
    click(&mut e, Control::Color(Color::Palette(2)));
    let s = e.drawing_style(&Style::DEFAULT);
    assert_eq!(
        (s.line.color, s.fill.color),
        (Color::Palette(2), Color::Palette(2))
    );
    // A fill given its own color stays.
    e.select_tab(1);
    click(&mut e, Control::Color(Color::Palette(3)));
    click(&mut e, Control::Opacity(40));
    e.select_tab(0);
    click(&mut e, Control::Color(Color::Palette(1)));
    let s = e.drawing_style(&Style::DEFAULT);
    assert_eq!(s.fill.color, Color::Palette(3));
    assert_eq!(s.fill.opacity, 40);
}

#[test]
fn custom_colors_are_typed_in_hex() {
    let mut e = trend();
    assert_eq!(click(&mut e, Control::Custom), Outcome::Nothing);
    assert_eq!(e.hex.as_deref(), Some(""));
    for ch in ["1", "2", "z", "a", "b", "C", "d", "9"] {
        e.key(ch, false, Some(ch));
    }
    // Non-hex is ignored, and it stops at six digits.
    assert_eq!(e.hex.as_deref(), Some("12abcd"));
    assert_eq!(e.key("enter", false, None), Outcome::Changed);
    assert_eq!(e.hex, None);
    let c = Color::Custom(Rgb::hex(0x12abcd));
    assert_eq!(e.drawing_style(&Style::DEFAULT).line.color, c);
    // Shown as its own chip, current.
    assert!(e.rows()[0].controls.contains(&(Control::Color(c), true)));
    // Reopening the entry starts from it; Esc leaves the entry, not the editor.
    click(&mut e, Control::Custom);
    assert_eq!(e.hex.as_deref(), Some("12abcd"));
    e.key("backspace", false, None);
    assert_eq!(e.key("enter", false, None), Outcome::Nothing, "five digits");
    assert_eq!(e.key("escape", false, None), Outcome::Nothing);
    assert_eq!(e.hex, None);
    assert_eq!(e.key("escape", false, None), Outcome::Close);
}

#[test]
fn keyboard_moves_picks_and_switches_tabs() {
    let mut e = StyleEditor::for_drawing(Tool::Rect, &Style::DEFAULT);
    // Down to "Line", right to "Dashed", Enter.
    e.key("down", false, None);
    e.key("right", false, None);
    assert_eq!(
        e.focused(&e.rows()),
        Some(Control::LineType(LineType::Dashed))
    );
    assert_eq!(e.key("enter", false, None), Outcome::Changed);
    assert_eq!(e.drawing_style(&Style::DEFAULT).line.line, LineType::Dashed);
    // Focus stays inside the rows.
    for _ in 0..50 {
        e.key("down", false, None);
        e.key("right", false, None);
    }
    let rows = e.rows();
    assert_eq!(e.row, rows.len() - 1);
    assert_eq!(e.focused(&rows), Some(Control::SaveDefault));
    assert_eq!(e.key("space", false, None), Outcome::SaveDefault);
    assert!(e.saved);
    // Tab, Shift+Tab.
    e.key("tab", false, None);
    assert_eq!((e.tab, e.row, e.col), (1, 0, 0));
    e.key("tab", true, None);
    assert_eq!(e.tab, 0);
    e.key("tab", true, None);
    assert_eq!(e.tab, 1, "wraps");
    assert_eq!(e.key("escape", false, None), Outcome::Close);
}

#[test]
fn sync_follows_an_undo_underneath() {
    let mut e = trend();
    click(&mut e, Control::Width(3));
    e.sync_drawing(&Style::DEFAULT);
    assert_eq!(e.drawing_style(&Style::DEFAULT), Style::DEFAULT);
}

fn ctxs() -> Vec<ThemeCtx> {
    crate::theme::Theme::all()
        .iter()
        .map(crate::theme::Theme::style_ctx)
        .collect()
}

fn type_hex(e: &mut StyleEditor, hex: &str) {
    click(e, Control::Custom);
    for _ in 0..6 {
        e.key("backspace", false, None);
    }
    for ch in hex.chars() {
        let t = ch.to_string();
        e.key(&t, false, Some(&t));
    }
    assert_eq!(e.key("enter", false, None), Outcome::Changed);
}

fn texts(w: &[Warning]) -> Vec<&str> {
    w.iter().map(|w| w.text.as_str()).collect()
}

/// The guard checks a custom color in all four theme/palette combinations and says where it
/// fails; it never blocks (the color is applied).
#[test]
fn the_guard_checks_every_theme_and_never_blocks() {
    let ctxs = ctxs();
    let mut e = trend();
    assert_eq!(e.warnings(&ctxs), []);
    // A yellow: fine on dark, low contrast on light.
    type_hex(&mut e, "f2e05a");
    let w = e.warnings(&ctxs);
    let all = texts(&w).join(" | ");
    assert!(
        all.contains("Low contrast in the light theme (1.3:1); lines need 3:1."),
        "{all}"
    );
    assert!(!all.contains("dark theme ("), "{all}");
    // An orange: like the colorblind palette's down candles, named with its swatch; meaning
    // matches come before contrast.
    type_hex(&mut e, "ff9f1a");
    let w = e.warnings(&ctxs);
    let down = w
        .iter()
        .find(|w| w.text.starts_with("Looks like the down color"))
        .unwrap_or_else(|| panic!("{:?}", texts(&w)));
    assert!(
        down.text.contains("colorblind") && down.text.ends_with("may be misread as down."),
        "{}",
        down.text
    );
    assert_eq!(down.swatch, Some(Rgb::hex(0xff9f1a)));
    assert!(w[0].text.starts_with("Looks like"));
    assert!(
        w.iter()
            .skip_while(|w| w.text.starts_with("Looks"))
            .all(|w| !w.text.starts_with("Looks"))
    );
    assert!(!w.iter().any(|w| w.text.contains("::")));
    assert_eq!(
        e.drawing_style(&Style::DEFAULT).line.color,
        Color::Custom(Rgb::hex(0xff9f1a))
    );
    // The warning color: worded as such.
    type_hex(&mut e, "f5b83d");
    let w = e.warnings(&ctxs);
    assert!(
        texts(&w)
            .iter()
            .any(|t| t.starts_with("Looks like the warning color")
                && t.ends_with("may be misread as a warning.")),
        "{:?}",
        texts(&w)
    );
    // Text needs 4.5:1 against its box; without the box the chart shows it anyway.
    let mut n = StyleEditor::for_drawing(Tool::Note, &Style::DEFAULT);
    assert_eq!(n.warnings(&ctxs), []);
    type_hex(&mut n, "5a6070");
    let w = texts(&n.warnings(&ctxs)).join(" | ");
    assert!(w.contains("against its box") && w.contains("4.5:1"), "{w}");
}

#[test]
fn where_a_finding_holds_is_worded() {
    let c = ctxs();
    assert_eq!(where_phrase(&c, &[true; 4]), "");
    assert_eq!(
        where_phrase(&c, &[false, false, true, true]),
        " in the light theme"
    );
    assert_eq!(
        where_phrase(&c, &[false, true, false, true]),
        " in the colorblind palette"
    );
    assert_eq!(
        where_phrase(&c, &[false, true, false, false]),
        " in the dark colorblind theme"
    );
}

/// Titles and "save as default" name the tool, in the plural.
#[test]
fn labels_name_the_tool() {
    let z = StyleEditor::for_drawing(Tool::Rect, &Style::DEFAULT);
    assert_eq!(z.title, "Zone style");
    assert_eq!(
        z.default_label.as_deref(),
        Some("Save as default for zones")
    );
    let n = StyleEditor::for_drawing(Tool::Note, &Style::DEFAULT);
    assert_eq!(
        n.default_label.as_deref(),
        Some("Save as default for notes")
    );
}

/// Swatches have names (never color alone); text offers the theme's text color once.
#[test]
fn swatches_are_named_and_not_duplicated() {
    let names = ["White", "Lime", "Olive", "Slate"];
    assert_eq!(color_name(Color::Palette(1), &names), "Lime");
    assert_eq!(color_name(Color::Text, &names), "Text");
    assert_eq!(
        color_name(Color::Custom(Rgb::hex(0xe0b000)), &names),
        "Custom #E0B000"
    );
    assert_eq!(
        color_name(Color::Custom(Rgb::hex(CUSTOM_SWATCHES[0].0)), &names),
        CUSTOM_SWATCHES[0].1
    );
    let n = StyleEditor::for_drawing(Tool::Note, &Style::DEFAULT);
    let colors = &n.rows()[1].controls;
    assert!(colors.contains(&(Control::Color(Color::Text), true)));
    assert!(
        !colors
            .iter()
            .any(|(c, _)| *c == Control::Color(Color::Palette(0)))
    );
    // On/off options are checkboxes.
    assert!(is_toggle(Control::TextBox) && is_toggle(Control::ExtendLeft));
    assert!(!is_toggle(Control::Width(2)));
}
