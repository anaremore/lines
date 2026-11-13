//! The "Download days…" window (decision 0030 §8): renders `crate::download_days::Model` and
//! runs its jobs (`lines_feed::download`) on their own thread, polling their updates; nothing
//! here blocks the UI on the network or the disk.
//!
//! Entry points: the palette ("Download days…"); [`open`] with a [`Prefill`] is the hook the
//! replay bar's "Open recording" menu will call. Keys: type into the focused field, Tab and
//! Shift+Tab move between fields, ← → step a choice or a date by a day, Enter checks the cost
//! (then downloads), Esc cancels a running job, else closes.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    App, AppContext, Context, FocusHandle, FontWeight, InteractiveElement, IntoElement,
    KeyDownEvent, MouseButton, ParentElement, Render, StatefulInteractiveElement, Styled,
    TitlebarOptions, Window, WindowHandle, WindowOptions, div, prelude::FluentBuilder, px,
};
use lines_core::UtcMillis;
use lines_feed::download::{self, Cancel, Job, PastDays, Target, Update};

use crate::download_days::{CancelAction, Field, Model, Phase, Primary, SourceRow};
use crate::theme::Theme;
use crate::workspace::Workspace;

/// The window's size, and the least it can be.
pub const SIZE: (f32, f32) = (620.0, 460.0);
pub const MIN_SIZE: (f32, f32) = (400.0, 340.0);
/// Field labels' column.
const LABEL_W: f32 = 72.0;
const DAY_W: f32 = 112.0;
const COST_W: f32 = 104.0;
const COST_W_COMPACT: f32 = 52.0;
const SIZE_W: f32 = 132.0;
const DAY_W_COMPACT: f32 = 72.0;
const SIZE_W_COMPACT: f32 = 84.0;
/// The "N more ↓" line under the list.
const MORE_H: f32 = 20.0;
/// Window widths below this use the compact columns.
const COMPACT_BELOW: f32 = 520.0;

/// Where downloaded days go: the run's store root, or why there is none.
#[derive(Clone, Debug)]
pub struct DownloadEnv {
    pub root: Result<PathBuf, String>,
    /// The store's size budget (`--store-budget-gb`), for the dialog's note.
    pub budget: Option<u64>,
}
impl gpui::Global for DownloadEnv {}

/// The open window, if any (there is only ever one).
#[derive(Default)]
pub struct DownloadWindowGlobal(pub Option<WindowHandle<DownloadWindow>>);
impl gpui::Global for DownloadWindowGlobal {}

/// What the dialog opens on (the replay bar's hook names its symbol).
#[derive(Clone, Debug, Default)]
pub struct Prefill {
    pub symbol: Option<String>,
}

/// One source that can download, with its seam.
#[derive(Clone)]
pub struct DaySource {
    pub row: SourceRow,
    pub days: Arc<dyn PastDays>,
}

/// Every registered source that can download past days.
pub fn sources() -> Vec<DaySource> {
    lines_feed::extension::registered()
        .into_iter()
        .filter_map(|e| {
            let days = e.past_days()?;
            let name = e.name();
            let mut label: String = name.to_owned();
            if let Some(f) = label.get_mut(..1) {
                f.make_ascii_uppercase();
            }
            Some(DaySource {
                row: SourceRow {
                    name: name.to_owned(),
                    label,
                },
                days,
            })
        })
        .collect()
}

pub struct DownloadWindow {
    theme: Theme,
    focus: FocusHandle,
    model: Model,
    sources: Vec<DaySource>,
    root: Result<PathBuf, String>,
    job: Option<Job>,
    /// The source and product the contents were asked for.
    contents_for: Option<(usize, Option<lines_core::ProductId>)>,
    /// The clock (screenshots freeze it).
    now: fn() -> UtcMillis,
    /// The window's own clock for the main button's settle rule.
    epoch: std::time::Instant,
    scroll: gpui::ScrollHandle,
    /// Day rows not wholly in view at the last frame (the list's "N more ↓").
    hidden: usize,
    /// The list area's height at the last frame (measured), and the whole-row height the list
    /// was given (so no row is cut at its bottom edge).
    avail: std::rc::Rc<std::cell::Cell<gpui::Pixels>>,
    snap: Option<gpui::Pixels>,
}

