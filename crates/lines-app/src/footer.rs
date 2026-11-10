//! The window footer (status line), in three zones (docs/design.md §3, §7):
//!
//! - **Safety** (left): what a trader must never miss, in a fixed order, never hidden or
//!   folded: PAPER, SIM or LIVE as a word; trading on with how many DOMs are armed; feeds that
//!   aren't live; recording, with its time and Stop; a pending restart to update; a notice that
//!   needs action. At narrow widths each item shortens its words ("ON 2", "! 1 stale"), never
//!   drops; the full words are in its tooltip.
//! - **Activity** (center): one line for background work (a data download, a data-folder
//!   copy), a static glyph and a percent, never a spinner (§2).
//! - **Links** (right): what the tab isn't showing and where to go ("1 open", "DOM hidden:
//!   show", "Layout: trading"). They fold into a ⋯ menu from the right as the window narrows,
//!   and a right click hides one (safety items can't be hidden).
//!
//! The words and the fit are pure ([`Safety::items`], [`fit`]) and tested at every width; the
//! window draws what [`fit`] decides.

/// One safety item, in its fixed place.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SafetyKind {
    Account,
    Trading,
    Feeds,
    Recording,
    Restart,
    Notice,
}

/// What the safety zone reports, gathered across every window and tab.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Safety {
    /// The most real account any DOM or chart trades on: "LIVE", "SIM" or "PAPER".
    pub account: Option<&'static str>,
    /// DOMs with trading enabled, hidden ones and other tabs' included.
    pub armed: usize,
    /// Feeds that aren't live: (symbol, state word: "reconnecting", "failed", …), each once.
    pub feeds: Vec<(String, &'static str)>,
    /// Recording: its elapsed time ("12:04").
    pub recording: Option<String>,
    /// An update is staged: "Restart to update" (the update's own words, for the tooltip).
    pub restart: Option<String>,
    /// A notice that needs action (a report, an update problem, an option trigger without its
    /// feed): its full text.
    pub notice: Option<String>,
}

/// An item's words: in full, shortened for a narrow window, and its tooltip.
#[derive(Clone, Debug, PartialEq)]
pub struct Item<K> {
    pub kind: K,
    pub full: String,
    pub short: String,
    pub tip: String,
    /// What it draws besides its words (a dot, a button's edges), in px at 100 %.
    pub extra: f32,
}

impl Safety {
    /// The items to show, in their fixed order.
    pub fn items(&self) -> Vec<Item<SafetyKind>> {
        let mut v = Vec::new();
        if let Some(a) = self.account {
            let tip = match a {
                "LIVE" => "A live account: orders are real",
                "SIM" => "A broker's simulated account",
                _ => "Paper trading: Lines simulates the fills",
            };
            v.push(Item {
                kind: SafetyKind::Account,
                full: a.to_owned(),
                short: a.to_owned(),
                tip: tip.to_owned(),
                // The badge's padding, and LIVE's border.
                extra: if a == "LIVE" { 10.0 } else { 8.0 },
            });
        }
        if self.armed > 0 {
            let n = self.armed;
            let doms = if n == 1 { "DOM" } else { "DOMs" };
            v.push(Item {
                kind: SafetyKind::Trading,
                full: format!("Trading on · {n} {doms}"),
                short: format!("ON {n}"),
                tip: format!("Trading is on in {n} {doms} (every window and tab)"),
                extra: 0.0,
            });
        }
        if let Some((sym, word)) = self.feeds.first() {
            let n = self.feeds.len();
            let full = if n == 1 {
                format!("! {sym} {word}")
            } else {
                format!("! {sym} {word} +{}", n - 1)
            };
            let tip = self
                .feeds
                .iter()
                .map(|(s, w)| format!("{s}: {w}"))
                .collect::<Vec<_>>()
                .join(" · ");
            v.push(Item {
                kind: SafetyKind::Feeds,
                full,
                short: format!("! {n} stale"),
                tip: format!("Not live: {tip}"),
                extra: 0.0,
            });
        }
        if let Some(t) = &self.recording {
            v.push(Item {
                kind: SafetyKind::Recording,
                // Its Stop button's words are measured with it ([`REC_STOP`]).
                full: format!("● REC {t} {}", REC_STOP.0).replace("  ", " "),
                short: format!("● REC {}", REC_STOP.1),
                tip: format!("Recording the live feeds ({t})"),
                // The Stop button's edges and padding.
                extra: 22.0,
            });
        }
        if let Some(version) = &self.restart {
            v.push(Item {
                kind: SafetyKind::Restart,
                full: "Restart to update".to_owned(),
                short: "Restart".to_owned(),
                tip: version.clone(),
                extra: 0.0,
            });
        }
        if let Some(n) = &self.notice {
            v.push(Item {
                kind: SafetyKind::Notice,
                full: format!("! {n}"),
                short: "! Notice".to_owned(),
                tip: n.clone(),
                extra: 0.0,
            });
        }
        v
    }
}

