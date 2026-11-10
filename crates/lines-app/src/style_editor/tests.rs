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
fn the_custom_picker_previews_keeps_and_puts_back() {
    use crate::color_picker::Zone;
    let mut e = trend();
    let before = e.drawing_style(&Style::DEFAULT);
    // "Custom…" asks the host to open the picker on the color as the theme shows it.
    assert_eq!(
        click(&mut e, Control::Custom),
        Outcome::OpenCustom { fill: false }
    );
    e.open_custom(false, Rgb::hex(0x808080));
    assert!(e.custom.is_some());
    // Every change previews; the hex field types six digits.
    assert_eq!(e.key("right", true, None), Outcome::Preview);
    e.custom.as_mut().unwrap().zone = Zone::Hex;
    for ch in ["1", "2", "a", "b", "c"] {
        assert_eq!(e.key(ch, false, Some(ch)), Outcome::Nothing);
    }
    assert_eq!(
        e.key("d", false, Some("d")),
        Outcome::Preview,
        "six digits apply"
    );
    let c = Color::Custom(Rgb::hex(0x12abcd));
    assert_eq!(e.drawing_style(&Style::DEFAULT).line.color, c);
    // Enter keeps it, and it joins the recent colors (a row of its own, current there).
    assert_eq!(e.key("enter", false, None), Outcome::Changed);
    assert!(e.custom.is_none());
    assert_eq!(e.recent, vec![Rgb::hex(0x12abcd)]);
    assert!(e.recent_changed);
    let recent = e.rows().iter().position(|r| r.label == "Recent").unwrap();
    assert!(
        e.rows()[recent]
            .controls
            .contains(&(Control::Color(c), true))
    );
    assert!(
        !e.rows()[0].controls.contains(&(Control::Color(c), true)),
        "not shown twice"
    );
    // Esc puts back the color it opened with, and leaves the picker, not the editor.
    click(&mut e, Control::Custom);
    e.open_custom(false, Rgb::hex(0x12abcd));
    e.custom.as_mut().unwrap().zone = Zone::Hue;
    assert_eq!(e.key("right", true, None), Outcome::Preview);
    assert_ne!(e.drawing_style(&Style::DEFAULT).line.color, c);
    assert_eq!(e.key("escape", false, None), Outcome::Changed);
    assert!(e.custom.is_none());
    assert_eq!(e.drawing_style(&Style::DEFAULT).line.color, c);
    assert_eq!(e.key("escape", false, None), Outcome::Close);
    // A click on any other control keeps the picker's color: the keys move again.
    let mut e = trend();
    click(&mut e, Control::Custom);
    e.open_custom(false, Rgb::hex(0x808080));
    e.custom_square(1.0, 0.0);
    let line = e.rows().iter().position(|r| r.label == "Line").unwrap();
    e.click(line, 0);
    assert!(e.custom.is_none());
    assert_ne!(
        e.drawing_style(&Style::DEFAULT).line.color,
        before.line.color
    );
    let row = e.row;
    e.key("down", false, None);
    assert_eq!(e.row, row + 1, "arrows reach the controls");
    // The eyedropper: armed from the picker, Esc ends it; a color taken previews.
    click(&mut e, Control::Custom);
    e.open_custom(false, Rgb::hex(0x808080));
    assert_eq!(e.arm_dropper(), Outcome::Dropper);
    assert_eq!(e.key("escape", false, None), Outcome::Nothing);
    assert!(
        !e.dropper && e.custom.is_some(),
        "Esc ends the dropper only"
    );
    e.arm_dropper();
    assert_eq!(e.take_color(Rgb::hex(0x3fcf9a)), Outcome::Preview);
    assert!(!e.dropper);
    assert_eq!(
        e.drawing_style(&Style::DEFAULT).line.color,
        Color::Custom(Rgb::hex(0x3fcf9a))
    );
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
    assert_eq!(
        click(e, Control::Custom),
        Outcome::OpenCustom { fill: false }
    );
    e.open_custom(false, Rgb::hex(0x808080));
    e.custom.as_mut().unwrap().zone = crate::color_picker::Zone::Hex;
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
        all.contains("Low contrast in the light theme, both palettes (1.3:1); lines need 3:1."),
        "{all}"
    );
    // No contrast figure for the dark theme.
    assert!(
        !all.contains("Low contrast in the dark") && !all.contains("and the dark theme ("),
        "{all}"
    );
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
    assert_eq!(where_phrase(&c, &[true; 4]), " in every theme and palette");
    assert_eq!(
        where_phrase(&c, &[false, false, true, true]),
        " in the light theme, both palettes"
    );
    assert_eq!(
        where_phrase(&c, &[false, true, false, true]),
        " in the colorblind palette, both themes"
    );
    assert_eq!(
        where_phrase(&c, &[false, true, false, false]),
        " in the dark theme, colorblind palette"
    );
    assert_eq!(
        where_phrase(&c, &[true, true, false, true]),
        " in the dark theme, standard palette; the dark theme, colorblind palette; and the light theme, colorblind palette"
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

/// Keeping a custom color adds a Recent row under Color; the click that kept it still lands on
/// the control it was on (here "Dotted", in the row under Color as drawn), not the row that moved
/// into its place.
#[test]
fn a_click_that_keeps_the_picker_lands_where_it_was_drawn() {
    let mut e = StyleEditor::for_drawing(Tool::Trend, &Style::DEFAULT);
    e.open_custom(false, Rgb::hex(0x808080));
    assert_eq!(e.custom_square(1.0, 0.0), Outcome::Preview);
    let rows = e.rows();
    let line = rows.iter().position(|r| r.label == "Line").unwrap();
    assert!(rows.iter().all(|r| r.label != "Recent"));
    let dotted = rows[line]
        .controls
        .iter()
        .position(|(c, _)| *c == Control::LineType(LineType::Dotted))
        .unwrap();
    assert_eq!(e.click(line, dotted), Outcome::Changed);
    assert!(
        e.rows().iter().any(|r| r.label == "Recent"),
        "kept: remembered"
    );
    assert_eq!(
        e.drawing_style(&Style::DEFAULT).line.line,
        LineType::Dotted,
        "the clicked control, not the Recent swatch now in its row"
    );
    assert_ne!(
        e.drawing_style(&Style::DEFAULT).line.color,
        Style::DEFAULT.line.color
    );
}

/// Tab keeps the picker's color (as Enter and a click elsewhere do), on a one-tab editor too.
#[test]
fn tab_keeps_the_picker_color() {
    let mut e = StyleEditor::for_drawing(Tool::Trend, &Style::DEFAULT);
    e.open_custom(false, Rgb::hex(0x808080));
    assert_eq!(e.custom_square(1.0, 0.0), Outcome::Preview);
    let previewed = e.drawing_style(&Style::DEFAULT);
    assert_eq!(e.key("tab", false, None), Outcome::Changed);
    assert!(e.custom.is_none());
    assert_eq!(e.drawing_style(&Style::DEFAULT), previewed);
}
