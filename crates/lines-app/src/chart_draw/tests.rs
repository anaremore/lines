use super::*;
use lines_core::style::{Color, LineStyle, LineType};
use lines_core::{Bar, BarsView, Price, Qty, UtcMillis};
use lines_feed::catalog;

const M: i64 = 60_000;

fn bars() -> Vec<Bar> {
    (0..120)
        .map(|i| Bar {
            time: UtcMillis(1_800_000_000_000 + i * M),
            open: Price(600_000),
            high: Price(600_100),
            low: Price(599_900),
            close: Price(600_050),
            volume: Qty(3),
        })
        .collect()
}

fn vp() -> Viewport {
    let mut vp = Viewport::default();
    vp.set_size(800.0, 400.0);
    vp.bar_px = 6.0;
    vp.right_index = 119.0;
    vp.price_lo = 599_000.0;
    vp.price_hi = 601_000.0;
    vp
}

struct Rig {
    bars: Vec<Bar>,
    vp: Viewport,
    d: ChartDrawings,
}

impl Rig {
    fn new() -> Rig {
        let es = catalog::get(catalog::ES).unwrap();
        Rig {
            bars: bars(),
            vp: vp(),
            d: ChartDrawings::new(ProductDrawings::ephemeral(es, Vec::new())),
        }
    }

    fn anchor(&self, x: f32, y: f32) -> Option<Anchor> {
        let axis = TimeAxis::new(BarsView::fixed(&self.bars, M));
        snap_anchor(&axis, &self.vp, 25, false, f64::from(x), f64::from(y))
    }

    /// One frame with the pointer at (`x`, `y`).
    fn frame(&mut self, x: f32, y: f32) {
        self.d.set_pointer(Some((x, y)));
        let axis = TimeAxis::new(BarsView::fixed(&self.bars, M));
        let fmt = catalog::get(catalog::ES).unwrap().price;
        self.d
            .prepare(&axis, &self.vp, 25, false, &fmt, Ratio::new(50, 1));
    }

    fn down(&mut self, x: f32, y: f32) -> bool {
        self.frame(x, y);
        let a = self.anchor(x, y);
        self.d.on_down(x, y, false, a)
    }

    fn up(&mut self, x: f32, y: f32) -> bool {
        self.frame(x, y);
        let a = self.anchor(x, y);
        self.d.on_up(x, y, a)
    }

    fn items(&self) -> Vec<Drawing> {
        self.d.data().lock().items().to_vec()
    }
}

#[test]
fn two_anchor_tools_draw_by_click_click_or_drag() {
    let mut r = Rig::new();
    r.d.set_tool(Tool::Trend);
    assert!(r.down(100.0, 100.0));
    assert!(
        r.up(101.0, 100.0),
        "a click leaves the first anchor waiting"
    );
    assert!(r.items().is_empty());
    r.frame(300.0, 200.0);
    assert!(r.d.preview.is_some(), "the line follows the pointer");
    assert!(r.down(300.0, 200.0));
    let items = r.items();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].tool, Tool::Trend);
    assert_eq!(items[0].a, r.anchor(100.0, 100.0).unwrap());
    assert_eq!(items[0].b, r.anchor(300.0, 200.0).unwrap());
    assert_eq!(r.d.tool(), None, "drawing mode ends after one drawing");
    assert_eq!(r.d.selected, Some(items[0].id));

    // Press, drag, release.
    r.d.set_tool(Tool::Rect);
    assert!(r.down(200.0, 50.0));
    assert!(r.up(400.0, 150.0));
    assert_eq!(r.items().len(), 2);
    assert_eq!(r.items()[1].tool, Tool::Rect);
}

#[test]
fn clicks_belong_to_drawings_only_in_drawing_mode_or_on_a_drawing() {
    let mut r = Rig::new();
    // No tool, nothing drawn: the chart gets the click (pan).
    assert!(!r.down(100.0, 100.0));
    r.d.set_tool(Tool::HLine);
    // Ctrl+click is reserved for chart trading, even in drawing mode.
    let a = r.anchor(100.0, 100.0);
    assert!(!r.d.on_down(100.0, 100.0, true, a));
    assert!(r.down(100.0, 100.0));
    assert_eq!(r.items().len(), 1);
    r.d.selected = None;
    // On an unselected line: it is selected and the chart still pans; once selected, the
    // line takes the press (drag); off it: the chart does.
    r.frame(0.0, 0.0);
    let y = r.d.drawn()[0].p0.1;
    assert!(!r.down(500.0, y + 2.0), "selects, doesn't capture");
    assert_eq!(r.d.selected, Some(r.items()[0].id));
    assert!(r.down(500.0, y + 2.0), "selected: drags");
    assert!(r.up(500.0, y + 2.0));
    assert!(!r.down(500.0, y + 40.0));
    assert_eq!(r.d.selected, None);
    // Hidden on this chart: nothing is hit.
    r.d.hidden = true;
    r.frame(0.0, 0.0);
    assert!(!r.down(500.0, y));
}

