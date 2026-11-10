use super::*;
use crate::study_menu::{StudyAction, StudyRef, StudySet, StudyTemplate, save_template};
use crate::style_editor::{Control, Outcome, ThemeCtx};
use crate::theme::Theme;
use lines_core::style::Rgb;
use lines_indicators::{Bands, Reset, VwapSpec};

const MAGENTA: Rgb = Rgb::hex(0xf719cb);

fn vwap(color: u8) -> IndicatorConfig {
    IndicatorConfig::Vwap {
        spec: VwapSpec {
            reset: Reset::Eth,
            bands: Bands(0b011),
        },
        color,
        display: Default::default(),
        style: Default::default(),
    }
}

fn delta() -> IndicatorConfig {
    IndicatorConfig::CumDelta {
        spec: Default::default(),
        display: Default::default(),
        style: Default::default(),
    }
}

fn set_line(look: &mut Look, f: impl FnOnce(&mut LineStyle)) {
    let PartStyle::Line(l) = &mut look.style else {
        panic!("not a line");
    };
    f(l);
}

fn set_fill(look: &mut Look, f: impl FnOnce(&mut FillStyle)) {
    let PartStyle::Fill(x) = &mut look.style else {
        panic!("not a fill");
    };
    f(x);
}

/// Every study's default is its parts' own colors at 100 % of their own translucency (a
/// VWAP's line: its palette color; band fill and level zones: the 25 % that is the
/// translucency they always had), and an optional part is off.
#[test]
fn defaults_are_todays_look_and_write_nothing() {
    for defs in [
        &VWAP[..],
        &DELTA[..],
        &VOLUME[..],
        &PROFILE[..],
        &FOOTPRINT[..],
        &LEVELS[..],
    ] {
        let looks: Vec<Look> = defs
            .iter()
            .map(|d| Look {
                style: d.style,
                shown: !d.optional,
                fill: None,
            })
            .collect();
        let mut map = PartStyles::new();
        write(&mut map, defs, &looks);
        assert!(map.is_empty(), "{defs:?}: {map:?}");
        for d in defs {
            assert!(
                d.own.is_some() || d.key == "line" && defs.len() == 3,
                "{}",
                d.key
            );
        }
    }
    assert!((VWAP_FILL_ALPHA * 0.25 - 0.06).abs() < 1e-6);
    assert!((LEVEL_ZONE_ALPHA * 0.25 - 0.10).abs() < 1e-6);
    assert_eq!(read(&PartStyles::new(), &PROFILE), Look::defaults(&PROFILE));
    // Today's look in every theme: the own color, unchanged.
    for t in Theme::all() {
        for own in [t.up, t.profile_buy, t.profile_va, t.text_dim.opacity(0.2)] {
            assert_eq!(t.part_color(Color::Own, own, 100), own);
        }
    }
}

/// Every part of every study round-trips through its saved form.
#[test]
fn every_part_round_trips() {
    let mut looks = Look::defaults(&PROFILE);
    set_fill(&mut looks[PROFILE_BUY], |f| f.color = Color::Palette(1));
    set_fill(&mut looks[PROFILE_SELL], |f| {
        f.color = Color::Custom(MAGENTA);
        f.opacity = 50;
    });
    set_line(&mut looks[PROFILE_POC], |l| {
        l.line = LineType::Dashed;
        l.width = 3;
    });
    set_fill(&mut looks[PROFILE_VA], |f| f.opacity = 75);
    let mut map = PartStyles::new();
    write(&mut map, &PROFILE, &looks);
    let json = serde_json::to_string(&map).unwrap();
    let back: PartStyles = serde_json::from_str(&json).unwrap();
    assert_eq!(read(&back, &PROFILE), looks);
    assert_eq!(map["sell"]["color"], "#f719cb");
    assert_eq!(map["buy"]["color"], "p1");
    assert!(
        map["va"].get("color").is_none(),
        "only what differs is written"
    );

    let mut looks = Look::defaults(&VWAP);
    set_line(&mut looks[VWAP_BANDS], |l| l.line = LineType::Dotted);
    looks[VWAP_BANDS].shown = true;
    set_fill(&mut looks[VWAP_FILL], |f| f.color = Color::Custom(MAGENTA));
    let mut map = PartStyles::new();
    write(&mut map, &VWAP, &looks);
    assert_eq!(map["bands"]["shown"], true);
    assert_eq!(read(&map, &VWAP), looks);
}

/// A part's fields a newer build wrote, and parts this build doesn't know, survive a
/// restyle; values this build can't use fall back to the default.
#[test]
fn unknown_fields_and_parts_are_kept() {
    let mut map: PartStyles = serde_json::from_str(
        r##"{"buy": {"color": "#f719cb", "glow": 3}, "halo": {"radius": 2},
             "poc": {"line": "wavy", "width": 99, "color": "text", "opacity": 0}}"##,
    )
    .unwrap();
    let looks = read(&map, &PROFILE);
    assert_eq!(looks[PROFILE_BUY].color(), Color::Custom(MAGENTA));
    let poc = looks[PROFILE_POC].line();
    // Unknown line type: the default; the theme text color isn't a study color; width and
    // opacity are kept in range.
    assert_eq!(
        (poc.line, poc.color, poc.width, poc.opacity),
        (LineType::Solid, Color::Own, 4, 10)
    );
    let mut looks = looks;
    set_fill(&mut looks[PROFILE_BUY], |f| f.opacity = 50);
    write(&mut map, &PROFILE, &looks);
    assert_eq!(map["buy"]["glow"], 3);
    assert_eq!(map["buy"]["opacity"], 50);
    assert_eq!(map["halo"]["radius"], 2);
    // Back to the default: the unknown field still keeps the part.
    write(&mut map, &PROFILE, &Look::defaults(&PROFILE));
    assert_eq!(map["buy"], serde_json::json!({"glow": 3}));
}

/// N2: a file a newer build wrote keeps its values: writing touches only the part whose look
/// changed, and in it only the fields that changed (a fill's `line` and `width`, which this
/// build doesn't read there, and out-of-range values it clamps on reading, stay as saved).
#[test]
fn writing_touches_only_the_changed_part_and_fields() {
    let mut map: PartStyles = serde_json::from_str(
        r##"{"buy": {"color": "#3d7bf5", "opacity": 250, "glow": 1},
             "poc": {"width": 9, "line": "zigzag", "opacity": 5},
             "sell": {"line": "dashed", "width": 3, "color": "p1"},
             "future": {"x": 1}}"##,
    )
    .unwrap();
    let before = map.clone();
    let mut looks = read(&map, &PROFILE);
    write(&mut map, &PROFILE, &looks);
    assert_eq!(map, before, "nothing changed: written back as read");
    // The value area's opacity: only that part, only that field.
    set_fill(&mut looks[PROFILE_VA], |f| f.opacity = 50);
    write(&mut map, &PROFILE, &looks);
    assert_eq!(map["va"], serde_json::json!({"opacity": 50}));
    for k in ["buy", "poc", "sell", "future"] {
        assert_eq!(map[k], before[k], "{k} untouched");
    }
    // A fill's color: its `line` and `width` (not read for a fill) stay.
    set_fill(&mut looks[PROFILE_SELL], |f| f.color = Color::Palette(2));
    write(&mut map, &PROFILE, &looks);
    assert_eq!(
        map["sell"],
        serde_json::json!({"line": "dashed", "width": 3, "color": "p2"})
    );
    // A line's color: its clamped width, unknown type and opacity stay as saved.
    set_line(&mut looks[PROFILE_POC], |l| l.color = Color::Palette(3));
    write(&mut map, &PROFILE, &looks);
    assert_eq!(
        map["poc"],
        serde_json::json!({"width": 9, "line": "zigzag", "opacity": 5, "color": "p3"})
    );
    assert_eq!(map["buy"], before["buy"]);
}

