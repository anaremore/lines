//! The symbol picker's UI, shared by every pane (roadmap 4b; the logic is `crate::picker`).
//!
//! A pane holds a [`PickerUi`], routes keys to it while it is open and renders its overlay.
//! Recents and favourites are app-wide ([`AppSettings`], a GPUI global), so every pane ranks
//! the same way and saves go through one background writer.
//!
//! While closed the picker costs nothing per frame.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{
    AnyElement, App, Context, FontFeatures, FontWeight, Global, InteractiveElement, IntoElement,
    KeyDownEvent, MouseButton, MouseDownEvent, ParentElement, Styled, div, prelude::*, px,
};
use lines_core::{CivilDate, ProductId};
use lines_feed::availability::Availability;
use lines_feed::catalog::{self, Target};

use crate::persist::Writer;
use crate::picker::{self, Picker, Settings};
use crate::theme::Theme;

/// Lines shown at once: result rows and group headings.
const PICKER_ROWS: usize = 10;
/// Height of one line (a row or a heading), so the list's height doesn't depend on how many
/// headings are in view.
const LINE_H: f32 = 28.0;
/// The symbol column: one line wide enough for the longest catalog symbol (`BTC-PERP.KR`,
/// 11 characters, about 84 px in 13 px semibold), the row's 8 px gap after it. A longer one
/// ends in "…" rather than wrapping.
const SYMBOL_W: f32 = 92.0;
const PICKER_W: f32 = 600.0;

/// Recents and favourites for the whole app.
pub struct AppSettings {
    pub settings: Settings,
    /// `None` (screenshots, tests) or not writable (a file we couldn't back up): never saved.
    path: Option<PathBuf>,
    writer: Option<Writer>,
}

impl Global for AppSettings {}

impl AppSettings {
    /// Load from `path` (reporting problems through `report`), saving through `writer`.
    pub fn load(path: Option<PathBuf>, writer: Option<Writer>) -> (AppSettings, Option<String>) {
        let Some(p) = path else {
            return (AppSettings::ephemeral(), None);
        };
        let (settings, report, writable) = Settings::load(&p);
        (
            AppSettings {
                settings,
                path: writable.then_some(p),
                writer,
            },
            report,
        )
    }

    /// Defaults that are never saved (screenshot mode: no user files are read or written).
    pub fn ephemeral() -> AppSettings {
        AppSettings {
            settings: Settings::default(),
            path: None,
            writer: None,
        }
    }

    /// Change the P&L display for the whole app (saved; every pane repaints).
    pub fn set_pnl_display(&mut self, m: crate::pnl::PnlDisplay) {
        if self.settings.pnl_display != m {
            self.settings.pnl_display = m;
            self.save();
            crate::pnl::display_changed();
            crate::journal_shots::set_pnl(m);
        }
    }

    fn save(&self) {
        if let (Some(p), Some(w)) = (&self.path, &self.writer) {
            w.write(p.clone(), self.settings.encode());
        }
    }
}

pub(crate) fn settings(cx: &App) -> &Settings {
    static EMPTY: std::sync::OnceLock<Settings> = std::sync::OnceLock::new();
    cx.try_global::<AppSettings>()
        .map(|s| &s.settings)
        .unwrap_or_else(|| EMPTY.get_or_init(Settings::default))
}

pub(crate) fn update_settings(cx: &mut App, f: impl FnOnce(&mut Settings)) {
    if cx.has_global::<AppSettings>() {
        let g = cx.global_mut::<AppSettings>();
        f(&mut g.settings);
        g.save();
    }
}

/// What a key press in the open picker did.
pub enum PickerKey {
    /// Handled; redraw.
    Handled,
    /// Enter (or a row click): switch to this target.
    Confirm(Target, ProductId),
    /// Not a picker key.
    Ignored,
}

pub struct PickerUi {
    entries: Vec<picker::Entry>,
    picker: Option<Picker>,
    /// Shown inline (e.g. an unavailable row, a switch that failed to start).
    pub error: Option<String>,
    /// Tabular digits for dates and day counts.
    tnum: FontFeatures,
}

impl PickerUi {
    pub fn new(open_on: Option<&str>, cx: &App) -> PickerUi {
        let entries = picker::entries(catalog::all());
        let picker = open_on.map(|q| Picker::open(q, &entries, settings(cx)));
        PickerUi {
            entries,
            picker,
            error: None,
            tnum: FontFeatures(Arc::new(vec![("tnum".into(), 1)])),
        }
    }

