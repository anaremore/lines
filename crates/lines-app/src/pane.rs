//! What every pane shares: it is given a [`PaneFeed`] and a theme, owns its own view state,
//! and redraws only when its own store changed.
//!
//! Each pane polls *its* store's atomic revision on the UI thread every 8 ms and notifies only
//! itself (decision 0001: never per tick). The workspace renders panes as cached views, so a
//! pane whose market didn't move replays its previous frame instead of repainting: N panes
//! don't cost N× when one market ticks. The same poll notices a live feed going quiet
//! (`ChartStore::stale_for`, lock-free) and redraws the pane with its stale marker. A pane
//! that isn't visible (a background tab, a
//! minimised window) doesn't notify at all, so it does no paint work; it catches up on the
//! first poll after it is shown again (decision 0016).
//!
//! The workspace tells a pane its link group (shown as a coloured letter in the header) and
//! whether it is visible; panes never reference each other.

use std::sync::Arc;
use std::time::Duration;

use gpui::{
    Context, Div, EventEmitter, FocusHandle, FontWeight, InteractiveElement, KeyDownEvent,
    MouseButton, MouseDownEvent, ParentElement, Styled, div, px,
};
use lines_core::CivilDate;
use lines_feed::store::ChartStore;

use crate::feeds::PaneFeed;
use crate::layout::{LinkGroup, PaneKind, link_letter};
use crate::theme::Theme;
use crate::wsdoc::PaneDoc;

/// A pane switched itself to another feed (e.g. its symbol picker). The workspace forwards it
/// to the other panes in the same link group.
#[derive(Clone)]
pub struct FeedChanged(pub PaneFeed);

/// State the workspace sets on every pane.
#[derive(Clone, Debug)]
pub struct PaneCommon {
    /// Shown in a visible window's active tab. Hidden panes skip their poll's notify.
    pub visible: bool,
    pub link: LinkGroup,
    /// Frames painted (the benchmark checks hidden panes paint nothing).
    pub paints: u64,
    /// Offer the one-click switch when unavailable (off when the tab offers it once for
    /// every pane).
    pub offer_switch: bool,
}

impl Default for PaneCommon {
    fn default() -> Self {
        PaneCommon {
            visible: true,
            link: None,
            paints: 0,
            offer_switch: true,
        }
    }
}

pub trait Pane: EventEmitter<FeedChanged> + Sized + 'static {
    fn store(&self) -> &Arc<ChartStore>;
    fn feed(&self) -> &PaneFeed;
    fn common(&mut self) -> &mut PaneCommon;
    /// The revision this pane last drew.
    fn seen_revision(&mut self) -> &mut u64;
    /// The store revision this pane redraws on. Default: every change. Panes that don't
    /// show depth use `ChartStore::chart_revision`, so a busy book repaints only the DOM.
    fn revision(&self) -> u64 {
        self.store().revision()
    }
    /// Follow another feed (a linked pane switched). Resets per-market view state.
    fn set_feed(&mut self, feed: PaneFeed, cx: &mut Context<Self>);
    /// Called on every poll tick while visible (e.g. rate counters); default nothing.
    fn on_tick(&mut self) {}
    /// Called on every poll tick while hidden: bookkeeping only (a few atomics), never paint
    /// work. Default nothing.
    fn on_hidden_tick(&mut self) {}
    /// Something besides the store changed (e.g. indicator values from the indicator
    /// thread); polled every visible tick, redraws when true. Default: nothing.
    fn changed(&mut self) -> bool {
        false
    }
    /// Where keyboard input goes when this pane has focus.
    fn focus_handle(&self) -> &FocusHandle;
    /// Write this pane's view state into its saved form.
    fn save_state(&self, doc: &mut PaneDoc);
    /// The one-click action on an unavailable pane: show the source's own symbol here (a
    /// deliberate switch, saved and followed by the link group like the picker's).
    fn show_suggested(&mut self, cx: &mut Context<Self>) {
        match self.feed().to_suggested(&crate::feeds::Feeds::registry(cx)) {
            Ok(feed) => {
                self.set_feed(feed.clone(), cx);
                cx.emit(FeedChanged(feed));
            }
            Err(e) => tracing::warn!("{e:#}"),
        }
    }
}

