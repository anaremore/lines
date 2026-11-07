use super::*;
use lines_core::{Price, Qty, UtcMillis};

fn sources() -> Vec<SourceInfo> {
    vec![SourceInfo {
        id: "demo".into(),
        label: "Demo levels".into(),
        kinds: vec!["pivot".into(), "zone".into()],
    }]
}

fn input<'a>(set: &'a StudySet, src: &'a [SourceInfo], templates: &'a [String]) -> MenuInput<'a> {
    MenuInput {
        set,
        sources: src,
        templates,
        has_rth: true,
    }
}

fn labels(items: &[Item]) -> Vec<String> {
    items
        .iter()
        .map(|i| match i {
            Item::Header(h) => format!("# {h}"),
            Item::Note(n) => format!("({n})"),
            Item::Study { label, hidden, .. } => {
                format!("{label}{}", if *hidden { " [hidden]" } else { "" })
            }
            Item::Control {
                group, label, on, ..
            } => format!("  {group}: {label}{}", if *on { " *" } else { "" }),
            Item::Action { label, .. } => format!("+ {label}"),
        })
        .collect()
}

/// Roadmap 10e: cumulative delta (one per chart, its own sub-pane) and the footprint are
/// studies like the others: listed, edited, hidden and removed through `StudyAction`s, and
/// saved in the set (templates, defaults).
#[test]
fn cumulative_delta_and_footprint_are_studies() {
    use crate::footprint::FootprintMode;
    use lines_indicators::{DeltaReset, IndicatorConfig};
    let src = sources();
    let mut set = StudySet::default();
    set.apply(&StudyAction::AddVwap(Reset::Eth));
    set.apply(&StudyAction::AddDelta(DeltaReset::Eth));
    set.apply(&StudyAction::AddDelta(DeltaReset::Rth)); // replaces: one sub-pane
    set.apply(&StudyAction::AddFootprint(FootprintMode::BidAsk));
    assert_eq!(set.indicators.iter().filter(|c| c.in_subpane()).count(), 1);
    let items = StudyMenu::default().items(&input(&set, &src, &[]));
    let l = labels(&items);
    assert!(l.contains(&"Cumulative delta · RTH".to_owned()), "{l:?}");
    assert!(l.contains(&"Footprint · sell × buy".to_owned()), "{l:?}");
    assert!(
        !l.iter()
            .any(|x| x.starts_with("+ Cumulative delta") || x.starts_with("+ Footprint")),
        "already on the chart: not offered again"
    );
    // The editors.
    set.apply(&StudyAction::SetDeltaReset(1, DeltaReset::None));
    set.apply(&StudyAction::SetFootprintMode(FootprintMode::Delta));
    set.apply(&StudyAction::SetImbalanceRatio(400));
    set.apply(&StudyAction::SetImbalanceMin(10));
    set.apply(&StudyAction::SetFootprintGrouping(Some(2)));
    let IndicatorConfig::CumDelta { spec, .. } = set.indicators[1] else {
        panic!("delta")
    };
    assert_eq!(spec.reset, DeltaReset::None);
    assert_eq!(
        (
            set.footprint.mode,
            set.footprint.imbalance_pct,
            set.footprint.imbalance_min,
            set.footprint.grouping
        ),
        (FootprintMode::Delta, 400, 10, Some(2))
    );
    let m = StudyMenu {
        editing: Some(StudyRef::Footprint),
        ..Default::default()
    };
    let groups: Vec<&str> = m
        .items(&input(&set, &src, &[]))
        .iter()
        .filter_map(|i| match i {
            Item::Control { group, .. } => Some(*group),
            _ => None,
        })
        .collect();
    for g in ["Show", "Imbalance", "Min volume", "Grouping"] {
        assert!(groups.contains(&g), "{g}: {groups:?}");
    }
    // Hide keeps the settings; remove turns off; the set round-trips through JSON.
    set.apply(&StudyAction::ToggleHidden(StudyRef::Footprint));
    set.apply(&StudyAction::ToggleHidden(StudyRef::Delta(1)));
    assert!(set.footprint.hidden && set.indicators[1].display().hidden);
    assert_eq!(set.footprint.imbalance_pct, 400);
    let back: StudySet = serde_json::from_str(&serde_json::to_string(&set).unwrap()).unwrap();
    assert_eq!(back, set);
    set.apply(&StudyAction::Remove(StudyRef::Footprint));
    set.apply(&StudyAction::Remove(StudyRef::Delta(1)));
    assert_eq!(set.footprint.mode, FootprintMode::Off);
    assert_eq!(set.indicators.len(), 1);
}

