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
    AppContext, Context, Div, EventEmitter, FocusHandle, FontWeight, InteractiveElement,
    KeyDownEvent, MouseButton, MouseDownEvent, ParentElement, Styled, div, px,
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
    /// In its own window (popped out): the header offers dock and re-attach.
    pub detached: bool,
}

impl Default for PaneCommon {
    fn default() -> Self {
        PaneCommon {
            visible: true,
            link: None,
            paints: 0,
            offer_switch: true,
            detached: false,
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

/// The timeframe chip's text in a chart header ("5m ▾"): one place for the renderer and the
/// header layout test.
pub fn timeframe_chip_text(tf: lines_core::Timeframe) -> String {
    format!("{tf} ▾")
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

// ---- pane header: the controls slot and space by priority ---------------------------------
//
// Every pane header is one row: its content, then a **controls slot** of fixed width at the
// right edge. The slot is always laid out (empty-looking until the header is hovered), so the
// pointer arriving never shifts or covers anything; the icons in it act on the pane through
// [`PaneControl`] events. The content gets the rest of the width by priority
// ([`layout_header`]): fixed items (chips, badges: the account chip and its PAPER badge) are
// never shrunk; then the symbol, the quotes, the status text and the rest get what is left in
// that order, elided with "…" when short.

/// One icon button in the controls slot (logical px).
pub const ICON_W: f32 = 22.0;
pub const ICON_H: f32 = 18.0;
const ICON_GAP: f32 = 2.0;
/// The slot: three icons and their gaps, plus its right padding.
pub const SLOT_W: f32 = 3.0 * ICON_W + 2.0 * ICON_GAP + 6.0;
// Three icons and their gaps fit the slot.
const _: () = assert!(3.0 * ICON_W + 2.0 * ICON_GAP <= SLOT_W);
/// Space between header items.
pub const HEADER_GAP: f32 = 8.0;
/// Narrowest width elided text is shown at (narrower: hidden).
pub const MIN_ELIDED: f32 = 28.0;

/// A header item's priority for space, highest first.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HeaderPrio {
    /// Never shrunk, placed first: the account chip and its PAPER badge, state badges
    /// (stale, a locked book). Hidden only if it can't fit at all, never drawn in part.
    Fixed,
    /// The contract: elided with "…".
    Symbol,
    /// Last / bid / ask: whole or hidden (an elided price would read as another price).
    Quotes,
    /// Small controls (link group, timeframe): whole or hidden.
    Chip,
    /// Replay / source / status text: elided.
    Status,
    /// Everything else (the product's description): elided first.
    Rest,
}

impl HeaderPrio {
    /// Whether items of this priority may be shown shortened with "…".
    pub fn elides(self) -> bool {
        matches!(self, Self::Symbol | Self::Status | Self::Rest)
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct HeaderItem {
    /// Its natural width (logical px).
    pub desired: f32,
    pub prio: HeaderPrio,
    /// Drawn at the right end of the content (just left of the slot).
    pub right: bool,
}

/// Where an item (or the slot) goes, header-local logical px; `w == 0`: not shown.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct Placed {
    pub x: f32,
    pub w: f32,
}

#[cfg(test)]
impl Placed {
    pub fn right(&self) -> f32 {
        self.x + self.w
    }
}

/// Lay out a header `width` wide with `pad` padding at its left: the controls slot at the
/// right edge, then `items` by priority into `out` (same order as `items`). Returns the slot.
/// Pure and allocation-free.
pub fn layout_header(width: f32, pad: f32, items: &[HeaderItem], out: &mut [Placed]) -> Placed {
    let slot = Placed {
        x: (width - SLOT_W).max(0.0),
        w: SLOT_W.min(width.max(0.0)),
    };
    let n = items.len().min(out.len());
    let mut left = (slot.x - pad - HEADER_GAP).max(0.0);
    for w in out[..n].iter_mut() {
        *w = Placed::default();
    }
    let mut starved = false;
    for prio in [
        HeaderPrio::Fixed,
        HeaderPrio::Symbol,
        HeaderPrio::Quotes,
        HeaderPrio::Chip,
        HeaderPrio::Status,
        HeaderPrio::Rest,
    ] {
        // Once a priority isn't shown whole, lower ones get nothing: leftover space never
        // shows lower-priority text while something more important is hidden or elided.
        if starved {
            break;
        }
        for i in 0..n {
            if items[i].prio != prio {
                continue;
            }
            let want = items[i].desired.max(0.0);
            let room = left.max(0.0);
            let w = if want <= room {
                want
            } else if prio.elides() && room >= want.min(MIN_ELIDED) {
                room
            } else {
                0.0
            };
            out[i].w = w;
            starved |= w < want;
            if w > 0.0 {
                left -= w + HEADER_GAP;
            }
        }
    }
    // Positions: left items from the padding on, right items back from the slot.
    let mut x = pad;
    for i in 0..n {
        if !items[i].right && out[i].w > 0.0 {
            out[i].x = x;
            x += out[i].w + HEADER_GAP;
        }
    }
    let mut r = slot.x - HEADER_GAP;
    for i in (0..n).rev() {
        if items[i].right && out[i].w > 0.0 {
            r -= out[i].w;
            out[i].x = r;
            r -= HEADER_GAP;
        }
    }
    slot
}

/// Logical px `text` takes in the UI font at `size` (render time; allocates).
pub fn measure(window: &mut gpui::Window, text: &str, size: f32, bold: bool) -> f32 {
    let mut font = window.text_style().font();
    if bold {
        font.weight = FontWeight::BOLD;
    }
    let run = gpui::TextRun {
        len: text.len(),
        font,
        color: gpui::black(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let line = window.text_system().shape_line(
        gpui::SharedString::from(text.to_owned()),
        px(size),
        &[run],
        None,
    );
    f32::from(line.width)
}

/// What a header part shows.
pub enum PartBody {
    /// Text, elided with "…" to its allocated width.
    Text {
        text: gpui::SharedString,
        color: gpui::Hsla,
    },
    /// An element of the given natural width (chips, badges, quotes groups).
    El(gpui::AnyElement),
}

pub struct HeaderPart {
    pub item: HeaderItem,
    pub body: PartBody,
}

/// A text part: `desired` measured by the caller.
pub fn text_part(
    text: impl Into<gpui::SharedString>,
    color: gpui::Hsla,
    desired: f32,
    prio: HeaderPrio,
    right: bool,
) -> HeaderPart {
    HeaderPart {
        item: HeaderItem {
            desired,
            prio,
            right,
        },
        body: PartBody::Text {
            text: text.into(),
            color,
        },
    }
}

/// An element part of natural width `desired`.
pub fn el_part(el: gpui::AnyElement, desired: f32, prio: HeaderPrio, right: bool) -> HeaderPart {
    HeaderPart {
        item: HeaderItem {
            desired,
            prio,
            right,
        },
        body: PartBody::El(el),
    }
}

/// The group name the header's hover reveals its controls by.
pub const HEADER_GROUP: &str = "pane-header";

/// A header row `width` wide (the pane's, 0 before the first layout: everything at its
/// natural width), with `pad` left padding: `parts` laid out by [`layout_header`], then the
/// controls `slot`. `root` carries the header's look (background, border, text size).
pub fn header_row(
    root: Div,
    width: f32,
    pad: f32,
    parts: Vec<HeaderPart>,
    slot: gpui::AnyElement,
) -> Div {
    use gpui::{InteractiveElement, IntoElement};
    let items: Vec<HeaderItem> = parts.iter().map(|p| p.item).collect();
    let mut placed = vec![Placed::default(); items.len()];
    let unlaid = width <= 0.0;
    if !unlaid {
        layout_header(width, pad, &items, &mut placed);
    }
    let mut lefts = Vec::new();
    let mut rights = Vec::new();
    for (p, at) in parts.into_iter().zip(placed) {
        let w = if unlaid { p.item.desired } else { at.w };
        if w <= 0.0 {
            continue;
        }
        let el = match p.body {
            PartBody::Text { text, color } => div()
                .w(px(w))
                .flex_shrink_0()
                .overflow_hidden()
                .truncate()
                .text_color(color)
                .child(text)
                .into_any_element(),
            PartBody::El(el) => div().flex_shrink_0().child(el).into_any_element(),
        };
        if p.item.right {
            rights.push(el);
        } else {
            lefts.push(el);
        }
    }
    root.group(HEADER_GROUP)
        .flex()
        .flex_row()
        .items_center()
        .whitespace_nowrap()
        .pl(px(pad))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(HEADER_GAP))
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .children(lefts)
                .child(div().flex_1())
                .children(rights),
        )
        .child(slot)
}

/// A pane asks its window to run a command from its header's controls (pop out, hide, the
/// pane menu, …): panes never act on the workspace themselves.
#[derive(Clone, Copy, Debug)]
pub struct PaneControl(pub crate::commands::Command);

/// Screenshot fixture (`--hover-pane KIND`): that kind's header controls are drawn as if
/// hovered. 0: none; else `PaneKind as u8 + 1`.
pub static FORCE_CONTROLS: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

fn forced(kind: PaneKind) -> bool {
    FORCE_CONTROLS.load(std::sync::atomic::Ordering::Relaxed) == kind as u8 + 1
}

/// The icons a header's controls slot offers, in order: in a main window, chart only (charts)
/// or hide (DOM, T&S), pop out, and the pane menu; in a pane's own window, dock beside the
/// chart, re-attach, and the menu.
pub fn slot_commands(
    kind: PaneKind,
    detached: bool,
) -> [(Icon, crate::commands::Command, &'static str); 3] {
    use crate::commands::Command as C;
    if detached {
        return [
            (Icon::Dock, C::DockBesideChart, "Dock beside the chart"),
            (
                Icon::Reattach,
                C::Reattach,
                "Re-attach to its previous spot",
            ),
            (Icon::Menu, C::Menu, "Pane menu"),
        ];
    }
    let first = match kind {
        PaneKind::Chart => (
            Icon::ChartOnly,
            C::ChartOnly,
            "Chart only (hide/restore linked DOM and T&S)",
        ),
        _ => (Icon::Hide, C::HidePane, "Hide pane"),
    };
    [
        first,
        (Icon::PopOut, C::PopOut, "Pop out into its own window"),
        (Icon::Menu, C::Menu, "Pane menu"),
    ]
}

/// The slot's icons, drawn from shapes (no font glyphs: the same on every machine).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Icon {
    Hide,
    ChartOnly,
    PopOut,
    Dock,
    Reattach,
    Menu,
}

fn icon_shape(icon: Icon, c: gpui::Hsla) -> Div {
    let frame = || div().w(px(11.0)).h(px(9.0)).border_1().border_color(c);
    match icon {
        Icon::Hide => div().w(px(10.0)).h(px(2.0)).bg(c),
        Icon::Menu => div()
            .flex()
            .flex_row()
            .gap(px(2.0))
            .children((0..3).map(|_| div().w(px(3.0)).h(px(3.0)).rounded_full().bg(c))),
        Icon::ChartOnly => frame().border_t_2(),
        Icon::PopOut => div()
            .relative()
            .w(px(13.0))
            .h(px(11.0))
            .child(frame().absolute().left_0().bottom_0())
            .child(
                div()
                    .absolute()
                    .right_0()
                    .top_0()
                    .w(px(5.0))
                    .h(px(5.0))
                    .bg(c),
            ),
        Icon::Reattach => div()
            .relative()
            .w(px(13.0))
            .h(px(11.0))
            .child(frame().absolute().right_0().top_0())
            .child(
                div()
                    .absolute()
                    .left_0()
                    .bottom_0()
                    .w(px(5.0))
                    .h(px(5.0))
                    .bg(c),
            ),
        Icon::Dock => div()
            .flex()
            .flex_row()
            .gap(px(1.0))
            .child(div().w(px(6.0)).h(px(9.0)).border_1().border_color(c))
            .child(div().w(px(6.0)).h(px(9.0)).bg(c)),
    }
}

/// A plain tooltip (the icon's words).
struct Tip {
    text: &'static str,
    theme: Theme,
}

impl gpui::Render for Tip {
    fn render(&mut self, _: &mut gpui::Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
        div()
            .px_2()
            .py_1()
            .bg(self.theme.panel)
            .border_1()
            .border_color(self.theme.axis_line)
            .rounded_sm()
            .text_size(px(12.0))
            .text_color(self.theme.text_main)
            .child(self.text)
    }
}

/// The header's controls slot: always laid out at its fixed width; its icons appear while the
/// header is hovered (or the `--hover-pane` fixture asks) and send [`PaneControl`]s.
pub fn controls_slot<V: EventEmitter<PaneControl> + 'static>(
    kind: PaneKind,
    detached: bool,
    t: &Theme,
    cx: &mut Context<V>,
) -> gpui::AnyElement {
    use gpui::{InteractiveElement, IntoElement, MouseButton, StatefulInteractiveElement};
    let theme = *t;
    let icons =
        slot_commands(kind, detached)
            .into_iter()
            .enumerate()
            .map(|(i, (icon, cmd, tip))| {
                let hover_bg = theme.axis_line;
                div()
                    .id(("pane-control", i))
                    .w(px(ICON_W))
                    .h(px(ICON_H))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_sm()
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover_bg))
                    .child(icon_shape(icon, theme.text_main))
                    .tooltip(move |_, cx| cx.new(|_| Tip { text: tip, theme }).into())
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |_: &mut V, _, _, cx| {
                            cx.stop_propagation();
                            cx.emit(PaneControl(cmd));
                        }),
                    )
            });
    let mut row = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_end()
        .gap(px(ICON_GAP))
        .children(icons);
    if !forced(kind) {
        row = row.invisible().group_hover(HEADER_GROUP, |s| s.visible());
    }
    div()
        .w(px(SLOT_W))
        .flex_shrink_0()
        .flex()
        .flex_row()
        .justify_end()
        .pr(px(6.0))
        .child(row)
        .into_any_element()
}