/// Start the pane's revision poll: notify when its store changed, nothing otherwise, and
/// nothing at all while hidden.
pub fn watch_store<V: Pane>(cx: &mut Context<V>) {
    cx.spawn(async move |this, cx| {
        // Whole seconds the feed has been stale (redraw when it changes: the marker counts).
        let mut stale_secs: Option<u64> = None;
        loop {
            cx.background_executor()
                .timer(Duration::from_millis(8))
                .await;
            let alive = this.update(cx, |pane, cx| {
                if !pane.common().visible {
                    pane.on_hidden_tick();
                    return;
                }
                pane.on_tick();
                let rev = pane.revision();
                let seen = pane.seen_revision();
                if rev != *seen {
                    *seen = rev;
                    cx.notify();
                }
                let stale = pane.store().stale_for().map(|d| d.as_secs());
                if stale != stale_secs {
                    stale_secs = stale;
                    cx.notify();
                }
                if pane.changed() {
                    cx.notify();
                }
            });
            if alive.is_err() {
                break;
            }
        }
    })
    .detach();
}

/// One hover control in a pane header: an icon button with its action's name (tooltip and
/// accessible label) and command.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Control {
    pub icon: crate::icons::Icon,
    pub name: &'static str,
    pub cmd: crate::commands::Command,
}

const fn control(
    icon: crate::icons::Icon,
    name: &'static str,
    cmd: crate::commands::Command,
) -> Control {
    Control { icon, name, cmd }
}

/// The hover controls a pane's header shows, in drawing order: in a main window, or in its
/// own window when popped out (`detached`). The workspace and the pane window draw them and
/// run their commands; they live here so every header keeps room for them.
pub fn controls(kind: PaneKind, detached: bool) -> &'static [Control] {
    use crate::commands::Command as C;
    use crate::icons::Icon as I;
    const DETACHED: [Control; 3] = [
        control(I::Dock, "Dock beside chart", C::DockBesideChart),
        control(I::Reattach, "Re-attach", C::Reattach),
        control(I::Menu, "Pane menu", C::Menu),
    ];
    const CHART: [Control; 3] = [
        control(I::ChartOnly, "Chart only", C::ChartOnly),
        control(I::PopOut, "Pop out", C::PopOut),
        control(I::Menu, "Pane menu", C::Menu),
    ];
    // Hide away from the menu: a near miss on ⋯ never hides the pane.
    const OTHER: [Control; 3] = [
        control(I::Hide, "Hide", C::HidePane),
        control(I::PopOut, "Pop out", C::PopOut),
        control(I::Menu, "Pane menu", C::Menu),
    ];
    match (kind, detached) {
        (_, true) => &DETACHED,
        (PaneKind::Chart, false) => &CHART,
        (PaneKind::Dom | PaneKind::Tape | PaneKind::Journal, false) => &OTHER,
    }
}

/// A pane header's height (logical px): its padding, one line of its text and its bottom
/// border (charts use the larger `text_sm`). The hover controls are centered on it.
pub fn header_height(kind: PaneKind) -> f32 {
    match kind {
        PaneKind::Chart => 33.0,
        PaneKind::Dom | PaneKind::Tape | PaneKind::Journal => 27.0,
    }
}

/// Gap between header icons and the row's padding (logical px).
pub const CONTROL_GAP: f32 = 4.0;
pub const CONTROL_PAD: f32 = 4.0;

/// Logical px every pane header keeps free at its right end, where the hover controls draw:
/// header content never reaches it (`header_content` clips there), so the controls never
/// cover anything (the account chip, status text). Sized for the most icons any header shows.
pub fn controls_gutter(kind: PaneKind) -> f32 {
    let n = controls(kind, false).len().max(controls(kind, true).len()) as f32;
    n * crate::icons::HIT + (n - 1.0) * CONTROL_GAP + 2.0 * CONTROL_PAD
}

/// The timeframe chip's text in a chart header ("5m ▾"): one place for the renderer and the
/// header layout test.
pub fn timeframe_chip_text(tf: lines_core::Timeframe) -> String {
    format!("{tf} ▾")
}

/// A pane header's content row: left and right groups, clipped to the header's content box
/// (the header's right padding is the controls' gutter).
pub fn header_content() -> Div {
    div()
        .flex()
        .flex_row()
        .flex_1()
        .min_w_0()
        .overflow_hidden()
        .justify_between()
        .items_center()
        .gap_2()
}