#[test]
fn menu_lists_studies_catalog_and_templates() {
    let src = sources();
    let mut set = StudySet::default();
    set.apply(&StudyAction::AddVwap(Reset::Eth));
    set.apply(&StudyAction::AddProfile(ProfileMode::Session));
    let templates = vec!["scalp".to_owned()];
    let m = StudyMenu::default();
    let items = m.items(&input(&set, &src, &templates));
    assert_eq!(
        labels(&items),
        [
            "# On this chart",
            "VWAP ETH",
            "Volume profile · session · ETH",
            "Levels · Demo levels",
            "# Add",
            "+ VWAP · ETH session",
            "+ VWAP · RTH session",
            "+ VWAP · weekly",
            "+ VWAP · monthly",
            "+ Anchored VWAP (click a bar)",
            "+ Moving average · EMA 20",
            "+ Moving average · SMA 20",
            "+ Bollinger Bands · 20, 2σ",
            "+ Cumulative delta · ETH session",
            "+ Cumulative delta · RTH session",
            "+ Cumulative delta · no reset",
            "+ Footprint · sell × buy",
            "+ Footprint · delta",
            "+ Footprint · volume",
            "# Templates",
            "+ Apply template \"scalp\"",
            "+ Save these studies as a template…",
            "+ Save as default for new charts",
            "(Changes apply to this chart.)",
        ]
    );
    // Typing filters the catalog, like the picker; the cursor follows the first match.
    let mut m = StudyMenu::default();
    for c in ["r", "t", "h"] {
        m.key(&items, c, Some(c));
    }
    let items = m.items(&input(&set, &src, &templates));
    m.follow_query(&items);
    let adds: Vec<_> = labels(&items)
        .into_iter()
        .filter(|l| l.starts_with("+ VWAP") || l.starts_with("+ Anchored"))
        .collect();
    assert_eq!(adds, ["+ VWAP · RTH session"]);
    assert_eq!(
        m.activate(&items, m.cursor),
        Some(MenuCmd::Set(StudyAction::AddVwap(Reset::Rth)))
    );
    // Nothing matches: said so.
    let m = StudyMenu {
        query: "zzz".into(),
        ..StudyMenu::default()
    };
    assert!(
        labels(&m.items(&input(&set, &src, &templates)))
            .contains(&"(No study matches \"zzz\")".to_owned())
    );
}

#[test]
fn editor_controls_are_commands_and_keys_navigate() {
    let src = sources();
    let mut set = StudySet::default();
    set.apply(&StudyAction::AddVwap(Reset::Eth));
    let mut m = StudyMenu::default();
    let items = m.items(&input(&set, &src, &[]));
    // Enter on the VWAP row opens its editor (the gear).
    m.clamp(&items);
    assert_eq!(m.cursor, 1);
    let cmd = m.key(&items, "enter", None).unwrap();
    assert_eq!(m.local(cmd), None);
    let items = m.items(&input(&set, &src, &[]));
    let l = labels(&items);
    assert_eq!(
        &l[1..20],
        [
            "VWAP ETH",
            "  Reset: ETH *",
            "  Reset: RTH",
            "  Reset: Week",
            "  Reset: Month",
            "  Reset: Anchored…",
            "  Bands: 1σ",
            "  Bands: 2σ",
            "  Bands: 3σ",
            "  Color: 1 *",
            "  Color: 2",
            "  Color: 3",
            "  Color: 4",
            "  Width: Thin",
            "  Width: Normal *",
            "  Width: Thick",
            "  Label: Show in legend *",
            "Levels · Demo levels",
            "# Add",
        ]
    );
    // Down three times: Reset RTH → Week → ... ; Enter applies.
    m.key(&items, "down", None);
    assert_eq!(
        m.key(&items, "enter", None),
        Some(MenuCmd::Set(StudyAction::SetReset(0, Reset::Eth)))
    );
    m.key(&items, "down", None);
    let c = m.key(&items, "space", None).unwrap(); // Space activates a control too
    assert_eq!(c, MenuCmd::Set(StudyAction::SetReset(0, Reset::Rth)));
    // Anchored… asks for a click on a bar.
    assert_eq!(m.activate(&items, 6), Some(MenuCmd::BeginAnchor(Some(0))));
    // The eye hides; Delete removes; Esc closes the editor, then the menu.
    assert_eq!(
        m.eye(&items, 1),
        Some(MenuCmd::Set(StudyAction::ToggleHidden(StudyRef::Vwap(0))))
    );
    let del = m.delete(&items, 1).unwrap();
    assert_eq!(m.local(del.clone()), Some(del));
    assert_eq!(
        m.editing, None,
        "removing the edited study closes its editor"
    );
    m.editing = Some(StudyRef::Vwap(0));
    assert_eq!(m.key(&items, "escape", None), None);
    assert_eq!(m.editing, None);
    assert_eq!(m.key(&items, "escape", None), Some(MenuCmd::Close));
    // Up from the top wraps to the last focusable item.
    let mut m = StudyMenu::default();
    m.clamp(&items);
    m.key(&items, "up", None);
    assert!(
        matches!(items[m.cursor], Item::Action { ref label, .. } if label.starts_with("Save as default"))
    );
}

