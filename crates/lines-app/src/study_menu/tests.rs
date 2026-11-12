use super::*;
use lines_core::{Price, Qty, UtcMillis};

fn sources() -> Vec<SourceInfo> {
    vec![SourceInfo {
        id: "demo".into(),
        label: "Demo levels".into(),
        kinds: vec!["pivot".into(), "zone".into()],
        series: Vec::new(),
        parents: Vec::new(),
        styled_off: Vec::new(),
    }]
}

fn input<'a>(set: &'a StudySet, src: &'a [SourceInfo], templates: &'a [String]) -> MenuInput<'a> {
    MenuInput {
        set,
        scope: StudyScope::Symbol("ES".into()),
        sources: src,
        templates,
        has_rth: true,
        opens: SessionOpens::default(),
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
            Item::Drop { group, label, .. } => format!("  {group}: {label} ▾"),
            Item::Step { group, label, .. } => format!("  {group}: − {label} +"),
            Item::Action { label, .. } => format!("+ {label}"),
        })
        .collect()
}

/// A control's state and command by its label: a chip, or an option in a dropdown.
fn option(items: &[Item], name: &str) -> Option<(bool, MenuCmd)> {
    items.iter().find_map(|it| match it {
        Item::Control { label, on, cmd, .. } if label == name => Some((*on, cmd.clone())),
        Item::Drop { options, .. } => options
            .iter()
            .find(|o| o.label == name)
            .map(|o| (o.on, o.cmd.clone())),
        _ => None,
    })
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
            Item::Control { group, .. } | Item::Drop { group, .. } | Item::Step { group, .. } => {
                Some(*group)
            }
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

/// Volume (owner-approved 2026-11-09): added from the catalog as plain bars on the chart
/// (owner, 2026-11-10; along the plot's bottom), one per chart; "Volume: in a pane" moves it; each option is a control in its editor, off by default; the average's
/// length is 20 when first turned on; it sits beside cumulative delta (each in its own pane).
#[test]
fn volume_is_a_study_with_its_options() {
    use lines_indicators::{DeltaReset, IndicatorConfig, VolumeSpec};
    let src = sources();
    let mut set = StudySet::default();
    let items = StudyMenu::default().items(&input(&set, &src, &[]));
    let add = items
        .iter()
        .position(|i| matches!(i, Item::Action { label, .. } if label == "Volume"))
        .expect("in the catalog");
    let mut m = StudyMenu::default();
    let Some(MenuCmd::Set(a)) = m.activate(&items, add) else {
        panic!("a study action")
    };
    set.apply(&a);
    set.apply(&StudyAction::AddVolume); // one per chart
    set.apply(&StudyAction::AddDelta(DeltaReset::Eth));
    assert_eq!(set.indicators.len(), 2);
    let IndicatorConfig::Volume { spec, .. } = set.indicators[0] else {
        panic!("volume")
    };
    assert_eq!(spec, VolumeSpec::added());
    assert!(
        spec.overlay && !set.indicators[0].in_subpane(),
        "on the chart"
    );
    // The command moves it into a pane (and back), never adding a second.
    set.apply(&StudyAction::PlaceVolume(false));
    assert_eq!(set.indicators.len(), 2);
    assert_eq!(
        set.indicators.iter().filter(|c| c.in_subpane()).count(),
        2,
        "each in its own lower pane"
    );
    let IndicatorConfig::Volume { spec, .. } = set.indicators[0] else {
        panic!("volume")
    };
    assert_eq!(spec, VolumeSpec::default());
    let l = labels(&m.items(&input(&set, &src, &[])));
    assert!(l.contains(&"Volume".to_owned()), "{l:?}");
    assert!(!l.contains(&"+ Volume".to_owned()), "not offered twice");
    // Its editor: every option off; the length only once the average is on.
    m.editing = Some(StudyRef::Volume(0));
    let l = labels(&m.items(&input(&set, &src, &[])));
    for want in [
        "  Color: By candle",
        "  Average: Show",
        "  Split: Buy / sell",
        "  Display: Pane *",
        "  Display: Overlay",
        "  Label: Show title *",
        "  Style: Style…",
    ] {
        assert!(l.contains(&want.to_owned()), "{want}: {l:?}");
    }
    assert!(!l.iter().any(|x| x.starts_with("  Length")));
    // Turn each on through its control's own command.
    for label in ["By candle", "Show", "Buy / sell", "Overlay"] {
        let items = m.items(&input(&set, &src, &[]));
        let i = items
            .iter()
            .position(|it| matches!(it, Item::Control { label: x, .. } if x == label))
            .unwrap();
        let Some(MenuCmd::Set(a)) = m.activate(&items, i) else {
            panic!("{label}")
        };
        set.apply(&a);
    }
    let IndicatorConfig::Volume { spec, .. } = set.indicators[0] else {
        panic!("volume")
    };
    assert!(spec.color_by_candle && spec.average && spec.split && spec.overlay);
    // On the chart already: placing it there keeps it (and its options), one Volume.
    set.apply(&StudyAction::PlaceVolume(true));
    assert_eq!(set.indicators.len(), 2);
    assert!(matches!(set.indicators[0], IndicatorConfig::Volume { spec: s, .. } if s == spec));
    assert_eq!(spec.average_len, 20, "20 when first turned on");
    assert!(
        !set.indicators[0].in_subpane(),
        "an overlay is in the price plot"
    );
    let l = labels(&m.items(&input(&set, &src, &[])));
    assert!(l.contains(&"  Length: − 20 bars +".to_owned()), "{l:?}");
    assert!(l.contains(&"  Label: Show in legend *".to_owned()), "{l:?}");
    assert!(
        l.contains(&"Volume · buy/sell · average 20 · overlay".to_owned()),
        "{l:?}"
    );
    set.apply(&StudyAction::SetVolume(
        0,
        VolumeSpec {
            average_len: 50,
            ..spec
        },
    ));
    // Hidden, styled, saved and removed like the others.
    set.apply(&StudyAction::ToggleHidden(StudyRef::Volume(0)));
    assert!(set.indicators[0].display().hidden);
    let mut looks = crate::study_style::Look::defaults(&crate::study_style::VOLUME);
    if let crate::style_editor::PartStyle::Fill(f) =
        &mut looks[crate::study_style::VOLUME_BARS].style
    {
        f.opacity = 60;
    }
    set.apply(&StudyAction::SetStyle(StudyRef::Volume(0), looks.to_vec()));
    assert_eq!(set.indicators[0].style()["bars"]["opacity"], 60);
    let back: StudySet = serde_json::from_str(&serde_json::to_string(&set).unwrap()).unwrap();
    assert_eq!(back, set);
    set.apply(&StudyAction::Remove(StudyRef::Volume(0)));
    assert!(matches!(
        set.indicators.as_slice(),
        [IndicatorConfig::CumDelta { .. }]
    ));
}

/// A level source's lower-pane series studies (decision 0022) are studies: each listed under
/// its source on charts it serves, added once, named on the chart, and removed like the rest.
#[test]
fn a_sources_series_studies_are_offered_added_and_removed() {
    use lines_indicators::IndicatorConfig;
    let src = vec![SourceInfo {
        id: "demo".into(),
        label: "Demo levels".into(),
        kinds: vec![],
        series: vec![
            ("osc".into(), "Demo oscillator".into()),
            ("swing".into(), "Demo swing".into()),
        ],
        parents: Vec::new(),
        styled_off: Vec::new(),
    }];
    let mut set = StudySet::default();
    let l = labels(&StudyMenu::default().items(&input(&set, &src, &[])));
    assert!(
        l.contains(&"+ Demo oscillator · Demo levels".to_owned()),
        "{l:?}"
    );
    assert!(
        l.contains(&"+ Demo swing · Demo levels".to_owned()),
        "{l:?}"
    );
    set.apply(&StudyAction::AddExternal("demo".into(), "osc".into()));
    set.apply(&StudyAction::AddExternal("demo".into(), "osc".into()));
    set.apply(&StudyAction::AddExternal("demo".into(), "swing".into()));
    assert_eq!(set.indicators.len(), 2, "each once");
    assert!(
        set.indicators.iter().all(IndicatorConfig::in_subpane),
        "own panes"
    );
    let l = labels(&StudyMenu::default().items(&input(&set, &src, &[])));
    assert!(
        l.contains(&"Demo oscillator · Demo levels".to_owned()),
        "{l:?}"
    );
    assert!(
        !l.iter().any(|x| x.starts_with("+ Demo")),
        "not offered again: {l:?}"
    );
    // Without the source (another product), the study stays and is named by its ids.
    let l = labels(&StudyMenu::default().items(&input(&set, &[], &[])));
    assert!(l.contains(&"osc · demo".to_owned()), "{l:?}");
    let back: StudySet = serde_json::from_str(&serde_json::to_string(&set).unwrap()).unwrap();
    assert_eq!(back, set);
    set.apply(&StudyAction::Remove(StudyRef::External(0)));
    assert!(matches!(
        set.indicators.as_slice(),
        [IndicatorConfig::External { spec, .. }] if spec.study == "swing"
    ));
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
            "# ES charts",
            "VWAP ETH",
            "Volume profile · session · ETH",
            "Levels · Demo levels",
            "+ Use separate studies on this chart",
            "# Add",
            "+ VWAP · ETH",
            "+ VWAP · RTH",
            "+ VWAP · week",
            "+ VWAP · month",
            "+ VWAP · from a bar…",
            "+ Volume",
            "+ Cumulative delta · ETH",
            "+ Cumulative delta · RTH",
            "+ Cumulative delta · no reset",
            "+ Footprint · sell × buy",
            "+ Footprint · delta",
            "+ Footprint · volume",
            "# Templates",
            "+ Apply template \"scalp\"",
            "+ Save these studies as a template…",
            "+ Save as default for new charts",
            "(Changes apply to every ES chart.)",
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
        .filter(|l| l.starts_with("+ VWAP"))
        .collect();
    assert_eq!(adds, ["+ VWAP · RTH"]);
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
        &l[1..16],
        [
            "VWAP ETH",
            "  Anchor: ETH ▾",
            "  Bands: None ▾",
            "  Color: 1 *",
            "  Color: 2",
            "  Color: 3",
            "  Color: 4",
            "  Width: Thin",
            "  Width: Normal *",
            "  Width: Thick",
            "  Width: Heavy",
            "  Label: Show in legend *",
            "  Style: Style…",
            "Levels · Demo levels",
            "+ Use separate studies on this chart",
        ]
    );
    // Down to Anchor; Enter opens its list at the anchor in force, Enter there picks it.
    m.key(&items, "down", None);
    assert_eq!(m.key(&items, "enter", None), None);
    assert_eq!(m.drop.as_ref().map(|d| (d.at, d.list.lit)), Some((2, 0)));
    assert_eq!(
        m.key(&items, "enter", None),
        Some(MenuCmd::Set(StudyAction::SetReset(0, Reset::Eth)))
    );
    assert!(m.drop.is_none(), "a pick closes the list");
    // Space opens it too; ↓ then Enter picks the next.
    assert_eq!(m.key(&items, "space", None), None);
    m.key(&items, "down", None);
    let c = m.key(&items, "enter", None).unwrap();
    assert_eq!(c, MenuCmd::Set(StudyAction::SetReset(0, Reset::Rth)));
    // Esc closes an open list, not the menu.
    m.activate(&items, 2);
    assert_eq!(m.key(&items, "escape", None), None);
    assert!(m.drop.is_none() && m.editing.is_some());
    // From a bar… asks for a click on a bar.
    m.activate(&items, 2);
    assert_eq!(
        m.drop_choose(&items, 4),
        Some(MenuCmd::BeginAnchor(Some(0)))
    );
    // Bands: several at once; a toggle keeps the list open.
    m.activate(&items, 3);
    assert_eq!(
        m.drop_choose(&items, 1),
        Some(MenuCmd::Set(StudyAction::ToggleBand(0, 2)))
    );
    assert!(m.drop.is_some(), "a toggle keeps the list open");
    m.drop = None;
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
        ..
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
    fresh.apply(&StudyAction::Replace(Box::new(back[0].set.clone())));
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
    assert_eq!(l[2], "  Anchor: ETH ▾", "the editor is open again");
    // Once reopened the draft is used up: closing a finished menu starts fresh next time.
    assert_eq!(draft, None);
}