/// Dragging a drawing moves it by whole bars and ticks, as one undo step; a handle reshapes
/// one anchor.
#[test]
fn dragging_moves_and_reshapes_as_one_undo_step() {
    let mut r = Rig::new();
    r.d.set_tool(Tool::Trend);
    r.down(100.0, 100.0);
    r.down(300.0, 200.0);
    let before = r.items()[0].clone();
    // Body: from the middle, 60 px right (10 bars) and 40 px down.
    r.frame(0.0, 0.0);
    assert!(r.down(200.0, 150.0));
    r.frame(230.0, 170.0);
    r.frame(260.0, 190.0);
    assert!(r.up(260.0, 190.0));
    let after = r.items()[0].clone();
    assert_eq!(after.a.time.0 - before.a.time.0, 10 * M);
    assert_eq!(after.b.time.0 - before.b.time.0, 10 * M);
    let dprice = after.a.price.0 - before.a.price.0;
    assert_eq!(dprice, after.b.price.0 - before.b.price.0);
    assert!(dprice < 0 && dprice % 25 == 0);
    // Handle 1 (the drawing is selected): only `b` moves.
    r.frame(0.0, 0.0);
    let (hx, hy) = r.d.drawn()[0].handles[1];
    assert!(r.down(hx, hy));
    assert!(r.up(hx + 60.0, hy));
    let reshaped = r.items()[0].clone();
    assert_eq!(reshaped.a, after.a);
    assert_eq!(reshaped.b.time.0 - after.b.time.0, 10 * M);
    // Two undo steps back to the original, then redo.
    assert!(r.d.undo());
    assert_eq!(r.items()[0], after);
    assert!(r.d.undo());
    assert_eq!(r.items()[0], before);
    assert!(r.d.redo());
    assert_eq!(r.items()[0], after);
}

#[test]
fn notes_measures_delete_and_clear() {
    let mut r = Rig::new();
    // A note: click, type, Enter.
    r.d.set_tool(Tool::Note);
    assert!(r.down(100.0, 100.0));
    assert!(r.d.typing());
    for c in ["H", "i", "!"] {
        assert_eq!(r.d.key(c, Some(c)), Key::Handled);
    }
    r.d.key("backspace", None);
    r.d.key("enter", None);
    assert!(!r.d.typing());
    let note = r.items()[0].clone();
    assert_eq!((note.tool, note.text.as_str()), (Tool::Note, "Hi"));
    r.frame(0.0, 0.0);
    assert_eq!(r.d.note_text(note.id), Some("Hi"));

    // A measure is temporary until pinned with Enter; a click elsewhere drops it.
    r.d.set_tool(Tool::Measure);
    r.down(200.0, 300.0);
    r.down(400.0, 100.0);
    assert_eq!(r.items().len(), 1, "not saved");
    r.frame(0.0, 0.0);
    let (_, stats) = r.d.measure_shown().unwrap();
    assert!(stats.ticks > 0 && stats.bars > 0);
    assert_eq!(r.d.key("enter", None), Key::Handled);
    assert_eq!(r.items().len(), 2);
    assert_eq!(r.items()[1].tool, Tool::Measure);

    // Delete removes the selection; Esc cancels a tool.
    assert_eq!(r.d.key("delete", None), Key::Handled);
    assert_eq!(r.items().len(), 1);
    r.d.set_tool(Tool::Trend);
    assert_eq!(r.d.key("escape", None), Key::Handled);
    assert_eq!(r.d.tool(), None);
    assert_eq!(r.d.key("escape", None), Key::Ignored);

    // Delete all waits for Enter (Esc keeps them), and undo brings them back.
    r.d.confirm_clear = true;
    assert_eq!(r.d.key("escape", None), Key::Handled);
    assert_eq!(r.items().len(), 1);
    r.d.confirm_clear = true;
    r.d.key("enter", None);
    assert!(r.items().is_empty());
    assert!(r.d.undo());
    assert_eq!(r.items().len(), 1);
}

