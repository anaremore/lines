use super::*;
use crate::bars::BarSeries;
use crate::price::{DisplayStyle, PriceScale};
use crate::qty::Qty;
use crate::style::LineType;
use crate::timeframe::{Derived, Timeframe};

const M: i64 = 60_000;

fn t(s: &str) -> UtcMillis {
    UtcMillis::parse_iso(s).unwrap()
}

fn es() -> PriceFormat {
    PriceFormat::new(
        PriceScale::decimal(2),
        DisplayStyle::Decimal { digits: 2 },
        25,
    )
}

/// 120 one-minute bars from 14:00Z, prices walking.
fn series() -> BarSeries {
    let mut s = BarSeries::new(M, 1_000);
    let start = t("2026-11-05T14:00:00Z").0;
    for i in 0..120 {
        let p = 600_000 + (i % 9) * 25;
        s.upsert(Bar {
            time: UtcMillis(start + i * M),
            open: Price(p),
            high: Price(p + 100),
            low: Price(p - 75),
            close: Price(p + 25),
            volume: Qty(5),
        });
    }
    s
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

fn trend(a: (&str, i64), b: (&str, i64)) -> Drawing {
    Drawing::new(
        Tool::Trend,
        Anchor {
            time: t(a.0),
            price: Price(a.1),
        },
        Anchor {
            time: t(b.0),
            price: Price(b.1),
        },
        Style::DEFAULT,
    )
}

/// Drawings live in data space: viewed on 15m and back on 1m, a trend line's anchors and its
/// 1-minute geometry are exactly as before; on 15m they sit proportionally inside the
/// buckets holding them.
#[test]
fn data_space_round_trip_through_timeframes() {
    let s = series();
    let d = trend(
        ("2026-11-05T14:07:00Z", 600_100),
        ("2026-11-05T15:22:00Z", 600_500),
    );
    let one = TimeAxis::new(BarsView::fixed(s.bars(), M));
    let vp = vp();
    let before = project(&d, &one, &vp).unwrap();
    assert_eq!(one.index_of(d.a.time), Some(7.0));
    assert_eq!(one.index_of(d.b.time), Some(82.0));

    let mut fifteen = Derived::new(Timeframe::Minutes(15), s.capacity());
    fifteen.update(&s, None);
    let axis = TimeAxis::new(fifteen.view());
    // 14:07 is 7/15 into the first bucket; 15:22 is 7/15 into the sixth.
    assert_eq!(axis.index_of(d.a.time), Some(7.0 / 15.0));
    assert_eq!(axis.index_of(d.b.time), Some(5.0 + 7.0 / 15.0));
    let on15 = project(&d, &axis, &vp).unwrap();
    assert_eq!(
        on15.p0.1, before.p0.1,
        "prices map the same on every timeframe"
    );

    // Nothing was changed by viewing: back on 1m it is the same drawing, drawn the same.
    assert_eq!(project(&d, &one, &vp).unwrap(), before);

    // Editing on 15m snaps the moved anchor to the 15m bar under the pointer.
    let x = vp.index_to_x(3.0);
    let at = snap_anchor(&axis, &vp, 25, false, x, 100.0).unwrap();
    assert_eq!(at.time, t("2026-11-05T14:45:00Z"));
    assert_eq!(at.price.0 % 25, 0);
}

#[test]
fn time_axis_extrapolates_beyond_the_bars() {
    let s = series();
    let axis = TimeAxis::new(BarsView::fixed(s.bars(), M));
    assert_eq!(axis.index_of(t("2026-11-05T13:58:00Z")), Some(-2.0));
    assert_eq!(axis.index_of(t("2026-11-05T16:05:00Z")), Some(125.0));
    assert_eq!(axis.slot_time(-2), Some(t("2026-11-05T13:58:00Z")));
    assert_eq!(axis.slot_time(125), Some(t("2026-11-05T16:05:00Z")));
    let empty = TimeAxis::new(BarsView::fixed(&[], M));
    assert_eq!(empty.index_of(t("2026-11-05T14:00:00Z")), None);
}

#[test]
fn snapping_to_the_grid_and_the_magnet() {
    let s = series();
    let axis = TimeAxis::new(BarsView::fixed(s.bars(), M));
    let vp = vp();
    // Bar 100 (price 600_100: open 600_100, high 600_200, low 600_025, close 600_125).
    let x = vp.index_to_x(100.2);
    let y = vp.price_to_y(600_190.0);
    let grid = snap_anchor(&axis, &vp, 25, false, x, y).unwrap();
    assert_eq!(grid.time, t("2026-11-05T15:40:00Z"));
    assert_eq!(grid.price, Price(600_200));
    let y = vp.price_to_y(600_160.0);
    assert_eq!(
        snap_anchor(&axis, &vp, 25, false, x, y).unwrap().price,
        Price(600_150),
        "grid: nearest tick"
    );
    assert_eq!(
        snap_anchor(&axis, &vp, 25, true, x, y).unwrap().price,
        Price(600_125),
        "magnet: nearest of O/H/L/C (close 6001.25)"
    );
    // Off the bars the magnet has nothing to snap to: the grid.
    let right = vp.index_to_x(130.0);
    let a = snap_anchor(&axis, &vp, 25, true, right, y).unwrap();
    assert_eq!(a.price, Price(600_150));
    assert_eq!(a.time, t("2026-11-05T16:10:00Z"));
    assert_eq!(grid_price(600_012.4, 25), Price(600_000));
    assert_eq!(grid_price(-12.6, 25), Price(-25));
}

#[test]
fn hit_testing_lines_zones_and_handles() {
    let s = series();
    let axis = TimeAxis::new(BarsView::fixed(s.bars(), M));
    let vp = vp();
    let d = Drawing {
        id: 7,
        ..trend(
            ("2026-11-05T15:00:00Z", 600_000),
            ("2026-11-05T15:30:00Z", 600_400),
        )
    };
    let p = project(&d, &axis, &vp).unwrap();
    let mid = ((p.p0.0 + p.p1.0) / 2.0, (p.p0.1 + p.p1.1) / 2.0);
    assert_eq!(
        hit(&p, mid.0, mid.1 + 3.0, 4.0, false, 5.0),
        Some(Hit::Body)
    );
    assert_eq!(hit(&p, mid.0, mid.1 + 30.0, 4.0, false, 5.0), None);
    // Handles only when selected.
    assert_eq!(
        hit(&p, p.p0.0 + 2.0, p.p0.1, 4.0, false, 5.0),
        Some(Hit::Body)
    );
    assert_eq!(
        hit(&p, p.p0.0 + 2.0, p.p0.1, 4.0, true, 5.0),
        Some(Hit::Handle(0))
    );
    assert_eq!(
        hit(&p, p.p1.0, p.p1.1 - 4.0, 4.0, true, 5.0),
        Some(Hit::Handle(1))
    );
    // A segment ends at its anchors.
    assert_eq!(hit(&p, p.p1.0 + 30.0, p.p1.1, 4.0, false, 5.0), None);

    // A ray goes on to the plot's edge; a zone is hit anywhere inside.
    let ray = Drawing {
        id: 8,
        tool: Tool::Ray,
        ..d.clone()
    };
    let r = project(&ray, &axis, &vp).unwrap();
    assert!(r.p1.0 >= p.p1.0 && (r.p1.0 - 800.0).abs() < 0.01 || r.p1.1.abs() < 0.01);
    let beyond = (
        p.p1.0 + 20.0,
        p.p1.1 + 20.0 * (p.p1.1 - p.p0.1) / (p.p1.0 - p.p0.0),
    );
    assert_eq!(
        hit(&r, beyond.0, beyond.1, 4.0, false, 5.0),
        Some(Hit::Body)
    );
    let zone = Drawing {
        id: 9,
        tool: Tool::Rect,
        ..d.clone()
    };
    let z = project(&zone, &axis, &vp).unwrap();
    // A zone is hit on its edges; inside it the chart keeps panning.
    assert_eq!(hit(&z, mid.0, mid.1 - 20.0, 4.0, false, 5.0), None);
    assert_eq!(
        hit(&z, z.p0.0 + 2.0, mid.1, 4.0, false, 5.0),
        Some(Hit::Body)
    );
    assert_eq!(
        hit(&z, mid.0, z.p1.1 - 1.0, 4.0, false, 5.0),
        Some(Hit::Body)
    );

    // `pick`: the selected drawing's handles, then the top-most (last drawn) body.
    let drawn = [p, z];
    assert_eq!(
        pick(&drawn, None, z.p0.0, z.p0.1 - 30.0, 4.0, 5.0),
        Some((9, Hit::Body))
    );
    assert_eq!(
        pick(&drawn, None, mid.0, mid.1, 4.0, 5.0),
        Some((7, Hit::Body))
    );
    assert_eq!(
        pick(&drawn, Some(7), p.p0.0, p.p0.1, 4.0, 5.0),
        Some((7, Hit::Handle(0)))
    );
    assert_eq!(pick(&drawn, None, 5.0, 5.0, 4.0, 5.0), None);
}

#[test]
fn horizontal_and_vertical_lines_span_the_plot() {
    let s = series();
    let axis = TimeAxis::new(BarsView::fixed(s.bars(), M));
    let vp = vp();
    let a = Anchor {
        time: t("2026-11-05T15:30:00Z"),
        price: Price(600_500),
    };
    let h = project(&Drawing::new(Tool::HLine, a, a, Style::DEFAULT), &axis, &vp).unwrap();
    assert_eq!((h.p0.0, h.p1.0), (0.0, 800.0));
    assert_eq!(h.p0.1, vp.price_to_y(600_500.0) as f32);
    let v = project(&Drawing::new(Tool::VLine, a, a, Style::DEFAULT), &axis, &vp).unwrap();
    assert_eq!((v.p0.1, v.p1.1), (0.0, 400.0));
    let r = project(&Drawing::new(Tool::HRay, a, a, Style::DEFAULT), &axis, &vp).unwrap();
    assert_eq!(r.p0.0, vp.index_to_x(90.0) as f32);
    assert_eq!(r.p1.0, 800.0);
    // Off-screen: skipped.
    let high = Anchor {
        price: Price(700_000),
        ..a
    };
    assert_eq!(
        project(
            &Drawing::new(Tool::HLine, high, high, Style::DEFAULT),
            &axis,
            &vp
        ),
        None
    );
}

#[test]
fn measure_reads_ticks_points_dollars_bars_and_time() {
    let s = series();
    let axis = TimeAxis::new(BarsView::fixed(s.bars(), M));
    let a = Anchor {
        time: t("2026-11-05T14:10:00Z"),
        price: Price(600_000),
    };
    let b = Anchor {
        time: t("2026-11-05T15:35:00Z"),
        price: Price(600_300),
    };
    // ES: 3.00 points = 12 ticks = $150 a contract.
    let m = measure(a, b, &axis, &es(), Ratio::new(50, 1));
    assert_eq!(
        m,
        MeasureStats {
            units: 300,
            ticks: 12,
            cents: 15_000,
            bars: 85,
            duration_ms: 85 * M,
        }
    );
    let text = |m: &MeasureStats, parts| {
        let mut t = crate::FixedStr::<128>::new();
        write_measure(m, &es(), parts, &mut t).unwrap();
        t.as_str().to_owned()
    };
    assert_eq!(
        text(&m, MeasureParts::ALL),
        "+12 ticks \u{b7} +3.00 pts \u{b7} +$150.00/contract \u{b7} 85 bars \u{b7} 1h 25m"
    );
    let down = measure(b, a, &axis, &es(), Ratio::new(50, 1));
    assert_eq!(
        text(&down, MeasureParts::ALL),
        "-12 ticks \u{b7} -3.00 pts \u{b7} -$150.00/contract \u{b7} 85 bars \u{b7} 1h 25m"
    );
    // P&L display off: no ticks, no money; ticks only; thousands separators.
    let off = MeasureParts {
        ticks: false,
        money: false,
        ..MeasureParts::ALL
    };
    assert_eq!(text(&m, off), "+3.00 pts \u{b7} 85 bars \u{b7} 1h 25m");
    let ticks = MeasureParts {
        money: false,
        ..MeasureParts::ALL
    };
    assert_eq!(
        text(&m, ticks),
        "+12 ticks \u{b7} +3.00 pts \u{b7} 85 bars \u{b7} 1h 25m"
    );
    // Only ticks and bars; one part; none.
    let tb = MeasureParts {
        ticks: true,
        points: false,
        money: false,
        bars: true,
        time: false,
    };
    assert_eq!(text(&m, tb), "+12 ticks \u{b7} 85 bars");
    let time = MeasureParts {
        ticks: false,
        points: false,
        money: false,
        bars: false,
        time: true,
    };
    assert_eq!(
        text(&m, time),
        "1h 25m",
        "no separator before the first part"
    );
    let none = MeasureParts {
        time: false,
        ..time
    };
    assert_eq!(text(&m, none), "");
    let big = MeasureStats {
        cents: -123_456_789,
        ..m
    };
    assert!(text(&big, MeasureParts::ALL).contains("-$1,234,567.89/contract"));
    // MES: $5 a point, one tick = $1.25.
    let tick = Anchor {
        price: Price(600_025),
        ..a
    };
    assert_eq!(measure(a, tick, &axis, &es(), Ratio::new(5, 1)).cents, 125);
}

#[test]
fn undo_and_redo_every_change() {
    let mut set = DrawingSet::default();
    let d = trend(
        ("2026-11-05T14:07:00Z", 600_100),
        ("2026-11-05T15:22:00Z", 600_500),
    );
    let id = set.add(d.clone());
    assert_eq!(id, 1);
    let moved = Drawing {
        b: Anchor {
            price: Price(600_600),
            ..d.b
        },
        ..set.get(id).unwrap().clone()
    };
    assert!(set.edit(moved.clone()));
    assert!(!set.edit(moved.clone()), "no change, no step");
    let id2 = set.add(d.clone());
    assert!(set.remove(id));
    assert_eq!(set.items().len(), 1);
    assert!(set.clear());
    assert!(set.items().is_empty());

    assert!(set.undo()); // clear
    assert_eq!(set.items().len(), 1);
    assert!(set.undo()); // remove
    assert_eq!(
        set.items().iter().map(|d| d.id).collect::<Vec<_>>(),
        [id, id2],
        "a restored drawing returns to its place in the drawing order"
    );
    assert_eq!(set.get(id), Some(&moved));
    assert!(set.undo()); // add id2
    assert!(set.undo()); // edit
    assert_eq!(set.get(id).unwrap().b, d.b);
    assert!(set.redo());
    assert_eq!(set.get(id), Some(&moved));
    let rev = set.revision();
    // A new change drops the redo steps.
    set.add(d.clone());
    assert!(!set.can_redo());
    assert!(set.revision() > rev);
    // Ids are never reused, even after undo.
    let from_file = DrawingSet::from_drawings(vec![Drawing {
        id: 41,
        ..d.clone()
    }]);
    let mut f = from_file;
    assert_eq!(f.add(d), 42);
}

#[test]
fn handles_reshape_one_anchor() {
    let d = trend(
        ("2026-11-05T14:07:00Z", 600_100),
        ("2026-11-05T15:22:00Z", 600_500),
    );
    let at = Anchor {
        time: t("2026-11-05T14:30:00Z"),
        price: Price(600_000),
    };
    assert_eq!(d.with_handle(1, at).b, at);
    assert_eq!(d.with_handle(1, at).a, d.a);
    let h = Drawing::new(Tool::HLine, d.a, d.a, Style::DEFAULT);
    let moved = h.with_handle(0, at);
    assert_eq!((moved.a, moved.b), (at, at));
    assert!(d.on_grid(25));
    assert!(
        !Drawing::new(
            Tool::HLine,
            Anchor {
                price: Price(3),
                ..at
            },
            at,
            Style::DEFAULT
        )
        .on_grid(25)
    );
    let wild = Style {
        line: LineStyle {
            color: Color::Palette(9),
            width: 7,
            opacity: 0,
            line: LineType::Dotted,
        },
        fill: FillStyle {
            color: Color::Palette(6),
            opacity: 200,
        },
        ..Style::DEFAULT
    }
    .normalised();
    assert_eq!(wild.line.color, Color::Palette(1));
    assert_eq!((wild.line.width, wild.line.opacity), (4, 10));
    assert_eq!(wild.line.line, LineType::Dotted);
    assert_eq!(
        (wild.fill.color, wild.fill.opacity),
        (Color::Palette(2), 100)
    );
    for tool in Tool::ALL {
        assert_eq!(Tool::from_key(tool.key()), Some(tool));
    }
}

fn projected(d: &Drawing) -> Option<Projected> {
    let s = series();
    let axis = TimeAxis::new(BarsView::fixed(s.bars(), M));
    project(d, &axis, &vp())
}

/// Extensions run the line to the plot's edges; the anchors (handles) stay where they are.
#[test]
fn extend_left_and_right_reach_the_plot_edges() {
    let base = trend(
        ("2026-11-05T15:00:00Z", 600_100),
        ("2026-11-05T15:20:00Z", 600_300),
    );
    let plain = projected(&base).unwrap();
    let both = projected(&Drawing {
        style: Style {
            extend_left: true,
            extend_right: true,
            ..Style::DEFAULT
        },
        ..base.clone()
    })
    .unwrap();
    assert_eq!(both.handles, plain.handles);
    assert!(
        both.p0.0 <= 0.5 || both.p0.1 <= 0.5 || both.p0.1 >= 399.5,
        "{both:?}"
    );
    assert!(
        both.p1.0 >= 799.5 || both.p1.1 <= 0.5 || both.p1.1 >= 399.5,
        "{both:?}"
    );
    // Same slope as the segment.
    let slope = |p: &Projected| (p.p1.1 - p.p0.1) / (p.p1.0 - p.p0.0);
    assert!((slope(&both) - slope(&plain)).abs() < 1e-3);
    // Only left: the right end stays at the second anchor.
    let left = projected(&Drawing {
        style: Style {
            extend_left: true,
            ..Style::DEFAULT
        },
        ..base.clone()
    })
    .unwrap();
    assert_eq!(left.p1, plain.p1);
    assert!(left.p0.0 < plain.p0.0);
    // A vertical line ignores extension flags (not offered for it).
    let mut v = base.clone();
    v.tool = Tool::VLine;
    v.style.extend_left = true;
    assert_eq!(projected(&v).unwrap().p0.1, 0.0);
    // A zone extended right reaches the right edge; a horizontal ray extended left the left.
    let mut z = base.clone();
    z.tool = Tool::Rect;
    z.style.extend_right = true;
    let pz = projected(&z).unwrap();
    assert_eq!(pz.p0.0.max(pz.p1.0), 800.0);
    assert_eq!(pz.handles, plain.handles);
    let mut r = Drawing::new(Tool::HRay, base.a, base.a, Style::DEFAULT);
    r.style.extend_left = true;
    assert_eq!(projected(&r).unwrap().p0.0, 0.0);
    // Prices for the axis tag are the anchors' exact prices.
    assert_eq!(pz.prices, [Price(600_100), Price(600_300)]);
}

#[test]
fn tool_capabilities() {
    assert!(Tool::Rect.has_fill() && Tool::Measure.has_fill() && !Tool::Trend.has_fill());
    assert!(Tool::Note.has_text() && !Tool::Note.has_stroke());
    assert_eq!(Tool::Ray.extends(), (true, false));
    assert_eq!(Tool::VLine.extends(), (false, false));
    assert!(Tool::HLine.has_axis_tag() && !Tool::Trend.has_axis_tag());
}
