//! The chart window: header + candlestick canvas with pan/zoom.
//!
//! Frame flow (decision 0001): a UI-thread timer compares the store's atomic revision and
//! calls `cx.notify()` only when data changed (so 5k msgs/s never means 5k redraws). In
//! paint we `try_lock` the store, update the viewport and rebuild the reusable `Scene` (no
//! heap allocation), release the lock, then emit quads. Text labels are shaped once and
//! cached. Coordinates handed to GPUI are relative to the plot origin before the `f32` cast.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    App, Bounds, ContentMask, Context, Font, Hsla, IntoElement, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Point, Render, ScrollEvent, ShapedLine,
    SharedString, Styled, TextAlign, TextRun, Window, canvas, div, fill, point, prelude::*, px,
    size,
};
use lines_core::{FixedStr, Instrument, Price, Scene, Viewport};
use lines_feed::FeedHandle;
use lines_feed::store::{ChartStore, FeedStatus};

use crate::theme::Theme;

const AXIS_W: f32 = 78.0;
const AXIS_H: f32 = 24.0;
const LABEL_PX: f32 = 12.0;
const MAX_CACHED_LABELS: usize = 4096;

#[derive(Copy, Clone, PartialEq, Eq, Hash)]
enum LabelKind {
    Price,
    Time,
    LastPrice,
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
    instrument: Instrument,
    last_trade: Option<Price>,
    series_len: usize,
    series_trimmed: u64,
    plot: Bounds<Pixels>,
    drag_from: Option<Point<Pixels>>,
    labels: HashMap<(LabelKind, i64), ShapedLine>,
    font: Option<Font>,
    rates: Rates,
    paint_time_ewma_us: f64,
}

impl ChartView {
    pub fn new(feed: FeedHandle, theme: Theme, frozen: bool, cx: &mut Context<Self>) -> Self {
        let store = feed.store.clone();
        let instrument = store.read().instrument.clone();
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
            drag_from: None,
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

    fn on_mouse_down(&mut self, ev: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.plot.contains(&ev.position) {
            return;
        }
        if ev.click_count >= 2 {
            self.vp.reset(self.series_len);
            self.drag_from = None;
            cx.notify();
        } else {
            self.drag_from = Some(ev.position);
        }
    }

    fn on_mouse_move(&mut self, ev: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(from) = self.drag_from else { return };
        if ev.pressed_button != Some(MouseButton::Left) {
            self.drag_from = None;
            return;
        }
        let d = ev.position - from;
        self.drag_from = Some(ev.position);
        self.vp.pan(f64::from(d.x), f64::from(d.y), self.series_len);
        cx.notify();
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.drag_from = None;
    }

    fn on_scroll(&mut self, ev: &ScrollEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.plot.contains(&ev.position) {
            return;
        }
        let dy = f64::from(ev.delta.pixel_delta(window.line_height()).y);
        if dy == 0.0 {
            return;
        }
        // Wheel up (positive delta) zooms in. ~10% per notch.
        let factor = (dy * 0.005).exp();
        let local = ev.position - self.plot.origin;
        if ev.modifiers.control {
            self.vp.zoom_price_at(f64::from(local.y), factor);
        } else {
            self.vp.zoom_at(f64::from(local.x), factor, self.series_len);
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
        self.plot = Bounds::new(bounds.origin, size(px(plot_w), px(plot_h)));
        self.vp.set_size(plot_w as f64, plot_h as f64);
        // Only allocates when the window grows wider than ever before.
        self.scene.ensure_capacity(plot_w as f64);

        // Sample the feed. If it holds the lock this instant, redraw the previous scene.
        if let Some(d) = self.store.try_read() {
            let trimmed = d.series.trimmed();
            if trimmed > self.series_trimmed && !self.vp.follow_live {
                // Bars dropped from the front shift indices; keep the view anchored.
                self.vp.right_index -= (trimmed - self.series_trimmed) as f64;
            }
            self.series_trimmed = trimmed;
            self.series_len = d.series.len();
            self.last_trade = d.last_trade;
            let fmt = self.instrument.price;
            let bars = d.series.bars();
            self.vp.on_data(bars, fmt.tick);
            self.scene
                .build(bars, &self.vp, &fmt, scale, d.series.period_ms());
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
                window.paint_quad(fill(
                    rect(
                        c.x - hair * 0.5,
                        c.wick_top,
                        hair,
                        c.wick_bottom - c.wick_top,
                    ),
                    color,
                ));
                window.paint_quad(fill(
                    rect(
                        c.x - c.half_width,
                        c.body_top,
                        c.half_width * 2.0,
                        c.body_bottom - c.body_top,
                    ),
                    color,
                ));
            }
            if let Some(p) = self.last_trade {
                let y = self.vp.price_to_y(p.0 as f64) as f32;
                window.paint_quad(fill(rect(0.0, y, pw, hair), t.last.opacity(0.6)));
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
    }

    /// Paint a cached label. Shaping (and its allocation) happens only on a cache miss,
    /// i.e. the first time a given price/time label is shown.
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
        if self.labels.len() >= MAX_CACHED_LABELS {
            self.labels.clear();
        }
        let font = self.font.clone();
        let line = self.labels.entry((kind, key)).or_insert_with(|| {
            let mut s = FixedStr::<32>::new();
            text(&mut s);
            let run = TextRun {
                len: s.len(),
                font: font.unwrap_or_else(|| window.text_style().font()),
                color,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            window.text_system().shape_line(
                SharedString::from(s.as_str().to_owned()),
                px(LABEL_PX),
                &[run],
                None,
            )
        });
        let _ = line.paint(origin, line_h, TextAlign::Left, None, window, cx);
    }

    fn header(&self) -> impl IntoElement {
        let t = self.theme;
        let mut left = String::with_capacity(96);
        let mut right = String::with_capacity(160);
        let Some(d) = self.store.try_read() else {
            return div(); // feed busy this instant; header refreshes next frame
        };
        let i = &d.instrument;
        let f = i.price;
        let fp = |p: Option<Price>| p.map(|p| f.format::<32>(p)).unwrap_or_default();
        let _ = write!(
            left,
            "{}  {}    last {}",
            i.symbol,
            i.description,
            fp(d.last_trade)
        );
        if d.capabilities.bbo {
            let _ = write!(left, "   bid {}   ask {}", fp(d.bid), fp(d.ask));
        }
        let _ = write!(right, "{}  ·  {}", d.provider, d.status.label());
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _ = &self.feed; // the view owns the feed; dropping the view stops it
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
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
                    .on_mouse_move(cx.listener(Self::on_mouse_move))
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
                    .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
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
                "drag: pan   ·   wheel: zoom time   ·   ctrl+wheel: zoom price   ·   double-click: reset & follow live",
            ))
    }
}