#[test]
fn restyling_applies_to_the_selection_or_new_drawings() {
    let mut r = Rig::new();
    let dashed = |s: Style| Style {
        line: LineStyle {
            line: LineType::Dashed,
            ..s.line
        },
        ..s
    };
    r.d.restyle(dashed);
    r.d.set_tool(Tool::HLine);
    r.down(100.0, 100.0);
    let d = r.items()[0].clone();
    assert_eq!(d.style.line.line, LineType::Dashed);
    r.d.restyle(|s| Style {
        line: LineStyle {
            color: Color::Palette(1),
            width: 2,
            ..s.line
        },
        ..s
    });
    let now = r.items()[0].style;
    assert_eq!(now.line.color, Color::Palette(1));
    assert_eq!((now.line.width, now.line.line), (2, LineType::Dashed));
    assert!(r.d.undo());
    assert_eq!(r.items()[0].style, d.style);

    // "Next color" on a zone: its fill moves with the line (it matched), on the selection
    // and on the session default the next zone is drawn with.
    r.d.set_tool(Tool::Rect);
    assert!(r.down(200.0, 50.0));
    assert!(r.up(400.0, 150.0));
    let zone = r.items()[1].clone();
    assert_eq!(r.d.selected, Some(zone.id));
    assert_eq!(zone.style.fill.color, zone.style.line.color);
    r.d.restyle(next_color);
    let s = r.items()[1].style;
    assert_ne!(s.line.color, zone.style.line.color);
    assert_eq!(s.fill.color, s.line.color, "the fill follows the line");
    // A fill set apart stays.
    r.d.restyle(|s| Style {
        fill: lines_core::style::FillStyle {
            color: Color::Palette(3),
            ..s.fill
        },
        line: LineStyle {
            color: Color::Palette(0),
            ..s.line
        },
        ..s
    });
    r.d.restyle(next_color);
    let s = r.items()[1].style;
    assert_eq!(
        (s.line.color, s.fill.color),
        (Color::Palette(1), Color::Palette(3))
    );
    // Nothing selected: the session defaults, fills included.
    r.d.selected = None;
    r.d.restyle(next_color);
    r.d.set_tool(Tool::Rect);
    assert!(r.down(200.0, 250.0));
    assert!(r.up(400.0, 350.0));
    let s = r.items()[2].style;
    assert_eq!(s.fill.color, s.line.color);
    assert_ne!(s.line.color, Style::DEFAULT.line.color);
}

/// The style editor: opened on a drawing under the pointer, each change one undo step,
/// "save as default" applies to the tool's next drawings, and it closes when its drawing goes.
#[test]
fn the_style_editor_edits_one_drawing() {
    let mut r = Rig::new();
    r.d.set_tool(Tool::HLine);
    r.down(100.0, 100.0);
    r.frame(400.0, 100.0);
    let id = r.d.hit_at(400.0, 100.0).expect("on the line");
    assert_eq!(r.d.hit_at(400.0, 300.0), None);
    assert!(r.d.open_editor(id));
    assert!(r.d.editing());
    let (out, _) = r.d.edit_style(|e| {
        let (row, col) = (1, 2); // Line: Dotted
        e.click(row, col)
    });
    assert_eq!(out, Outcome::Changed);
    assert_eq!(r.items()[0].style.line.line, LineType::Dotted);
    // Undo underneath: the editor follows.
    assert!(r.d.undo());
    assert_eq!(r.items()[0].style, Style::DEFAULT);
    let e = r.d.editor().unwrap();
    assert_eq!(e.drawing_style(&Style::DEFAULT), Style::DEFAULT);
    // Width 3, then save as default for horizontal lines.
    r.d.edit_style(|e| e.click(2, 2));
    let (out, saved) = r.d.edit_style(|e| {
        let last = e.rows().len() - 1;
        e.click(last, 0)
    });
    assert_eq!(out, Outcome::SaveDefault);
    let (tool, style) = saved.unwrap();
    assert_eq!((tool, style.line.width), (Tool::HLine, 3));
    // Esc closes it.
    assert_eq!(
        r.d.edit_style(|e| e.key("escape", false, None)).0,
        Outcome::Close
    );
    assert!(!r.d.editing());
    // The next horizontal line is 3 px; a trend line keeps its own default.
    r.d.set_tool(Tool::HLine);
    r.down(100.0, 200.0);
    assert_eq!(r.items()[1].style.line.width, 3);
    r.d.set_tool(Tool::Trend);
    r.down(100.0, 300.0);
    r.down(200.0, 320.0);
    assert_eq!(r.items()[2].style.line.width, 1);
    // Deleted (here, from another chart of the product): the editor closes.
    assert!(r.d.open_editor(id));
    r.d.data().change(|s| (s.remove(id), ()));
    assert!(r.d.editor().is_none());
    // Hidden: it closes too, and it can't open on hidden drawings.
    let other = r.items()[0].id;
    assert!(r.d.open_editor(other));
    r.d.set_hidden(true);
    assert!(!r.d.editing());
    assert!(!r.d.open_editor(other));
}