    pub fn is_open(&self) -> bool {
        self.picker.is_some()
    }

    pub fn open(&mut self, query: &str, cx: &App) {
        self.error = None;
        self.picker = Some(Picker::open(query, &self.entries, settings(cx)));
    }

    pub fn close(&mut self) -> bool {
        self.error = None;
        let closed = self.picker.take().is_some();
        if closed {
            crate::popover::mark_moved_under_pointer();
        }
        closed
    }

    /// Route a key to the open picker.
    pub fn key(
        &mut self,
        ev: &KeyDownEvent,
        today: CivilDate,
        availability: &Availability,
        cx: &mut App,
    ) -> PickerKey {
        let k = &ev.keystroke;
        let m = k.modifiers;
        let Some(p) = self.picker.as_mut() else {
            return PickerKey::Ignored;
        };
        self.error = None;
        let entries = &self.entries;
        match k.key.as_str() {
            "escape" => self.picker = None,
            "enter" => return self.confirm(today, availability),
            "up" => p.move_selection(-1),
            "down" => p.move_selection(1),
            "tab" => p.cycle_contract(if m.shift { -1 } else { 1 }, entries, today),
            "backspace" => p.backspace(entries, settings(cx)),
            "m" if m.alt => p.toggle_size(entries, settings(cx)),
            "d" if m.control => {
                if let Some(id) = p.selected_entry(entries).map(|e| e.product.id) {
                    update_settings(cx, |s| {
                        s.toggle_favourite(id);
                    });
                    if let Some(p) = self.picker.as_mut() {
                        p.refresh(&self.entries, settings(cx));
                    }
                }
            }
            _ => {
                let Some(text) = k.key_char.as_deref().filter(|t| {
                    !m.control && !m.alt && !m.platform && t.chars().all(|c| !c.is_control())
                }) else {
                    return PickerKey::Ignored;
                };
                p.push_str(text, entries, settings(cx));
            }
        }
        PickerKey::Handled
    }

    /// Enter on the selected row. Unavailable rows stay open and say why.
    pub fn confirm(&mut self, today: CivilDate, availability: &Availability) -> PickerKey {
        let Some(row) = self
            .picker
            .as_ref()
            .and_then(|p| p.selected_row(&self.entries, today))
        else {
            return PickerKey::Handled;
        };
        if let Err(reason) = availability.check(&row.target) {
            self.error = Some(reason);
            return PickerKey::Handled;
        }
        PickerKey::Confirm(row.target, row.contract.product)
    }

    /// A row was clicked: select it and confirm.
    pub fn click(
        &mut self,
        row: usize,
        today: CivilDate,
        availability: &Availability,
    ) -> PickerKey {
        if let Some(p) = self.picker.as_mut()
            && p.selected != row
        {
            p.selected = row;
            p.offset = 0;
        }
        crate::popover::mark_moved_under_pointer();
        self.confirm(today, availability)
    }

    /// The pane switched successfully: close and remember the product.
    pub fn switched(&mut self, product: ProductId, cx: &mut App) {
        self.picker = None;
        self.error = None;
        update_settings(cx, |s| s.push_recent(product));
    }