/// Every study's editor ends with "Style…", which asks for the style editor on that study
/// (one entry and one key path: arrow to it, Enter).
#[test]
fn every_study_editor_offers_its_styles() {
    let src = sources();
    let mut set = StudySet::default();
    set.apply(&StudyAction::AddVwap(Reset::Eth));
    set.apply(&StudyAction::AddDelta(lines_indicators::DeltaReset::Eth));
    set.apply(&StudyAction::AddProfile(ProfileMode::Session));
    set.apply(&StudyAction::AddFootprint(FootprintMode::BidAsk));
    for r in [
        StudyRef::Vwap(0),
        StudyRef::Delta(1),
        StudyRef::Profile,
        StudyRef::Footprint,
        StudyRef::Levels(src[0].id.clone()),
    ] {
        let mut m = StudyMenu {
            editing: Some(r.clone()),
            ..StudyMenu::default()
        };
        let items = m.items(&input(&set, &src, &[]));
        let at = items
            .iter()
            .position(|i| {
                matches!(
                    i,
                    Item::Control {
                        cmd: MenuCmd::Style(_),
                        ..
                    }
                )
            })
            .unwrap_or_else(|| panic!("{r:?}: no Style…"));
        // The last control of the study's editor.
        assert!(!matches!(items[at + 1], Item::Control { .. }), "{r:?}");
        m.clamp(&items);
        while m.cursor != at {
            m.key(&items, "down", None);
        }
        let cmd = m.key(&items, "enter", None).unwrap();
        assert_eq!(m.local(cmd), Some(MenuCmd::Style(r.clone())));
    }
}