/// P7: a styled study line never looks like the position line (a dim dashed line, 6/4) nor a
/// held order (3/3), at any VWAP width step: the pattern differs whatever its color.
#[test]
fn styled_study_dashes_never_look_like_trading_lines() {
    use crate::theme::{HELD_DASH, POSITION_DASH, drawing_dash};
    for w in 1..=4u8 {
        for lt in LineType::ALL {
            let d = drawing_dash(lt, vwap_px(w));
            if d.is_solid() {
                continue;
            }
            let period = d.seg[0] + d.seg[1];
            for t in [POSITION_DASH, HELD_DASH] {
                let tp = t.on + t.off;
                let alike = d.n == 2
                    && (period - tp).abs() <= tp / 3.0
                    && (d.seg[0] / period - t.on / tp).abs() <= 0.2;
                assert!(!alike, "{lt:?} at width step {w} looks like {t:?}");
            }
        }
    }
}

/// A VWAP's line color stays the config's palette index and its width the config's width
/// (older builds read those); a custom color is a saved part.
#[test]
fn vwap_line_keeps_the_legacy_fields() {
    let mut c = vwap(2);
    let looks = vwap_looks(&c);
    assert_eq!(looks[VWAP_LINE].color(), Color::Palette(2));
    assert_eq!(looks[VWAP_LINE].line().width, 2);
    assert!(
        !looks[VWAP_BANDS].shown,
        "band edges are off, as before styles"
    );

    let mut new = looks;
    set_line(&mut new[VWAP_LINE], |l| {
        l.color = Color::Palette(3);
        l.width = 4;
    });
    set_vwap(&mut c, &new);
    let IndicatorConfig::Vwap {
        color,
        display,
        style,
        ..
    } = &c
    else {
        unreachable!()
    };
    assert_eq!((*color, display.width), (3, 4));
    assert!(style.is_empty(), "{style:?}");

    set_line(&mut new[VWAP_LINE], |l| l.color = Color::Custom(MAGENTA));
    set_vwap(&mut c, &new);
    assert_eq!(c.style()["line"]["color"], "#f719cb");
    assert_eq!(vwap_looks(&c), new);
    // The studies menu's theme color replaces it.
    let mut set = StudySet {
        indicators: vec![c],
        ..StudySet::default()
    };
    set.apply(&StudyAction::SetColor(0, 1));
    assert_eq!(
        vwap_looks(&set.indicators[0])[VWAP_LINE].color(),
        Color::Palette(1)
    );
    assert!(set.indicators[0].style().is_empty());
}

/// Files written before styles load with today's look; a study set or chart without styles
/// writes none; a template carries its styles and applying it restores them.
#[test]
fn study_sets_and_templates_carry_styles() {
    let old: StudySet = serde_json::from_str(
        r#"{"indicators": [{"kind": "vwap", "reset": "eth", "bands": 1, "color": 1}],
            "profile": {"mode": "session"}}"#,
    )
    .unwrap();
    assert!(old.styles.is_empty());
    assert_eq!(
        vwap_looks(&old.indicators[0])[VWAP_LINE].color(),
        Color::Palette(1)
    );
    assert!(!serde_json::to_string(&old).unwrap().contains("style"));

    let mut set = old.clone();
    set.indicators.push(delta());
    let mut v = Look::defaults(&VWAP);
    set_line(&mut v[VWAP_LINE], |l| l.color = Color::Custom(MAGENTA));
    set.apply(&StudyAction::SetStyle(StudyRef::Vwap(0), v.to_vec()));
    let mut d = Look::defaults(&DELTA);
    set_fill(&mut d[DELTA_UP], |f| f.color = Color::Palette(1));
    set.apply(&StudyAction::SetStyle(StudyRef::Delta(1), d.to_vec()));
    let mut p = Look::defaults(&PROFILE);
    set_line(&mut p[PROFILE_POC], |l| l.width = 2);
    set.apply(&StudyAction::SetStyle(StudyRef::Profile, p.to_vec()));
    let mut f = Look::defaults(&FOOTPRINT);
    set_line(&mut f[FP_POC], |l| l.color = Color::Palette(3));
    set.apply(&StudyAction::SetStyle(StudyRef::Footprint, f.to_vec()));
    let mut l = Look::defaults(&LEVELS);
    set_fill(&mut l[LEVEL_ZONES], |z| z.opacity = 50);
    set.apply(&StudyAction::SetStyle(
        StudyRef::Levels("demo".into()),
        l.to_vec(),
    ));

    let mut templates = Vec::new();
    save_template(&mut templates, "styled", set.clone());
    let json = serde_json::to_string(&templates).unwrap();
    let back: Vec<StudyTemplate> = serde_json::from_str(&json).unwrap();
    assert_eq!(back[0].set, set);
    let t = &back[0].set;
    assert_eq!(
        vwap_looks(&t.indicators[0])[VWAP_LINE].color(),
        Color::Custom(MAGENTA)
    );
    assert_eq!(read(t.indicators[1].style(), &DELTA), d);
    assert_eq!(read(&t.styles.profile, &PROFILE), p);
    assert_eq!(read(&t.styles.footprint, &FOOTPRINT), f);
    assert_eq!(read(&t.styles.levels["demo"], &LEVELS), l);

    let mut chart = StudySet::default();
    chart.apply(&StudyAction::Replace(Box::new(t.clone())));
    assert_eq!(chart, set);
    // A level override back to the source's own style is dropped; removing a study drops
    // its styles.
    chart.apply(&StudyAction::SetStyle(
        StudyRef::Levels("demo".into()),
        Look::defaults(&LEVELS).to_vec(),
    ));
    assert!(chart.styles.levels.is_empty());
    chart.apply(&StudyAction::Remove(StudyRef::Profile));
    assert!(chart.styles.profile.is_empty());
}

/// Styles a newer build saved for a study this one doesn't have are kept.
#[test]
fn unknown_study_styles_are_kept() {
    let s: StudyStyles = serde_json::from_str(
        r#"{"profile": {"poc": {"width": 2}}, "heatmap": {"cells": {"color": "p2"}}}"#,
    )
    .unwrap();
    assert_eq!(read(&s.profile, &PROFILE)[PROFILE_POC].line().width, 2);
    let back = serde_json::to_value(&s).unwrap();
    assert_eq!(back["heatmap"]["cells"]["color"], "p2");
    assert!(StudyStyles::default().is_empty());
    assert_eq!(
        serde_json::to_string(&StudyStyles::default()).unwrap(),
        "{}"
    );
}

/// A study's editor: one tab per part, the part's own color offered first (named), study
/// colors, line opacities everywhere; styling the hidden band edges shows them.
#[test]
fn the_editor_has_a_tab_per_part() {
    let c = vwap(0);
    let mut e = editor("VWAP style".into(), &VWAP, &vwap_looks(&c), None);
    assert!(e.study);
    let titles: Vec<_> = e.parts.iter().map(|p| p.title).collect();
    assert_eq!(titles, ["Line", "Band edges", "Band fill"]);
    assert_eq!(e.default_label, None, "the studies menu saves study sets");
    // The line has no own color: its theme colors first.
    let rows = e.rows();
    assert_eq!(
        rows[0].controls[0],
        (Control::Color(Color::Palette(0)), true)
    );
    e.select_tab(1);
    let rows = e.rows();
    assert_eq!(rows[0].label, "Show");
    assert_eq!(rows[0].controls, [(Control::Shown, false)]);
    assert_eq!(rows[1].controls[0], (Control::Color(Color::Own), true));
    assert_eq!(e.own_name(), "Line color");
    // Pick the first study color for the bands: they show.
    assert_eq!(e.click(1, 1), Outcome::Changed);
    let looks = looks_of(&e);
    assert!(looks[VWAP_BANDS].shown);
    assert_eq!(looks[VWAP_BANDS].color(), Color::Palette(0));
    // The fill tab offers the line opacities (its own translucency scaled).
    e.select_tab(2);
    let op = e.rows().into_iter().find(|r| r.label == "Opacity").unwrap();
    let values: Vec<_> = op.controls.iter().map(|(c, _)| *c).collect();
    assert_eq!(
        values,
        [100, 75, 50, 25].map(Control::Opacity),
        "with 25 % selected"
    );
    assert!(op.controls[3].1);
    // A level source's own color shows the source's first color.
    let e = editor(
        "Levels style".into(),
        &LEVELS,
        &Look::defaults(&LEVELS),
        Some(MAGENTA),
    );
    assert_eq!(
        e.parts[0].own.map(|o| o.token),
        Some(OwnToken::Source(Some(MAGENTA)))
    );
    assert!(
        e.parts.iter().all(|p| !p.stroke),
        "source keeps type and weight"
    );
}