/// The recording item's Stop button: in full, and short.
pub const REC_STOP: (&str, &str) = ("Stop recording", "Stop");

/// A link item. The order is the fold order's reverse: the last folds first.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LinkKind {
    /// "1 open": opens the Positions pane.
    Positions,
    /// "DOM hidden: show": shows the tab's hidden panes.
    Hidden,
    /// "1 popped out: re-attach".
    PoppedOut,
    /// "Crosshair sync": turns it off.
    CrosshairSync,
    /// An option trigger's quiet status (the loud one is a safety notice).
    Triggers,
    /// "Layout: trading": opens another layout (never reads as "trading is on").
    Layout,
}

impl LinkKind {
    pub const ALL: [LinkKind; 6] = [
        LinkKind::Positions,
        LinkKind::Hidden,
        LinkKind::PoppedOut,
        LinkKind::CrosshairSync,
        LinkKind::Triggers,
        LinkKind::Layout,
    ];

    /// Its name in the footer's menu (show or hide it).
    pub fn name(self) -> &'static str {
        match self {
            LinkKind::Positions => "Open positions",
            LinkKind::Hidden => "Hidden panes",
            LinkKind::PoppedOut => "Popped-out panes",
            LinkKind::CrosshairSync => "Crosshair sync",
            LinkKind::Triggers => "Option triggers",
            LinkKind::Layout => "Layout name",
        }
    }

    /// Saved name (workspace `hidden_footer_links`).
    pub fn key(self) -> &'static str {
        match self {
            LinkKind::Positions => "positions",
            LinkKind::Hidden => "hidden",
            LinkKind::PoppedOut => "popped_out",
            LinkKind::CrosshairSync => "crosshair_sync",
            LinkKind::Triggers => "triggers",
            LinkKind::Layout => "layout",
        }
    }

    pub fn from_key(k: &str) -> Option<LinkKind> {
        LinkKind::ALL.into_iter().find(|l| l.key() == k)
    }
}

/// The layout's name as the footer says it: "Layout: trading", never the bare name.
pub fn layout_text(name: &str) -> String {
    format!("Layout: {name}")
}

/// Background work: in full ("↓ Download days 62%") and short ("↓ 62%").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Activity {
    pub full: String,
    pub short: String,
    pub tip: String,
}

impl Activity {
    /// `what` at `done` of `total` (a static glyph and a whole percent, never over 100).
    pub fn progress(glyph: &str, what: &str, done: u64, total: u64) -> Activity {
        let pct = if total == 0 {
            0
        } else {
            (done.min(total) as u128 * 100 / total as u128) as u64
        };
        Activity {
            full: format!("{glyph} {what} {pct}%"),
            short: format!("{glyph} {pct}%"),
            tip: format!("{what}: {pct}% done"),
        }
    }
}

/// Space between items, and the zones' padding, at 100 % (`zoom::f` scales it).
pub const GAP: f32 = 12.0;
/// The ⋯ that holds folded links.
pub const MORE: &str = "⋯";