impl DownloadWindow {
    /// A running download's days (done, of total).
    pub(crate) fn days_progress(&self) -> Option<(usize, usize)> {
        self.model.days_progress()
    }

    fn new(
        theme: Theme,
        sources: Vec<DaySource>,
        root: Result<PathBuf, String>,
        symbol: &str,
        now: fn() -> UtcMillis,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut model = Model::new(
            sources.iter().map(|s| s.row.clone()).collect(),
            symbol,
            now(),
        );
        if let Err(why) = &root {
            model.error = Some(why.clone());
        }
        model.budget = cx.try_global::<DownloadEnv>().and_then(|e| e.budget);
        let mut v = DownloadWindow {
            theme,
            focus: cx.focus_handle(),
            model,
            sources,
            root,
            job: None,
            contents_for: None,
            now,
            epoch: std::time::Instant::now(),
            scroll: gpui::ScrollHandle::new(),
            hidden: 0,
            avail: Default::default(),
            snap: None,
        };
        v.refresh_contents();
        v
    }

    /// Ask the source for its choices when the source or the product changed (cheap: no
    /// network).
    fn refresh_contents(&mut self) {
        let product = self.model.product();
        let key = (self.model.source, product.as_ref().map(|p| p.id));
        if self.contents_for == Some(key) {
            return;
        }
        self.contents_for = Some(key);
        let contents = match (self.sources.get(self.model.source), product) {
            (Some(s), Some(p)) => s.days.contents(&p),
            _ => Vec::new(),
        };
        self.model.set_contents(contents);
    }

    fn target(&self, req: &crate::download_days::Request) -> Option<Target> {
        let root = self.root.as_ref().ok()?;
        Some(Target {
            root: root.clone(),
            source: self.model.source_name()?.to_owned(),
            product: req.product.clone(),
            contract: req.contract,
        })
    }

    /// Check cost (Enter in the editing phase).
    fn check_cost(&mut self, cx: &mut Context<Self>) {
        if self.model.busy() {
            return;
        }
        if let Err(why) = &self.root {
            self.model.refuse(why.clone());
            return;
        }
        let req = match self.model.request() {
            Ok(r) => r,
            Err(e) => {
                self.model.refuse(e);
                return;
            }
        };
        let (Some(target), Some(source), Some(contents)) = (
            self.target(&req),
            self.sources.get(self.model.source).map(|s| s.days.clone()),
            self.model.contents_id(),
        ) else {
            return;
        };
        // The store is read on the job's thread (its plan is the first update).
        let label = req.contract.label(&req.product).as_str().to_owned();
        self.model.begin_estimate(label);
        self.job = Some(download::estimate(
            source,
            target,
            contents,
            req.dates,
            (self.now)(),
            Cancel::new(),
        ));
        self.poll_job(cx);
    }

    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    /// Download (Enter once priced).
    fn start_download(&mut self, cx: &mut Context<Self>) {
        let Ok(req) = self.model.request() else {
            return;
        };
        let (Some(target), Some(source), Some(contents)) = (
            self.target(&req),
            self.sources.get(self.model.source).map(|s| s.days.clone()),
            self.model.contents_id(),
        ) else {
            return;
        };
        let days = self.model.begin_download();
        if days.is_empty() {
            return;
        }
        self.job = Some(download::download(
            source,
            target,
            contents,
            days,
            (self.now)(),
            Cancel::new(),
        ));
        self.poll_job(cx);
    }

    /// A press of the main button (Enter or a click); `held`: a key repeat.
    fn primary(&mut self, held: bool, cx: &mut Context<Self>) {
        match self.model.press(self.now_ms(), held) {
            Some(Primary::CheckCost) => self.check_cost(cx),
            Some(Primary::Download) => self.start_download(cx),
            None => {}
        }
        cx.notify();
    }

