//! External lower-pane line studies (decision 0022: a level source may offer them,
//! `lines_feed::levels::SeriesStudyDef`): each line is one of the source's [`Series`] in the
//! chart's level map for that source, drawn in its own lower pane (`crate::subpane`) on the
//! chart's time axis.
//!
//! - **Gaps stay gaps:** a line is drawn only inside its series' segments; a value holds from
//!   its point to the next point or the segment's end, and no further. Nothing is
//!   interpolated across a gap, and a missing value is never drawn as zero.
//! - **Stale:** when the newest report is older than the study's `stale_after_secs` on the
//!   source's clock (wall live, market in a replay), or the source's map is stale, the pane
//!   says STALE; the lines already end where the reports did.
//! - Building is allocation-free within buffers reserved when the plot widens; paint strokes
//!   paths (GPUI tessellates them: its allocation, as the VWAP's).

use std::sync::Arc;

use gpui::{PathBuilder, Window, point, px};
use lines_core::drawings::TimeAxis;
use lines_core::{UtcMillis, Viewport};
use lines_feed::levels::runtime::LevelStatus;
use lines_feed::levels::{LevelMap, Series, SeriesStudyDef};

use crate::study_style::{self, Look};
use crate::subpane::Scale;
use crate::theme::Theme;

/// Lines a study may have.
pub const MAX_LINES: usize = 8;

/// A stale pane's lines, at this share of their opacity: visibly faded, still 3:1 on the
/// chart (the theme tests check every role).
pub const STALE_FADE: f32 = 0.75;

/// What the pane's title says about its data.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ExtStatus {
    Live,
    /// Fresh data stopped (or the source's map is stale): the lines end where it did.
    Stale,
    /// Nothing to draw, and why.
    Missing(&'static str),
}

/// One line's visible points: plot-local x, value, and whether it starts a new run (after a
/// gap).
#[derive(Clone, Debug, Default)]
pub struct ExtLine {
    pub pts: Vec<(f32, f64, bool)>,
}

/// One external study's frame.
#[derive(Clone, Debug)]
pub struct ExtFrame {
    pub lines: Vec<ExtLine>,
    /// The visible values' extremes (zero included for a study with a zero line).
    pub lo_hi: Option<(f64, f64)>,
    pub map: Option<Arc<LevelMap>>,
    pub status: ExtStatus,
    /// The source's clock at the last build: values and freshness are read at it.
    pub now: UtcMillis,
}

impl Default for ExtFrame {
    fn default() -> Self {
        ExtFrame {
            lines: Vec::new(),
            lo_hi: None,
            map: None,
            status: ExtStatus::Missing("waiting for data"),
            now: UtcMillis(i64::MIN),
        }
    }
}

/// The source's view of this chart: its map, freshness and clock (`LevelsLayer::source_view`).
pub type SourceView<'a> = Option<(Option<&'a Arc<LevelMap>>, LevelStatus, UtcMillis)>;

fn find<'a>(map: &'a LevelMap, key: &str) -> Option<&'a Series> {
    map.series.iter().find(|s| s.key == key).map(|s| &**s)
}

impl ExtFrame {
    /// Room for `n` points per line (only grows; when the plot widens).
    pub fn reserve(&mut self, lines: usize, n: usize) {
        if self.lines.len() < lines {
            self.lines.resize_with(lines, ExtLine::default);
        }
        for l in &mut self.lines {
            if l.pts.capacity() < n {
                l.pts.reserve(n - l.pts.len());
            }
        }
    }