/// What a footer of a given width shows.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Fit {
    /// Safety items in their short words.
    pub short_safety: bool,
    /// The activity line: `None` hidden (no work, or no room), `Some(true)` in short words.
    pub activity: Option<bool>,
    /// How many links show, from the first; the rest are in the ⋯ menu.
    pub links: usize,
    /// The ⋯ menu shows (some links folded).
    pub more: bool,
    /// Even the short safety words don't fit beside the other zones (an extreme width): the
    /// safety items take the whole footer as one flow, in order, with ⋯ last, wrapping onto
    /// [`Fit::rows`] rows; nothing is clipped or dropped.
    pub wrapped: bool,
    /// Rows the footer takes (more than 1 only when wrapped).
    pub rows: usize,
}

/// Lines needed to lay out `widths` (in order) within `width`, `gap` apart: a greedy wrap,
/// one item at least per line.
pub fn wrap_rows(widths: &[f32], width: f32, gap: f32) -> usize {
    let mut rows = 1;
    let mut x = 0.0f32;
    for w in widths {
        let need = if x == 0.0 { *w } else { gap + w };
        if x > 0.0 && x + need > width {
            rows += 1;
            x = *w;
        } else {
            x += need;
        }
    }
    rows
}

/// Fit the footer into `width` (logical px, at the zoom `measure` uses). Links fold first
/// (from the last), then the activity line shortens, then safety words shorten, then the
/// activity line goes; safety items never drop. `gap` is the space between items.
pub fn fit(
    width: f32,
    safety: &[Item<SafetyKind>],
    activity: Option<&Activity>,
    links: &[String],
    gap: f32,
    zoom: f32,
    measure: &dyn Fn(&str) -> f32,
) -> Fit {
    let zone = |ws: &mut dyn Iterator<Item = f32>| -> (f32, usize) {
        ws.fold((0.0, 0), |(w, n), x| (w + x, n + 1))
    };
    let span = |(w, n): (f32, usize)| {
        if n == 0 {
            0.0
        } else {
            w + gap * (n - 1) as f32
        }
    };
    let safety_w = |short: bool| {
        span(zone(&mut safety.iter().map(|i| {
            measure(if short { &i.short } else { &i.full }) + i.extra * zoom
        })))
    };
    let act_w = |short: Option<bool>| match (activity, short) {
        (Some(a), Some(s)) => measure(if s { &a.short } else { &a.full }),
        _ => 0.0,
    };
    let links_w = |k: usize| {
        let more = k < links.len();
        let shown = links[..k].iter().map(|l| measure(l));
        let (w, n) = zone(&mut shown.chain(more.then(|| measure(MORE))));
        span((w, n))
    };
    // The three zones are always laid out, so two gaps separate them, empty or not.
    let total = |short: bool, act: Option<bool>, k: usize| {
        safety_w(short) + act_w(act) + links_w(k) + 2.0 * gap
    };
    let act = activity.map(|_| false);
    let n = links.len();
    // Full words with as many links as fit.
    for k in (0..=n).rev() {
        if total(false, act, k) <= width {
            return Fit {
                short_safety: false,
                activity: act,
                links: k,
                more: k < n,
                wrapped: false,
                rows: 1,
            };
        }
    }
    // Then the activity line short, then the safety words short, then no activity line.
    let steps = [
        (false, activity.map(|_| true)),
        (true, activity.map(|_| true)),
        (true, None),
    ];
    // Every link is folded by now: they stay in ⋯, so a narrower window never shows more.
    for (short, act) in steps {
        if total(short, act, 0) <= width {
            return Fit {
                short_safety: short,
                activity: act,
                links: 0,
                more: n > 0,
                wrapped: false,
                rows: 1,
            };
        }
    }
    // Too narrow even for that: the safety items in short words wrap onto more rows, in
    // order, with ⋯ last to reach the links. Nothing is clipped or dropped.
    let mut widths: Vec<f32> = safety
        .iter()
        .map(|i| measure(&i.short) + i.extra * zoom)
        .collect();
    if n > 0 {
        widths.push(measure(MORE));
    }
    Fit {
        short_safety: true,
        activity: None,
        links: 0,
        more: n > 0,
        wrapped: true,
        rows: wrap_rows(&widths, width, gap),
    }
}