#[test]
fn actions_change_the_set_and_hidden_studies_keep_their_settings() {
    let mut set = StudySet::default();
    set.apply(&StudyAction::AddVwap(Reset::Eth));
    set.apply(&StudyAction::AddVwap(Reset::Rth));
    // New VWAPs take the next free colour.
    let colours: Vec<u8> = set
        .indicators
        .iter()
        .filter_map(|c| match c {
            IndicatorConfig::Vwap { color, .. } => Some(*color),
            _ => None,
        })
        .collect();
    assert_eq!(colours, [0, 1]);
    for a in [
        StudyAction::ToggleBand(0, 1),
        StudyAction::ToggleBand(0, 3),
        StudyAction::SetColor(0, 2),
        StudyAction::SetWidth(0, 3),
        StudyAction::ToggleLabel(0),
        StudyAction::ToggleHidden(StudyRef::Vwap(0)),
    ] {
        set.apply(&a);
    }
    let IndicatorConfig::Vwap {
        spec,
        color,
        display,
    } = set.indicators[0]
    else {
        panic!("a VWAP")
    };
    assert_eq!((spec.bands.0, color), (0b101, 2));
    assert_eq!(
        (display.width, display.label, display.hidden),
        (3, false, true)
    );
    // Showing again restores it exactly.
    set.apply(&StudyAction::ToggleHidden(StudyRef::Vwap(0)));
    let IndicatorConfig::Vwap {
        spec: s2,
        display: d2,
        ..
    } = set.indicators[0]
    else {
        panic!("a VWAP")
    };
    assert_eq!((s2, d2.hidden, d2.width), (spec, false, 3));
    // Profile: add, edit, hide, remove.
    set.apply(&StudyAction::AddProfile(ProfileMode::Composite));
    set.apply(&StudyAction::SetProfileSession(ProfileSession::Rth));
    set.apply(&StudyAction::ToggleProfileSplit);
    set.apply(&StudyAction::SetProfileGrouping(Some(5)));
    set.apply(&StudyAction::ToggleHidden(StudyRef::Profile));
    assert!(set.profile.hidden && set.profile.split);
    assert_eq!(set.profile.grouping, Some(5));
    set.apply(&StudyAction::Remove(StudyRef::Profile));
    assert_eq!(set.profile.mode, ProfileMode::Off);
    // Levels: per-kind visibility and labels; hiding the source keeps them.
    set.apply(&StudyAction::ToggleLevelKind("demo".into(), "zone".into()));
    set.apply(&StudyAction::ToggleLevelLabels("demo".into()));
    set.apply(&StudyAction::ToggleHidden(StudyRef::Levels("demo".into())));
    assert!(!set.levels.source_shown("demo"));
    assert!(!set.levels.kind_shown("demo", "zone") && !set.levels.labels_shown("demo"));
    // Out-of-range indices are ignored (a stale menu), never a panic.
    set.apply(&StudyAction::SetColor(9, 1));
    set.apply(&StudyAction::Remove(StudyRef::Vwap(9)));
    set.apply(&StudyAction::Remove(StudyRef::Vwap(1)));
    assert_eq!(set.indicators.len(), 1);
}

fn bar(i: i64) -> Bar {
    Bar {
        time: UtcMillis(1_000_000 + i * 60_000),
        open: Price(600_000),
        high: Price(600_100),
        low: Price(599_900),
        close: Price(600_025),
        volume: Qty(1),
    }
}