/// The link-group chip as a header part (whole or hidden).
pub fn link_part(
    link: LinkGroup,
    t: &Theme,
    window: &mut gpui::Window,
    size: f32,
) -> Option<HeaderPart> {
    use gpui::IntoElement;
    let chip = link_chip(link, t)?;
    let w = 8.0 + 4.0 + measure(window, link_letter(link), size, true);
    Some(el_part(chip.into_any_element(), w, HeaderPrio::Chip, false))
}

/// The stale badge as a fixed header part (state: never shrunk).
pub fn stale_part(
    store: &ChartStore,
    t: &Theme,
    window: &mut gpui::Window,
    size: f32,
) -> Option<HeaderPart> {
    let quiet = store.stale_for()?;
    let text = format!("STALE · no data {} s", quiet.as_secs());
    let w = measure(window, &text, size, true) + 8.0;
    Some(el_part(stale_badge(store, t)?, w, HeaderPrio::Fixed, false))
}

#[cfg(test)]
mod header_tests {
    use super::*;

    /// A wide glyph width (14 px text: 8 px a character, wider than the UI font's average).
    const CHAR: f32 = 8.0;
    const PAD: f32 = 12.0;
    /// The account chip's index in [`chart_items`].
    const ACCOUNT: usize = 3;

    fn text(s: &str) -> f32 {
        s.chars().count() as f32 * CHAR
    }