/// Axis tags: horizontal lines and rays tag their price, a zone its top and bottom; only with
/// the option on.
#[test]
fn axis_tags_follow_the_option() {
    let mut r = Rig::new();
    r.d.set_tool(Tool::HLine);
    r.down(100.0, 100.0);
    r.d.set_tool(Tool::Rect);
    r.down(200.0, 150.0);
    r.down(300.0, 250.0);
    r.frame(0.0, 0.0);
    assert_eq!(r.d.axis_tags(400.0).count(), 0);
    for d in r.items() {
        let mut d = d.clone();
        d.style.axis_tag = true;
        r.d.data().change(|s| (s.edit(d), ()));
    }
    r.frame(0.0, 0.0);
    let tags: Vec<_> = r.d.axis_tags(400.0).map(|(p, _, _)| p).collect();
    let items = r.items();
    assert_eq!(tags, [items[0].a.price, items[1].a.price, items[1].b.price]);
}

/// Every chart of a product shares its drawings: a change on one is seen by the other.
#[test]
fn charts_of_a_product_share_drawings() {
    let mut a = Rig::new();
    let mut b = ChartDrawings::new(a.d.data().clone());
    assert!(b.changed());
    assert!(!b.changed());
    a.d.set_tool(Tool::VLine);
    a.down(100.0, 100.0);
    assert!(b.changed());
    assert_eq!(b.count(), 1);
}

/// A drawing made on 1m and moved on a 15m chart keeps each anchor's minute inside its
/// bucket: it moves by whole 15m bars, never collapsing onto bucket starts.
#[test]
fn moving_on_a_higher_timeframe_keeps_minutes() {
    use lines_core::{BarSeries, Derived, Timeframe};
    let mut series = BarSeries::new(M, 1_000);
    for b in bars() {
        series.upsert(b);
    }
    let mut fifteen = Derived::new(Timeframe::Minutes(15), series.capacity());
    fifteen.update(&series, None);
    let axis = TimeAxis::new(fifteen.view());
    let t0 = 1_800_000_000_000; // a 15-minute boundary
    let at = |min: i64, p: i64| Anchor {
        time: UtcMillis(t0 + min * M),
        price: Price(p),
    };
    let original = Drawing::new(Tool::Trend, at(8, 600_000), at(22, 600_100), Style::DEFAULT);
    // Pressed on bar 0 (its start), released on bar 1, 2 ticks higher.
    let drag = Drag {
        hit: Hit::Body,
        original: original.clone(),
        from: at(0, 600_000),
        press: (0.0, 0.0),
    };
    let moved = dragged(&drag, at(15, 600_050), &axis, 25);
    assert_eq!(moved.a, at(23, 600_050));
    assert_eq!(moved.b, at(37, 600_150));
    // Not moved in time: the anchors keep their exact minutes.
    let still = dragged(&drag, at(0, 600_000), &axis, 25);
    assert_eq!((still.a, still.b), (original.a, original.b));
}

/// A click on a selected drawing's handle (no drag) selects; it never moves the anchor.
#[test]
fn a_click_on_a_handle_moves_nothing() {
    let mut r = Rig::new();
    r.d.set_tool(Tool::Trend);
    r.down(100.0, 100.0);
    r.down(300.0, 200.0);
    let before = r.items()[0].clone();
    r.frame(0.0, 0.0);
    let (hx, hy) = r.d.drawn()[0].handles[0];
    assert!(r.down(hx + 3.0, hy + 3.0));
    assert!(r.up(hx + 3.0, hy + 3.0));
    assert_eq!(r.items()[0], before);
    assert!(!r.d.data().lock().can_redo());
}

/// Choosing a tool clears the temporary measure, so a new measure's preview shows.
#[test]
fn a_new_tool_clears_the_temporary_measure() {
    let mut r = Rig::new();
    r.d.set_tool(Tool::Measure);
    r.down(200.0, 300.0);
    r.down(400.0, 100.0);
    r.frame(0.0, 0.0);
    let old = r.d.measure_shown().unwrap().0;
    r.d.set_tool(Tool::Measure);
    r.down(100.0, 50.0);
    r.frame(150.0, 80.0);
    let shown = r.d.measure_shown().unwrap().0;
    assert_ne!(
        shown.p0, old.p0,
        "the new measure's preview, not the old one"
    );
}

/// Hiding drawings ends everything in progress: a hidden drawing can't stay selected and be
/// deleted unseen. A lost release drops a drag without moving the drawing.
#[test]
fn hiding_clears_the_selection_and_a_lost_release_drops_the_drag() {
    let mut r = Rig::new();
    r.d.set_tool(Tool::HLine);
    r.down(100.0, 100.0);
    r.d.set_tool(Tool::Measure);
    r.down(200.0, 300.0);
    r.down(400.0, 150.0);
    r.frame(0.0, 0.0);
    let y = r.d.drawn()[0].p0.1;
    assert!(
        !r.down(500.0, y),
        "selects the line (the chart pans); the measure stays"
    );
    assert!(r.d.selected.is_some());
    r.d.set_hidden(true);
    assert_eq!(r.d.selected, None);
    assert_eq!(r.d.key("delete", None), Key::Ignored);
    assert_eq!(r.items().len(), 1, "nothing deleted while hidden");
    r.d.set_hidden(false);

    let before = r.items()[0].clone();
    r.frame(0.0, 0.0);
    r.down(500.0, y); // select
    assert!(r.down(500.0, y));
    r.frame(500.0, y + 60.0);
    assert!(r.d.release_lost());
    assert!(!r.up(500.0, y + 60.0), "nothing left to release");
    assert_eq!(r.items()[0], before);
}