/// The Kinds rows show a kind its parent hid as hidden, and clicking it shows that kind alone.
#[test]
fn kinds_hidden_through_their_parent() {
    let src = vec![SourceInfo {
        id: "src".into(),
        label: "Src".into(),
        kinds: vec!["open".into(), "mid".into(), "pivot".into()],
        parents: vec![Some("session"), Some("session"), None],
        styled_off: Vec::new(),
        series: Vec::new(),
    }];
    let mut set = StudySet::default();
    set.levels.hidden_kinds = vec!["src/session".into()];
    let m = StudyMenu {
        editing: Some(StudyRef::Levels("src".into())),
        ..StudyMenu::default()
    };
    let items = m.items(&input(&set, &src, &[]));
    let row = |name: &str| option(&items, name).unwrap();
    assert!(!row("open").0 && !row("mid").0 && row("pivot").0);
    let MenuCmd::Set(a) = row("mid").1 else {
        panic!("not a change");
    };
    set.apply(&a);
    assert!(set.levels.kind_shown_under("src", "mid", Some("session")));
    assert!(!set.levels.kind_shown_under("src", "open", Some("session")));
}

/// A kind whose type is turned off in the style editor reads unchecked, and says where.
#[test]
fn a_type_off_in_style_reads_unchecked() {
    let src = vec![SourceInfo {
        id: "src".into(),
        label: "Src".into(),
        kinds: vec!["pivot".into(), "zone".into()],
        parents: Vec::new(),
        styled_off: vec![true, false],
        series: Vec::new(),
    }];
    let set = StudySet::default();
    let m = StudyMenu {
        editing: Some(StudyRef::Levels("src".into())),
        ..StudyMenu::default()
    };
    let items = m.items(&input(&set, &src, &[]));
    let on = |name: &str| option(&items, name).map(|(on, _)| on);
    assert_eq!(on("pivot (off in Style…)"), Some(false));
    assert_eq!(on("zone"), Some(true));
}