/// The pane header's contract text: the active contract is always shown (decision 0013);
/// unknown is labelled, not hidden.
pub fn contract_text(d: &lines_feed::ChartData) -> String {
    match d.contract_label() {
        Some(c) => c.to_string(),
        None => format!("{} (contract unknown)", d.product.symbol),
    }
}

/// The link group marker for a pane header: a colour swatch plus the group's letter, so the
/// group never relies on colour alone. Nothing for unlinked panes.
pub fn link_chip(link: LinkGroup, theme: &Theme) -> Option<Div> {
    let g = link?;
    Some(
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .child(
                div()
                    .w(px(8.0))
                    .h(px(8.0))
                    .rounded_sm()
                    .bg(theme.link[g as usize % theme.link.len()]),
            )
            .child(
                div()
                    .text_color(theme.text_dim)
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(link_letter(link)),
            ),
    )
}

/// The body of a pane whose target the source can't serve: the symbol and the reason, greyed,
/// and (when `offer`) a one-click switch to the source's own symbol, also on Enter.
pub fn unavailable_body<V: Pane>(
    feed: &PaneFeed,
    theme: &Theme,
    offer: bool,
    cx: &mut Context<V>,
) -> Option<Div> {
    let u = feed.unavailable.as_ref()?;
    let button = feed.suggest.as_ref().filter(|_| offer).map(|s| {
        div()
            .id("show-source")
            .mt_2()
            .px_3()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(theme.accent)
            .text_size(px(12.0))
            .text_color(theme.text_main)
            .hover(|d| d.bg(theme.accent.opacity(0.16)))
            .child(format!("Show {} here  (Enter)", s.label))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|v: &mut V, _: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    v.show_suggested(cx);
                }),
            )
    });
    Some(
        div()
            .flex_1()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_1()
            .bg(theme.bg)
            .text_color(theme.text_dim)
            .child(
                div()
                    .text_size(px(15.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(u.symbol.clone()),
            )
            .child(
                div()
                    .text_size(px(12.0))
                    .child(format!("unavailable: {}", u.reason)),
            )
            .children(button)
            .child(
                div()
                    .text_size(px(11.0))
                    .child("Ctrl+K: choose another symbol"),
            ),
    )
}

/// Enter on an unavailable pane: the one-click switch. `true` if it handled the key.
pub fn unavailable_key<V: Pane>(v: &mut V, ev: &KeyDownEvent, cx: &mut Context<V>) -> bool {
    let k = &ev.keystroke;
    let m = k.modifiers;
    let offered =
        v.feed().unavailable.is_some() && v.feed().suggest.is_some() && v.common().offer_switch;
    if offered && k.key == "enter" && !(m.control || m.alt || m.platform || m.shift) {
        v.show_suggested(cx);
        cx.stop_propagation();
        return true;
    }
    false
}

/// The header note of a pane showing the replay's contract for this run.
pub fn adopted_note(feed: &PaneFeed) -> Option<String> {
    feed.adopted
        .as_ref()
        .map(|a| format!("replay: {} (workspace: {})", a.shown, a.workspace))
}

/// The header's left text for an unavailable pane.
pub fn unavailable_title(feed: &PaneFeed) -> Option<String> {
    feed.unavailable
        .as_ref()
        .map(|u| format!("{}  ·  unavailable", u.symbol))
}

/// The date contract choices are computed for: the replay's market date when the clock is
/// frozen (screenshots, reproducible), else today (UTC).
pub fn today(frozen: bool, store: &ChartStore) -> CivilDate {
    let market = frozen
        .then(|| store.try_read().and_then(|d| d.market_time))
        .flatten();
    CivilDate::from_utc(market.unwrap_or_else(lines_feed::now_utc))
}

/// The stale marker for a pane header (docs/design.md §7): shown while the pane's live feed
/// has delivered nothing for longer than its cadence, so a frozen pane never looks live.
pub fn stale_badge(store: &ChartStore, t: &Theme) -> Option<gpui::AnyElement> {
    use gpui::IntoElement;
    let quiet = store.stale_for()?;
    Some(
        div()
            .px_1()
            .bg(t.last)
            .text_color(t.on_last)
            .font_weight(FontWeight::BOLD)
            .child(format!("STALE · no data {} s", quiet.as_secs()))
            .into_any_element(),
    )
}

/// Dim a stale pane's content (paint after the content).
pub fn paint_stale_veil(
    store: &ChartStore,
    bounds: gpui::Bounds<gpui::Pixels>,
    t: &Theme,
    window: &mut gpui::Window,
) {
    if store.stale_for().is_some() {
        window.paint_quad(gpui::fill(bounds, t.bg.opacity(0.55)));
    }
}

/// Logical px the chart header's left group takes through its chips, as the renderer lays it
/// out: row padding, link chip, contract text, timeframe chip, account chip (and its PAPER
/// badge), at `char_px` a character. The quotes and status that follow may be clipped at the
/// controls' gutter; the chips must fit before it (`CHART_HEADER_MIN_W`).
#[cfg(test)]
fn chart_header_chips_px(
    linked: bool,
    contract: &str,
    timeframe: &str,
    account: Option<(&str, bool)>,
    char_px: f32,
) -> f32 {
    let tw = |s: &str| s.chars().count() as f32 * char_px;
    let mut w = header_base_px(linked, contract, &tw) + header_chip_px(timeframe, &tw);
    if let Some(a) = account {
        w += header_account_px(a, &tw);
    }
    w
}

/// Width of a header text in logical px: measured from the shaped text by the chart, a
/// fixed glyph width in tests.
pub type TextPx<'a> = &'a dyn Fn(&str) -> f32;

const HEADER_GAP: f32 = 8.0; // gap_2 between the left group's items
const HEADER_CHIP_PAD: f32 = 10.0; // px_1 each side + a 1 px border each side
/// The PAPER/LIVE badges are bold: their text is measured regular and widened by this.
const BOLD: f32 = 1.12;

/// Row padding, link chip and contract: the part of the left group that never gives way.
fn header_base_px(linked: bool, contract: &str, tw: TextPx) -> f32 {
    let mut w = 12.0 + tw(contract); // px_3
    if linked {
        w += 8.0 + 4.0 + tw("A") + HEADER_GAP; // swatch, gap_1, letter
    }
    w
}

/// A bordered chip ("5m ▾") after a gap.
fn header_chip_px(text: &str, tw: TextPx) -> f32 {
    HEADER_GAP + tw(text) + HEADER_CHIP_PAD
}

/// The badge alone, after a gap: "PAPER", or the bordered "LIVE" (the wider of the two).
fn header_badge_px(tw: TextPx) -> f32 {
    HEADER_GAP + tw("PAPER").max(tw("LIVE")) * BOLD + HEADER_CHIP_PAD
}

/// The account chip (bold), with its PAPER badge.
fn header_account_px((name, paper): (&str, bool), tw: TextPx) -> f32 {
    let mut w = HEADER_GAP + tw(name) * BOLD + HEADER_CHIP_PAD;
    if paper {
        w += 4.0 + tw("PAPER") * BOLD + 8.0; // gap_1, the badge (px_1)
    }
    w
}

/// What the chart header shows whole beside the hover controls' gutter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeaderFit {
    /// The account chip; without it the header shows only the PAPER or LIVE badge.
    pub account: bool,
    pub timeframe: bool,
    /// The status word whole; else it truncates with "…" (only when even the badge and the
    /// contract leave no room for it).
    pub status: bool,
    /// The link chip; it gives way (the pane menu still shows the group) before the status
    /// word truncates.
    pub link: bool,
}