    /// The open picker: query line, ranked rows, key hints. Built only while open. Row clicks
    /// call `on_row(view, row, cx)`.
    pub fn overlay<V: 'static>(
        &self,
        theme: &Theme,
        availability: &Availability,
        today: CivilDate,
        on_row: fn(&mut V, usize, &mut Context<V>),
        cx: &mut Context<V>,
    ) -> Option<AnyElement> {
        let p = self.picker.as_ref()?;
        let t = *theme;
        let settings = settings(cx).clone();
        // Headings count as lines, and every line is one height, so the list keeps its height
        // as the selection moves (docs/design.md §7). In a short pane the list gives way first
        // (clipped), never the query or the hints line.
        let mut rows = div()
            .flex()
            .flex_col()
            .py_1()
            .flex_shrink(1.0)
            .min_h_0()
            .overflow_hidden()
            // A full list reserves all its lines, so a window shown one short (it won't end
            // on a heading with no row) keeps the same height.
            .when(p.hits.len() >= PICKER_ROWS, |d| {
                d.h(px(PICKER_ROWS as f32 * LINE_H + 8.0))
            });
        let lines =
            picker::visible_lines(&self.entries, &p.hits, &p.query, p.selected, PICKER_ROWS);
        for line in lines {
            let i = match line {
                picker::Line::Heading(label) => {
                    rows = rows.child(
                        div()
                            .flex_none()
                            .h(px(LINE_H))
                            .flex()
                            .items_end()
                            .px_3()
                            .pb_1()
                            .text_size(px(11.0))
                            .text_color(t.text_dim)
                            .child(label),
                    );
                    continue;
                }
                picker::Line::Row(i) => i,
            };
            let hit = &p.hits[i];
            let e = &self.entries[hit.index];
            let selected = i == p.selected;
            let row = picker::contract_row(&e.product, today, if selected { p.offset } else { 0 });
            let unavailable = match &row {
                Some(r) => availability.check(&r.target).err(),
                None => Some("no listed contract".to_owned()),
            };
            let fg = if unavailable.is_some() {
                t.text_dim
            } else {
                t.text_main
            };
            let mut right = div()
                .flex_none()
                .whitespace_nowrap()
                .flex()
                .flex_row()
                .gap_2()
                .text_color(t.text_dim);
            match (&unavailable, &row) {
                (Some(reason), _) => right = right.child(reason.clone()),
                (None, Some(r)) => {
                    right = right.child(div().text_color(fg).child(r.label.as_str().to_owned()));
                    if r.is_front && !e.product.is_spot() && !e.product.is_equity() {
                        right = right.child("front");
                    }
                    if let Some(x) = r.expiry {
                        right = right.child(format!("exp {x}"));
                    }
                    if let Some(d) = r.days_to_roll {
                        let roll = div().child(if r.roll_soon {
                            format!("roll soon · {d}d")
                        } else {
                            format!("roll {d}d")
                        });
                        right = right.child(if r.roll_soon {
                            roll.text_color(t.text_main).font_weight(FontWeight::BOLD)
                        } else {
                            roll
                        });
                    }
                }
                (None, None) => {}
            }
            let star = if settings.is_favourite(e.product.id) {
                "★"
            } else {
                ""
            };
            rows = rows.child(
                div()
                    .id(("picker-row", i))
                    .flex_none()
                    .h(px(LINE_H))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .border_l_2()
                    .border_color(if selected {
                        t.accent
                    } else {
                        gpui::transparent_black()
                    })
                    .when(selected, |d| d.bg(t.accent.opacity(0.16)))
                    .child(div().w(px(12.0)).text_color(t.text_dim).child(star))
                    .child(
                        div()
                            .flex_none()
                            .w(px(SYMBOL_W))
                            .truncate()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(fg)
                            .child(e.product.symbol),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(t.text_dim)
                            .child(format!("{}  ·  {}", e.product.description, e.exchange)),
                    )
                    .child(right)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |view, _: &MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            on_row(view, i, cx);
                            cx.notify();
                        }),
                    ),
            );
        }
        if p.hits.is_empty() {
            rows = rows.child(
                div()
                    .px_3()
                    .py_1()
                    .text_color(t.text_dim)
                    .child("no matching symbols"),
            );
        }
        let query = if p.query.is_empty() {
            div().text_color(t.text_dim).child("Search symbols")
        } else {
            div().text_color(t.text_main).child(p.query.clone())
        };
        let hints = div()
            .flex_none()
            .flex()
            .flex_row()
            .justify_between()
            .px_3()
            .py_1()
            .border_t_1()
            .border_color(t.axis_line)
            .text_size(px(11.0))
            .text_color(t.text_dim)
            .child("↑↓ select   Tab month   Alt+M micro/mini   Ctrl+D favorite   Enter open   Esc close")
            .when_some(self.error.clone(), |d, err| {
                d.child(div().text_color(t.text_main).child(err))
            });
        Some(
            div()
                .absolute()
                .top(px(24.0))
                .left_0()
                .right_0()
                .bottom_0()
                .flex()
                .justify_center()
                .items_start()
                .child(
                    div()
                        .id("symbol-picker")
                        .occlude()
                        .w(px(PICKER_W))
                        .max_w_full()
                        .max_h_full()
                        .flex()
                        .flex_col()
                        .bg(t.panel)
                        .border_1()
                        .border_color(t.axis_line)
                        .rounded_md()
                        .text_size(px(13.0))
                        .font_features(self.tnum.clone())
                        .child(
                            div()
                                .flex_none()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap_1()
                                .px_3()
                                .py_2()
                                .border_b_1()
                                .border_color(t.axis_line)
                                .child(query)
                                .child(div().w(px(2.0)).h(px(15.0)).bg(t.accent)),
                        )
                        .child(rows)
                        .child(hints),
                )
                .into_any_element(),
        )
    }
}