fn study_ctxs() -> Vec<ThemeCtx> {
    Theme::all().iter().map(Theme::study_ctx).collect()
}

/// The readability guard per part: a part's own color is never flagged (the theme's token,
/// the source's colors); a custom color near a meaning is, naming where.
#[test]
fn readability_warnings_per_part() {
    let ctxs = study_ctxs();
    for (defs, n) in [
        (&VWAP[..], 3),
        (&DELTA[..], 2),
        (&VOLUME[..], 4),
        (&PROFILE[..], 4),
        (&FOOTPRINT[..], 3),
        (&LEVELS[..], 2),
    ] {
        // Opaque, so what is seen is the color typed.
        let looks: Vec<Look> = defs
            .iter()
            .map(|d| {
                let mut style = d.style;
                if let PartStyle::Line(l) = &mut style {
                    l.opacity = 100;
                }
                Look {
                    style,
                    shown: true,
                    fill: None,
                }
            })
            .collect();
        let mut e = editor("x".into(), defs, &looks, None);
        for tab in 0..n {
            e.select_tab(tab);
            if e.parts[tab].own.is_some() {
                assert!(e.warnings(&ctxs).is_empty(), "{}", e.parts[tab].title);
            }
            // The dark standard palette's up color, typed in.
            let row = e.rows().iter().position(|r| r.label == "Color").unwrap();
            let col = e.rows()[row].controls.len() - 1;
            assert_eq!(e.click(row, col), Outcome::OpenCustom { fill: false });
            e.open_custom(false, lines_core::style::Rgb::hex(0x808080));
            e.custom.as_mut().unwrap().zone = crate::color_picker::Zone::Hex;
            for ch in "3fcf9a".chars() {
                e.key(&ch.to_string(), false, Some(&ch.to_string()));
            }
            assert_eq!(e.key("enter", false, None), Outcome::Changed);
            let w = e.warnings(&ctxs);
            assert!(
                w.iter().any(|w| w.text.contains("up color")
                    && w.text.contains("dark theme, standard palette")),
                "{}: {w:?}",
                e.parts[tab].title
            );
        }
    }
}

/// A custom color keeps a part's own translucency (a profile row stays see-through).
#[test]
fn custom_colors_keep_the_parts_translucency() {
    for t in Theme::all() {
        let c = t.part_color(Color::Custom(MAGENTA), t.profile_buy, 100);
        assert!((c.a - t.profile_buy.a).abs() < 1e-6);
        assert_eq!(crate::theme::rgb_of(c), MAGENTA);
        let c = t.part_color(Color::Palette(1), t.text_main, 50);
        assert!((c.a - 0.5).abs() < 1e-6);
        assert_eq!(crate::theme::rgb_of(c), crate::theme::rgb_of(t.study[1]));
    }
}

// ---- no allocation in styled paint ----------------------------------------------------------

struct Counting;

