//! The chart window: header + candlestick canvas with pan/zoom/scaling and a crosshair.
//!
//! Frame flow (decision 0001): a UI-thread timer compares the store's atomic revision and
//! calls `cx.notify()` only when data changed (so 5k msgs/s never means 5k redraws). In
//! paint we `try_lock` the store, update the viewport and rebuild the reusable `Scene` (no
//! heap allocation), release the lock, then emit quads. Text labels are shaped once and
//! cached. Coordinates handed to GPUI are relative to the plot origin before the `f32` cast.
//!
//! Crosshair: input handlers only record the pointer position (plot-local logical px). Paint
//! hit-tests it against the viewport *after* this frame's pan/zoom/data update
//! (`lines_core::geometry::crosshair`), so the crosshair can never lag or fight the view.
//!
//! Input map: drag = pan; Ctrl+drag (plot) or drag on an axis = scale (`ScaleDrag`);
//! wheel = time zoom (pinned to the right edge while the newest bar is visible);
//! Ctrl+wheel = price zoom; Shift+wheel or horizontal scroll = pan through time;
//! double-click plot / Home = newest bar + auto-scale; double-click price axis = auto-scale.
//! While any drag is active the viewport is `held`, so arriving bars can't move the axes
//! under the pointer.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    App, Bounds, ContentMask, Context, FocusHandle, Font, Hsla, IntoElement, KeyDownEvent,
    MouseButton, MouseDownEvent, MouseExitEvent, MouseMoveEvent, MouseUpEvent, ParentElement,
    Pixels, Point, Render, ScrollEvent, ShapedLine, SharedString, Styled, TextAlign, TextRun,
    Window, canvas, div, fill, point, prelude::*, px, size,
};
use lines_core::geometry::{Crosshair, crosshair, snap, write_ohlcv};
use lines_core::viewport::{ScaleDrag, ScaleMode};
use lines_core::{Bar, FixedStr, Price, Product, Scene, Viewport};
use lines_feed::FeedHandle;
use lines_feed::store::{ChartStore, FeedStatus};

use crate::theme::Theme;

const AXIS_W: f32 = 78.0;
const AXIS_H: f32 = 24.0;
const LABEL_PX: f32 = 12.0;
const MAX_CACHED_LABELS: usize = 4096;
/// Shown in the price-axis corner while price auto-scale is off.
const MANUAL_LABEL: &str = "manual";

#[derive(Copy, Clone, PartialEq, Eq, Hash)]
enum LabelKind {
    Price,
    Time,
    LastPrice,
    CrossPrice,
    CrossTime,
    Indicator,
}

/// Where a pointer event landed.
#[derive(Copy, Clone, PartialEq, Eq)]
enum Region {
    Plot,
    /// The price axis column, including the corner below it.
    PriceAxis,
    TimeAxis,
}

/// Deterministic input for screenshot mode (decision 0009), standing in for the mouse.
#[derive(Copy, Clone, Debug, Default)]
pub struct Fixture {
    /// Fixed pointer position, plot-local logical px.
    pub cursor: Option<(f32, f32)>,
    /// Zoom the auto-fitted price range by this factor around the plot's centre, as Ctrl+wheel
    /// would (turns auto-scale off).
    pub price_zoom: Option<f64>,
}

enum Drag {
    /// Last pointer position.
    Pan(Point<Pixels>),
    Scale(ScaleDrag),
}

struct Rates {
    at: Instant,
    updates: u64,
    frames: u64,
    updates_per_sec: f64,
    frames_per_sec: f64,
}