/// Horizontal and vertical drawing lines sit on whole device pixels at DPI 1 and 2, so they
/// never blur into two faint rows.
#[test]
fn axis_aligned_lines_snap_to_device_pixels() {
    for (hair, width) in [(1.0, 1.0), (1.0, 2.0), (0.5, 1.0), (0.5, 2.0)] {
        for at in [10.0, 10.3, 10.5, 10.75, 211.49] {
            let (w, start) = crisp_band(at, width, hair);
            let dev = |v: f32| (v / hair - (v / hair).round()).abs() < 1e-4;
            assert!(dev(w) && dev(start), "hair {hair} width {width} at {at}");
            assert!((w - width).abs() < 1e-4);
            assert!(
                (start + w * 0.5 - at).abs() <= hair * 0.5 + 1e-4,
                "centred on {at}"
            );
        }
    }
}

/// Inside a zone the chart pans: only its edges select it, and only a selected zone drags.
#[test]
fn the_chart_pans_inside_a_zone() {
    let mut r = Rig::new();
    r.d.set_tool(Tool::Rect);
    r.down(100.0, 100.0);
    r.down(500.0, 300.0);
    r.d.selected = None;
    r.frame(0.0, 0.0);
    assert!(!r.down(300.0, 200.0), "inside: the chart's");
    assert_eq!(r.d.selected, None);
    let (x0, _) = r.d.drawn()[0].p0;
    assert!(
        !r.down(x0, 200.0),
        "edge of an unselected zone: selects only"
    );
    assert!(r.d.selected.is_some());
    assert!(r.down(x0, 200.0), "edge of the selected zone: drags");
}

/// Double click opens the editor on a drawing, never with Ctrl/Cmd or in drawing mode; right
/// click selects what is under it and clears an older selection on empty chart.
#[test]
fn double_and_right_clicks_route_to_the_drawing_under_them() {
    let mut r = Rig::new();
    r.d.set_tool(Tool::HLine);
    r.down(100.0, 100.0);
    r.frame(400.0, 100.0);
    let id = r.d.hit_at(400.0, 100.0).unwrap();
    assert_eq!(r.d.double_click(400.0, 100.0, false), Some(id));
    assert_eq!(r.d.double_click(400.0, 100.0, true), None, "Ctrl/Cmd");
    assert_eq!(r.d.double_click(400.0, 300.0, false), None, "empty chart");
    r.d.set_tool(Tool::Trend);
    assert_eq!(r.d.double_click(400.0, 100.0, false), None, "drawing mode");
    r.d.set_tool(Tool::Trend); // off again
    r.d.selected = None;
    assert!(r.d.right_click(400.0, 100.0));
    assert!(r.d.selection_shown());
    assert!(!r.d.right_click(400.0, 300.0));
    assert_eq!(r.d.selected, None, "a right click on empty chart clears it");
    // A selection scrolled off screen isn't offered.
    r.d.selected = Some(id);
    r.vp.price_lo = 610_000.0;
    r.vp.price_hi = 612_000.0;
    r.frame(0.0, 0.0);
    assert!(!r.d.selection_shown());
}

/// An off-screen tag (a zone edge above the plot) takes no slot.
#[test]
fn axis_tags_off_the_plot_are_left_out() {
    let mut r = Rig::new();
    r.d.set_tool(Tool::Rect);
    r.down(200.0, 150.0);
    r.down(300.0, 250.0);
    let mut d = r.items()[0].clone();
    d.style.axis_tag = true;
    r.d.data().change(|s| (s.edit(d), ()));
    r.frame(0.0, 0.0);
    assert_eq!(r.d.axis_tags(400.0).count(), 2);
    assert_eq!(
        r.d.axis_tags(200.0).count(),
        1,
        "the bottom edge is below a 200 px plot"
    );
    let id = r.items()[0].id;
    let (x0, y0, x1, y1) = r.d.extent(id).unwrap();
    assert!(x0 <= 200.0 && x1 >= 300.0 && y0 <= 150.0 && y1 >= 250.0);
}

/// "Drawing style…" with nothing selected on screen says why.
#[test]
fn the_style_command_says_why_it_cant_open() {
    let mut r = Rig::new();
    assert_eq!(
        r.d.style_unavailable(),
        "No drawings on this chart to style"
    );
    r.d.set_tool(Tool::HLine);
    r.down(100.0, 100.0);
    r.d.selected = None;
    assert!(
        r.d.style_unavailable()
            .starts_with("Select a drawing first")
    );
    r.d.set_hidden(true);
    assert!(r.d.style_unavailable().starts_with("Drawings are hidden"));
}