/// Where the update's word goes: (restart: "Restart to update", its tooltip the label; a
/// notice that needs action; background news for the activity line). `label` is
/// `update::status_label`'s (text, loud, link).
pub fn route_update(
    label: Option<(String, bool, Option<crate::commands::Command>)>,
) -> (Option<String>, Option<String>, Option<String>) {
    use crate::commands::Command;
    match label {
        None => (None, None, None),
        Some((text, _, Some(Command::RestartToUpdate))) => (Some(text), None, None),
        Some((text, true, _)) => (None, Some(text), None),
        // Held until positions close, or available: news, not an action.
        Some((text, false, _)) => (None, None, Some(text)),
    }
}

/// The most real of two account words: LIVE over SIM over PAPER.
pub fn more_real(a: &'static str, b: &'static str) -> &'static str {
    let rank = |w: &str| match w {
        "LIVE" => 2,
        "SIM" => 1,
        _ => 0,
    };
    if rank(b) > rank(a) { b } else { a }
}

/// A feed's word when it isn't live: silent while its market is open ("stale"), or its
/// connection's state. An ended replay is not a problem (it says so in its panes).
pub fn feed_word(stale: bool, status: &lines_feed::store::FeedStatus) -> Option<&'static str> {
    if stale {
        return Some("stale");
    }
    crate::pane::status_word(status).filter(|w| *w != "ended")
}