/// What the chart header shows in `room` logical px (the header's width less the controls'
/// gutter; 0 or less: not laid out yet, everything shows). The order of importance: link,
/// contract and the PAPER/LIVE badge never give way; then the feed's status word (truncated,
/// never dropped); then the account chip (dropped to its badge), then the timeframe chip. An
/// open timeframe list keeps its chip (the list hangs under it), taking the room from the
/// account chip.
#[allow(clippy::too_many_arguments)]
pub fn chart_header_fit(
    room: f32,
    linked: bool,
    contract: &str,
    status: Option<&str>,
    timeframe: &str,
    account: Option<(&str, bool)>,
    menu_open: bool,
    tw: TextPx,
) -> HeaderFit {
    if room <= 0.0 {
        return HeaderFit {
            account: true,
            timeframe: true,
            status: true,
            link: true,
        };
    }
    let badge = account.map_or(0.0, |_| header_badge_px(tw));
    let mut w =
        header_base_px(linked, contract, tw) + badge + status.map_or(0.0, |s| HEADER_GAP + tw(s));
    // Too narrow for the badge and the status word: the link chip goes first.
    let link = !linked || w <= room;
    if !link {
        w -= header_base_px(true, contract, tw) - header_base_px(false, contract, tw);
    }
    let status_fits = w <= room;
    let tf = header_chip_px(timeframe, tw);
    let reserve = if menu_open { tf } else { 0.0 };
    let full = account.map_or(0.0, |a| header_account_px(a, tw));
    let fits_account = account.is_none() || w - badge + full + reserve <= room;
    if fits_account {
        w += full - badge;
    }
    HeaderFit {
        account: fits_account,
        timeframe: menu_open || w + tf <= room,
        status: status_fits,
        link,
    }
}

