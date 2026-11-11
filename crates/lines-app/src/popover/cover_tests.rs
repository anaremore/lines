//! The cover rule on a real GPUI window (owner, 2026-11-10): an open list covers what is under
//! it, Flatten and Cancel all included, and with a popover open anywhere the first press on
//! either only closes it. GPUI's test platform (`screenshot` builds it): run with
//! `cargo test -p lines-app --features screenshot --lib popover::cover_tests`.
//!
//! The panel here is the trading panel's shape: a field whose list (`dropdown::dropdown`, with
//! its backdrop) opens under it over Flatten and Cancel all, which guard their press as the real
//! ones do (`popover::press_closes_popover`, then act).

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    Context, InteractiveElement, IntoElement, Modifiers, MouseButton, ParentElement, Render,
    Styled, TestAppContext, Window, div, point, px,
};

#[derive(Clone, Default)]
struct Hits {
    flatten: Rc<Cell<u32>>,
    cancel_all: Rc<Cell<u32>>,
    row: Rc<Cell<u32>>,
    dismissed: Rc<Cell<u32>>,
    closed: Rc<Cell<u32>>,
}

struct Panel {
    hits: Hits,
    list_open: bool,
}

impl Panel {
    fn close_list(&mut self, cx: &mut Context<Self>) {
        self.list_open = false;
        cx.notify();
    }

    /// A popover's close as `popover::opened` keeps it (a studies menu, in another window).
    fn close_menu(&mut self, cx: &mut Context<Self>) {
        self.hits.closed.set(self.hits.closed.get() + 1);
        cx.notify();
    }
}

/// An order control at (x, y), 200 × 24, guarded as the real ones are.
fn order_button(
    id: &'static str,
    (x, y): (f32, f32),
    hit: Rc<Cell<u32>>,
    cx: &mut Context<Panel>,
) -> impl IntoElement {
    div()
        .id(id)
        .absolute()
        .left(px(x))
        .top(px(y))
        .w(px(200.0))
        .h(px(24.0))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |_, _, _, cx| {
                cx.stop_propagation();
                if crate::popover::press_closes_popover(cx) {
                    return;
                }
                hit.set(hit.get() + 1);
            }),
        )
}

impl Render for Panel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let row = self.hits.row.clone();
        // The list: 100 wide (the field's width), 100 tall, under the field at y 32.
        let list = self.list_open.then(|| {
            div()
                .w(px(100.0))
                .child(div().id("list-row").w_full().h(px(100.0)).on_mouse_down(
                    MouseButton::Left,
                    move |_, _, cx| {
                        cx.stop_propagation();
                        row.set(row.get() + 1);
                    },
                ))
                .into_any_element()
        });
        let dismissed = self.hits.dismissed.clone();
        let me = cx.weak_entity();
        let field = crate::dropdown::dropdown(div().w(px(100.0)).h(px(20.0)), list).on_dismiss(
            Box::new(move |_, _, cx| {
                dismissed.set(dismissed.get() + 1);
                let _ = me.update(cx, Panel::close_list);
            }),
        );
        div()
            .size_full()
            .relative()
            .child(order_button(
                "flatten",
                (10.0, 40.0),
                self.hits.flatten.clone(),
                cx,
            ))
            .child(order_button(
                "cancel-all",
                (10.0, 80.0),
                self.hits.cancel_all.clone(),
                cx,
            ))
            .child(div().absolute().left(px(10.0)).top(px(10.0)).child(field))
    }
}

fn press(cx: &mut gpui::VisualTestContext, x: f32, y: f32) {
    cx.simulate_click(point(px(x), px(y)), Modifiers::none());
    cx.run_until_parked();
}

/// The owner's case: the panel's list open over Flatten and Cancel all. A press where the list
/// covers them hits the list; a press on their visible part beside it hits the backdrop, which
/// only closes the list. Neither acts until the list is closed; then one press acts.
#[gpui::test]
fn an_open_list_covers_flatten_and_cancel_all(cx: &mut TestAppContext) {
    let hits = Hits::default();
    let h = hits.clone();
    let (_view, cx) = cx.add_window_view(move |_, _| Panel {
        hits: h,
        list_open: true,
    });
    cx.run_until_parked();
    // Over the list, on Flatten's and Cancel all's rows: the list's row takes each press.
    press(cx, 50.0, 50.0);
    press(cx, 50.0, 90.0);
    assert_eq!(hits.row.get(), 2, "the list took both presses");
    assert_eq!((hits.flatten.get(), hits.cancel_all.get()), (0, 0));
    // Flatten's visible part beside the list: the backdrop takes it and the list closes.
    press(cx, 160.0, 50.0);
    assert_eq!(hits.dismissed.get(), 1);
    assert_eq!(
        hits.flatten.get(),
        0,
        "the press that closed the list did nothing else"
    );
    // Closed: Flatten is one press again.
    press(cx, 160.0, 50.0);
    assert_eq!(hits.flatten.get(), 1);
    // Cancel all likewise: reopened, its visible part only closes the list.
    _view.update(cx, |v, cx| {
        v.list_open = true;
        cx.notify();
    });
    cx.run_until_parked();
    press(cx, 160.0, 90.0);
    assert_eq!((hits.dismissed.get(), hits.cancel_all.get()), (2, 0));
    press(cx, 160.0, 90.0);
    assert_eq!(hits.cancel_all.get(), 1);
}

/// A popover open where no backdrop covers the buttons (a studies menu in another window): a
/// press on Flatten or Cancel all only closes it; the next press acts.
#[gpui::test]
fn a_press_with_a_popover_open_elsewhere_only_closes_it(cx: &mut TestAppContext) {
    let hits = Hits::default();
    let h = hits.clone();
    let (view, cx) = cx.add_window_view(move |_, _| Panel {
        hits: h,
        list_open: false,
    });
    cx.run_until_parked();
    for (y, hit) in [(50.0, &hits.flatten), (90.0, &hits.cancel_all)] {
        view.update(cx, |_, cx| {
            crate::popover::opened(cx, crate::popover::Kind::Studies, Panel::close_menu)
        });
        let closed = hits.closed.get();
        press(cx, 160.0, y);
        assert_eq!(hits.closed.get(), closed + 1, "the menu closed");
        assert_eq!(hit.get(), 0, "and nothing else happened");
        press(cx, 160.0, y);
        assert_eq!(hit.get(), 1, "the next press acts");
    }
}