    fn item(desired: f32, prio: HeaderPrio, right: bool) -> HeaderItem {
        HeaderItem {
            desired,
            prio,
            right,
        }
    }

    /// The chart header as `ChartView::header` builds it, with `account` (name, paper).
    /// Returns the items and the PAPER badge's width (the account chip's right end).
    fn chart_items(
        contract: &str,
        description: &str,
        tf: lines_core::Timeframe,
        account: (&str, bool),
        stale: bool,
        status: &str,
    ) -> (Vec<HeaderItem>, f32) {
        let paper_w = text("PAPER") + 8.0;
        let account_w = crate::trade_bar::account_chip_width(
            text(&format!("{} ▾", account.0)),
            account.1.then(|| text("PAPER")),
        );
        let mut v = vec![
            item(8.0 + 4.0 + CHAR, HeaderPrio::Chip, false),
            item(text(contract), HeaderPrio::Symbol, false),
            item(
                text(&timeframe_chip_text(tf)) + 10.0,
                HeaderPrio::Chip,
                false,
            ),
            item(account_w, HeaderPrio::Fixed, false),
            item(text("last 21456.25"), HeaderPrio::Quotes, false),
            item(text("bid 21456.00"), HeaderPrio::Quotes, false),
            item(text("ask 21456.25"), HeaderPrio::Quotes, false),
        ];
        if stale {
            v.push(item(
                text("STALE · no data 12 s") + 8.0,
                HeaderPrio::Fixed,
                false,
            ));
        }
        v.push(item(text(description), HeaderPrio::Rest, false));
        v.push(item(text(status), HeaderPrio::Status, true));
        (v, if account.1 { paper_w } else { 0.0 })
    }