impl Rig {
    fn shift_down(&mut self, x: f32, y: f32) -> bool {
        self.frame(x, y);
        let a = self.anchor(x, y);
        self.d.quick_down(x, y, a)
    }
}

/// Shift+drag measures from press to release with no tool chosen; the measure stays (temporary,
/// as with Alt+U) until Esc or a click elsewhere, and Enter still pins it.
#[test]
fn shift_drag_measures_from_press_to_release() {
    let mut r = Rig::new();
    assert!(r.shift_down(200.0, 300.0));
    r.frame(300.0, 200.0);
    assert!(r.d.measure_shown().is_some(), "previewed while dragging");
    assert!(r.up(400.0, 100.0));
    assert_eq!(r.d.tool(), None, "no tool left chosen");
    r.frame(0.0, 0.0);
    let (_, stats) = r.d.measure_shown().expect("kept after the release");
    assert!(stats.ticks > 0 && stats.bars > 0);
    assert!(r.items().is_empty(), "temporary, not saved");
    // Esc clears it.
    assert_eq!(r.d.key("escape", None), Key::Handled);
    r.frame(0.0, 0.0);
    assert!(r.d.measure_shown().is_none());
    // Again, then a plain click elsewhere clears it and pans.
    r.shift_down(200.0, 300.0);
    r.up(400.0, 100.0);
    assert!(!r.down(600.0, 200.0), "the chart pans");
    r.frame(0.0, 0.0);
    assert!(r.d.measure_shown().is_none());
    // Enter pins one.
    r.shift_down(200.0, 300.0);
    r.up(400.0, 100.0);
    assert_eq!(r.d.key("enter", None), Key::Handled);
    assert_eq!(r.items()[0].tool, Tool::Measure);
}

/// A Shift+click leaves nothing behind; no bars to anchor to, or a tool chosen: the press isn't
/// the ruler's.
#[test]
fn a_shift_click_measures_nothing() {
    let mut r = Rig::new();
    assert!(r.shift_down(200.0, 300.0));
    assert!(r.up(201.0, 301.0));
    r.frame(0.0, 0.0);
    assert!(r.d.measure_shown().is_none());
    assert_eq!(r.d.tool(), None);
    assert!(
        !r.d.quick_down(10.0, 10.0, None),
        "no bars: nothing to anchor"
    );
    r.d.set_tool(Tool::Trend);
    assert!(!r.shift_down(200.0, 300.0), "drawing mode keeps its press");
    assert_eq!(r.d.tool(), Some(Tool::Trend));
}

/// A release off the plot ends where the pointer last was on it; a lost release ends there too.
#[test]
fn a_shift_drag_released_off_the_plot_or_lost_still_measures() {
    let mut r = Rig::new();
    r.shift_down(200.0, 300.0);
    r.frame(400.0, 100.0);
    assert!(r.d.on_up(900.0, 100.0, None));
    r.frame(0.0, 0.0);
    assert!(r.d.measure_shown().is_some());
    r.d.cancel();
    r.shift_down(200.0, 300.0);
    r.frame(400.0, 100.0);
    assert!(r.d.release_lost());
    assert_eq!(r.d.tool(), None);
    r.frame(0.0, 0.0);
    assert!(r.d.measure_shown().is_some());
    // Esc mid-drag ends it with nothing.
    r.d.cancel();
    r.shift_down(200.0, 300.0);
    assert!(r.d.cancel());
    r.up(400.0, 100.0);
    r.frame(0.0, 0.0);
    assert!(r.d.measure_shown().is_none());
}

/// What keeps its own press over Shift: Ctrl/Cmd or Alt, a chosen tool, an order's tag,
/// marker or handle (or the position tag), a drawing or its handles.
#[test]
fn shift_measures_only_on_empty_plot_with_shift_alone() {
    assert!(shift_measures(true, false, false, false, false));
    assert!(
        !shift_measures(false, false, false, false, false),
        "plain drag pans"
    );
    assert!(
        !shift_measures(true, true, false, false, false),
        "Ctrl, Cmd or Alt"
    );
    assert!(
        !shift_measures(true, false, true, false, false),
        "drawing mode"
    );
    assert!(
        !shift_measures(true, false, false, true, false),
        "an order line"
    );
    assert!(
        !shift_measures(true, false, false, false, true),
        "a drawing"
    );
}

