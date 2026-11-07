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
    let red = Color::Custom(Rgb::hex(CUSTOM_SWATCHES[0]));
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

#[test]
fn the_guard_words_what_it_found_and_never_blocks() {
    let ink = Rgb::hex(0x11141b);
    let panel = Rgb::hex(0x171b24);
    let meanings = [("the up color", Rgb::hex(0x3fcf9a))];
    let resolve = |c: Color| match c {
        Color::Custom(rgb) => rgb,
        _ => Rgb::hex(0xeceaf4),
    };
    let mut e = trend();
    assert_eq!(e.warning(resolve, ink, panel, &meanings), None);
    // A green near "up": warned, and still applied.
    click(&mut e, Control::Custom);
    for ch in "40d09a".chars() {
        let t = ch.to_string();
        e.key(&t, false, Some(&t));
    }
    assert_eq!(e.key("enter", false, None), Outcome::Changed);
    let w = e.warning(resolve, ink, panel, &meanings).unwrap();
    assert!(w.contains("Close to the up color"), "{w}");
    // Faint paper: low contrast for a line.
    click(&mut e, Control::Color(Color::Palette(0)));
    click(&mut e, Control::Opacity(25));
    let w = e.warning(resolve, ink, panel, &meanings).unwrap();
    assert!(
        w.contains("Low contrast") && w.contains("lines need 3:1"),
        "{w}"
    );
    // Text needs 4.5:1, against its box when it has one.
    let mut n = StyleEditor::for_drawing(Tool::Note, &Style::DEFAULT);
    click(&mut n, Control::Custom);
    for ch in "5a6070".chars() {
        let t = ch.to_string();
        n.key(&t, false, Some(&t));
    }
    n.key("enter", false, None);
    let w = n.warning(resolve, ink, panel, &meanings).unwrap();
    assert!(w.contains("its box") && w.contains("4.5:1"), "{w}");
}