/// The feed's state as one short word for the chart header, for every state but live. It
/// never gives way: an ended or failed feed must not read as live at any width.
pub fn status_word(s: &lines_feed::store::FeedStatus) -> Option<&'static str> {
    use lines_feed::store::FeedStatus as S;
    match s {
        S::Live => None,
        S::Connecting | S::LoggingOn => Some("connecting"),
        S::Reconnecting { .. } => Some("reconnecting"),
        S::ReplayFinished => Some("ended"),
        S::Failed(_) => Some("failed"),
        S::Stopped => Some("stopped"),
    }
}

/// Glyph width the header model tests assume (wide: the UI font's lowercase is narrower).
#[cfg(test)]
const HEADER_CHAR_PX: f32 = 7.0;

#[cfg(test)]
mod tests {
    use super::*;

    fn tw7(s: &str) -> f32 {
        s.chars().count() as f32 * HEADER_CHAR_PX
    }

    fn fit(room: f32, account: Option<(&str, bool)>, menu_open: bool) -> HeaderFit {
        chart_header_fit(
            room,
            true,
            "ESZ26",
            Some("ended"),
            "5m ▾",
            account,
            menu_open,
            &tw7,
        )
    }

    /// Width at which exactly the given items fit (link, contract, badge, "ended", the rest).
    fn need(account: Option<(&str, bool)>, chip: bool, tf: bool) -> f32 {
        let mut w = header_base_px(true, "ESZ26", &tw7) + HEADER_GAP + tw7("ended");
        if let Some(a) = account {
            w += if chip {
                header_account_px(a, &tw7)
            } else {
                header_badge_px(&tw7)
            };
        }
        if tf {
            w += header_chip_px("5m ▾", &tw7);
        }
        w
    }

    const ALL: HeaderFit = HeaderFit {
        account: true,
        timeframe: true,
        status: true,
        link: true,
    };

    #[test]
    fn header_gives_way_in_order_timeframe_account_chip_status() {
        for paper in [true, false] {
            let a = Some(("Eval 50K ▾", paper));
            let wide = need(a, true, true);
            assert_eq!(fit(wide, a, false), ALL);
            // Just short of everything: the timeframe chip goes first.
            let f = fit(wide - 1.0, a, false);
            assert!(f.account && f.status && !f.timeframe, "paper {paper}");
            // Short of the account chip: it drops to its badge (PAPER or LIVE), and the
            // timeframe chip comes back only if it fits beside the badge.
            let chip = need(a, true, false);
            let f = fit(chip - 1.0, a, false);
            assert!(!f.account && f.status, "paper {paper}");
            assert_eq!(f.timeframe, need(a, false, true) <= chip - 1.0);
            // Short of the badge and the status word: the link chip goes, then (short of
            // even that) the status word truncates, last.
            let bare = need(a, false, false);
            let f = fit(bare - 1.0, a, false);
            assert!(
                !f.account && !f.timeframe && !f.link && f.status,
                "paper {paper}"
            );
            let link = header_base_px(true, "ESZ26", &tw7) - header_base_px(false, "ESZ26", &tw7);
            let f = fit(bare - link - 1.0, a, false);
            assert!(!f.link && !f.status, "paper {paper}");
        }
    }

    #[test]
    fn header_without_an_account_or_width_shows_what_fits() {
        assert_eq!(fit(0.0, Some(("A ▾", false)), false), ALL);
        let tf = need(None, false, true);
        assert_eq!(fit(tf, None, false), ALL);
        assert!(!fit(tf - 1.0, None, false).timeframe);
    }