/// Anything that ends a press mid-drag leaves no tool chosen and no stray measure.
#[test]
fn a_shift_drag_interrupted_leaves_nothing_armed() {
    let es = catalog::get(catalog::ES).unwrap();
    // Another tool chosen mid-drag: that tool, and the release does nothing.
    let mut r = Rig::new();
    r.shift_down(200.0, 300.0);
    r.d.set_tool(Tool::Trend);
    assert_eq!(r.d.tool(), Some(Tool::Trend));
    r.up(400.0, 100.0);
    assert!(r.items().is_empty());
    // Hidden mid-drag: the release ends it, nothing left armed.
    let mut r = Rig::new();
    r.shift_down(200.0, 300.0);
    r.d.set_hidden(true);
    r.up(400.0, 100.0);
    assert_eq!(r.d.tool(), None);
    // Another product mid-drag: everything ends.
    let mut r = Rig::new();
    r.shift_down(200.0, 300.0);
    r.d.set_data(ProductDrawings::ephemeral(es, Vec::new()));
    r.up(400.0, 100.0);
    assert_eq!(r.d.tool(), None);
    r.frame(0.0, 0.0);
    assert!(r.d.measure_shown().is_none());
}

/// A pending "Delete all" doesn't take the Enter that pins a Shift+drag's measure.
#[test]
fn enter_after_a_shift_drag_pins_and_never_deletes_all() {
    let mut r = Rig::new();
    r.d.set_tool(Tool::HLine);
    r.down(100.0, 100.0);
    assert_eq!(r.items().len(), 1);
    r.d.confirm_clear = true;
    r.shift_down(200.0, 300.0);
    r.up(400.0, 100.0);
    assert_eq!(r.d.key("enter", None), Key::Handled);
    assert_eq!(r.items().len(), 2, "pinned; nothing deleted");
}

impl Rig {
    fn measure_down(&mut self, x: f32, y: f32) -> bool {
        self.frame(x, y);
        let a = self.anchor(x, y);
        self.d.measure_down(x, y, a)
    }

    fn measure(&self) -> (Anchor, Anchor) {
        self.d.measure.expect("a measure")
    }
}

/// The grip on a measure's box: inside moves, an edge resizes by its anchor, a corner by both.
#[test]
fn a_measure_box_has_move_edge_and_corner_grips() {
    let (p0, p1) = ((100.0, 300.0), (300.0, 100.0));
    assert_eq!(grip(p0, p1, 200.0, 200.0, 5.0), Some(Grip::Move));
    assert_eq!(grip(p0, p1, 400.0, 200.0, 5.0), None, "outside");
    assert_eq!(
        grip(p0, p1, 101.0, 200.0, 5.0),
        Some(Grip::Resize {
            time: Some(0),
            price: None
        }),
        "a's vertical edge"
    );
    assert_eq!(
        grip(p0, p1, 200.0, 99.0, 5.0),
        Some(Grip::Resize {
            time: None,
            price: Some(1)
        }),
        "b's horizontal edge"
    );
    assert_eq!(
        grip(p0, p1, 299.0, 102.0, 5.0),
        Some(Grip::Resize {
            time: Some(1),
            price: Some(1)
        }),
        "b's corner"
    );
    // Too small to tell edges apart: it moves.
    assert_eq!(
        grip((10.0, 10.0), (14.0, 12.0), 12.0, 11.0, 5.0),
        Some(Grip::Move)
    );
    use gpui::CursorStyle as C;
    assert_eq!(Grip::Move.cursor(p0, p1), C::OpenHand);
    let corner = Grip::Resize {
        time: Some(1),
        price: Some(1),
    };
    assert_eq!(corner.cursor(p0, p1), C::ResizeUpRightDownLeft);
    let edge = Grip::Resize {
        time: Some(0),
        price: None,
    };
    assert_eq!(edge.cursor(p0, p1), C::ResizeLeftRight);
}

/// Dragging inside a placed measure moves it by whole bars and ticks, its size in bars and
/// ticks kept, and the readout follows; a click on it leaves it where it was.
#[test]
fn a_placed_measure_moves_keeping_its_size() {
    let mut r = Rig::new();
    r.shift_down(200.0, 300.0);
    r.up(400.0, 100.0);
    r.frame(0.0, 0.0);
    let (a, b) = r.measure();
    let (_, before) = r.d.measure_shown().unwrap();
    assert!(
        r.measure_down(300.0, 200.0),
        "inside: it's the measure's press"
    );
    r.frame(360.0, 150.0);
    let (_, during) = r.d.measure_shown().unwrap();
    assert_eq!(
        (during.ticks, during.bars),
        (before.ticks, before.bars),
        "live, same size"
    );
    assert!(r.up(360.0, 150.0));
    let (a2, b2) = r.measure();
    assert_ne!(a2, a, "moved");
    assert_eq!(b2.price.0 - a2.price.0, b.price.0 - a.price.0, "ticks kept");
    r.frame(0.0, 0.0);
    let (_, after) = r.d.measure_shown().unwrap();
    assert_eq!((after.ticks, after.bars), (before.ticks, before.bars));
    // A click on it: nothing moves.
    assert!(r.measure_down(300.0, 200.0));
    assert!(r.up(301.0, 201.0));
    assert_eq!(r.measure(), (a2, b2));
    // A plain click elsewhere still clears it (the chart pans).
    r.frame(0.0, 0.0);
    assert!(!r.measure_down(700.0, 380.0));
    assert!(!r.down(700.0, 380.0));
    r.frame(0.0, 0.0);
    assert!(r.d.measure_shown().is_none());
}