/// The armed DOM count and the account word, from every DOM's switch (`dom_switches`). The
/// word is the most real account among the armed DOMs, so an idle LIVE DOM beside an armed
/// paper one never reads as live trading; with none armed, the most real of all.
pub(crate) fn accounts(
    doms: &[crate::workspace::DomSwitch],
    cx: &gpui::App,
) -> (usize, Option<&'static str>) {
    let trading = crate::trading::Trading::try_global(cx);
    let switches: Vec<(Option<Option<&'static str>>, bool)> = doms
        .iter()
        .map(|(_, id, _, on)| {
            let badge = id
                .as_ref()
                .map(|id| trading.and_then(|t| t.account(id)).map(|a| a.badge));
            (badge, *on)
        })
        .collect();
    armed_and_word(&switches)
}

/// [`accounts`] without the app: each DOM's account (`None`: none chosen; `Some(None)`: a
/// broker's account not listed yet; `Some(Some(badge))`) and whether it is armed.
pub fn armed_and_word(
    doms: &[(Option<Option<&'static str>>, bool)],
) -> (usize, Option<&'static str>) {
    let armed = doms.iter().filter(|d| d.1).count();
    let mut account = None;
    for (badge, on) in doms {
        if armed > 0 && !on {
            continue;
        }
        // As the DOM's own badge reads: no account is paper; a broker's account not listed
        // yet is LIVE until it says otherwise (never shown as simulated).
        let word = match badge {
            None => "PAPER",
            Some(b) => b.unwrap_or("LIVE"),
        };
        account = Some(account.map_or(word, |w| more_real(w, word)));
    }
    (armed, account)
}

/// Each feed's last word (`ChartStore::feed_id`, then its label and word, `None` when live).
pub type FeedWords = Vec<(u64, Option<(String, &'static str)>)>;

/// Gather the safety zone's accounts, armed DOMs and feeds (every window and tab, hidden
/// panes included), and the activity line's background work. Never waits on a feed: a store
/// busy this instant keeps its own last word from `prev` (the UI never blocks on a feed).
/// The rest (recording, restart, notices) comes from the workspace.
pub(crate) fn gather(cx: &gpui::App, prev: &FeedWords) -> (Safety, Option<Activity>, FeedWords) {
    let mut safety = Safety::default();
    let doms = crate::workspace::dom_switches(cx, None);
    (safety.armed, safety.account) = accounts(&doms, cx);
    let mut words: FeedWords = Vec::new();
    for store in crate::feeds::Feeds::registry(cx).stores() {
        let id = store.feed_id();
        let stale = store.stale_for().is_some();
        let word = match store.try_read() {
            Some(d) => {
                let label = d
                    .contract_label()
                    .map(|l| l.as_str().to_owned())
                    .unwrap_or_else(|| d.product.symbol.to_owned());
                feed_word(stale, &d.status).map(|w| (label, w))
            }
            // Busy this instant: its last word stands (none if never seen).
            None => prev
                .iter()
                .find(|(p, _)| *p == id)
                .and_then(|(_, w)| w.clone()),
        };
        if let Some((label, w)) = &word
            && !safety.feeds.iter().any(|(l, _)| l == label)
        {
            safety.feeds.push((label.clone(), *w));
        }
        words.push((id, word));
    }
    safety.feeds.sort();
    // Background work: a day download, then a data-folder copy.
    let days = cx
        .try_global::<crate::download_view::DownloadWindowGlobal>()
        .and_then(|g| g.0)
        .and_then(|w| w.read(cx).ok().and_then(|v| v.days_progress()));
    let activity = match days {
        Some((done, total)) => Some(Activity::progress(
            "↓",
            "Download days",
            done as u64,
            total as u64,
        )),
        None => match crate::data_dir::facts().copy {
            Some((done, total, false)) => {
                Some(Activity::progress("⇢", "Copying data", done, total))
            }
            _ => None,
        },
    };
    (safety, activity, words)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tw(s: &str) -> f32 {
        s.chars().count() as f32 * 7.0
    }

    fn busy() -> Safety {
        Safety {
            account: Some("PAPER"),
            armed: 2,
            feeds: vec![("ESZ26".into(), "reconnecting")],
            recording: Some("12:04".into()),
            restart: Some("v0.4.0".into()),
            notice: Some("Couldn't save the layout: disk full".into()),
        }
    }

    fn links() -> Vec<String> {
        vec![
            "1 open".into(),
            "DOM hidden: show".into(),
            layout_text("trading"),
        ]
    }

    /// The safety items are present, in their fixed order, at every width: shortened, never
    /// dropped.
    #[test]
    fn safety_items_show_at_every_width_in_a_fixed_order() {
        let items = busy().items();
        let kinds: Vec<_> = items.iter().map(|i| i.kind).collect();
        let mut sorted = kinds.clone();
        sorted.sort();
        assert_eq!(kinds, sorted, "fixed order");
        assert_eq!(kinds.len(), 6);
        let a = Activity::progress("↓", "Download days", 62, 100);
        // From the widest single item (no width can show less) up.
        let widest = items
            .iter()
            .map(|i| tw(&i.short) + i.extra)
            .fold(0.0f32, f32::max);
        for zoom in [1.0f32, 1.5] {
            let z = |s: &str| tw(s) * zoom;
            for w in (widest * zoom).ceil() as usize..=2400 {
                let w = w as f32;
                let f = fit(w, &items, Some(&a), &links(), GAP * zoom, zoom, &z);
                if !f.wrapped {
                    assert_eq!(f.rows, 1);
                    // One row: everything shown fits; nothing overlaps.
                    let used = width_scaled(&f, &items, Some(&a), &links(), zoom);
                    assert!(used <= w, "{w} at {zoom}: {f:?} needs {used}");
                } else {
                    // Wrapped: every row holds its items, in order, nothing else shows.
                    assert!(
                        f.short_safety && f.activity.is_none() && f.links == 0,
                        "{f:?}"
                    );
                    let mut widths: Vec<f32> =
                        items.iter().map(|i| z(&i.short) + i.extra * zoom).collect();
                    widths.push(z(MORE));
                    assert_eq!(f.rows, wrap_rows(&widths, w, GAP * zoom));
                    for x in &widths {
                        assert!(*x <= w, "an item wider than the row at {w}");
                    }
                }
            }
        }
    }

    fn width_scaled(
        f: &Fit,
        s: &[Item<SafetyKind>],
        a: Option<&Activity>,
        l: &[String],
        zoom: f32,
    ) -> f32 {
        let tw = |x: &str| tw(x) * zoom;
        let gap = GAP * zoom;
        let zone = |ws: Vec<f32>| -> f32 {
            ws.iter().sum::<f32>() + gap * ws.len().saturating_sub(1) as f32
        };
        let safety = zone(
            s.iter()
                .map(|i| tw(if f.short_safety { &i.short } else { &i.full }) + i.extra * zoom)
                .collect(),
        );
        let act = match (a, f.activity) {
            (Some(a), Some(short)) => tw(if short { &a.short } else { &a.full }),
            _ => 0.0,
        };
        let mut links: Vec<f32> = l[..f.links].iter().map(|x| tw(x)).collect();
        if f.more {
            links.push(tw(MORE));
        }
        safety + act + zone(links) + 2.0 * gap
    }

    #[test]
    fn rows_wrap_in_order() {
        assert_eq!(wrap_rows(&[10.0, 10.0, 10.0], 100.0, 5.0), 1);
        assert_eq!(wrap_rows(&[10.0, 10.0, 10.0], 25.0, 5.0), 2);
        assert_eq!(wrap_rows(&[10.0, 10.0, 10.0], 12.0, 5.0), 3);
        assert_eq!(wrap_rows(&[], 12.0, 5.0), 1);
    }

    /// The update's word: a staged update is "Restart to update" with its label as the tip; a
    /// problem (loud) is a notice; held or available is background news.
    #[test]
    fn update_words_go_where_they_belong() {
        use crate::commands::Command;
        let ready = "Update ready: v0.4.0 · Restart to update".to_owned();
        let (restart, notice, news) =
            route_update(Some((ready.clone(), false, Some(Command::RestartToUpdate))));
        assert_eq!(
            (restart.as_deref(), notice, news),
            (Some(ready.as_str()), None, None)
        );
        let s = Safety {
            restart,
            ..Default::default()
        };
        let item = &s.items()[0];
        assert_eq!(
            (item.full.as_str(), item.tip.as_str()),
            ("Restart to update", ready.as_str())
        );
        let roll = "This version failed to start twice: Roll back to v0.3.13".to_owned();
        assert_eq!(
            route_update(Some((roll.clone(), true, Some(Command::RollBackUpdate)))),
            (None, Some(roll), None)
        );
        let held = "Update ready: v0.4.0 · Waits: 1 open position".to_owned();
        assert_eq!(
            route_update(Some((held.clone(), false, None))),
            (None, None, Some(held))
        );
        assert_eq!(route_update(None), (None, None, None));
    }

    /// The safety words shorten only after every link folded and the activity shortened.
    #[test]
    fn links_fold_first_then_activity_then_safety_words() {
        let items = busy().items();
        let a = Activity::progress("↓", "Download days", 62, 100);
        let l = links();
        let mut last = fit(2400.0, &items, Some(&a), &l, GAP, 1.0, &tw);
        assert_eq!(
            (last.links, last.more, last.short_safety),
            (3, false, false)
        );
        assert_eq!(last.activity, Some(false));
        for w in (100..2400).rev().step_by(4) {
            let f = fit(w as f32, &items, Some(&a), &l, GAP, 1.0, &tw);
            // Monotone: narrower never shows more.
            assert!(
                f.links <= last.links || f.short_safety != last.short_safety,
                "{w}"
            );
            if f.short_safety {
                assert_eq!(f.links, 0, "{w}: links fold before safety words shorten");
                assert_ne!(f.activity, Some(false), "{w}");
            }
            if f.links < l.len() {
                assert!(f.more, "{w}: folded links are in ⋯");
            }
            last = f;
        }
        // The narrow mock (~480 px): short safety words and ⋯.
        let f = fit(480.0, &items, Some(&a), &l, GAP, 1.0, &tw);
        assert!(f.short_safety && f.more && f.links == 0, "{f:?}");
    }

    #[test]
    fn short_words_and_tooltips() {
        let items = busy().items();
        let t = items
            .iter()
            .find(|i| i.kind == SafetyKind::Trading)
            .unwrap();
        assert_eq!(
            (t.full.as_str(), t.short.as_str()),
            ("Trading on · 2 DOMs", "ON 2")
        );
        assert!(t.tip.contains("every window and tab"));
        let f = items.iter().find(|i| i.kind == SafetyKind::Feeds).unwrap();
        assert_eq!(
            (f.full.as_str(), f.short.as_str()),
            ("! ESZ26 reconnecting", "! 1 stale")
        );
        let one = Safety {
            armed: 1,
            ..Default::default()
        };
        assert_eq!(one.items()[0].full, "Trading on · 1 DOM");
        let rec = items
            .iter()
            .find(|i| i.kind == SafetyKind::Recording)
            .unwrap();
        assert_eq!(rec.full, "● REC 12:04 Stop recording");
        assert_eq!(rec.short, "● REC Stop");
        // Nothing to report: no items (no account, no armed DOM, every feed live).
        assert!(Safety::default().items().is_empty());
        assert_eq!(layout_text("trading"), "Layout: trading");
    }

    #[test]
    fn progress_is_a_whole_percent_never_over_100() {
        let a = Activity::progress("↓", "Download days", 2, 3);
        assert_eq!(
            (a.full.as_str(), a.short.as_str()),
            ("↓ Download days 66%", "↓ 66%")
        );
        assert_eq!(Activity::progress("↓", "x", 5, 3).short, "↓ 100%");
        assert_eq!(Activity::progress("↓", "x", 0, 0).short, "↓ 0%");
    }

    #[test]
    fn the_most_real_account_and_feed_words() {
        assert_eq!(more_real("PAPER", "LIVE"), "LIVE");
        assert_eq!(more_real("LIVE", "SIM"), "LIVE");
        assert_eq!(more_real("PAPER", "SIM"), "SIM");
        assert_eq!(more_real("PAPER", "PAPER"), "PAPER");
        use lines_feed::store::FeedStatus as S;
        assert_eq!(feed_word(false, &S::Live), None);
        assert_eq!(feed_word(true, &S::Live), Some("stale"));
        assert_eq!(
            feed_word(false, &S::ReplayFinished),
            None,
            "an ended replay is fine"
        );
        assert_eq!(feed_word(false, &S::Connecting), Some("connecting"));
        assert_eq!(feed_word(false, &S::Stopped), Some("stopped"));
    }

    /// The account word: from the armed DOMs when any is armed (an idle LIVE DOM never makes an
    /// armed paper one read live, and an idle PAPER one never hides an armed LIVE one); a
    /// broker account not listed yet is LIVE; no DOM, no word.
    #[test]
    fn the_account_word_follows_the_armed_doms() {
        let (paper, live, sim) = (Some(Some("PAPER")), Some(Some("LIVE")), Some(Some("SIM")));
        assert_eq!(
            armed_and_word(&[(live, false), (paper, true)]),
            (1, Some("PAPER"))
        );
        assert_eq!(
            armed_and_word(&[(paper, false), (live, true)]),
            (1, Some("LIVE"))
        );
        assert_eq!(
            armed_and_word(&[(paper, false), (live, false)]),
            (0, Some("LIVE"))
        );
        assert_eq!(
            armed_and_word(&[(sim, true), (paper, true)]),
            (2, Some("SIM"))
        );
        assert_eq!(
            armed_and_word(&[(Some(None), true)]),
            (1, Some("LIVE")),
            "unlisted"
        );
        assert_eq!(
            armed_and_word(&[(None, false)]),
            (0, Some("PAPER")),
            "no account"
        );
        assert_eq!(armed_and_word(&[]), (0, None));
    }

    #[test]
    fn link_keys_round_trip() {
        for l in LinkKind::ALL {
            assert_eq!(LinkKind::from_key(l.key()), Some(l));
        }
        assert_eq!(LinkKind::from_key("nope"), None);
    }
}