    /// Build the visible lines of `def` from the source's `view`, on the chart's time axis.
    /// Never allocates (points past a line's buffer are dropped; the buffer holds two per
    /// visible bar plus a margin, more than a once-a-bar series can need).
    pub fn build(
        &mut self,
        def: &SeriesStudyDef,
        view: SourceView<'_>,
        axis: &TimeAxis<'_>,
        vp: &Viewport,
    ) {
        for l in &mut self.lines {
            l.pts.clear();
        }
        self.lo_hi = None;
        let Some((map, level_status, now)) = view else {
            self.now = UtcMillis(i64::MIN);
            self.map = None;
            self.status = ExtStatus::Missing("source not on this chart");
            return;
        };
        let Some(map) = map else {
            self.map = None;
            self.status = ExtStatus::Missing("waiting for data");
            return;
        };
        if self.map.as_ref().is_none_or(|m| !Arc::ptr_eq(m, map)) {
            self.map = Some(map.clone());
        }
        self.now = now;
        // The visible time window, a bar either side; with a right inset (the level labels'
        // gutter) it ends at the last bar wholly left of it, as the bars do, so no line is
        // drawn under the gutter or scaled by values no bar shows.
        let left = vp.x_to_index(0.0).floor() as i64 - 1;
        let right = if vp.right_inset > 0.0 {
            (vp.right_index - 0.5).floor() as i64
        } else {
            vp.x_to_index(vp.width).ceil() as i64 + 1
        };
        let (Some(t0), Some(t1)) = (axis.slot_time(left), axis.slot_time(right)) else {
            self.status = ExtStatus::Missing("no bars");
            return;
        };
        let t1 = t1.min(now.max(t0));
        let x = |t: UtcMillis| axis.x_of(vp, t);
        let mut newest: Option<UtcMillis> = None;
        let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
        for (i, line) in def.lines.iter().enumerate().take(self.lines.len()) {
            let Some(s) = find(map, line.key) else {
                continue;
            };
            // Its newest report, never past the source's clock (a synthetic source's day runs
            // ahead of a replay's).
            if let Some(last) = s.segments.last() {
                let n = last.1.min(now);
                newest = Some(newest.map_or(n, |m| m.max(n)));
            }
            let pts = &mut self.lines[i].pts;
            // Two points per half pixel at most (the first and the latest), so a dense series
            // on a long timeframe fits the buffer whole; the scale sees every value.
            let mut push = |xv: Option<f32>, v: f64, brk: bool| {
                let Some(xv) = xv else { return };
                lo = lo.min(v);
                hi = hi.max(v);
                let n = pts.len();
                if !brk && n >= 2 && !pts[n - 1].2 && (xv - pts[n - 2].0).abs() < 0.5 {
                    pts[n - 1] = (xv, v, false);
                    return;
                }
                if n < pts.capacity() {
                    pts.push((xv, v, brk));
                }
            };
            let first = s.segments.partition_point(|g| g.1 < t0);
            for &(from, to) in &s.segments[first..] {
                if from > t1 {
                    break;
                }
                let (a, b) = (from.max(t0), to.min(t1));
                if b < a {
                    continue;
                }
                // The value in force where the visible run starts, each point inside, and the
                // last value held to the run's end.
                let mut held = s.value_at(a);
                if let Some(v) = held {
                    push(x(a), v, true);
                }
                let p0 = s.points.partition_point(|p| p.0 <= a);
                for &(t, v) in &s.points[p0..] {
                    if t > b {
                        break;
                    }
                    push(x(t), v, held.is_none());
                    held = Some(v);
                }
                if let Some(v) = held {
                    push(x(b), v, false);
                }
            }
        }
        if def.zero_line && lo <= hi {
            lo = lo.min(0.0);
            hi = hi.max(0.0);
        }
        self.lo_hi = (lo <= hi).then_some((lo, hi));
        let stale_ms = i64::from(def.stale_after_secs) * 1000;
        self.status = match (newest, level_status) {
            (None, _) => ExtStatus::Missing("no data"),
            (_, LevelStatus::Stale { .. }) => ExtStatus::Stale,
            (Some(n), _) if now.0.saturating_sub(n.0) > stale_ms => ExtStatus::Stale,
            _ => ExtStatus::Live,
        };
    }

    /// Each line's value at `t` (the crosshair's time; nothing after the source's clock), else
    /// its value now while live: the latest value in force, also when its run ended (the next
    /// report not due yet) within the study's stale time.
    pub fn values(&self, def: &SeriesStudyDef, t: Option<UtcMillis>) -> [Option<f64>; MAX_LINES] {
        let mut out = [None; MAX_LINES];
        let Some(map) = &self.map else {
            return out;
        };
        for (i, line) in def.lines.iter().enumerate().take(MAX_LINES) {
            let Some(s) = find(map, line.key) else {
                continue;
            };
            out[i] = match t {
                Some(t) if t <= self.now => s.value_at(t),
                Some(_) => None,
                None if self.status == ExtStatus::Live => {
                    let stale_ms = i64::from(def.stale_after_secs) * 1000;
                    let seg = s.segments.partition_point(|g| g.0 <= self.now);
                    seg.checked_sub(1)
                        .map(|g| s.segments[g].1.min(self.now))
                        .filter(|end| self.now.0.saturating_sub(end.0) <= stale_ms)
                        .and_then(|end| s.value_at(end))
                }
                None => None,
            };
        }
        out
    }
}