/// Trade history on new charts is optional in the saved defaults (absent: on) and written only
/// when off; it isn't a study, so a set holding only it has none.
#[test]
fn trades_off_is_optional_and_not_a_study() {
    let set: StudySet = serde_json::from_str("{}").unwrap();
    assert!(!set.trades_off && !set.has_studies());
    assert_eq!(serde_json::to_string(&set).unwrap(), "{}");
    let off = StudySet {
        trades_off: true,
        ..Default::default()
    };
    assert_eq!(
        serde_json::to_string(&off).unwrap(),
        r#"{"trades_off":true}"#
    );
    assert!(!off.has_studies());
    let profile = StudySet {
        profile: ProfileConfig {
            mode: crate::profile::ProfileMode::Session,
            ..Default::default()
        },
        ..off.clone()
    };
    assert!(profile.has_studies());
}

/// The session choices name their opens from the product's calendar (ES: CME's 17:00 and
/// 08:30 Chicago) on the chart's clock, in the Add catalog and the VWAP editor ("Anchor").
#[test]
fn session_choices_name_the_calendars_opens() {
    let cal = lines_feed::catalog::sessions(lines_feed::catalog::ES);
    use lines_core::display_tz::{DisplayTz, ZoneChoice};
    // Tuesday 09:00 CST: RTH trading (its open named), ETH's next open this evening.
    let now = lines_core::UtcMillis::parse_iso("2026-11-10T15:00:00Z").unwrap();
    let chicago = DisplayTz::resolve(&ZoneChoice::Named("America/Chicago".into())).unwrap();
    let opens = session_opens(cal, &chicago, now);
    assert_eq!(opens.eth.as_deref(), Some("5:00 pm CST"));
    assert_eq!(opens.rth.as_deref(), Some("8:30 am CST"));
    // The default chart clock (New York): the same opens, an hour later.
    let opens = session_opens(cal, &DisplayTz::exchange(), now);
    assert_eq!(opens.eth.as_deref(), Some("6:00 pm ET"));
    assert_eq!(opens.rth.as_deref(), Some("9:30 am ET"));
    let mut set = StudySet::default();
    set.apply(&StudyAction::AddVwap(Reset::Eth));
    let src = sources();
    let mut inp = input(&set, &src, &[]);
    inp.opens = opens;
    let mut m = StudyMenu {
        editing: Some(StudyRef::Vwap(0)),
        ..StudyMenu::default()
    };
    let items = m.items(&inp);
    let l = labels(&items);
    assert!(
        l.contains(&"  Anchor: ETH (6:00 pm ET open) ▾".to_owned()),
        "{l:#?}"
    );
    assert_eq!(
        option(&items, "RTH (9:30 am ET open)").map(|(on, _)| on),
        Some(false),
        "in the Anchor list"
    );
    m.editing = None;
    let l = labels(&m.items(&inp));
    assert!(
        l.contains(&"+ VWAP · RTH (9:30 am ET open)".to_owned()),
        "{l:#?}"
    );
    // A 24/7 product: its day start, no RTH; no calendar: no time named.
    let btc = lines_feed::catalog::sessions(lines_feed::catalog::BTC_USD);
    assert_eq!(session_opens(btc, &chicago, now).rth, None);
    assert!(session_opens(btc, &chicago, now).eth.is_some());
    assert_eq!(session_opens(None, &chicago, now), SessionOpens::default());
}