    /// Cancel / Cancel remaining / Stop now / Close (Esc, or a click); `held`: a key repeat,
    /// which never acts.
    fn secondary(&mut self, held: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.model.busy() {
            let action = self.model.cancel(self.now_ms(), held);
            if let Some(j) = &self.job {
                match action {
                    CancelAction::Now => j.cancel(),
                    CancelAction::Remaining => j.cancel_remaining(),
                    CancelAction::None => {}
                }
            }
            if action == CancelAction::Remaining {
                // Redraw once Stop now may be pressed.
                cx.spawn(async move |this, cx| {
                    cx.background_executor()
                        .timer(Duration::from_millis(crate::download_days::STOP_AFTER_MS))
                        .await;
                    let _ = this.update(cx, |_, cx| cx.notify());
                })
                .detach();
            }
            cx.notify();
        } else if !held {
            window.remove_window();
        }
    }

    /// Pull the job's updates until it finishes (every 50 ms, off the job's thread).
    fn poll_job(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(50))
                    .await;
                let done = this.update(cx, |v, cx| {
                    let Some(job) = &v.job else {
                        return true;
                    };
                    let updates = job.poll();
                    if updates.is_empty() {
                        return false;
                    }
                    let mut finished = false;
                    for u in updates {
                        finished |= u == Update::Finished;
                        v.model.apply(u);
                    }
                    if finished {
                        v.job = None;
                    }
                    let now = v.now_ms();
                    v.model.settle(now);
                    if v.model.ready_at.is_some() {
                        // Redraw once Download may be pressed.
                        cx.spawn(async move |this, cx| {
                            cx.background_executor()
                                .timer(Duration::from_millis(crate::download_days::SETTLE_MS))
                                .await;
                            let _ = this.update(cx, |_, cx| cx.notify());
                        })
                        .detach();
                    }
                    cx.notify();
                    finished
                });
                if done.unwrap_or(true) {
                    break;
                }
            }
        })
        .detach();
    }

    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        let k = &ev.keystroke;
        let m = k.modifiers;
        match k.key.as_str() {
            "tab" => self.model.next_field(m.shift),
            "left" => self.model.step(-1),
            "right" => self.model.step(1),
            "backspace" => self.model.backspace(),
            "enter" => self.primary(ev.is_held, cx),
            "up" => self.model.select(-1),
            "down" => self.model.select(1),
            "escape" => self.secondary(ev.is_held, window, cx),
            _ => {
                if let Some(c) = k.key_char.as_deref().filter(|_| !m.control && !m.alt) {
                    self.model.type_text(c);
                }
            }
        }
        self.refresh_contents();
        cx.notify();
    }

    fn field_row(&self, f: Field, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let t = self.theme;
        let focused = self.model.focus == f;
        let editable = !self.model.busy();
        let mut value = self.model.value(f);
        if f.is_text() && focused && editable {
            value.push('▏');
        }
        if !f.is_text() {
            value.push_str(" ▾");
        }
        let note = match f {
            Field::Symbol => self
                .model
                .contract_label
                .clone()
                .filter(|l| *l != self.model.symbol),
            _ => None,
        };
        if !editable {
            // During a job the inputs can't change: plain values, no box or caret.
            let plain = self.model.value(f);
            return div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .min_h(crate::zoom::hit(22.0))
                .child(
                    div()
                        .w(crate::zoom::ui(LABEL_W))
                        .flex_none()
                        .text_color(t.text_dim)
                        .child(f.label()),
                )
                .child(div().px_2().text_color(t.text_dim).child(plain))
                .into_any_element();
        }
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(
                div()
                    .w(crate::zoom::ui(LABEL_W))
                    .flex_none()
                    .text_color(t.text_dim)
                    .child(f.label()),
            )
            .child(
                div()
                    .id(("download-field", f as usize))
                    .min_w(crate::zoom::hit(160.0))
                    .min_h(crate::zoom::hit(22.0))
                    .px_2()
                    .flex()
                    .items_center()
                    .rounded_sm()
                    .bg(t.panel)
                    .border_1()
                    .border_color(if focused && editable {
                        t.accent
                    } else {
                        t.axis_line
                    })
                    .text_color(if editable { t.text_main } else { t.text_dim })
                    .when(editable, |d| d.cursor_pointer())
                    .child(value)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |v, _, window, cx| {
                            if v.model.focus == f && !f.is_text() {
                                v.model.step(1);
                                v.refresh_contents();
                            }
                            v.model.focus(f);
                            v.focus.focus(window, cx);
                            cx.notify();
                        }),
                    ),
            )
            .children(note.map(|n| div().text_color(t.text_dim).child(format!("= {n}"))))
            .into_any_element()
    }

    fn button(
        &self,
        id: &'static str,
        label: String,
        primary: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let t = self.theme;
        div()
            .id(id)
            .min_h(crate::zoom::hit(24.0))
            .px_3()
            .flex()
            .items_center()
            .rounded_sm()
            .border_1()
            .cursor_pointer()
            .when(primary, |d| {
                d.bg(t.accent)
                    .border_color(t.accent)
                    .text_color(t.bg)
                    .font_weight(FontWeight::BOLD)
            })
            .when(!primary, |d| {
                if self.model.phase == Phase::Downloading && self.model.canceling {
                    // Stop now discards the day in flight: destructive (design, Menus 6).
                    d.border_color(t.destructive).text_color(t.destructive)
                } else if self.model.phase == Phase::Downloading {
                    // The key that acts during a download (Esc): ringed in the accent.
                    d.border_2().border_color(t.accent)
                } else {
                    d.border_color(t.control_edge)
                }
            })
            .child(label)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |v, _, window, cx| {
                    if primary {
                        v.primary(false, cx);
                    } else {
                        v.secondary(false, window, cx);
                    }
                }),
            )
    }
}