thread_local! {
    static ALLOCS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

unsafe impl std::alloc::GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        let _ = ALLOCS.try_with(|c| c.set(c.get() + 1));
        unsafe { std::alloc::System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        unsafe { std::alloc::System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: std::alloc::Layout, size: usize) -> *mut u8 {
        let _ = ALLOCS.try_with(|c| c.set(c.get() + 1));
        unsafe { std::alloc::System.realloc(ptr, layout, size) }
    }
}

/// Counts this thread's allocations (lines-app's unit tests: the styled paint check below).
#[global_allocator]
static GLOBAL: Counting = Counting;

fn allocs_during(f: impl FnOnce()) -> usize {
    let before = ALLOCS.with(|c| c.get());
    f();
    ALLOCS.with(|c| c.get()) - before
}

/// What a frame does with styles once they are resolved: the color and dash calls the styled
/// paint paths make (profile colors, VWAP line, band and fill colors and dashes, delta
/// candles, footprint cell shades, level lines, zones and zone edges, legend text), and the
/// footprint's shading when it is recomputed. None of it allocates. Not covered here: the
/// paint functions themselves (they need a GPUI window; their paths, quads and text are
/// GPUI's) and the geometry (`lines-core/tests/no_alloc.rs`).
#[test]
fn styled_study_paint_does_not_allocate() {
    let themes = Theme::all();
    let mut profile = Look::defaults(&PROFILE);
    set_line(&mut profile[PROFILE_POC], |l| {
        l.line = LineType::DashDot;
        l.width = 2;
        l.color = Color::Custom(MAGENTA);
    });
    let mut vwap = Look::defaults(&VWAP);
    set_line(&mut vwap[VWAP_LINE], |l| l.line = LineType::Dotted);
    vwap[VWAP_BANDS].shown = true;
    let fp = Look::defaults(&FOOTPRINT);
    let delta = Look::defaults(&DELTA);
    let levels = Look::defaults(&LEVELS);
    assert_eq!(allocs_during(|| drop(vec![1u8])), 1, "the counter counts");
    let mut sink = 0.0f32;
    let n = allocs_during(|| {
        for frame in 0..200 {
            let t = &themes[frame % 4];
            let p = crate::profile::ProfileColors::of(t, &profile);
            sink += p.poc.a + p.edge.a;
            let poc = profile[PROFILE_POC].line();
            crate::theme::drawing_dash(poc.line, f32::from(poc.width)).segments(300.0, |a, b| {
                sink += b - a;
            });
            for l in vwap {
                let line = l.line();
                let c = t.part_color(line.color, t.study[1], line.opacity);
                let d = crate::theme::drawing_dash(line.line, vwap_px(line.width));
                sink += c.a + d.seg[0];
            }
            for l in fp.iter().chain(&delta).chain(&levels) {
                sink += t.part_color(l.color(), t.up.opacity(0.3), l.opacity()).a;
                sink += t.own_color(OwnToken::Source(Some(MAGENTA)), t.text_main).a;
            }
            let shades =
                crate::footprint::CellShades::of(t, &fp, crate::footprint::FootprintMode::Delta);
            for tint in [
                crate::footprint::Tint::Neutral,
                crate::footprint::Tint::Up,
                crate::footprint::Tint::Down,
            ] {
                sink += shades.color(tint, 0.5).a;
            }
            sink += t.level_line(&levels, 0x3d7bf5ff, 1.0).a;
            sink += t.level_zone(&levels, 0x3d7bf5ff, 1.0).a;
            sink += t.level_zone_edge(&levels, 0x3d7bf5ff, 0.6).a;
            sink += t.legend_text(t.study_line(1, Color::Custom(MAGENTA))).a;
            // The caps under the candles (cached in paint; here every frame).
            sink += t.under_candles(t.study[frame % 4].opacity(0.24), 3).0.a;
            sink += crate::levels_layer::zone_scale(t, &levels, [0x3d7bf5ffu32].into_iter());
        }
    });
    assert!(sink > 0.0);
    assert_eq!(n, 0, "styled study paint allocated {n} times");
}

/// The colors a part may take in the sweeps: its own, the study colors, the swatches and
/// sampled custom colors (light, dark, saturated, close to up/down).
fn sweep_colors() -> Vec<Color> {
    let mut colors = vec![Color::Own];
    colors.extend((0..4).map(Color::Palette));
    for hex in [
        0xf719cb, 0x9c8756, 0xffffff, 0x000000, 0x808080, 0x3fcf9a, 0xff6b7f, 0x0a7350, 0x8c7bff,
        0xfff7c0, 0x1a1a1a,
    ] {
        colors.push(Color::Custom(Rgb::hex(hex)));
    }
    colors
}

/// `fill` drawn `layers` times over the chart in `t`, as composited.
fn under(t: &Theme, fill: gpui::Hsla, layers: u8) -> Rgb {
    let mut seen = crate::theme::rgb_of(t.bg);
    for _ in 0..layers {
        seen = crate::footprint::blend(crate::theme::rgb_of(fill), fill.a, seen);
    }
    seen
}

/// The candles' up and down colors and the last-price line (as drawn: translucent) keep 3:1
/// over `seen` (a mark under 3:1 on the bare chart: `UNDER_3_KEEP` of its contrast there).
fn assert_marks_read(t: &Theme, seen: Rgb, what: &str) {
    use crate::footprint::blend;
    use lines_core::style::contrast;
    let bg = crate::theme::rgb_of(t.bg);
    let last = crate::theme::LAST_LINE_ALPHA;
    for (name, m, a) in [
        ("up", t.up, 1.0),
        ("down", t.down, 1.0),
        ("last", t.last, last),
    ] {
        let m = crate::theme::rgb_of(m);
        let bare = contrast(blend(m, a, bg), bg);
        // Already under 3:1 on the bare chart (the translucent last-price line): it keeps
        // most of what it has there.
        let need = if bare >= 3.0 {
            3.0
        } else {
            bare * crate::theme::UNDER_3_KEEP
        };
        let r = contrast(blend(m, a, seen), seen);
        assert!(
            r >= need,
            "{} {what}: {name} {r:.2}:1 (needs {need:.2})",
            t.name
        );
    }
}

/// B1: a VWAP's band fill, under the candles, never takes the up/down candles or the
/// last-price line below 3:1 (WCAG 1.4.11), whatever its color and opacity and however many
/// bands nest, in every theme and palette; a fill that isn't held back is exactly the
/// control's, and an unstyled one is never held back.
#[test]
fn band_fill_keeps_the_candles_visible() {
    for t in Theme::all() {
        for line in (0..4).map(Color::Palette).chain([Color::Custom(MAGENTA)]) {
            let base = t.study_line(0, line);
            for &color in &sweep_colors() {
                for opacity in [10, 25, 50, 75, 100] {
                    let want = t.part_color(color, base.opacity(VWAP_FILL_ALPHA), opacity);
                    for layers in 1..=3u8 {
                        let (got, held) = t.under_candles(want, layers);
                        for k in [0.25, 0.5, 1.0] {
                            let weaker = crate::theme::with_alpha(got, got.a * k);
                            let what = format!("{line:?} fill {color:?} {opacity}% x{layers}");
                            assert_marks_read(&t, under(&t, weaker, layers), &what);
                        }
                        if !held {
                            assert_eq!(got, want);
                        }
                        if color == Color::Own && opacity == 25 {
                            assert!(!held, "{} unstyled fill x{layers} held back", t.name);
                        }
                    }
                }
            }
        }
    }
}

/// B1 for level zones (also under the candles): any color and opacity, every theme and
/// palette; unstyled zones of the sampled source colors are never held back.
#[test]
fn level_zones_keep_the_candles_visible() {
    let sources = [
        0x3d7bf5ffu32,
        0xf5b83dff,
        0xffffffff,
        0x000000ff,
        0xd94c4cff,
        0x2fbf71ff,
    ];
    for t in Theme::all() {
        for &color in &sweep_colors() {
            for opacity in [10, 25, 50, 75, 100] {
                let mut looks = Look::defaults(&LEVELS);
                set_fill(&mut looks[LEVEL_ZONES], |f| {
                    f.color = color;
                    f.opacity = opacity;
                });
                for rgba in sources {
                    let k = crate::levels_layer::zone_scale(&t, &looks, std::iter::once(rgba));
                    let mut z = t.level_zone(&looks, rgba, 1.0);
                    z.a *= k;
                    let what = format!("zone {color:?} {opacity}% {rgba:08x}");
                    assert_marks_read(&t, under(&t, z, 1), &what);
                    if color == Color::Own && opacity == 25 {
                        assert_eq!(k, 1.0, "{} unstyled zone {rgba:08x} held back", t.name);
                    }
                }
                // One scale for a source: its most held-back zone.
                let all = crate::levels_layer::zone_scale(&t, &looks, sources.into_iter());
                for rgba in sources {
                    let one = crate::levels_layer::zone_scale(&t, &looks, std::iter::once(rgba));
                    assert!(all <= one);
                }
            }
        }
    }
}

/// B1: the Band fill and Zones tabs warn when held back, naming theme and palette; unstyled
/// and light fills don't.
#[test]
fn held_fills_are_warned_about() {
    let themes = Theme::all();
    let ctxs = study_ctxs();
    let mut looks = vwap_looks(&vwap(3)).to_vec();
    assert_eq!(
        band_fill_warning(&looks, 3, &themes, &ctxs),
        None,
        "unstyled"
    );
    set_fill(&mut looks[VWAP_FILL], |f| {
        f.color = Color::Custom(MAGENTA);
        f.opacity = 100;
    });
    let w = band_fill_warning(&looks, 3, &themes, &ctxs).expect("heavy magenta, 3 bands");
    assert!(w.text.starts_with("Shown fainter in "), "{}", w.text);
    assert!(w.text.contains("3:1"), "{}", w.text);
    assert_eq!(
        band_fill_warning(&looks, 0, &themes, &ctxs),
        None,
        "no bands, no fill"
    );
    set_fill(&mut looks[VWAP_FILL], |f| f.opacity = 30);
    assert_eq!(
        band_fill_warning(&looks, 1, &themes, &ctxs),
        None,
        "light enough"
    );

    let mut z = Look::defaults(&LEVELS).to_vec();
    let src = Some(Rgb::hex(0x3d7bf5));
    assert_eq!(zones_warning(&z, src, &themes, &ctxs), None, "unstyled");
    set_fill(&mut z[LEVEL_ZONES], |f| {
        f.color = Color::Palette(0);
        f.opacity = 100;
    });
    let w = zones_warning(&z, src, &themes, &ctxs).expect("heavy zones");
    assert!(w.text.contains("over the zones"), "{}", w.text);

    // Through the editor, on its tab only.
    let set = StudySet {
        indicators: vec![vwap(3)],
        ..Default::default()
    };
    let r = StudyRef::Vwap(0);
    let chart = ChartCtx::of(&set, &r, &themes);
    assert_eq!(chart.vwap_layers, 2);
    let mut e = editor("VWAP style".into(), &VWAP, &looks, None);
    e.parts[VWAP_FILL].style = PartStyle::Fill(FillStyle {
        color: Color::Custom(MAGENTA),
        opacity: 100,
    });
    e.tab = VWAP_FILL;
    let all = warnings(&r, &e, &themes, &chart);
    assert!(all.iter().any(|w| w.text.contains("band fill")), "{all:?}");
    e.tab = VWAP_LINE;
    let all = warnings(&r, &e, &themes, &chart);
    assert!(!all.iter().any(|w| w.text.contains("band fill")), "{all:?}");
}

/// A study line styled like another visible study's (close color and the same line type) is
/// warned about by name; another line type or color, or a hidden study, isn't.
#[test]
fn a_line_like_another_studys_is_warned_about() {
    let themes = Theme::all();
    let mut second = vwap(1);
    if let IndicatorConfig::Vwap { spec, .. } = &mut second {
        spec.reset = Reset::Rth;
    }
    let set = StudySet {
        indicators: vec![vwap(1), second],
        ..Default::default()
    };
    let r = StudyRef::Vwap(1);
    let chart = ChartCtx::of(&set, &r, &themes);
    let looks = vwap_looks(&set.indicators[1]);
    let mut e = editor("VWAP RTH style".into(), &VWAP, &looks, None);
    let alike = |e: &StyleEditor, chart: &ChartCtx| {
        warnings(&r, e, &themes, chart)
            .into_iter()
            .filter(|w| w.text.contains("same line type"))
            .collect::<Vec<_>>()
    };
    let w = alike(&e, &chart);
    assert_eq!(w.len(), 1, "{w:?}");
    assert!(w[0].text.contains("the VWAP ETH line"), "{}", w[0].text);
    assert!(
        w[0].text.contains("every theme and palette"),
        "{}",
        w[0].text
    );
    // Another line type tells them apart.
    if let PartStyle::Line(l) = &mut e.parts[VWAP_LINE].style {
        l.line = LineType::Dashed;
    }
    assert!(alike(&e, &chart).is_empty());
    // Another color too.
    if let PartStyle::Line(l) = &mut e.parts[VWAP_LINE].style {
        l.line = LineType::Solid;
        l.color = Color::Palette(0);
    }
    assert!(alike(&e, &chart).is_empty());
    // A hidden study isn't on the chart.
    let mut hidden = set.clone();
    hidden.indicators[0].display_mut().hidden = true;
    let e = editor("VWAP RTH style".into(), &VWAP, &looks, None);
    assert!(alike(&e, &ChartCtx::of(&hidden, &r, &themes)).is_empty());
}

/// The Imbalances tab warns once a color is picked (both sides share it); its own colors
/// don't.
#[test]
fn a_shared_imbalance_color_is_warned_about() {
    let mut looks = Look::defaults(&FOOTPRINT);
    assert_eq!(crate::footprint::imbalance_warning(&looks), None);
    set_line(&mut looks[FP_IMBALANCE], |l| l.color = Color::Palette(1));
    let w = crate::footprint::imbalance_warning(&looks).expect("one color for both sides");
    assert!(w.text.contains("every theme and palette"), "{}", w.text);
    assert!(w.text.contains("place"), "{}", w.text);
}

// ---- level types --------------------------------------------------------------------------

use lines_core::levels::{LabelAt, LabelColor, LabelMode, LevelType, TypeStyle};

/// A source's declared types, like a real source's: a zone type with a recommended theme
/// color and fill, a line type with a recommended text color, one keeping the levels' own.
static TYPES: [LevelType; 3] = [
    LevelType::zone(
        "demand",
        "Demand zone",
        "Zones",
        TypeStyle {
            shown: true,
            line: LineStyle {
                color: Color::Palette(2),
                line: LineType::Solid,
                width: 1,
                opacity: 100,
            },
            fill: FillStyle {
                color: Color::Palette(2),
                opacity: 50,
            },
            ..TypeStyle::OWN
        },
    ),
    LevelType::line(
        "mid",
        "Mid line",
        "Session",
        TypeStyle {
            line: LineStyle {
                color: Color::Text,
                line: LineType::Dashed,
                width: 1,
                opacity: 100,
            },
            ..TypeStyle::OWN
        },
    ),
    LevelType::line("other", "Other", "Lines", TypeStyle::OWN),
];

/// A type never styled (also one a source adds later) gets its recommended look; nothing is
/// saved for it.
#[test]
fn an_untouched_type_gets_its_recommended_look() {
    let empty = PartStyles::new();
    let src = Look::defaults(&LEVELS);
    for t in &TYPES {
        let ty = read_type(&empty, t);
        let r = resolve_type(t, &src, &ty);
        assert_eq!(r.shown, t.style.shown);
        assert_eq!(r.line, t.style.line, "{}", t.id);
        assert_eq!(r.fill, t.style.fill, "{}", t.id);
        let mut m = PartStyles::new();
        write_type(&mut m, t, &ty);
        assert!(m.is_empty(), "{}: {m:?}", t.id);
    }
    // A file saved before a type existed: the new type is at its default.
    let saved: PartStyles = serde_json::from_str(r#"{"types": {"cat": {"color": "p1"}}}"#).unwrap();
    let r = resolve_type(&TYPES[1], &src, &read_type(&saved, &TYPES[1]));
    assert_eq!(r.line, TYPES[1].style.line);
}

/// Precedence, field by field: the type's override, else the source's (Lines, Zones), else
/// the type's recommended look (which may be each level's own color).
#[test]
fn type_styles_resolve_by_precedence() {
    let t = &TYPES[0];
    let mut src = Look::defaults(&LEVELS);
    set_line(&mut src[LEVEL_LINES], |l| {
        l.color = Color::Custom(MAGENTA);
        l.opacity = 50;
    });
    set_fill(&mut src[LEVEL_ZONES], |f| f.color = Color::Palette(3));
    // The source's override over the type's recommended look.
    let mut ty = read_type(&PartStyles::new(), t);
    let r = resolve_type(t, &src, &ty);
    assert_eq!(r.line.color, Color::Custom(MAGENTA));
    assert_eq!(r.line.opacity, 50);
    assert_eq!(r.fill.color, Color::Palette(3));
    assert_eq!(
        r.fill.opacity, 50,
        "the source's Zones opacity is its default"
    );
    // The type's override over the source's.
    set_line(&mut ty, |l| {
        l.color = Color::Palette(1);
        l.opacity = 75;
        l.width = 3;
    });
    ty.fill = Some(FillStyle {
        color: Color::Custom(MAGENTA),
        opacity: 25,
    });
    let r = resolve_type(t, &src, &ty);
    assert_eq!(
        (r.line.color, r.line.opacity, r.line.width),
        (Color::Palette(1), 75, 3)
    );
    assert_eq!(
        r.fill,
        FillStyle {
            color: Color::Custom(MAGENTA),
            opacity: 25
        }
    );
    // No overrides on a type keeping the levels' own colors: `Own` (today's look).
    let own = resolve_type(
        &TYPES[2],
        &Look::defaults(&LEVELS),
        &read_type(&PartStyles::new(), &TYPES[2]),
    );
    assert_eq!(own.line.color, Color::Own);
}

/// A chart saved before types (a per-source override only) loads and applies unchanged, and
/// restyling a type keeps it: the per-source fields stay as saved, types go beside them.
#[test]
fn a_saved_per_source_style_migrates() {
    let mut set: StudySet = serde_json::from_str(
        r##"{"styles": {"levels": {"demo": {"lines": {"color": "#f719cb", "opacity": 75},
                                            "zones": {"opacity": 50}}}}}"##,
    )
    .unwrap();
    let saved = set.styles.levels["demo"].clone();
    let src = read(&saved, &LEVELS);
    assert_eq!(src[LEVEL_LINES].color(), Color::Custom(MAGENTA));
    // A type left alone takes the source's override.
    let r = resolve_type(&TYPES[1], &src, &read_type(&saved, &TYPES[1]));
    assert_eq!((r.line.color, r.line.opacity), (Color::Custom(MAGENTA), 75));
    // Restyle one type through the editor's action.
    let mut looks = src.to_vec();
    for t in &TYPES {
        looks.push(read_type(&saved, t));
    }
    looks[2 + 1].shown = false;
    // Promoted app-wide once (no app-wide entry yet), then edited there.
    let mut app = BTreeMap::new();
    assert!(promote_level_styles(&mut app, &set.styles.levels));
    assert!(
        !promote_level_styles(&mut app, &set.styles.levels),
        "once: an entry is kept"
    );
    set_level_style(
        &mut app,
        "demo",
        &LevelEdit {
            looks,
            types: &TYPES,
            label: LabelMode::NamePrice,
            label_default: LabelMode::Name,
        },
    );
    set.styles.levels = app;
    let m = &set.styles.levels["demo"];
    assert_eq!(m["lines"], saved["lines"]);
    assert_eq!(m["zones"], saved["zones"]);
    assert_eq!(m["types"], serde_json::json!({"mid": {"shown": false}}));
    assert_eq!(m["label"], "name-price");
    // Round trip through JSON.
    let back: StudySet = serde_json::from_str(&serde_json::to_string(&set).unwrap()).unwrap();
    assert_eq!(back, set);
    assert_eq!(
        read_label(&back.styles.levels["demo"], LabelMode::Name),
        LabelMode::NamePrice
    );
    // The source's own label mode isn't saved.
    let mut m2 = m.clone();
    write_label(&mut m2, LabelMode::Name, LabelMode::Name);
    assert!(!m2.contains_key(LABEL_KEY));
}