pub struct ChartView {
    feed: FeedHandle,
    store: Arc<ChartStore>,
    theme: Theme,
    /// Screenshot mode: no wall-clock-dependent text (rates, paint time) is shown.
    frozen: bool,
    seen_revision: u64,
    vp: Viewport,
    scene: Scene,
    instrument: Product,
    last_trade: Option<Price>,
    series_len: usize,
    series_trimmed: u64,
    plot: Bounds<Pixels>,
    /// The whole canvas: plot plus axes.
    chart: Bounds<Pixels>,
    drag: Option<Drag>,
    focus: FocusHandle,
    focus_requested: bool,
    /// Screenshot fixture: price zoom to apply once, after the first auto-fit.
    pending_price_zoom: Option<f64>,
    /// Pointer position in plot-local logical px, while it is over the plot.
    hover: Option<(f32, f32)>,
    /// This frame's crosshair (from `hover`), and a copy of the bar under it.
    cross: Option<Crosshair>,
    cross_bar: Option<Bar>,
    period_ms: i64,
    /// OHLCV readout, shaped once per distinct bar value.
    readout: Option<(Bar, ShapedLine)>,
    labels: HashMap<(LabelKind, i64), ShapedLine>,
    font: Option<Font>,
    rates: Rates,
    paint_time_ewma_us: f64,
}