#[test]
fn anchored_vwap_takes_the_clicked_bar() {
    let bars: Vec<Bar> = (0..10).map(bar).collect();
    let mut vp = Viewport::default();
    vp.set_size(800.0, 400.0);
    vp.bar_px = 10.0;
    vp.right_index = 9.0; // the newest bar's centre at x = 800
    // Bar 9 at x 800, bar 5 at x 760; a click within half a bar picks it.
    assert_eq!(anchor_at(&bars, &vp, 800.0), Some(bars[9].time.0));
    assert_eq!(anchor_at(&bars, &vp, 756.0), Some(bars[5].time.0));
    assert_eq!(anchor_at(&bars, &vp, 764.0), Some(bars[5].time.0));
    // Left of the first bar or right of the last: nothing to anchor at.
    assert_eq!(anchor_at(&bars, &vp, 600.0), None);
    vp.right_index = 12.0;
    assert_eq!(anchor_at(&bars, &vp, 790.0), None);
    assert_eq!(anchor_at(&[], &vp, 400.0), None);
    // Anchoring an existing VWAP, or adding an anchored one.
    let mut set = StudySet::default();
    set.apply(&StudyAction::AddVwap(Reset::Eth));
    set.apply(&StudyAction::AnchorAt {
        index: Some(0),
        from: bars[5].time.0,
    });
    set.apply(&StudyAction::AnchorAt {
        index: None,
        from: bars[2].time.0,
    });
    let resets: Vec<Reset> = set
        .indicators
        .iter()
        .filter_map(|c| match c {
            IndicatorConfig::Vwap { spec, .. } => Some(spec.reset),
            _ => None,
        })
        .collect();
    assert_eq!(
        resets,
        [
            Reset::Anchored {
                from: bars[5].time.0
            },
            Reset::Anchored {
                from: bars[2].time.0
            }
        ]
    );
}