impl Render for DownloadWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        window.set_rem_size(crate::zoom::rem());
        // Follows a theme change made while the window is open (Settings refreshes windows).
        if let Some(ws) = cx.try_global::<Workspace>() {
            self.theme = ws.env.theme.get();
        }
        let t = self.theme;
        let mut form = div().flex().flex_col().gap_2().px_3().pt_2().pb_2();
        for f in self.model.fields() {
            form = form.child(self.field_row(f, cx));
        }
        // What the last frame showed of the list: rows not wholly in view, a scrollbar.
        let view = self.scroll.bounds();
        let off = self.scroll.offset().y;
        let hidden = (0..self.model.rows.len())
            .filter(|&i| {
                self.scroll.bounds_for_item(i).is_none_or(|b| {
                    b.top() + off < view.top() - px(0.5)
                        || b.bottom() + off > view.bottom() + px(0.5)
                })
            })
            .count();
        if hidden != self.hidden {
            self.hidden = hidden;
            window.request_animation_frame();
        }
        let max = self.scroll.max_offset().y;
        // Whole rows only: from the last frame's row heights, the most that fit above the
        // "N more ↓" line.
        let pad = crate::zoom::ui(8.0);
        let avail = self.avail.get();
        let heights: Vec<gpui::Pixels> = (0..self.model.rows.len())
            .filter_map(|i| self.scroll.bounds_for_item(i).map(|b| b.size.height))
            .collect();
        let content = heights.iter().fold(pad, |a, h| a + *h);
        let snap = (avail > px(0.0) && heights.len() == self.model.rows.len() && content > avail)
            .then(|| {
                // Real (wrapped) row heights; under a snapped list the "N more ↓" line takes
                // the bottom padding's place, so only the top padding counts.
                let limit = avail - crate::zoom::ui(MORE_H);
                let mut acc = pad / 2.0;
                for h in &heights {
                    if acc + *h > limit {
                        break;
                    }
                    acc += *h;
                }
                acc
            });
        if snap != self.snap {
            self.snap = snap;
            window.request_animation_frame();
        }
        // Narrow windows: shorter day and size columns, so a message keeps room.
        let compact = window.viewport_size().width < crate::zoom::ui(COMPACT_BELOW);
        let mut days = div()
            .id("download-days")
            .track_scroll(&self.scroll)
            .w_full()
            .map(|d| match self.snap {
                Some(h) => d.h(h).flex_none(),
                None => d.flex_1().min_h_0(),
            })
            .flex()
            .flex_col()
            .overflow_y_scroll()
            .px_3()
            .py_1();
        for (i, r) in self.model.rows.iter().enumerate() {
            let mut rt = self.model.row_text(r);
            // Problems always show whole (the provider's words), spanning the empty cost and
            // size columns and wrapping; others whole when selected.
            let whole = rt.problem || self.model.selected == Some(i);
            let columns = !(rt.problem && rt.cost.is_empty() && rt.size.is_empty());
            if compact {
                // `Mon 11-02`; the size without the estimate.
                rt.day = format!("{} {:02}-{:02}", &rt.day[..3], r.date.month, r.date.day);
                rt.size = compact_size(&rt.size);
                // `$1.63` (the status says whether it is an estimate or charged).
                rt.cost = rt.cost.rsplit(' ').next().unwrap_or_default().to_owned();
            }
            days = days.child(
                div()
                    .id(("download-day", i))
                    .flex()
                    .flex_row()
                    .flex_none()
                    .items_start()
                    .gap_2()
                    .py_1()
                    .px_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .when(self.model.selected == Some(i), |d| d.bg(t.panel))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |v, _, _, cx| {
                            v.model.selected = Some(i);
                            cx.notify();
                        }),
                    )
                    .child(
                        div()
                            .w(crate::zoom::ui(if compact { DAY_W_COMPACT } else { DAY_W }))
                            .flex_none()
                            .whitespace_nowrap()
                            .text_color(if rt.quiet { t.text_dim } else { t.text_main })
                            .child(rt.day),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .when(!whole, |d| d.truncate())
                            .text_color(if rt.problem {
                                t.warn
                            } else if rt.quiet {
                                t.text_dim
                            } else {
                                t.text_main
                            })
                            .when(rt.problem, |d| d.font_weight(FontWeight::BOLD))
                            .child(rt.status),
                    )
                    .when(columns, |d| {
                        d.child(
                            div()
                                .w(crate::zoom::ui(if compact {
                                    COST_W_COMPACT
                                } else {
                                    COST_W
                                }))
                                .flex_none()
                                .flex()
                                .justify_end()
                                .whitespace_nowrap()
                                .child(rt.cost.clone()),
                        )
                    })
                    .when(columns, |d| {
                        d.child(
                            div()
                                .w(crate::zoom::ui(if compact {
                                    SIZE_W_COMPACT
                                } else {
                                    SIZE_W
                                }))
                                .flex_none()
                                .flex()
                                .justify_end()
                                .whitespace_nowrap()
                                .text_color(t.text_dim)
                                .child(rt.size.clone()),
                        )
                    }),
            );
        }
        let avail_cell = self.avail.clone();
        let mut list = div()
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(
                gpui::canvas(
                    move |bounds, _, _| avail_cell.set(bounds.size.height),
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .child(days);
        if max > px(0.5) {
            // A scrollbar thumb on the list's right edge.
            let h = view.size.height;
            let thumb = (h * (h / (h + max))).max(px(16.0));
            let top = (h - thumb) * (-off / max).clamp(0.0, 1.0);
            list = list.child(
                div()
                    .absolute()
                    .right(px(2.0))
                    .top(top)
                    .w(px(4.0))
                    .h(thumb)
                    .rounded_sm()
                    .bg(t.control_edge),
            );
        }
        if self.hidden > 0 {
            // At the list's bottom edge, under the last whole row.
            list = list.child(
                div()
                    .h(crate::zoom::ui(MORE_H))
                    .flex_none()
                    .px_3()
                    .flex()
                    .justify_end()
                    .items_center()
                    .text_size(crate::zoom::ui(12.0))
                    .text_color(t.text_dim)
                    .child(format!("{} more ↓", self.hidden)),
            );
        }
        let summary = self.model.summary();
        let mut notes: Vec<String> = Vec::new();
        notes.extend(self.model.budget_note());
        let mut buttons = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .justify_end()
            .gap_2();
        if let Some(p) = self.model.primary() {
            buttons = buttons.child(self.button("download-primary", p, true, cx));
        }
        buttons = buttons.child(self.button(
            "download-secondary",
            self.model.secondary().to_owned(),
            false,
            cx,
        ));
        div()
            .id("download")
            .size_full()
            // The UI font (`crate::fonts`): every text in the window inherits it.
            .font_family(crate::fonts::ui_family())
            .flex()
            .flex_col()
            .bg(t.bg)
            .text_color(t.text_main)
            .text_size(crate::zoom::ui(13.0))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .child(
                div()
                    .px_3()
                    .pt_2()
                    .font_weight(FontWeight::BOLD)
                    .child("Download days"),
            )
            .child(form)
            .children(self.model.error.clone().map(|e| {
                div()
                    .px_3()
                    .pb_2()
                    .text_color(t.warn)
                    .font_weight(FontWeight::BOLD)
                    .child(e)
            }))
            .child(div().border_t_1().border_color(t.axis_line))
            .child(list)
            .child(
                // The footer: the summary and notes over the buttons, which stay right-aligned
                // at every width.
                div()
                    .flex()
                    .flex_col()
                    .flex_none()
                    .gap_1()
                    .px_3()
                    .pt_2()
                    .map(|d| if compact { d.pb_3() } else { d.pb_2() })
                    .border_t_1()
                    .border_color(t.axis_line)
                    .child(div().text_color(t.text_main).child(summary))
                    .children(notes.into_iter().map(|n| {
                        // A fact about the store, not a hint: "!" and secondary text.
                        div()
                            .flex()
                            .flex_row()
                            .gap_1()
                            .child(
                                div()
                                    .flex_none()
                                    .text_color(t.warn)
                                    .font_weight(FontWeight::BOLD)
                                    .child("!"),
                            )
                            .text_color(t.text_dim)
                            .child(div().flex_1().min_w_0().child(n))
                    }))
                    .child(buttons),
            )
    }
}