/// Fields and types a newer build saved are kept when a type is restyled; values this build
/// can't read fall back to the type's default.
#[test]
fn unknown_type_fields_are_kept() {
    let mut m: PartStyles = serde_json::from_str(
        r##"{"types": {"demand": {"glow": 2, "fill": {"color": "#f719cb", "hatch": 1},
                                     "line": "wavy"},
                       "future-kind": {"x": 1}},
             "label": "price-only", "halo": {"r": 1}}"##,
    )
    .unwrap();
    let t = &TYPES[0];
    let mut look = read_type(&m, t);
    assert_eq!(look.line().line, LineType::Solid);
    assert_eq!(look.fill.unwrap().color, Color::Custom(MAGENTA));
    assert_eq!(read_label(&m, LabelMode::NamePrice), LabelMode::NamePrice);
    set_line(&mut look, |l| l.width = 2);
    write_type(&mut m, t, &look);
    let z = &m["types"]["demand"];
    assert_eq!(z["glow"], 2);
    assert_eq!(z["line"], "wavy", "not changed: left as saved");
    assert_eq!(z["width"], 2);
    assert_eq!(z["fill"]["hatch"], 1);
    assert_eq!(m["types"]["future-kind"]["x"], 1);
    assert_eq!(m["halo"]["r"], 1);
    assert_eq!(m["label"], "price-only");
    assert_eq!(read_type(&m, t), look);
}