    /// Every shown item lies in the content box (from the padding to the slot, gap kept),
    /// shown items never overlap, nothing is drawn in the slot, and only text elides.
    fn assert_sound(width: f32, items: &[HeaderItem], out: &[Placed], slot: Placed) {
        assert_eq!(slot.w, SLOT_W.min(width));
        assert_eq!(slot.right(), width, "the slot sits at the right edge");
        let mut shown: Vec<Placed> = out.iter().copied().filter(|p| p.w > 0.0).collect();
        shown.sort_by(|a, b| a.x.total_cmp(&b.x));
        for p in &shown {
            assert!(p.x >= PAD - 1e-3, "{width}: {p:?} left of the padding");
            assert!(
                p.right() <= slot.x - HEADER_GAP + 1e-3,
                "{width}: {p:?} reaches the controls slot at {}",
                slot.x
            );
        }
        for w in shown.windows(2) {
            assert!(
                w[0].right() + HEADER_GAP <= w[1].x + 1e-3,
                "{width}: {w:?} overlap"
            );
        }
        for (it, p) in items.iter().zip(out) {
            if p.w > 0.0 && !it.prio.elides() {
                assert_eq!(p.w, it.desired, "{width}: {it:?} shortened");
            }
            if p.w > 0.0 && it.prio.elides() {
                assert!(p.w >= it.desired.min(MIN_ELIDED) - 1e-3);
            }
        }
    }