/// The window closes: a running job ends as "Cancel remaining" (Stop now is the one way to
/// discard a day in flight).
fn close_job(job: &Job) {
    job.cancel_remaining();
}

/// A narrow window's size column: `212/~316 MiB` for `212 MiB / ~316 MiB` (same unit), else
/// the bytes received alone.
fn compact_size(size: &str) -> String {
    let Some((got, est)) = size.split_once(" / ~") else {
        return size.to_owned();
    };
    match (got.split_once(' '), est.split_once(' ')) {
        (Some((a, u1)), Some((b, u2))) if u1 == u2 => format!("{a}/~{b} {u1}"),
        _ => got.to_owned(),
    }
}

/// The window's options: a normal window, or hidden (screenshots, tests).
fn options(ws: &Workspace, size: (f32, f32), cx: &App) -> WindowOptions {
    if ws.env.track_visibility {
        WindowOptions {
            titlebar: Some(TitlebarOptions {
                title: Some("Lines · Download days".into()),
                ..Default::default()
            }),
            window_bounds: Some(gpui::WindowBounds::Windowed(gpui::Bounds::centered(
                None,
                gpui::size(px(size.0), px(size.1)),
                cx,
            ))),
            window_min_size: Some(gpui::size(px(MIN_SIZE.0), px(MIN_SIZE.1))),
            icon: crate::window_icon(),
            ..Default::default()
        }
    } else {
        crate::workspace::hidden_window("Lines (download days)", size)
    }
}