/// With recent colors, a zone fill's picker opens once, under Fill (the Recent row is a
/// named row, not a second place for it), and a kept custom fill that becomes a recent color
/// is still checked in the Fill row.
#[test]
fn a_fill_picker_opens_once_beside_recent_colors() {
    use lines_core::style::Rgb;
    let mut e = level_editor(
        "Levels · Demo style".into(),
        &PartStyles::default(),
        &TYPES,
        LabelMode::NamePrice,
        None,
        |_| None,
    );
    e.select_tab(2);
    let names = &["a", "b", "c", "d"];
    let grows = |e: &mut crate::style_editor::StyleEditor| {
        let closed = crate::style_editor_view::content_height(e, &[], 400.0, names);
        e.open_custom(true, Rgb::hex(0x808080));
        assert!(e.custom.is_some());
        let open = crate::style_editor_view::content_height(e, &[], 400.0, names);
        e.key("escape", false, None);
        assert!(e.custom.is_none());
        open - closed
    };
    let alone = grows(&mut e);
    e.recent = vec![Rgb::hex(0x123456)];
    assert_eq!(grows(&mut e), alone, "one picker, recent colors or not");
    // Keep a custom fill: remembered, and still checked under Fill.
    e.open_custom(true, Rgb::hex(0x808080));
    e.take_color(Rgb::hex(0xf719cb));
    e.key("enter", false, None);
    assert!(e.custom.is_none());
    assert!(e.recent.contains(&Rgb::hex(0xf719cb)));
    let rows = e.rows();
    let fill = rows.iter().find(|r| r.label == "Fill").unwrap();
    assert!(
        fill.controls.iter().any(|(_, on)| *on),
        "the kept fill is checked"
    );
}

/// The editor shows what is saved and saves what it shows: grouped tabs after the source's
/// own, each type's look (a zone's fill too) and the label option, round-tripped.
#[test]
fn the_level_editor_round_trips() {
    let mut styles: PartStyles = serde_json::from_str(
        r##"{"types": {"mid": {"width": 2}, "demand": {"fill": {"opacity": 75}}}}"##,
    )
    .unwrap();
    let mut e = level_editor(
        "Levels · Demo style".into(),
        &styles,
        &TYPES,
        LabelMode::NamePrice,
        None,
        |_| None,
    );
    let titles: Vec<_> = e.parts.iter().map(|p| (p.group, p.title)).collect();
    assert_eq!(
        titles,
        vec![
            (Some(SOURCE_GROUP), "Lines"),
            (Some(SOURCE_GROUP), "Zones"),
            (Some("Zones"), "Demand zone"),
            (Some("Session"), "Mid line"),
            (Some("Lines"), "Other"),
        ]
    );
    assert_eq!(e.options.label_price, Some(true));
    let edit = LevelEdit::of(&e, &TYPES, LabelMode::NamePrice);
    assert_eq!(edit.looks[3], read_type(&styles, &TYPES[1]));
    assert_eq!(edit.looks[2].fill.unwrap().opacity, 75);
    // The zone type's tab: show, color, line, width, opacity, then its fill.
    e.select_tab(2);
    let labels: Vec<_> = e.rows().iter().map(|r| r.label).collect();
    assert_eq!(
        labels,
        [
            "Show",
            "Color",
            "Line",
            "Width",
            "Opacity",
            "Fill",
            "Fill opacity",
            "Label color",
            "Label at"
        ]
    );
    // Its labels: theme text, under its low edge.
    let rows = e.rows();
    let color = rows.iter().position(|r| r.label == "Label color").unwrap();
    let text = rows[color]
        .controls
        .iter()
        .position(|(c, _)| *c == Control::LabelColor(LabelColor::Text))
        .unwrap();
    assert_eq!(e.click(color, text), Outcome::Changed);
    let at = rows.iter().position(|r| r.label == "Label at").unwrap();
    let low = rows[at]
        .controls
        .iter()
        .position(|(c, _)| *c == Control::LabelAt(LabelAt::Low))
        .unwrap();
    assert_eq!(e.click(at, low), Outcome::Changed);
    assert_eq!(e.click(at, low), Outcome::Nothing, "already there");
    // A line type offers the label color, never an edge (a line has one price).
    e.select_tab(3);
    let labels: Vec<_> = e.rows().iter().map(|r| r.label).collect();
    assert!(labels.contains(&"Label color") && !labels.contains(&"Label at"));
    e.select_tab(2);
    // Pick the fill's second study color and hide the type; then turn the price off (tab 0).
    let rows = e.rows();
    let fill = rows.iter().position(|r| r.label == "Fill").unwrap();
    let k = rows[fill]
        .controls
        .iter()
        .position(|(c, _)| *c == Control::FillColor(Color::Palette(1)))
        .unwrap();
    assert_eq!(e.click(fill, k), Outcome::Changed);
    assert_eq!(e.click(0, 0), Outcome::Changed);
    e.select_tab(0);
    let r = e.rows().iter().position(|r| r.label == "Labels").unwrap();
    assert_eq!(e.click(r, 0), Outcome::Changed);
    let edit = LevelEdit::of(&e, &TYPES, LabelMode::NamePrice);
    let mut ss = StudyStyles::default();
    ss.levels.insert("demo".into(), styles.clone());
    set_level_style(&mut ss.levels, "demo", &edit);
    styles = ss.levels["demo"].clone();
    assert_eq!(
        styles["types"]["demand"],
        serde_json::json!({
            "shown": false,
            "fill": {"color": "p1", "opacity": 75},
            "label-color": "text",
            "label-at": "low"
        })
    );
    assert_eq!(
        read_type_label(&styles, &TYPES[0]),
        TypeLabel {
            color: LabelColor::Text,
            at: LabelAt::Low
        }
    );
    assert_eq!(styles["types"]["mid"], serde_json::json!({"width": 2}));
    assert_eq!(styles["label"], "name");
    // Opened again: the same looks.
    let again = level_editor(
        "t".into(),
        &styles,
        &TYPES,
        LabelMode::NamePrice,
        None,
        |_| None,
    );
    assert_eq!(looks_of(&again), edit.looks);
    assert_eq!(again.options.label_price, Some(false));
    // A custom fill typed as hex goes to the fill, not the line.
    let mut e = again;
    e.select_tab(2);
    let line_before = e.parts[2].style;
    let rows = e.rows();
    let fill = rows.iter().position(|r| r.label == "Fill").unwrap();
    let custom = rows[fill].controls.len() - 1;
    assert_eq!(e.click(fill, custom), Outcome::OpenCustom { fill: true });
    e.open_custom(true, lines_core::style::Rgb::hex(0x808080));
    assert!(e.custom.as_ref().is_some_and(|c| c.fill));
    e.custom.as_mut().unwrap().zone = crate::color_picker::Zone::Hex;
    for ch in ["f", "7", "1", "9", "c", "b"] {
        e.key(ch, false, Some(ch));
    }
    assert_eq!(e.key("enter", false, None), Outcome::Changed);
    assert_eq!(e.parts[2].fill.unwrap().0.color, Color::Custom(MAGENTA));
    assert_eq!(e.parts[2].style, line_before, "the line is untouched");
}