    /// The owner's report: "chart only" and "pop out" covered the "Paper 50K eval" chip and
    /// its PAPER badge. At several widths, for every product, timeframe and default account,
    /// the controls slot and the account chip never intersect, and the chip with its PAPER
    /// badge is whole whenever the header has room for it at all.
    #[test]
    fn controls_slot_never_meets_the_account_chip() {
        for p in lines_feed::catalog::all() {
            let month = if p.is_spot() { "" } else { "Z26" };
            let contract = format!("{}{month}", p.symbol);
            for tf in lines_core::Timeframe::ALL {
                for account in ["Paper", "Paper 50K eval"] {
                    for stale in [false, true] {
                        let (items, paper_w) = chart_items(
                            &contract,
                            p.description,
                            tf,
                            (account, true),
                            stale,
                            "replay: NQU25 (workspace: ES)  ·  lines  ·  live",
                        );
                        let need = PAD + items[ACCOUNT].desired + HEADER_GAP + SLOT_W;
                        for width in [
                            120.0, 200.0, 280.0, 320.0, 400.0, 520.0, 700.0, 1000.0, 1600.0,
                        ] {
                            let mut out = vec![Placed::default(); items.len()];
                            let slot = layout_header(width, PAD, &items, &mut out);
                            assert_sound(width, &items, &out, slot);
                            let a = out[ACCOUNT];
                            if width >= need {
                                assert_eq!(
                                    a.w, items[ACCOUNT].desired,
                                    "{contract} {account} at {width}"
                                );
                                // PAPER is the chip's right end: fully left of the slot.
                                let badge = Placed {
                                    x: a.right() - paper_w,
                                    w: paper_w,
                                };
                                assert!(badge.x >= a.x);
                                assert!(badge.right() <= slot.x - HEADER_GAP);
                            }
                            // Never in part, never under the slot.
                            assert!(a.w == 0.0 || a.right() <= slot.x);
                        }
                    }
                }
            }
        }
    }

