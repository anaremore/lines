//! Scheduled economic releases on a chart (roadmap 8b, phase 2): a dashed vertical at each
//! release in view that moves the chart's product, with a tag at the foot of the plot: high
//! importance filled in the warning color, medium outlined. Charts of products no release
//! affects show none. The schedule is the bundled one the trading core reads.

use std::collections::HashMap;
use std::sync::Arc;

use gpui::{
    App, Bounds, Pixels, ShapedLine, SharedString, TextAlign, TextRun, Window, fill, point, px,
    size,
};
use lines_core::econ::{EconSchedule, Importance};
use lines_core::geometry::snap;
use lines_core::{BarsView, Viewport};
use lines_feed::ChartData;

use crate::chart_view::ChartLayer;
use crate::theme::Theme;

const TAG_PX: f32 = 11.0;
/// Marks kept per frame (a chart never shows more releases than this).
const MAX_MARKS: usize = 64;

pub struct EconLayer {
    schedule: Arc<EconSchedule>,
    /// This frame's marks: plot-local x, the event's release, high importance.
    marks: Vec<(f32, usize, bool)>,
    /// Shaped tags by release (built once each).
    shaped: HashMap<(usize, bool), ShapedLine>,
}

impl EconLayer {
    pub fn new() -> EconLayer {
        EconLayer {
            schedule: crate::econ_ui::schedule(),
            marks: Vec::with_capacity(MAX_MARKS),
            shaped: HashMap::new(),
        }
    }
}

impl Default for EconLayer {
    fn default() -> Self {
        Self::new()
    }
}

/// The releases in `[from, to)` that move `symbol` (medium importance and up): (time,
/// release, high).
pub fn marks_in<'a>(
    s: &'a EconSchedule,
    symbol: &'a str,
    from: lines_core::UtcMillis,
    to: lines_core::UtcMillis,
) -> impl Iterator<Item = (lines_core::UtcMillis, usize, bool)> + 'a {
    s.upcoming(from)
        .take_while(move |e| e.at < to)
        .filter_map(move |e| {
            let r = s.release(e);
            (r.importance >= Importance::Medium && r.affects(symbol)).then_some((
                e.at,
                e.release,
                r.importance == Importance::High,
            ))
        })
}

impl ChartLayer for EconLayer {
    fn prepare(&mut self, data: &ChartData, bars: BarsView<'_>, vp: &Viewport) {
        self.marks.clear();
        let r = vp.visible_range(bars.len());
        if r.is_empty() {
            return;
        }
        let (from, to) = (bars.bars[r.start].time, bars.end(r.end - 1));
        let axis = lines_core::drawings::TimeAxis::new(bars);
        for (at, release, high) in marks_in(&self.schedule, data.product.symbol, from, to) {
            if self.marks.len() == MAX_MARKS {
                break;
            }
            if let Some(x) = axis.x_of(vp, at) {
                self.marks.push((x, release, high));
            }
        }
    }

    fn paint(
        &mut self,
        plot: Bounds<Pixels>,
        _vp: &Viewport,
        theme: &Theme,
        window: &mut Window,
        cx: &mut App,
    ) {
        if self.marks.is_empty() {
            return;
        }
        let t = theme;
        let scale = window.scale_factor();
        let hair = 1.0 / scale;
        let (ox, oy) = (f32::from(plot.origin.x), f32::from(plot.origin.y));
        let (pw, ph) = (f32::from(plot.size.width), f32::from(plot.size.height));
        let quad = |window: &mut Window, x: f32, y: f32, w: f32, h: f32, c| {
            window.paint_quad(fill(
                Bounds::new(point(px(ox + x), px(oy + y)), size(px(w), px(h))),
                c,
            ));
        };
        let box_h = TAG_PX * 1.5;
        let tag_y = snap(f64::from((ph - box_h - 2.0).max(0.0)), scale);
        for &(x, release, high) in &self.marks {
            if !(0.0..pw).contains(&x) {
                continue;
            }
            let x = snap(f64::from(x), scale);
            // Dashed vertical over the plot, down to the tag.
            let (dash, gap): (f32, f32) = (4.0, 3.0);
            let mut y = 0.0;
            while y < tag_y {
                quad(window, x, y, hair, dash.min(tag_y - y), t.control_edge);
                y += dash + gap;
            }
            let line = self.shaped.entry((release, high)).or_insert_with(|| {
                let text = self.schedule.releases[release].name.clone();
                let run = TextRun {
                    len: text.len(),
                    font: window.text_style().font(),
                    color: if high { t.on_warn } else { t.text_main },
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };
                window
                    .text_system()
                    .shape_line(SharedString::from(text), px(TAG_PX), &[run], None)
            });
            let w = f32::from(line.width) + 8.0;
            let left = snap(
                f64::from((x - w * 0.5).clamp(0.0, (pw - w).max(0.0))),
                scale,
            );
            if high {
                quad(window, left, tag_y, w, box_h, t.warn);
            } else {
                quad(window, left, tag_y, w, box_h, t.panel);
                for (bx, by, bw, bh) in [
                    (left, tag_y, w, hair),
                    (left, tag_y + box_h - hair, w, hair),
                    (left, tag_y, hair, box_h),
                    (left + w - hair, tag_y, hair, box_h),
                ] {
                    quad(window, bx, by, bw, bh, t.control_edge);
                }
            }
            let _ = line.paint(
                point(
                    px(ox + left + 4.0),
                    px(oy + tag_y + box_h * 0.5 - TAG_PX * 0.65),
                ),
                px(TAG_PX * 1.3),
                TextAlign::Left,
                None,
                window,
                cx,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lines_core::UtcMillis;

    /// The bundled schedule's releases that move ES in a week of November 2026, and none for
    /// a product no release affects.
    #[test]
    fn marks_follow_the_product_and_the_range() {
        let s = lines_feed::econ::bundled();
        let from = UtcMillis::parse_iso("2026-11-09T00:00:00Z").unwrap();
        let to = UtcMillis::parse_iso("2026-11-16T00:00:00Z").unwrap();
        let es: Vec<_> = marks_in(&s, "ES", from, to).collect();
        assert!(!es.is_empty(), "CPI or PPI that week");
        assert!(es.iter().all(|(at, _, _)| *at >= from && *at < to));
        assert!(es.iter().all(|(_, r, _)| s.releases[*r].affects("ES")
            && s.releases[*r].importance >= Importance::Medium));
        assert_eq!(marks_in(&s, "BTC", from, to).count(), 0);
    }
}