impl ChartView {
    pub fn new(
        feed: FeedHandle,
        theme: Theme,
        frozen: bool,
        fixture: Fixture,
        cx: &mut Context<Self>,
    ) -> Self {
        let store = feed.store.clone();
        let instrument = store.read().product.clone();
        // Poll the lock-free revision counter at ~120 Hz; redraw only on change.
        let poll_store = store.clone();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(8))
                    .await;
                let rev = poll_store.revision();
                let alive = this.update(cx, |view, cx| {
                    view.update_rates();
                    if rev != view.seen_revision {
                        view.seen_revision = rev;
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();

        ChartView {
            feed,
            store,
            theme,
            frozen,
            seen_revision: u64::MAX,
            vp: Viewport::default(),
            scene: Scene::with_capacity(Scene::candles_needed(4096.0)),
            instrument,
            last_trade: None,
            series_len: 0,
            series_trimmed: 0,
            plot: Bounds::default(),
            chart: Bounds::default(),
            drag: None,
            focus: cx.focus_handle(),
            focus_requested: false,
            hover: fixture.cursor,
            pending_price_zoom: fixture.price_zoom,
            cross: None,
            cross_bar: None,
            period_ms: 60_000,
            readout: None,
            labels: HashMap::with_capacity(MAX_CACHED_LABELS),
            font: None,
            rates: Rates {
                at: Instant::now(),
                updates: 0,
                frames: 0,
                updates_per_sec: 0.0,
                frames_per_sec: 0.0,
            },
            paint_time_ewma_us: 0.0,
        }
    }

    fn update_rates(&mut self) {
        let dt = self.rates.at.elapsed().as_secs_f64();
        if dt < 1.0 {
            return;
        }
        if let Some(d) = self.store.try_read() {
            let (u, f) = (d.stats.updates_in, d.stats.frames_in);
            self.rates.updates_per_sec = (u - self.rates.updates.min(u)) as f64 / dt;
            self.rates.frames_per_sec = (f - self.rates.frames.min(f)) as f64 / dt;
            self.rates.updates = u;
            self.rates.frames = f;
            self.rates.at = Instant::now();
        }
    }

    // ---- input ---------------------------------------------------------------------

    fn region(&self, pos: Point<Pixels>) -> Option<Region> {
        if !self.chart.contains(&pos) {
            None
        } else if self.plot.contains(&pos) {
            Some(Region::Plot)
        } else if pos.x >= self.plot.right() {
            Some(Region::PriceAxis)
        } else {
            Some(Region::TimeAxis)
        }
    }

    /// Plot-local logical px (may lie outside the plot, e.g. on an axis).
    fn local(&self, pos: Point<Pixels>) -> (f64, f64) {
        let l = pos - self.plot.origin;
        (f64::from(l.x), f64::from(l.y))
    }

    fn start_drag(&mut self, drag: Drag) {
        self.drag = Some(drag);
        self.vp.set_held(true, self.series_len);
    }

    fn end_drag(&mut self) {
        self.drag = None;
        self.vp.set_held(false, self.series_len);
    }

    fn on_mouse_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(region) = self.region(ev.position) else {
            return;
        };
        self.focus.focus(window, cx);
        self.end_drag();
        let (x, y) = self.local(ev.position);
        if ev.click_count >= 2 {
            match region {
                Region::Plot => self.vp.reset(self.series_len),
                Region::PriceAxis => self.vp.restore_auto_price(),
                Region::TimeAxis => return,
            }
            cx.notify();
            return;
        }
        let drag = match region {
            Region::Plot if ev.modifiers.control => {
                Drag::Scale(ScaleDrag::new(&self.vp, ScaleMode::Free, x, y))
            }
            Region::Plot => Drag::Pan(ev.position),
            Region::PriceAxis => Drag::Scale(ScaleDrag::new(&self.vp, ScaleMode::Price, x, y)),
            Region::TimeAxis => Drag::Scale(ScaleDrag::new(&self.vp, ScaleMode::Time, x, y)),
        };
        self.start_drag(drag);
    }

    fn on_key_down(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if ev.keystroke.key == "home" {
            self.vp.reset(self.series_len);
            cx.notify();
        }
    }

    /// Track the pointer for the crosshair; it shows only while the pointer is over the plot.
    fn set_hover(&mut self, pos: Point<Pixels>, cx: &mut Context<Self>) {
        let hover = self.plot.contains(&pos).then(|| {
            let l = pos - self.plot.origin;
            (f32::from(l.x), f32::from(l.y))
        });
        if hover != self.hover {
            self.hover = hover;
            cx.notify();
        }
    }

    fn on_mouse_exit(&mut self, _: &MouseExitEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.hover.take().is_some() {
            cx.notify();
        }
    }

    fn on_mouse_move(&mut self, ev: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.set_hover(ev.position, cx);
        if self.drag.is_none() {
            return;
        }
        if ev.pressed_button != Some(MouseButton::Left) {
            self.end_drag();
            return;
        }
        let (x, y) = self.local(ev.position);
        let len = self.series_len;
        let tick = self.instrument.price.tick;
        match &mut self.drag {
            Some(Drag::Pan(from)) => {
                let d = ev.position - *from;
                *from = ev.position;
                self.vp.pan(f64::from(d.x), f64::from(d.y), len);
            }
            Some(Drag::Scale(drag)) => drag.update(&mut self.vp, x, y, len, tick),
            None => return,
        }
        cx.notify();
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.drag.is_some() {
            self.end_drag();
            cx.notify();
        }
    }

    fn on_scroll(&mut self, ev: &ScrollEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.set_hover(ev.position, cx);
        if !self.plot.contains(&ev.position) {
            return;
        }
        let d = ev.delta.pixel_delta(window.line_height());
        let (dx, dy) = (f64::from(d.x), f64::from(d.y));
        let (x, y) = self.local(ev.position);
        let len = self.series_len;
        if ev.modifiers.shift || dx.abs() > dy.abs() {
            // Shift+wheel (GPUI on Windows already reports it as delta.x), tilt wheel and
            // trackpad swipes pan through time; the content follows the delta.
            let amount = if dx != 0.0 { dx } else { dy };
            if amount == 0.0 {
                return;
            }
            self.vp.pan(amount, 0.0, len);
        } else if dy == 0.0 {
            return;
        } else {
            // Wheel up (positive delta) zooms in. ~10% per notch.
            let factor = (dy * 0.005).exp();
            if ev.modifiers.control {
                self.vp.zoom_price_at(y, factor, self.instrument.price.tick);
            } else {
                let anchor = self.vp.time_zoom_anchor(x, len);
                self.vp.zoom_at(anchor, factor, len);
            }
        }
        cx.notify();
    }

    // ---- paint ---------------------------------------------------------------------

    fn paint_chart(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        let t0 = Instant::now();
        let scale = window.scale_factor();
        let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
        let plot_w = (w - AXIS_W).max(10.0);
        let plot_h = (h - AXIS_H).max(10.0);
        self.chart = bounds;
        self.plot = Bounds::new(bounds.origin, size(px(plot_w), px(plot_h)));
        self.vp.set_size(plot_w as f64, plot_h as f64);
        // Only allocates when the window grows wider than ever before.
        self.scene.ensure_capacity(plot_w as f64);

        // Sample the feed. If it holds the lock this instant, redraw the previous scene.
        if let Some(d) = self.store.try_read() {
            let trimmed = d.series.trimmed();
            if trimmed > self.series_trimmed && (!self.vp.follow_live || self.vp.held) {
                // Bars dropped from the front shift indices; keep the view anchored.
                self.vp.right_index -= (trimmed - self.series_trimmed) as f64;
            }
            self.series_trimmed = trimmed;
            self.series_len = d.series.len();
            self.last_trade = d.last_trade;
            let fmt = self.instrument.price;
            let bars = d.series.bars();
            self.vp.on_data(bars, fmt.tick);
            self.period_ms = d.series.period_ms();
            // Only once bars are visible, i.e. auto-scale has fitted a real range (the first
            // layout pass can paint at a placeholder size with nothing on screen).
            if !self.vp.visible_range(bars.len()).is_empty()
                && let Some(f) = self.pending_price_zoom.take()
            {
                self.vp.zoom_price_at(self.vp.height * 0.5, f, fmt.tick);
            }
            self.scene
                .build(bars, &self.vp, &fmt, scale, self.period_ms);
            // Hit-test after the viewport update so the crosshair matches this frame.
            self.cross = self.hover.and_then(|(x, y)| {
                crosshair(
                    bars,
                    &self.vp,
                    fmt.tick,
                    self.period_ms,
                    scale,
                    f64::from(x),
                    f64::from(y),
                )
            });
            self.cross_bar = self.cross.and_then(|c| c.bar).map(|i| bars[i]);
        }
        if !self.frozen && !window.is_window_hovered() {
            // GPUI on Windows reports leaving the window only through this flag (it redraws
            // on change) and never sends `MouseExitEvent`. Screenshots keep their fixed cursor.
            self.hover = None;
        }
        if self.hover.is_none() {
            // Left the plot while the feed held the lock: still hide immediately.
            self.cross = None;
            self.cross_bar = None;
        }

        if self.font.is_none() {
            self.font = Some(window.text_style().font());
        }
        self.draw(bounds, window, cx);

        let us = t0.elapsed().as_secs_f64() * 1e6;
        self.paint_time_ewma_us = if self.paint_time_ewma_us == 0.0 {
            us
        } else {
            self.paint_time_ewma_us * 0.95 + us * 0.05
        };
    }

    fn draw(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        let t = self.theme;
        let scale = window.scale_factor();
        let hair = 1.0 / scale; // one device pixel
        let ox = f32::from(bounds.origin.x);
        let oy = f32::from(bounds.origin.y);
        let plot = self.plot;
        let (pw, ph) = (f32::from(plot.size.width), f32::from(plot.size.height));
        let rect = |x: f32, y: f32, w: f32, h: f32| {
            Bounds::new(point(px(ox + x), px(oy + y)), size(px(w), px(h)))
        };
        let (bw, bh) = (f32::from(bounds.size.width), f32::from(bounds.size.height));

        window.paint_quad(fill(bounds, t.bg));
        window.paint_quad(fill(rect(pw, 0.0, bw - pw, bh), t.panel));
        window.paint_quad(fill(rect(0.0, ph, pw, bh - ph), t.panel));

        let fmt = self.instrument.price;
        window.with_content_mask(Some(ContentMask { bounds: plot }), |window| {
            for l in &self.scene.price_lines {
                window.paint_quad(fill(rect(0.0, l.y, pw, hair), t.grid));
            }
            for l in &self.scene.time_lines {
                window.paint_quad(fill(rect(l.x, 0.0, hair, ph), t.grid));
            }
            for c in &self.scene.candles {
                let color = if c.up { t.up } else { t.down };
                let (bx, bw) = (c.x - c.half_width, c.half_width * 2.0);
                let bh = c.body_bottom - c.body_top;
                // Direction is shown by shape as well as colour (docs/design.md §4): one
                // direction's bodies are hollow. A body needs 3 device pixels each way to
                // have an interior; smaller ones stay filled.
                let hollow = t.hollow.is_hollow(c.up) && bw >= 3.0 * hair && bh >= 3.0 * hair;
                if hollow {
                    // Wick above and below the body only, then a one-device-pixel outline.
                    let wx = c.x - hair * 0.5;
                    if c.body_top > c.wick_top {
                        window.paint_quad(fill(
                            rect(wx, c.wick_top, hair, c.body_top - c.wick_top),
                            color,
                        ));
                    }
                    if c.wick_bottom > c.body_bottom {
                        window.paint_quad(fill(
                            rect(wx, c.body_bottom, hair, c.wick_bottom - c.body_bottom),
                            color,
                        ));
                    }
                    window.paint_quad(fill(rect(bx, c.body_top, bw, hair), color));
                    window.paint_quad(fill(rect(bx, c.body_bottom - hair, bw, hair), color));
                    window.paint_quad(fill(rect(bx, c.body_top, hair, bh), color));
                    window.paint_quad(fill(rect(bx + bw - hair, c.body_top, hair, bh), color));
                } else {
                    window.paint_quad(fill(
                        rect(
                            c.x - hair * 0.5,
                            c.wick_top,
                            hair,
                            c.wick_bottom - c.wick_top,
                        ),
                        color,
                    ));
                    window.paint_quad(fill(rect(bx, c.body_top, bw, bh), color));
                }
            }
            if let Some(p) = self.last_trade {
                let y = self.vp.price_to_y(p.0 as f64) as f32;
                window.paint_quad(fill(rect(0.0, y, pw, hair), t.last.opacity(0.6)));
            }
            if let Some(c) = self.cross {
                // Vertical line through the wick; the horizontal one stops either side of it
                // so the translucent lines don't double up where they cross.
                let (left, right) = (c.x - hair * 0.5, c.x + hair * 0.5);
                window.paint_quad(fill(rect(left, 0.0, hair, ph), t.crosshair));
                window.paint_quad(fill(rect(0.0, c.y, left.max(0.0), hair), t.crosshair));
                window.paint_quad(fill(rect(right, c.y, pw - right, hair), t.crosshair));
            }
        });

        // Axis borders.
        window.paint_quad(fill(rect(pw, 0.0, hair, ph), t.axis_line));
        window.paint_quad(fill(rect(0.0, ph, pw, hair), t.axis_line));

        let line_h = px(LABEL_PX * 1.3);
        for i in 0..self.scene.price_lines.len() {
            let l = self.scene.price_lines[i];
            if l.y < 0.0 || l.y > ph {
                continue;
            }
            let origin = point(px(ox + pw + 8.0), px(oy + l.y - LABEL_PX * 0.65));
            self.paint_label(
                LabelKind::Price,
                l.price.0,
                origin,
                line_h,
                t.text_dim,
                window,
                cx,
                |s| {
                    let _ = fmt.write(l.price, s);
                },
            );
        }
        for i in 0..self.scene.time_lines.len() {
            let l = self.scene.time_lines[i];
            if l.x < 0.0 || l.x > pw - 20.0 {
                continue;
            }
            let origin = point(px(ox + l.x + 4.0), px(oy + ph + 4.0));
            self.paint_label(
                LabelKind::Time,
                l.time.0,
                origin,
                line_h,
                t.text_dim,
                window,
                cx,
                |s| {
                    let _ = l.time.write_axis_label(s);
                },
            );
        }
        if let Some(p) = self.last_trade {
            // `max` first: the plot can be shorter than a label (tiny windows).
            let y = (self.vp.price_to_y(p.0 as f64) as f32)
                .min(ph - LABEL_PX)
                .max(0.0);
            window.paint_quad(fill(
                rect(pw + hair, y - LABEL_PX * 0.75, AXIS_W, LABEL_PX * 1.5),
                t.last,
            ));
            let origin = point(px(ox + pw + 8.0), px(oy + y - LABEL_PX * 0.65));
            self.paint_label(
                LabelKind::LastPrice,
                p.0,
                origin,
                line_h,
                t.on_last,
                window,
                cx,
                |s| {
                    let _ = fmt.write(p, s);
                },
            );
        }
        if !self.vp.auto_price {
            self.draw_manual_scale_indicator(bounds, window, cx);
        }
        if let Some(c) = self.cross {
            self.draw_crosshair_tags(c, bounds, window, cx);
        }
        if let Some(bar) = self.cross_bar {
            self.draw_readout(bar, bounds, window, cx);
        }
    }

    /// Price auto-scale is off: a small accent word in the empty corner under the price axis,
    /// where it never covers a label or moves anything. Double-click the price axis or press
    /// Home to restore auto-scale.
    fn draw_manual_scale_indicator(
        &mut self,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let (ox, oy) = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
        let (pw, ph) = (
            f32::from(self.plot.size.width),
            f32::from(self.plot.size.height),
        );
        let accent = self.theme.accent;
        let w = f32::from(
            self.label(LabelKind::Indicator, 0, accent, window, |s| {
                let _ = s.write_str(MANUAL_LABEL);
            })
            .width,
        );
        let corner_w = f32::from(bounds.size.width) - pw;
        let origin = point(
            px(ox + pw + ((corner_w - w) * 0.5).max(4.0)),
            px(oy + ph + 4.0),
        );
        self.paint_label(
            LabelKind::Indicator,
            0,
            origin,
            px(LABEL_PX * 1.3),
            accent,
            window,
            cx,
            |s| {
                let _ = s.write_str(MANUAL_LABEL);
            },
        );
    }

    /// Price tag on the price axis and time tag on the time axis, drawn over the axis labels.
    fn draw_crosshair_tags(
        &mut self,
        c: Crosshair,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let t = self.theme;
        let scale = window.scale_factor();
        let hair = 1.0 / scale;
        let (ox, oy) = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
        let (pw, ph) = (
            f32::from(self.plot.size.width),
            f32::from(self.plot.size.height),
        );
        let line_h = px(LABEL_PX * 1.3);
        let fmt = self.instrument.price;
        let tag_h = LABEL_PX * 1.5;

        // Price tag: same box as the last-price tag, kept fully inside the plot's height.
        let y = c.y.min(ph - tag_h * 0.5).max(tag_h * 0.5);
        let top = snap(f64::from(y - tag_h * 0.5), scale);
        window.paint_quad(fill(
            Bounds::new(
                point(px(ox + pw + hair), px(oy + top)),
                size(px(AXIS_W), px(tag_h)),
            ),
            t.cross_tag,
        ));
        let price = c.price;
        self.paint_label(
            LabelKind::CrossPrice,
            price.0,
            point(px(ox + pw + 8.0), px(oy + y - LABEL_PX * 0.65)),
            line_h,
            t.on_cross_tag,
            window,
            cx,
            |s| {
                let _ = fmt.write(price, s);
            },
        );

        // Time tag: centred on the vertical line, clamped to the time axis.
        let Some(time) = c.time else { return };
        let period = self.period_ms;
        let text = move |s: &mut FixedStr<32>| {
            let _ = time.write_crosshair_label(period, s);
        };
        let text_w = f32::from(
            self.label(LabelKind::CrossTime, time.0, t.on_cross_tag, window, text)
                .width,
        );
        let box_w = text_w + 12.0;
        let left = snap(
            f64::from((c.x - box_w * 0.5).min(pw - box_w).max(0.0)),
            scale,
        );
        window.paint_quad(fill(
            Bounds::new(
                point(px(ox + left), px(oy + ph + hair)),
                size(px(box_w), px(AXIS_H - hair)),
            ),
            t.cross_tag,
        ));
        self.paint_label(
            LabelKind::CrossTime,
            time.0,
            point(px(ox + left + 6.0), px(oy + ph + 4.0)),
            line_h,
            t.on_cross_tag,
            window,
            cx,
            text,
        );
    }

    /// OHLCV of the hovered bar, top-left of the plot. Re-shaped only when the bar changes
    /// (a new bar is hovered, or the forming bar under the pointer ticks).
    fn draw_readout(
        &mut self,
        bar: Bar,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let t = self.theme;
        if self.readout.as_ref().is_none_or(|(b, _)| *b != bar) {
            let mut s = FixedStr::<128>::new();
            let _ = write_ohlcv(&bar, &self.instrument.price, self.instrument.qty, &mut s);
            let line = self.shape(s.as_str(), t.text_main, window);
            self.readout = Some((bar, line));
        }
        let Some((_, line)) = &self.readout else {
            return;
        };
        let scale = window.scale_factor();
        let (ox, oy) = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
        let box_h = LABEL_PX * 1.6;
        let (left, top) = (snap(6.0, scale), snap(6.0, scale));
        let box_w = snap(f64::from(f32::from(line.width) + 12.0), scale);
        window.paint_quad(fill(
            Bounds::new(
                point(px(ox + left), px(oy + top)),
                size(px(box_w), px(box_h)),
            ),
            t.panel.opacity(0.85),
        ));
        let origin = point(
            px(ox + left + 6.0),
            px(oy + top + box_h * 0.5 - LABEL_PX * 0.65),
        );
        let _ = line.paint(
            origin,
            px(LABEL_PX * 1.3),
            TextAlign::Left,
            None,
            window,
            cx,
        );
    }

    fn shape(&self, text: &str, color: Hsla, window: &mut Window) -> ShapedLine {
        let run = TextRun {
            len: text.len(),
            font: self
                .font
                .clone()
                .unwrap_or_else(|| window.text_style().font()),
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        window.text_system().shape_line(
            SharedString::from(text.to_owned()),
            px(LABEL_PX),
            &[run],
            None,
        )
    }

    /// A cached shaped label. Shaping (and its allocation) happens only on a cache miss,
    /// i.e. the first time a given price/time label is shown.
    fn label(
        &mut self,
        kind: LabelKind,
        key: i64,
        color: Hsla,
        window: &mut Window,
        text: impl FnOnce(&mut FixedStr<32>),
    ) -> &ShapedLine {
        if self.labels.len() >= MAX_CACHED_LABELS && !self.labels.contains_key(&(kind, key)) {
            self.labels.clear();
        }
        if !self.labels.contains_key(&(kind, key)) {
            let mut s = FixedStr::<32>::new();
            text(&mut s);
            let line = self.shape(s.as_str(), color, window);
            self.labels.insert((kind, key), line);
        }
        &self.labels[&(kind, key)]
    }

    /// Paint a cached label (see [`Self::label`]).
    #[allow(clippy::too_many_arguments)]
    fn paint_label(
        &mut self,
        kind: LabelKind,
        key: i64,
        origin: Point<Pixels>,
        line_h: Pixels,
        color: Hsla,
        window: &mut Window,
        cx: &mut App,
        text: impl FnOnce(&mut FixedStr<32>),
    ) {
        let line = self.label(kind, key, color, window, text);
        let _ = line.paint(origin, line_h, TextAlign::Left, None, window, cx);
    }

    fn header(&self) -> impl IntoElement {
        let t = self.theme;
        let mut left = String::with_capacity(96);
        let mut right = String::with_capacity(160);
        let Some(d) = self.store.try_read() else {
            return div(); // feed busy this instant; header refreshes next frame
        };
        let i = &d.product;
        let f = i.price;
        let fp = |p: Option<Price>| p.map(|p| f.format::<32>(p)).unwrap_or_default();
        // The active contract is always shown (decision 0013); unknown is labelled, not hidden.
        let _ = match d.contract_label() {
            Some(c) => write!(left, "{c}  {}    last {}", i.description, fp(d.last_trade)),
            None => write!(
                left,
                "{} (contract unknown)  {}    last {}",
                i.symbol,
                i.description,
                fp(d.last_trade)
            ),
        };
        if d.capabilities.bbo {
            let _ = write!(left, "   bid {}   ask {}", fp(d.bid), fp(d.ask));
        }
        let _ = write!(right, "{}  ·  {}", d.provider, d.status.label());
        // History back-fill is shown as state, not a spinner (docs/design.md).
        if let Some(h) = d.history.label() {
            let _ = write!(right, "  ·  {h}");
        }
        if self.frozen {
            // Frozen clock: only data-derived time is shown, so screenshots are reproducible.
            if let Some(mt) = d.market_time {
                let _ = write!(right, "  ·  {mt}");
            }
        } else {
            let _ = write!(
                right,
                "  ·  {:.0} updates/s  ·  {:.0} frames/s  ·  paint {:.0} µs",
                self.rates.updates_per_sec, self.rates.frames_per_sec, self.paint_time_ewma_us
            );
        }
        if d.stats.reconnects > 0 {
            let _ = write!(right, "  ·  reconnects {}", d.stats.reconnects);
        }
        if d.stats.gaps > 0 || d.stats.rejected > 0 {
            let _ = write!(
                right,
                "  ·  gaps {} rejected {}",
                d.stats.gaps, d.stats.rejected
            );
        }
        if let FeedStatus::Failed(msg) | FeedStatus::Reconnecting { reason: msg, .. } = &d.status {
            let _ = write!(right, "  ·  {msg}");
        }
        div()
            .flex()
            .flex_row()
            .justify_between()
            .px_3()
            .py_1()
            .bg(t.panel)
            .border_b_1()
            .border_color(t.axis_line)
            .text_sm()
            .child(div().text_color(t.text_main).child(left))
            .child(div().text_color(t.text_dim).child(right))
    }
}

impl Render for ChartView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _ = &self.feed; // the view owns the feed; dropping the view stops it
        if !self.focus_requested {
            // Keyboard shortcuts (Home) work without clicking the chart first.
            self.focus_requested = true;
            self.focus.focus(window, cx);
        }
        let t = self.theme;
        let entity = cx.entity();
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(t.bg)
            .child(self.header())
            .child(
                div()
                    .flex_1()
                    .size_full()
                    .track_focus(&self.focus)
                    .on_key_down(cx.listener(Self::on_key_down))
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
                    .on_mouse_move(cx.listener(Self::on_mouse_move))
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
                    .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
                    .on_mouse_exit(cx.listener(Self::on_mouse_exit))
                    .on_scroll(cx.listener(Self::on_scroll))
                    .child(
                        canvas(
                            |_, _, _| (),
                            move |bounds, (), window, cx| {
                                entity.update(cx, |view, cx| view.paint_chart(bounds, window, cx));
                            },
                        )
                        .size_full(),
                    ),
            )
            .child(div().px_3().py_1().text_xs().text_color(t.text_dim).bg(t.panel).child(
                "drag: pan   ·   wheel: zoom time   ·   shift+wheel: scroll   ·   ctrl+wheel, ctrl+drag or drag an axis: scale   ·   double-click price axis: auto-scale   ·   double-click / Home: latest",
            ))
    }
}