/// A replay opened before its first frame names its sessions on its own first recorded frame,
/// never the wall clock; the feed's clock, then the trading core's, come first.
#[test]
fn session_opens_go_by_the_replay_before_its_first_frame() {
    let t = |s: &str| lines_core::UtcMillis::parse_iso(s).unwrap();
    // A manual wall clock: read only live.
    let wall = t("2026-11-10T18:00:00Z");
    let unread = || -> lines_core::UtcMillis { panic!("the wall clock was read") };
    let first = t("2026-11-05T14:00:00Z");
    assert_eq!(opens_clock(None, None, Some(Some(first)), unread), first);
    let feed = t("2026-11-05T15:00:00Z");
    assert_eq!(
        opens_clock(Some(feed), None, Some(Some(first)), unread),
        feed
    );
    let core = t("2026-11-05T16:00:00Z");
    assert_eq!(
        opens_clock(None, Some(core), Some(Some(first)), unread),
        core
    );
    // Live (or a replay with no frame yet): the wall clock.
    assert_eq!(opens_clock(None, None, None, || wall), wall);
    assert_eq!(opens_clock(None, None, Some(None), || wall), wall);
}

/// "Volume: in a pane" (or "on the chart") on a chart without Volume adds one there.
#[test]
fn placing_volume_adds_one_where_asked() {
    use lines_indicators::IndicatorConfig;
    for overlay in [false, true] {
        let mut set = StudySet::default();
        set.apply(&StudyAction::PlaceVolume(overlay));
        let [IndicatorConfig::Volume { spec, .. }] = set.indicators.as_slice() else {
            panic!("one volume: {:?}", set.indicators)
        };
        assert_eq!(spec.overlay, overlay);
    }
}