/// Open the window (or bring it forward). The replay bar's "Open recording" menu calls this
/// with its symbol.
pub fn open(prefill: Prefill, cx: &mut App) {
    open_with(prefill, sources(), SIZE, lines_feed::now_utc, cx);
}

fn open_with(
    prefill: Prefill,
    sources: Vec<DaySource>,
    size: (f32, f32),
    now: fn() -> UtcMillis,
    cx: &mut App,
) -> Option<WindowHandle<DownloadWindow>> {
    if let Some(h) = cx.try_global::<DownloadWindowGlobal>().and_then(|g| g.0) {
        let ok = h
            .update(cx, |v, window, cx| {
                window.activate_window();
                v.focus.focus(window, cx);
                cx.notify();
            })
            .is_ok();
        if ok {
            return Some(h);
        }
    }
    let ws = cx.try_global::<Workspace>()?;
    let theme = ws.env.theme.get();
    let options = options(ws, size, cx);
    let root = cx.try_global::<DownloadEnv>().map_or_else(
        || Err("The local store is off for this run.".into()),
        |e| e.root.clone(),
    );
    let symbol = prefill.symbol.unwrap_or_else(|| {
        sources
            .first()
            .and_then(|s| lines_feed::extension::by_name(&s.row.name))
            .and_then(|e| lines_feed::catalog::get(e.default_product()))
            .map_or_else(|| "ES".to_owned(), |p| p.symbol.to_owned())
    });
    let h = cx
        .open_window(options, move |window, cx| {
            cx.new(|cx| {
                let v = DownloadWindow::new(theme, sources, root, &symbol, now, cx);
                v.focus.focus(window, cx);
                v
            })
        })
        .ok()?;
    cx.set_global(DownloadWindowGlobal(Some(h)));
    let _ = h.update(cx, |_, window, cx| {
        window.on_window_should_close(cx, |window, cx| {
            // Closing is "Cancel remaining": the day in flight, maybe already billed, finishes
            // and is stored on the job's thread; no other day starts. (The view is reached
            // through the window: this runs inside its update.)
            if let Some(Some(v)) = window.root::<DownloadWindow>() {
                v.update(cx, |v, _| {
                    if let Some(j) = v.job.take() {
                        close_job(&j);
                    }
                });
            }
            cx.set_global(DownloadWindowGlobal(None));
            true
        });
    });
    Some(h)
}