/// An edge or corner resizes: only that anchor's time or price follows the pointer.
#[test]
fn a_placed_measure_resizes_by_an_edge_or_corner() {
    let mut r = Rig::new();
    r.shift_down(200.0, 300.0);
    r.up(400.0, 100.0);
    r.frame(0.0, 0.0);
    let (a, b) = r.measure();
    let (p, _) = r.d.measure_shown().unwrap();
    // b's horizontal edge, dragged up: only b's price changes.
    let (ex, ey) = ((p.p0.0 + p.p1.0) / 2.0, p.p1.1);
    assert!(r.measure_down(ex, ey));
    r.frame(ex, ey - 40.0);
    r.up(ex, ey - 40.0);
    let (a2, b2) = r.measure();
    assert_eq!((a2, b2.time), (a, b.time));
    assert!(b2.price.0 > b.price.0);
    // Esc mid-drag puts it back; Esc again clears it.
    let (p, _) = {
        r.frame(0.0, 0.0);
        r.d.measure_shown().unwrap()
    };
    assert!(r.measure_down(p.p0.0, p.p0.1));
    r.frame(p.p0.0 - 60.0, p.p0.1);
    assert!(r.d.cancel());
    assert_eq!(r.measure(), (a2, b2));
    assert!(r.d.cancel());
    assert!(r.d.measure.is_none());
}

/// A tool key or Enter mid-drag ends the drag: the measure never comes back under a tool, and
/// Enter pins it once (no temporary copy left over it).
#[test]
fn a_tool_or_a_pin_mid_drag_ends_the_measure_drag() {
    let mut r = Rig::new();
    r.shift_down(200.0, 300.0);
    r.up(400.0, 100.0);
    r.frame(0.0, 0.0);
    assert!(r.measure_down(300.0, 200.0));
    r.frame(330.0, 180.0);
    assert_eq!(r.d.key("enter", None), Key::Handled);
    r.frame(360.0, 150.0);
    r.up(360.0, 150.0);
    assert_eq!(r.items().len(), 1, "pinned once");
    assert!(r.d.measure.is_none(), "no temporary copy");
    r.shift_down(200.0, 300.0);
    r.up(400.0, 100.0);
    r.frame(0.0, 0.0);
    assert!(r.measure_down(300.0, 200.0));
    r.d.set_tool(Tool::Trend);
    r.frame(360.0, 150.0);
    assert!(!r.d.measure_dragging());
}

/// Resizing never collapses the box: an edge stops short of the opposite one.
#[test]
fn a_resize_never_collapses_the_box() {
    let mut r = Rig::new();
    r.shift_down(200.0, 300.0);
    r.up(400.0, 100.0);
    r.frame(0.0, 0.0);
    let (p, _) = r.d.measure_shown().unwrap();
    let (a, b) = r.measure();
    // b's vertical edge dragged onto a's.
    let ex = p.p1.0;
    let ey = (p.p0.1 + p.p1.1) / 2.0;
    assert!(r.measure_down(ex, ey));
    r.frame(p.p0.0, ey);
    r.up(p.p0.0, ey);
    let (a2, b2) = r.measure();
    assert_ne!(a2.time, b2.time, "still has width");
    assert_eq!((a2, b2.price), (a, b.price));
}

/// A tool chosen or hidden drawings: a press on the box isn't the measure's.
#[test]
fn the_measure_takes_no_press_in_drawing_mode() {
    let mut r = Rig::new();
    r.shift_down(200.0, 300.0);
    r.up(400.0, 100.0);
    r.frame(0.0, 0.0);
    r.d.set_tool(Tool::Trend);
    assert!(!r.measure_down(300.0, 200.0));
}

#[test]
fn measure_parts_toggle_and_save_by_name() {
    let mut s = MeasureShow::default();
    assert!(MeasurePart::ALL.iter().all(|p| s.get(*p)));
    s.toggle(MeasurePart::Time);
    assert!(!s.get(MeasurePart::Time) && !s.is_default());
    let json = serde_json::to_string(&s).unwrap();
    let back: MeasureShow = serde_json::from_str(&json).unwrap();
    assert_eq!(back, s);
    let partial: MeasureShow = serde_json::from_str(r#"{"bars": false}"#).unwrap();
    assert!(!partial.bars && partial.ticks, "missing parts stay on");
}