/// The audited control groups are one field each (owner-approved controls, 2026-11-11): the
/// VWAP's anchor and bands, the profile's mode, labels and grouping, the footprint's grouping,
/// a level source's kinds; numbers have − / +. The rest stay chips.
#[test]
fn audited_groups_are_dropdowns_and_steps() {
    use crate::footprint::FootprintMode;
    use lines_indicators::DeltaReset;
    let src = sources();
    let mut set = StudySet::default();
    set.apply(&StudyAction::AddVwap(Reset::Eth));
    set.apply(&StudyAction::AddDelta(DeltaReset::Eth));
    set.apply(&StudyAction::AddFootprint(FootprintMode::BidAsk));
    set.apply(&StudyAction::AddProfile(
        crate::profile::ProfileMode::Session,
    ));
    let kinds = |r: StudyRef| -> Vec<(String, &'static str)> {
        let m = StudyMenu {
            editing: Some(r),
            ..StudyMenu::default()
        };
        m.items(&input(&set, &src, &[]))
            .iter()
            .filter_map(|i| match i {
                Item::Control { group, .. } => Some(("chips".into(), *group)),
                Item::Drop {
                    group,
                    multi,
                    custom,
                    ..
                } => Some((
                    format!(
                        "drop{}{}",
                        if *multi { " multi" } else { "" },
                        if custom.is_some() { " custom" } else { "" }
                    ),
                    *group,
                )),
                Item::Step { group, .. } => Some(("step".into(), *group)),
                _ => None,
            })
            .collect()
    };
    let has = |v: &[(String, &str)], kind: &str, group: &str| {
        v.iter().any(|(k, g)| k == kind && *g == group)
    };
    let vwap = kinds(StudyRef::Vwap(0));
    assert!(has(&vwap, "drop", "Anchor") && has(&vwap, "drop multi", "Bands"));
    assert!(has(&vwap, "chips", "Color") && has(&vwap, "chips", "Width"));
    let profile = kinds(StudyRef::Profile);
    assert!(has(&profile, "drop", "Mode") && has(&profile, "drop multi", "Label"));
    assert!(has(&profile, "drop custom", "Grouping") && has(&profile, "chips", "Session"));
    let fp = kinds(StudyRef::Footprint);
    assert!(has(&fp, "drop custom", "Grouping") && has(&fp, "chips", "Show"));
    assert!(has(&fp, "step", "Imbalance") && has(&fp, "step", "Min volume"));
    assert!(has(&kinds(StudyRef::Delta(1)), "chips", "Reset"));
    assert!(has(
        &kinds(StudyRef::Levels("demo".into())),
        "drop multi",
        "Kinds"
    ));
}