/// A type's label options are saved as optional fields, each only while it differs from the
/// type's own; back at the type's own they go (and an emptied entry with them); fields this
/// build doesn't know, and unreadable values, stay.
#[test]
fn type_label_options_save_only_what_differs() {
    let t = &TYPES[0];
    let own = TypeLabel::of(t);
    assert_eq!(own.color, LabelColor::Level);
    assert_eq!(own.at, LabelAt::Own);
    let mut m = PartStyles::new();
    assert_eq!(read_type_label(&m, t), own);
    write_type_label(&mut m, t, own);
    assert!(m.is_empty(), "nothing saved for the type's own");
    let high = TypeLabel {
        at: LabelAt::High,
        ..own
    };
    write_type_label(&mut m, t, high);
    assert_eq!(
        serde_json::to_value(&m).unwrap(),
        serde_json::json!({"types": {"demand": {"label-at": "high"}}})
    );
    write_type_label(&mut m, t, own);
    assert!(m.is_empty(), "back at its own: the entry goes");
    // A newer build's value this build can't read: kept while the field isn't changed.
    let mut m: PartStyles =
        serde_json::from_str(r##"{"types": {"demand": {"label-at": "middle", "glow": 1}}}"##)
            .unwrap();
    assert_eq!(read_type_label(&m, t), own);
    write_type_label(
        &mut m,
        t,
        TypeLabel {
            color: LabelColor::Text,
            ..own
        },
    );
    assert_eq!(
        serde_json::to_value(&m).unwrap(),
        serde_json::json!({"types": {"demand": {"label-at": "middle", "glow": 1, "label-color": "text"}}})
    );
}

/// A type's light-theme fill (`TypeStyle::fill_light`) applies on the light themes only, and
/// only while the fill is the type's own.
#[test]
fn a_types_light_fill_applies_on_light_only() {
    let t = LevelType::zone(
        "gray",
        "Gray zone",
        "Zones",
        TypeStyle {
            fill: FillStyle {
                color: Color::Custom(Rgb::hex(0x5a5e68)),
                opacity: 50,
            },
            fill_light: Some(Color::Custom(Rgb::hex(0x9a9ea8))),
            ..TypeStyle::OWN
        },
    );
    let src = Look::defaults(&LEVELS);
    let ty = read_type(&PartStyles::new(), &t);
    let r = resolve_type(&t, &src, &ty);
    assert_eq!(r.fill_color(true), Color::Custom(Rgb::hex(0x5a5e68)));
    assert_eq!(r.fill_color(false), Color::Custom(Rgb::hex(0x9a9ea8)));
    let mut picked = ty;
    picked.fill = Some(FillStyle {
        color: Color::Palette(1),
        opacity: 50,
    });
    let r = resolve_type(&t, &src, &picked);
    assert_eq!(r.fill_color(false), Color::Palette(1), "picked: everywhere");
}

/// A source declaring no types keeps the editor it had (Lines and Zones on one plain tab row),
/// plus its label option.
#[test]
fn a_source_without_types_keeps_its_two_tabs() {
    let e = level_editor(
        "t".into(),
        &PartStyles::new(),
        &[],
        LabelMode::Name,
        None,
        |_| None,
    );
    assert_eq!(e.parts.len(), 2);
    assert!(e.parts.iter().all(|p| p.group.is_none()));
    assert_eq!(e.options.label_price, Some(false));
}

/// A source's Zones tab still warns when held back under the candles when the source has
/// types (its editor has more parts than Lines and Zones).
#[test]
fn zones_warn_through_a_typed_sources_editor() {
    let themes = Theme::all();
    let mut e = level_editor(
        "t".into(),
        &PartStyles::new(),
        &lines_feed::levels::demo::TYPES,
        LabelMode::Name,
        Some(Rgb::hex(0x3d7bf5)),
        |_| None,
    );
    e.select_tab(LEVEL_ZONES);
    let r = StudyRef::Levels("demo".into());
    let chart = ChartCtx::default();
    let quiet = warnings(&r, &e, &themes, &chart);
    assert!(quiet.iter().all(|w| !w.text.contains("over the zones")));
    // A heavy Zones color: held back, and said so.
    let rows = e.rows();
    let color = rows.iter().position(|r| r.label == "Color").unwrap();
    let k = rows[color]
        .controls
        .iter()
        .position(|(c, _)| *c == Control::Color(Color::Palette(0)))
        .unwrap();
    e.click(color, k);
    let op = e.rows().iter().position(|r| r.label == "Opacity").unwrap();
    e.click(op, 0); // 100 %
    let w = warnings(&r, &e, &themes, &chart);
    assert!(w.iter().any(|w| w.text.contains("over the zones")), "{w:?}");
}

/// A user-picked zone fill on a type's tab is checked like any picked color: a color with a
/// meaning is named as the fill's, and a fill held back under the candles says so. Its
/// recommended (own) color isn't checked.
#[test]
fn a_types_zone_fill_is_guarded() {
    let themes = Theme::all();
    let r = StudyRef::Levels("demo".into());
    let chart = ChartCtx::default();
    let mut e = level_editor(
        "t".into(),
        &PartStyles::new(),
        &TYPES,
        LabelMode::Name,
        None,
        |_| None,
    );
    e.select_tab(2); // the demand zone type
    assert!(
        warnings(&r, &e, &themes, &chart)
            .iter()
            .all(|w| !w.text.starts_with("Fill") && !w.text.contains("zone fill")),
        "the recommended fill isn't checked"
    );
    let up = Theme::dark(crate::theme::Palette::Standard).meanings()[0].1;
    let set_fill = |e: &mut crate::style_editor::StyleEditor, c: Color, opacity: u8| {
        if let Some((f, _)) = e.parts[2].fill.as_mut() {
            f.color = c;
            f.opacity = opacity;
        }
    };
    // The dark theme's up color, faint: looks like up.
    set_fill(&mut e, Color::Custom(up), 25);
    let w = warnings(&r, &e, &themes, &chart);
    assert!(
        w.iter()
            .any(|w| w.text.starts_with("Fill: Looks like the up color")),
        "{w:?}"
    );
    // Heavy white: held back under the candles, said of the fill.
    set_fill(&mut e, Color::Custom(Rgb::hex(0xffffff)), 100);
    let w = warnings(&r, &e, &themes, &chart);
    assert!(
        w.iter().any(|w| w.text.contains("over the zone fill")),
        "{w:?}"
    );
}

/// A type left at `TypeStyle::OWN` keeps the look before types (each level's own weight,
/// dashes and edges); restyling its line ends that. A recommended color with a light-theme
/// counterpart uses it on light themes only, and only while it is the type's own.
#[test]
fn own_types_keep_the_old_look_and_light_counterparts_apply() {
    let src = Look::defaults(&LEVELS);
    let own = &TYPES[2];
    let mut ty = read_type(&PartStyles::new(), own);
    assert!(resolve_type(own, &src, &ty).own_line);
    set_line(&mut ty, |l| l.width = 2);
    assert!(!resolve_type(own, &src, &ty).own_line);

    let lit = LevelType::line(
        "band",
        "Upper band",
        "Lines",
        TypeStyle {
            line: LineStyle {
                color: Color::Custom(Rgb::hex(0x7ed957)),
                ..TypeStyle::OWN.line
            },
            light: Some(Color::Custom(Rgb::hex(0x2e7d32))),
            ..TypeStyle::OWN
        },
    );
    let ty = read_type(&PartStyles::new(), &lit);
    let r = resolve_type(&lit, &src, &ty);
    assert!(!r.own_line);
    assert_eq!(r.line_color(true), Color::Custom(Rgb::hex(0x7ed957)));
    assert_eq!(r.line_color(false), Color::Custom(Rgb::hex(0x2e7d32)));
    // Picked for the type, or for the whole source: the same in every theme.
    let mut picked = ty;
    set_line(&mut picked, |l| l.color = Color::Palette(1));
    assert_eq!(
        resolve_type(&lit, &src, &picked).line_color(false),
        Color::Palette(1)
    );
    let mut s2 = src;
    set_line(&mut s2[LEVEL_LINES], |l| l.color = Color::Custom(MAGENTA));
    assert_eq!(
        resolve_type(&lit, &s2, &ty).line_color(false),
        Color::Custom(MAGENTA)
    );
}

/// Level styles are app-wide by source: a chart's own saved entry (from before) is promoted
/// only where the source has none; a chart whose entry differs from the app-wide one is
/// ignored (Decided: dropped on its next save, because one look per source is the point and
/// a silent merge of two edits can't be right; alternative: keep per-chart overrides above
/// the app-wide style). Settings keep fields and sources this build doesn't know.
#[test]
fn level_styles_are_app_wide_and_keep_unknown_fields() {
    let mut app: BTreeMap<String, PartStyles> = serde_json::from_str(
        r##"{"demo": {"lines": {"opacity": 50}, "glow": 1}, "future-src": {"x": 2}}"##,
    )
    .unwrap();
    let chart: BTreeMap<String, PartStyles> = serde_json::from_str(
        r##"{"demo": {"lines": {"opacity": 25}}, "other": {"zones": {"opacity": 75}}}"##,
    )
    .unwrap();
    assert!(promote_level_styles(&mut app, &chart));
    assert_eq!(
        app["demo"]["lines"]["opacity"], 50,
        "the app-wide entry wins"
    );
    assert_eq!(app["other"]["zones"]["opacity"], 75, "promoted");
    // An edit keeps what this build doesn't read.
    let mut looks = read(&app["demo"], &LEVELS).to_vec();
    set_line(&mut looks[LEVEL_LINES], |l| l.color = Color::Palette(1));
    set_level_style(
        &mut app,
        "demo",
        &LevelEdit {
            looks,
            types: &[],
            label: LabelMode::Name,
            label_default: LabelMode::Name,
        },
    );
    assert_eq!(app["demo"]["glow"], 1);
    assert_eq!(app["future-src"]["x"], 2);
    // Through the settings file.
    let s = crate::picker::Settings {
        level_styles: app.clone(),
        ..Default::default()
    };
    let back: crate::picker::Settings =
        serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
    assert_eq!(back.level_styles, app);
    assert!(
        !serde_json::to_string(&crate::picker::Settings::default())
            .unwrap()
            .contains("level_styles")
    );
}

/// A zone type's edges follow the source's Zones color (at 60 %, decision 0026) while the
/// type's own line color isn't set; set, the type's color wins.
#[test]
fn zone_type_edges_follow_the_sources_zones_color() {
    let t = &TYPES[0];
    let mut src = Look::defaults(&LEVELS);
    let ty = read_type(&PartStyles::new(), t);
    assert_eq!(resolve_type(t, &src, &ty).edge, None);
    set_fill(&mut src[LEVEL_ZONES], |f| f.color = Color::Palette(3));
    assert_eq!(resolve_type(t, &src, &ty).edge, Some(Color::Palette(3)));
    let mut own = ty;
    set_line(&mut own, |l| l.color = Color::Palette(1));
    assert_eq!(resolve_type(t, &src, &own).edge, None);
}

/// A chart restored from an older file and saved without ever being shown (a background tab)
/// keeps its level style: saved with the chart until promoted; once promoted, in the
/// app-wide styles, and no longer saved with the chart. Promoting writes only when it adds.
#[test]
fn an_unshown_chart_keeps_its_level_style() {
    let old: StudyStyles =
        serde_json::from_str(r##"{"levels": {"demo": {"lines": {"color": "#f719cb"}}}}"##).unwrap();
    let mut pending = PendingLevels::default();
    pending.restore(&old.levels);
    // Saved before any promotion: the same entry.
    assert_eq!(pending.saved(), old.levels);
    // Promoted (on restore): app-wide, and the chart saves nothing more.
    let mut app = BTreeMap::new();
    assert!(pending.adds_to(&app));
    assert!(pending.promote(&mut app));
    assert_eq!(app, old.levels);
    assert!(pending.saved().is_empty());
    // A second chart with another style for the same source adds nothing: no write.
    let mut other = PendingLevels::default();
    other.restore(&serde_json::from_str(r##"{"demo": {"lines": {"opacity": 50}}}"##).unwrap());
    assert!(!other.adds_to(&app));
}

/// A style edit changes only its source's entry: another source's (another chart's edit)
/// stays as it was.
#[test]
fn an_edit_changes_only_its_source() {
    let app: BTreeMap<String, PartStyles> = serde_json::from_str(
        r##"{"other": {"lines": {"opacity": 50}}, "demo": {"zones": {"opacity": 75}}}"##,
    )
    .unwrap();
    let mut looks = read(&app["demo"], &LEVELS).to_vec();
    set_line(&mut looks[LEVEL_LINES], |l| l.color = Color::Palette(1));
    let out = with_level_style(
        &app,
        "demo",
        &LevelEdit {
            looks,
            types: &[],
            label: LabelMode::Name,
            label_default: LabelMode::Name,
        },
    );
    assert_eq!(out["other"], app["other"]);
    assert_eq!(out["demo"]["zones"], app["demo"]["zones"]);
    assert_eq!(out["demo"]["lines"]["color"], "p1");
}

/// Settings never saved (screenshots, an unwritable file): a chart's own level style is
/// promoted for this run (the app-wide styles carry it) and the chart keeps saving it.
#[test]
fn unsaved_settings_keep_the_charts_own_style() {
    let own: BTreeMap<String, PartStyles> =
        serde_json::from_str(r##"{"demo": {"lines": {"color": "#f719cb"}}}"##).unwrap();
    let settings = crate::picker_view::AppSettings::ephemeral();
    assert!(!settings.writable());
    let mut app = settings.settings.level_styles.clone();
    let mut pending = PendingLevels::default();
    pending.restore(&own);
    assert!(pending.promote_into(&mut app, settings.writable()));
    assert_eq!(app, own, "this run shows it");
    assert_eq!(pending.saved(), own, "and the chart still saves it");
    // Saved settings: promoted and forgotten.
    let mut app = BTreeMap::new();
    assert!(pending.promote_into(&mut app, true));
    assert!(pending.saved().is_empty());
}