    /// As the header narrows, lower priorities give way first: the description, then the
    /// status text, then the chips, then the quotes, then the symbol; the account chip stays.
    #[test]
    fn narrowing_drops_lower_priorities_first() {
        let (items, _) = chart_items(
            "MNQZ26",
            "Micro E-mini Nasdaq-100",
            lines_core::Timeframe::Minutes(5),
            ("Paper 50K eval", true),
            false,
            "lines  ·  live  ·  history complete",
        );
        // Shown share of a priority's natural width at `width`.
        let share = |width: f32, prio: HeaderPrio| {
            let mut out = vec![Placed::default(); items.len()];
            layout_header(width, PAD, &items, &mut out);
            let (got, want) = items
                .iter()
                .zip(&out)
                .filter(|(i, _)| i.prio == prio)
                .fold((0.0, 0.0), |(g, w), (i, p)| (g + p.w, w + i.desired));
            got / want
        };
        let order = [
            HeaderPrio::Symbol,
            HeaderPrio::Quotes,
            HeaderPrio::Chip,
            HeaderPrio::Status,
            HeaderPrio::Rest,
        ];
        let mut width = 1600.0;
        while width >= 120.0 {
            for pair in order.windows(2) {
                // A lower priority shows only once every higher one is whole.
                let (hi, lo) = (share(width, pair[0]), share(width, pair[1]));
                assert!(
                    lo <= 1e-3 || hi >= 1.0 - 1e-3,
                    "{width}: {pair:?} {hi} {lo}"
                );
            }
            width -= 10.0;
        }
        assert!(order.iter().all(|&p| share(1600.0, p) >= 1.0 - 1e-3));
        assert_eq!(share(1600.0, HeaderPrio::Fixed), 1.0);
        assert_eq!(share(300.0, HeaderPrio::Fixed), 1.0);
        assert_eq!(share(300.0, HeaderPrio::Rest), 0.0);
    }

    /// Elided text is never shown narrower than `MIN_ELIDED` (hidden instead); right items
    /// end at the slot; every header's slot holds three icons, each with a tooltip, the last
    /// one the pane menu.
    #[test]
    fn elision_and_the_slot() {
        let items = [item(400.0, HeaderPrio::Status, true)];
        let mut out = [Placed::default()];
        let slot = layout_header(PAD + 20.0 + HEADER_GAP + SLOT_W, PAD, &items, &mut out);
        assert_eq!(out[0].w, 0.0, "a 20 px room hides it");
        assert_eq!(slot.w, SLOT_W);
        let slot = layout_header(PAD + 100.0 + HEADER_GAP + SLOT_W, PAD, &items, &mut out);
        assert_eq!(out[0].w, 100.0);
        assert_eq!(out[0].right(), slot.x - HEADER_GAP);
        for kind in [PaneKind::Chart, PaneKind::Dom, PaneKind::Tape] {
            for detached in [false, true] {
                let cmds = slot_commands(kind, detached);
                assert!(matches!(cmds[2].1, crate::commands::Command::Menu));
                assert!(
                    cmds.iter().all(|c| !c.2.is_empty()),
                    "every icon has a tooltip"
                );
            }
        }
    }
}