/// Grouping's "Custom…": typed ticks per row set it (and it is then listed, in force); a bad
/// value sets nothing and keeps the field.
#[test]
fn a_typed_grouping_sets_it() {
    use crate::footprint::FootprintMode;
    let src = sources();
    let mut set = StudySet::default();
    set.apply(&StudyAction::AddFootprint(FootprintMode::BidAsk));
    let mut m = StudyMenu {
        editing: Some(StudyRef::Footprint),
        ..StudyMenu::default()
    };
    let items = m.items(&input(&set, &src, &[]));
    let at = items
        .iter()
        .position(|i| {
            matches!(
                i,
                Item::Drop {
                    group: "Grouping",
                    ..
                }
            )
        })
        .unwrap();
    m.activate(&items, at);
    m.key(&items, "end", None);
    assert_eq!(
        m.key(&items, "enter", None),
        None,
        "Custom… opens its field"
    );
    for bad in ["0", "x"] {
        m.key(&items, bad, Some(bad));
        assert_eq!(m.key(&items, "enter", None), None, "{bad}");
        assert!(m.drop.is_some(), "the field stays");
        m.key(&items, "backspace", None);
    }
    m.key(&items, "4", Some("4"));
    let cmd = m.key(&items, "enter", None);
    assert_eq!(
        cmd,
        Some(MenuCmd::Set(StudyAction::SetFootprintGrouping(Some(4))))
    );
    assert!(m.drop.is_none());
    let Some(MenuCmd::Set(a)) = cmd else {
        unreachable!()
    };
    set.apply(&a);
    let items = m.items(&input(&set, &src, &[]));
    assert!(labels(&items).contains(&"  Grouping: 4 ▾".to_owned()));
    assert_eq!(option(&items, "4").map(|(on, _)| on), Some(true));
}

/// − / + (and ← / → on the line) step a footprint's imbalance by 25 % and its least volume by
/// one, stopping at their ends.
#[test]
fn steps_nudge_footprint_numbers() {
    use crate::footprint::FootprintMode;
    let src = sources();
    let mut set = StudySet::default();
    set.apply(&StudyAction::AddFootprint(FootprintMode::BidAsk));
    let mut m = StudyMenu {
        editing: Some(StudyRef::Footprint),
        ..StudyMenu::default()
    };
    let items = m.items(&input(&set, &src, &[]));
    let at = |g: &str| {
        items
            .iter()
            .position(|i| matches!(i, Item::Step { group, .. } if *group == g))
            .unwrap()
    };
    let pct = set.footprint.imbalance_pct;
    assert_eq!(
        m.nudge(&items, at("Imbalance"), true),
        Some(MenuCmd::Set(StudyAction::SetImbalanceRatio(pct + 25)))
    );
    m.cursor = at("Min volume");
    let min = set.footprint.imbalance_min;
    assert_eq!(
        m.key(&items, "right", None),
        Some(MenuCmd::Set(StudyAction::SetImbalanceMin(min + 1)))
    );
    set.apply(&StudyAction::SetImbalanceMin(1));
    let items = m.items(&input(&set, &src, &[]));
    assert_eq!(m.nudge(&items, m.cursor, false), None, "at its least");
}

/// A level source's kinds (RS Levels' among them) are one multi-select: its field counts them.
#[test]
fn level_kinds_are_a_multi_select() {
    let src = vec![SourceInfo {
        id: "rs".into(),
        label: "RS Levels".into(),
        kinds: vec!["pivot".into(), "zone".into(), "varis".into()],
        parents: Vec::new(),
        styled_off: Vec::new(),
        series: Vec::new(),
    }];
    let mut set = StudySet::default();
    let mut m = StudyMenu {
        editing: Some(StudyRef::Levels("rs".into())),
        ..StudyMenu::default()
    };
    let items = m.items(&input(&set, &src, &[]));
    assert!(labels(&items).contains(&"  Kinds: All ▾".to_owned()));
    let at = items
        .iter()
        .position(|i| matches!(i, Item::Drop { group: "Kinds", .. }))
        .unwrap();
    m.activate(&items, at);
    let Some(MenuCmd::Set(a)) = m.key(&items, "space", Some(" ")) else {
        panic!("Space toggles a kind");
    };
    set.apply(&a);
    let items = m.items(&input(&set, &src, &[]));
    m.clamp(&items);
    assert!(m.drop.is_some(), "the list stays open");
    assert!(labels(&items).contains(&"  Kinds: zone, varis ▾".to_owned()));
    // The source learns a kind: the open list's rows would be stale, so it closes.
    let mut grown = src.clone();
    grown[0].kinds.push("anchor".into());
    let items = m.items(&input(&set, &grown, &[]));
    m.clamp(&items);
    assert!(m.drop.is_none(), "options changed under the open list");
}