/// A value as the pane prints it: one decimal from 10 up, two below (a swing of 0.75 keeps its
/// digits), a minus sign where negative.
pub fn write_value(v: f64, out: &mut impl std::fmt::Write) {
    let _ = if v.abs() >= 10.0 {
        write!(out, "{v:.1}")
    } else {
        write!(out, "{v:.2}")
    };
}

/// The study's line parts (`study_style`): one per line, its own color its theme role and its
/// line type the source's.
pub fn part_defs(def: &'static SeriesStudyDef) -> &'static [study_style::PartDef] {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<usize, &'static [study_style::PartDef]>>> =
        OnceLock::new();
    let key = def as *const SeriesStudyDef as usize;
    let mut cache = CACHE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    cache.entry(key).or_insert_with(|| {
        let v: Vec<study_style::PartDef> = def
            .lines
            .iter()
            .map(|l| study_style::series_part(l.key, l.label, l.color, l.line))
            .collect();
        Box::leak(v.into_boxed_slice())
    })
}

/// Paint `frame`'s lines in the pane (`origin`: its top-left), and the dashed zero line when
/// the study has one and zero is in range.
#[allow(clippy::too_many_arguments)]
pub fn paint(
    frame: &ExtFrame,
    def: &SeriesStudyDef,
    looks: &[Look],
    origin: (f32, f32),
    scale: &Scale,
    pane_w: f32,
    theme: &Theme,
    window: &mut Window,
) {
    let at = |x: f32, v: f64| point(px(origin.0 + x), px(origin.1 + scale.y(v)));
    if def.zero_line && scale.contains(0.0) {
        let y = origin.1 + scale.y(0.0);
        let mut b = PathBuilder::stroke(px(1.0)).dash_array(&[px(4.0), px(4.0)]);
        b.move_to(point(px(origin.0), px(y)));
        b.line_to(point(px(origin.0 + pane_w), px(y)));
        if let Ok(path) = b.build() {
            window.paint_path(path, theme.text_dim);
        }
    }
    for (i, line) in def.lines.iter().enumerate() {
        let (Some(l), Some(look)) = (frame.lines.get(i), looks.get(i)) else {
            continue;
        };
        if l.pts.is_empty() || !look.shown {
            continue;
        }
        let st = look.line();
        let base = theme.series[series_index(line.color)];
        let color = theme.part_color(st.color, base, st.opacity);
        // Stale is a state: the lines fade, so their end never reads as live.
        let color = if frame.status == ExtStatus::Stale {
            color.opacity(color.a * STALE_FADE)
        } else {
            color
        };
        let w = study_style::vwap_px(st.width);
        let d = crate::theme::drawing_dash(st.line, w);
        let seg: [gpui::Pixels; 4] = d.seg.map(px);
        let mut start = 0;
        while start < l.pts.len() {
            let mut end = start + 1;
            while end < l.pts.len() && !l.pts[end].2 {
                end += 1;
            }
            let run = &l.pts[start..end];
            if run.len() > 1 {
                let mut b = PathBuilder::stroke(px(w));
                if d.n > 0 {
                    b = b.dash_array(&seg[..d.n]);
                }
                b.move_to(at(run[0].0, run[0].1));
                for p in &run[1..] {
                    b.line_to(at(p.0, p.1));
                }
                if let Ok(path) = b.build() {
                    window.paint_path(path, color);
                }
            }
            start = end;
        }
    }
}

/// The theme's series color index for a role.
pub fn series_index(c: lines_feed::levels::SeriesColor) -> usize {
    use lines_feed::levels::SeriesColor as C;
    match c {
        C::Foreground => 0,
        C::Orange => 1,
        C::Blue => 2,
        C::Amber => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lines_core::style::LineType;
    use lines_core::{Bar, BarsView, Price, ProductId, Qty};
    use lines_feed::levels::{SeriesColor, SeriesLineDef};

    const DEF: SeriesStudyDef = SeriesStudyDef {
        id: "osc",
        name: "Osc",
        lines: &[SeriesLineDef {
            key: "osc.a",
            label: "A",
            color: SeriesColor::Foreground,
            line: LineType::Solid,
        }],
        zero_line: true,
        stale_after_secs: 300,
    };

    fn t(m: i64) -> UtcMillis {
        UtcMillis(m * 60_000)
    }

    fn bars() -> Vec<Bar> {
        (0..60)
            .map(|i| Bar {
                time: t(i),
                open: Price(100),
                high: Price(100),
                low: Price(100),
                close: Price(100),
                volume: Qty(1),
            })
            .collect()
    }

    fn map(points: &[(i64, f64)], segments: &[(i64, i64)]) -> Arc<LevelMap> {
        let s = Series {
            key: "osc.a".into(),
            points: points.iter().map(|(m, v)| (t(*m), *v)).collect(),
            segments: segments.iter().map(|(a, b)| (t(*a), t(*b))).collect(),
        };
        Arc::new(
            LevelMap::builder("demo", ProductId(1), t(0))
                .series(Arc::new(s))
                .build(25)
                .unwrap(),
        )
    }

    fn vp() -> Viewport {
        let mut vp = Viewport::default();
        vp.set_size(600.0, 200.0);
        vp.bar_px = 10.0;
        vp.right_index = 59.0;
        vp
    }

    /// A gap breaks the line (a new run), values hold to their segment's end, zero joins the
    /// range for a zero-line study, and nothing is drawn past the source's clock.
    #[test]
    fn lines_follow_segments_and_stop_at_gaps() {
        let b = bars();
        let view = BarsView::fixed(&b, 60_000);
        let axis = TimeAxis::new(view);
        let m = map(&[(10, 2.0), (20, 3.0), (40, -1.0)], &[(10, 30), (40, 50)]);
        let mut f = ExtFrame::default();
        f.reserve(1, 256);
        f.build(
            &DEF,
            Some((Some(&m), LevelStatus::Current, t(55))),
            &axis,
            &vp(),
        );
        let pts = &f.lines[0].pts;
        let runs = pts.iter().filter(|p| p.2).count();
        assert_eq!(runs, 2, "the gap 30-40 splits the line: {pts:?}");
        // The first run holds 3.0 from minute 20 to its end at 30.
        let x30 = axis.x_of(&vp(), t(30)).unwrap();
        assert!(pts.iter().any(|p| p.0 == x30 && p.1 == 3.0));
        // Nothing between 30 and 40.
        let (x31, x39) = (
            axis.x_of(&vp(), t(31)).unwrap(),
            axis.x_of(&vp(), t(39)).unwrap(),
        );
        assert!(!pts.iter().any(|p| p.0 >= x31 && p.0 <= x39), "{pts:?}");
        assert_eq!(f.lo_hi, Some((-1.0, 3.0)));
        assert_eq!(f.status, ExtStatus::Live);
        assert_eq!(
            f.values(&DEF, Some(t(35)))[0],
            None,
            "a gap reads as no value"
        );
        assert_eq!(f.values(&DEF, Some(t(25)))[0], Some(3.0));
        // The clock at 45: the second run ends there, not at 50.
        f.build(
            &DEF,
            Some((Some(&m), LevelStatus::Current, t(45))),
            &axis,
            &vp(),
        );
        let x46 = axis.x_of(&vp(), t(46)).unwrap();
        assert!(f.lines[0].pts.iter().all(|p| p.0 < x46));
    }

    /// With the level labels' gutter (a right inset), scrolled back from the newest bar, the
    /// lines stop where the bars do: nothing is drawn under the gutter, and a value only the
    /// gutter would cover doesn't set the pane's scale.
    #[test]
    fn lines_stop_at_the_gutter_like_the_bars() {
        let b = bars();
        let axis = TimeAxis::new(BarsView::fixed(&b, 60_000));
        let m = map(&[(0, 1.0), (30, 2.0), (45, 99.0)], &[(0, 59)]);
        let mut vp = vp();
        vp.right_inset = 96.0;
        vp.right_index = 40.0;
        let mut f = ExtFrame::default();
        f.reserve(1, 256);
        f.build(
            &DEF,
            Some((Some(&m), LevelStatus::Current, t(59))),
            &axis,
            &vp,
        );
        let pts = &f.lines[0].pts;
        assert!(!pts.is_empty());
        assert!(
            pts.iter().all(|p| f64::from(p.0) <= vp.edge()),
            "edge {}: {pts:?}",
            vp.edge()
        );
        assert_eq!(f.lo_hi, Some((0.0, 2.0)), "99 sits under the gutter");
    }

    /// Fresh data stopped: STALE (the source's clock past the newest report by more than the
    /// study's threshold), or the source's map stale; missing data says so, never zero.
    #[test]
    fn staleness_and_missing_data_are_said() {
        let b = bars();
        let axis = TimeAxis::new(BarsView::fixed(&b, 60_000));
        let m = map(&[(10, 2.0)], &[(10, 20)]);
        let mut f = ExtFrame::default();
        f.reserve(1, 256);
        f.build(
            &DEF,
            Some((Some(&m), LevelStatus::Current, t(24))),
            &axis,
            &vp(),
        );
        assert_eq!(f.status, ExtStatus::Live, "4 minutes: fresh");
        f.build(
            &DEF,
            Some((Some(&m), LevelStatus::Current, t(26))),
            &axis,
            &vp(),
        );
        assert_eq!(f.status, ExtStatus::Stale, "6 minutes: stale");
        assert_eq!(f.values(&DEF, None)[0], None, "no newest value while stale");
        f.build(
            &DEF,
            Some((Some(&m), LevelStatus::Stale { age_ms: 1 }, t(21))),
            &axis,
            &vp(),
        );
        assert_eq!(f.status, ExtStatus::Stale);
        f.build(
            &DEF,
            Some((None, LevelStatus::Missing, t(21))),
            &axis,
            &vp(),
        );
        assert!(matches!(f.status, ExtStatus::Missing(_)));
        assert!(f.lines[0].pts.is_empty() && f.lo_hi.is_none());
        f.build(&DEF, None, &axis, &vp());
        assert_eq!(f.status, ExtStatus::Missing("source not on this chart"));
    }

    /// A once-a-minute series on hourly bars: the whole visible span is drawn (decimated to
    /// the pixel), never cut where a buffer filled; values and freshness are read at the
    /// source's clock, never from points after it.
    #[test]
    fn dense_series_fit_and_nothing_reads_past_the_clock() {
        let hourly: Vec<Bar> = (0..60)
            .map(|i| Bar {
                time: UtcMillis(i * 3_600_000),
                ..bars()[0]
            })
            .collect();
        let view = BarsView::fixed(&hourly, 3_600_000);
        let axis = TimeAxis::new(view);
        let minutes: Vec<(i64, f64)> = (0..60 * 60).map(|m| (m, (m % 7) as f64)).collect();
        let m = map(&minutes, &[(0, 60 * 60 - 1)]);
        let mut f = ExtFrame::default();
        let mut v = vp();
        v.bar_px = 10.0;
        f.reserve(1, 600 * 4 + 64);
        f.build(
            &DEF,
            Some((Some(&m), LevelStatus::Current, t(3600))),
            &axis,
            &v,
        );
        let pts = &f.lines[0].pts;
        let last_x = pts.last().unwrap().0;
        let end_x = axis.x_of(&v, t(3599)).unwrap();
        assert!(
            (last_x - end_x).abs() < 1.0,
            "drawn to the end: {last_x} vs {end_x}"
        );
        assert!(pts.len() < 600 * 4, "decimated: {}", pts.len());
        assert_eq!(f.lo_hi, Some((0.0, 6.0)), "every value scales");
        // The clock at minute 100: the newest value is minute 100's, and the line ends there.
        f.build(
            &DEF,
            Some((Some(&m), LevelStatus::Current, t(100))),
            &axis,
            &v,
        );
        assert_eq!(f.values(&DEF, None)[0], Some((100 % 7) as f64));
        assert_eq!(f.status, ExtStatus::Live);
        // The crosshair reads up to the clock, never past it (a replay's map can hold more).
        assert_eq!(f.values(&DEF, Some(t(50)))[0], Some((50 % 7) as f64));
        assert_eq!(f.values(&DEF, Some(t(200)))[0], None);
    }

    /// Live with the newest run ended (the next report not due yet): the title keeps showing
    /// the latest value until the study's stale time.
    #[test]
    fn a_live_title_holds_the_last_report() {
        let axis_bars = bars();
        let view = BarsView::fixed(&axis_bars, 60_000);
        let axis = TimeAxis::new(view);
        let m = map(&[(0, 1.5), (10, 2.5)], &[(0, 10)]);
        let mut f = ExtFrame::default();
        f.reserve(1, 256);
        let at = |f: &mut ExtFrame, now: i64| {
            f.build(
                &DEF,
                Some((Some(&m), LevelStatus::Current, t(now))),
                &axis,
                &vp(),
            );
            (f.status, f.values(&DEF, None)[0])
        };
        assert_eq!(at(&mut f, 12), (ExtStatus::Live, Some(2.5)));
        assert_eq!(at(&mut f, 16), (ExtStatus::Stale, None));
    }

    #[test]
    fn values_print_with_their_digits() {
        let s = |v| {
            let mut o = String::new();
            write_value(v, &mut o);
            o
        };
        assert_eq!(s(22.84), "22.8");
        assert_eq!(s(0.75), "0.75");
        assert_eq!(s(-0.5), "-0.50");
    }
}