    /// The timeframe list hangs under its chip: while open, the chip stays (a list opened from
    /// the palette is never invisible) and the account chip gives way to it instead.
    #[test]
    fn open_timeframe_list_keeps_its_chip() {
        let a = Some(("Eval 50K ▾", true));
        let f = fit(need(a, true, true) - 1.0, a, true);
        assert!(!f.account && f.timeframe);
        assert!(fit(10.0, None, true).timeframe);
    }

    #[test]
    fn every_state_but_live_has_a_status_word() {
        use lines_feed::store::FeedStatus as S;
        assert_eq!(status_word(&S::Live), None);
        for (s, w) in [
            (S::Connecting, "connecting"),
            (S::LoggingOn, "connecting"),
            (
                S::Reconnecting {
                    attempt: 1,
                    reason: "x".into(),
                },
                "reconnecting",
            ),
            (S::ReplayFinished, "ended"),
            (S::Failed("x".into()), "failed"),
            (S::Stopped, "stopped"),
        ] {
            assert_eq!(status_word(&s), Some(w));
        }
    }

    /// The owner's report: "chart only" and "pop out" drew over the chart header's account
    /// chip. The controls draw in a fixed gutter at the header's right end, and the gutter
    /// must hold their labels even at a wide glyph width (11 px text: 7 px a character,
    /// wider than the UI font's lowercase average), plus each button's padding, the gaps and
    /// the row's padding as `workspace::header_controls` lays them out.
    #[test]
    fn hover_controls_fit_the_gutter_headers_keep_free() {
        for (kind, detached) in [
            PaneKind::Chart,
            PaneKind::Dom,
            PaneKind::Tape,
            PaneKind::Journal,
        ]
        .into_iter()
        .flat_map(|k| [(k, false), (k, true)])
        {
            let c = controls(kind, detached);
            let n = c.len() as f32;
            let need = n * crate::icons::HIT + CONTROL_GAP * (n - 1.0) + 2.0 * CONTROL_PAD;
            // Icons, not words: about 3 × 20 px plus gaps.
            assert!(controls_gutter(kind) <= 80.0, "{kind:?}");
            // Every control is named (tooltip, accessible label).
            assert!(c.iter().all(|c| !c.name.is_empty()));
            assert!(
                controls_gutter(kind) >= need,
                "{kind:?} (detached: {detached}): gutter {} < {need}",
                controls_gutter(kind)
            );
        }
    }

    /// The narrowest chart header (logical px, gutter included) that still shows its chips whole:
    /// a chart pane narrower than this clips them at the gutter (never draws under it).
    const CHART_HEADER_MIN_W: f32 = 700.0;

    /// The chart header's chips (timeframe, account) sit with the contract on the left and
    /// stay clear of the hover controls' gutter: for every catalog product, every timeframe
    /// and the default accounts, the group through the account chip fits before the gutter in
    /// a header `CHART_HEADER_MIN_W` wide (at the same wide glyph width as above).
    #[test]
    fn chart_header_chips_fit_before_the_gutter() {
        const CHAR: f32 = 7.0;
        let room = CHART_HEADER_MIN_W - controls_gutter(PaneKind::Chart);
        let mut widest: f32 = 0.0;
        for p in lines_feed::catalog::all() {
            // Contract label ("MNQZ26"; a spot product's is its symbol). The description
            // follows the chips and shrinks, so it isn't part of the group modelled here.
            let month = if p.is_spot() { "" } else { "Z26" };
            let contract = format!("{}{month}", p.symbol);
            for tf in lines_core::Timeframe::ALL {
                for account in ["Paper", "Paper 50K eval"] {
                    let w = chart_header_chips_px(
                        true,
                        &contract,
                        &timeframe_chip_text(tf),
                        Some((&format!("{account} ▾"), true)),
                        CHAR,
                    );
                    assert!(
                        w <= room,
                        "{contract} {tf} {account}: {w} px > {room} px before the gutter"
                    );
                    widest = widest.max(w);
                }
            }
        }
        // The timeframe chip is counted: without it the group is narrower.
        let without = chart_header_chips_px(true, "ESZ26", "", None, CHAR);
        let with = chart_header_chips_px(
            true,
            "ESZ26",
            &timeframe_chip_text(lines_core::Timeframe::Minutes(15)),
            None,
            CHAR,
        );
        assert_eq!(with - without, 5.0 * CHAR);
        assert!(widest > 0.0);
    }
}