/// Screenshots (`--download-days STATE`): the window on the synthetic demo source and a
/// scratch store, in one of its states, from the real engine (estimates and stored days) and
/// the model's own update path (progress).
#[cfg(feature = "screenshot")]
pub mod shot {
    use super::*;
    use lines_core::CivilDate;
    use lines_feed::download::demo::{self, Demo};
    use lines_feed::download::{Charge, Outcome};

    /// `stopping`: a download after one "Cancel remaining" press (Stop now offered).
    pub const STATES: [&str; 5] = ["estimate", "progress", "stopping", "error", "stored"];

    /// `STATE` or `STATE:depth` (the "Full depth" contents).
    fn split(state: &str) -> (&str, bool) {
        match state.strip_suffix(":depth") {
            Some(s) => (s, true),
            None => (state, false),
        }
    }

    fn now() -> UtcMillis {
        UtcMillis::parse_iso("2026-11-09T12:00:00Z").expect("time")
    }

    fn d(day: u8) -> CivilDate {
        CivilDate::new(2026, 11, day).expect("date")
    }

    pub fn check(state: &str) -> anyhow::Result<()> {
        if !STATES.contains(&split(state).0) {
            anyhow::bail!(
                "--download-days {state:?}: one of {}, optionally with :depth",
                STATES.join(", ")
            );
        }
        Ok(())
    }

    fn run(job: Job) -> Vec<Update> {
        let mut v = Vec::new();
        while let Some(u) = job.next(Duration::from_secs(30)) {
            let end = u == Update::Finished;
            v.push(u);
            if end {
                break;
            }
        }
        job.join();
        v
    }