#[test]
fn templates_save_apply_and_round_trip() {
    let mut set = StudySet::default();
    set.apply(&StudyAction::AddVwap(Reset::Rth));
    set.apply(&StudyAction::ToggleBand(0, 2));
    set.apply(&StudyAction::AddProfile(ProfileMode::Session));
    set.apply(&StudyAction::ToggleLevelKind("demo".into(), "zone".into()));
    let mut templates = Vec::new();
    save_template(&mut templates, "open drive", set.clone());
    save_template(&mut templates, "plain", StudySet::default());
    save_template(&mut templates, "open drive", set.clone()); // replaces, no duplicate
    assert_eq!(templates.len(), 2);
    // Persisted as JSON (app settings) and back.
    let json = serde_json::to_string(&templates).unwrap();
    let back: Vec<StudyTemplate> = serde_json::from_str(&json).unwrap();
    assert_eq!(back, templates);
    // An empty set writes nothing but its name.
    assert!(json.contains(r#"{"name":"plain","set":{}}"#), "{json}");
    // Applying a template to a new chart gives exactly its studies.
    let mut fresh = StudySet::default();
    fresh.apply(&StudyAction::Replace(back[0].set.clone()));
    assert_eq!(fresh, set);
    // Naming flow: StartSaveTemplate, type, Enter.
    let mut m = StudyMenu::default();
    assert_eq!(m.local(MenuCmd::StartSaveTemplate), None);
    for c in ["O", "D"] {
        assert_eq!(m.key(&[], c, Some(c)), None);
    }
    assert_eq!(
        m.key(&[], "enter", None),
        Some(MenuCmd::SaveTemplate("OD".into()))
    );
    assert_eq!(m.naming, None);
}

/// The popover rule: a menu closed half-edited (an outside click, Esc, another popover
/// opening) comes back exactly as it was: the editor open on the same study, the cursor on the
/// same control, a template name half typed.
#[test]
fn a_closed_menu_reopens_with_its_draft() {
    use crate::popover::{reopen, shut};
    let src = sources();
    let mut set = StudySet::default();
    set.apply(&StudyAction::AddVwap(Reset::Eth));
    let mut open = Some(StudyMenu::default());
    let mut draft = None;
    let m = open.as_mut().unwrap();
    let items = m.items(&input(&set, &src, &[]));
    m.clamp(&items);
    let cmd = m.key(&items, "enter", None).unwrap();
    assert_eq!(m.local(cmd), None);
    let items = m.items(&input(&set, &src, &[]));
    m.key(&items, "down", None);
    m.naming = Some("scal".into());
    let before = m.clone();
    assert_eq!(before.editing, Some(StudyRef::Vwap(0)));

    assert!(shut(&mut open, &mut draft));
    assert_eq!(open, None);
    let mut again = reopen(&mut draft);
    again.clamp(&again.items(&input(&set, &src, &[])));
    assert_eq!(again, before);
    let l = labels(&again.items(&input(&set, &src, &[])));
    assert_eq!(l[2], "  Reset: ETH *", "the editor is open again");
    // Once reopened the draft is used up: closing a finished menu starts fresh next time.
    assert_eq!(draft, None);
}

/// Roadmap 10a: EMA, SMA and Bollinger Bands are in the Add list, listed with their
/// parameters, edited (length, source, σ, color, line, label), hidden and removed through
/// `StudyAction`s, and saved in the set as optional fields.
#[test]
fn averages_are_studies() {
    use lines_indicators::{AverageKind, IndicatorConfig, Sigma, Source};
    let src = sources();
    let mut set = StudySet::default();
    let items = StudyMenu::default().items(&input(&set, &src, &[]));
    let l = labels(&items);
    for want in [
        "+ Moving average · EMA 20",
        "+ Moving average · SMA 20",
        "+ Bollinger Bands · 20, 2σ",
    ] {
        assert!(l.contains(&want.to_owned()), "{l:?}");
    }
    set.apply(&StudyAction::AddVwap(Reset::Eth));
    set.apply(&StudyAction::AddAverage(AverageKind::Ema));
    set.apply(&StudyAction::AddAverage(AverageKind::Bollinger));
    // Colors continue after the VWAP's.
    assert_eq!(set.indicators[1].color(), Some(1));
    assert_eq!(set.indicators[2].color(), Some(2));
    set.apply(&StudyAction::SetLength(1, 9));
    set.apply(&StudyAction::SetSource(2, Source::Hlc3));
    set.apply(&StudyAction::SetSigma(2, Sigma(25)));
    set.apply(&StudyAction::SetSigma(1, Sigma(25))); // not Bollinger: ignored
    set.apply(&StudyAction::SetColor(1, 3));
    set.apply(&StudyAction::SetWidth(1, 3));
    set.apply(&StudyAction::SetLength(1, 0)); // clamped, never a 0-bar window
    assert_eq!(set.indicators[1].average().unwrap().length, 1);
    set.apply(&StudyAction::SetLength(1, 9));
    let menu = StudyMenu {
        editing: Some(StudyRef::Average(2)),
        ..StudyMenu::default()
    };
    let l = labels(&menu.items(&input(&set, &src, &[])));
    assert!(l.contains(&"EMA 9".to_owned()), "{l:?}");
    assert!(l.contains(&"Bollinger 20 HLC3 2.5σ".to_owned()), "{l:?}");
    for want in [
        "  Length: 20 *",
        "  Source: HLC3 *",
        "  Width (σ): 2.5σ *",
        "  Color: 3 *",
        "  Line: Normal *",
        "  Label: Show in legend *",
    ] {
        assert!(l.contains(&want.to_owned()), "{want}: {l:?}");
    }
    let ema = &set.indicators[1];
    assert_eq!(ema.color(), Some(3));
    assert_eq!(ema.display().width, 3);
    // Hide and show; the configuration stays.
    set.apply(&StudyAction::ToggleHidden(StudyRef::Average(1)));
    assert!(set.indicators[1].display().hidden);
    let l = labels(&StudyMenu::default().items(&input(&set, &src, &[])));
    assert!(l.contains(&"EMA 9 [hidden]".to_owned()), "{l:?}");
    set.apply(&StudyAction::ToggleHidden(StudyRef::Average(1)));
    // Saved and restored with the set.
    let json = serde_json::to_string(&set).unwrap();
    assert!(json.contains(r#""kind":"ema","length":9"#), "{json}");
    let back: StudySet = serde_json::from_str(&json).unwrap();
    assert_eq!(back, set);
    // Removed.
    set.apply(&StudyAction::Remove(StudyRef::Average(1)));
    assert!(matches!(
        set.indicators[1],
        IndicatorConfig::Bollinger { .. }
    ));
}