    /// Open the window (hidden) in `state`.
    pub fn open(state: &str, size: (f32, f32), cx: &mut App) -> Option<gpui::AnyWindowHandle> {
        let (state, depth) = split(state);
        let contents = if depth { demo::DEPTH } else { demo::TOB };
        // A scratch store for the demo's days, read once below. It goes when this returns (an
        // early `?` return included), before the screenshot run ends with `process::exit`.
        let scratch = lines_core::scratch::Scratch::new("download-shot");
        let root = scratch.to_path_buf();
        let es = lines_feed::catalog::get(lines_feed::catalog::ES)?;
        let target = Target {
            root: root.clone(),
            source: "demo".into(),
            product: es.clone(),
            contract: lines_core::ContractKey::monthly(
                es.id,
                2026,
                lines_core::Month::new(12).expect("month"),
            ),
        };
        let stored: &[u8] = match state {
            "stored" => &[4, 5, 6],
            _ => &[4],
        };
        let days: Vec<(CivilDate, Charge)> =
            stored.iter().map(|&n| (d(n), Charge::Included)).collect();
        run(download::download(
            Arc::new(Demo::default()),
            target.clone(),
            demo::TOB.id,
            days,
            now(),
            Cancel::new(),
        ));
        let refusal = "Your account isn't entitled to this data before 2026-11-04. \
                       Upgrade your plan or pick later dates.";
        let src = Demo {
            included_from: Some(d(4)),
            price_usd_micros: 1_630_000,
            refuse: if state == "error" {
                vec![(d(2), refusal.into()), (d(3), refusal.into())]
            } else {
                vec![]
            },
            ..Demo::default()
        };
        let sources = vec![DaySource {
            row: SourceRow {
                name: "demo".into(),
                label: "Demo".into(),
            },
            days: Arc::new(src.clone()),
        }];
        cx.set_global(DownloadEnv {
            root: Ok(root.clone()),
            // Small, so the estimate shows the budget note.
            budget: Some(1 << 30),
        });
        let (from, to) = match state {
            "stored" => (d(4), d(6)),
            "progress" | "stopping" => (d(3), d(6)),
            _ => (d(2), d(8)),
        };
        let h = open_with(
            Prefill {
                symbol: Some("ESZ26".into()),
            },
            sources,
            size,
            now,
            cx,
        )?;
        let dates = download::dates(from, to).ok()?;
        let estimate = run(download::estimate(
            Arc::new(src),
            target,
            contents.id,
            dates,
            now(),
            Cancel::new(),
        ));
        let _ = h.update(cx, |v, _, cx| {
            v.model.from = from.to_string();
            v.model.to = to.to_string();
            if depth {
                v.model.content = 1;
            }
            v.model.begin_estimate("ESZ26".into());
            for u in estimate {
                v.model.apply(u);
            }
            v.model.settle(0);
            if state == "progress" || state == "stopping" {
                v.model.begin_download();
                v.model.apply(Update::Started { date: d(3) });
                v.model.apply(Update::Requested { date: d(3) });
                v.model.apply(Update::Day {
                    date: d(3),
                    outcome: Outcome::Stored(lines_feed::disk::day::Written {
                        bytes: 61_234_567,
                        ..Default::default()
                    }),
                });
                v.model.apply(Update::Started { date: d(5) });
                v.model.apply(Update::Requested { date: d(5) });
                v.model.apply(Update::Progress {
                    date: d(5),
                    received: 212 * 1024 * 1024,
                });
                if state == "stopping" {
                    v.model.cancel(0, false);
                }
            }
            cx.notify();
        });
        // Everything shown came from the store already: the scratch store goes.
        drop(scratch);
        Some(h.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lines_core::{CivilDate, ContractKey, Month};
    use lines_feed::download::demo::{self, Demo};
    use lines_feed::download::{Charge, Outcome};

    /// The close button's path (`close_job`): the day in flight finishes and is stored; the
    /// next day never starts.
    #[test]
    fn closing_the_window_finishes_the_day_in_flight() {
        let scratch = lines_core::scratch::Scratch::new("download-close");
        let root = scratch.to_path_buf();
        let es = lines_feed::catalog::get(lines_feed::catalog::ES).unwrap();
        let target = Target {
            root: root.clone(),
            source: "demo".into(),
            product: es.clone(),
            contract: ContractKey::monthly(es.id, 2026, Month::new(12).unwrap()),
        };
        let d = |n| CivilDate::new(2026, 11, n).unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let src = Demo {
            hold_at: Some((100, barrier.clone())),
            ..Demo::default()
        };
        let asked = src.asked.clone();
        let now = UtcMillis::parse_iso("2026-11-09T12:00:00Z").unwrap();
        let job = download::download(
            Arc::new(src),
            target.clone(),
            demo::TOB.id,
            vec![(d(5), Charge::Included), (d(6), Charge::Included)],
            now,
            Cancel::new(),
        );
        barrier.wait(); // mid-day
        close_job(&job);
        barrier.wait();
        // The job runs to its end: the day in flight is stored, the next one never asked for.
        let mut outcomes = Vec::new();
        while let Some(u) = job.next(Duration::from_secs(60)) {
            match u {
                Update::Day { date, outcome } => outcomes.push((date, outcome)),
                Update::Finished => break,
                _ => {}
            }
        }
        assert!(
            matches!(outcomes[0], (day, Outcome::Stored(_)) if day == d(5)),
            "{outcomes:?}"
        );
        assert_eq!(outcomes[1], (d(6), Outcome::Canceled));
        assert_eq!(
            asked.lock().unwrap().clone(),
            vec![d(5)],
            "day 6 never asked"
        );
        assert!(target.dir().segment_path(d(5)).exists());
    }
}
