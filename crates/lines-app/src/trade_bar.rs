//! A pane's trading controls (roadmap 7; trade panel v2; docs/design.md §3): which account it
//! trades, the trading switch, the position, rule meters, quantity and bracket presets, order
//! buttons, Flatten and Cancel all. The DOM shows all of it in a fixed-height panel over the
//! ladder (its height depends only on its width, so the ladder never moves); a chart shows its
//! account.
//!
//! Trust rules this module keeps:
//! - The account is always named, with its badge as a word on an inverted chip (not only a
//!   colour: decision 0006): **PAPER**, **SIM** (a broker's simulation) or **LIVE** (warning
//!   colours). A broker account also shows its connection (connected / stale / disconnected).
//! - **Flatten** and **Cancel All** are always visible and always sent: neither is gated by
//!   arming, risk or prop rules (the core lets them through). The one exception: a
//!   **read-only** account (a broker in observe mode), where every command is refused before
//!   sending; there both read "off" and refuse with the reason, and the panel says why.
//! - The trading switch is explicit and shown as words ("Trading: Enabled / Disabled") and by
//!   shape (a filled knob vs a ring). It is sticky and saved with the pane; when orders would
//!   be denied anyway (market closed, a lockout) it says so in a badge, without flipping.
//!   Refusals appear inline, briefly, never as dialogs.
//! - Keys appear in tooltips, never in button labels.
//! - Everything here is also a workspace command with a hotkey and a palette entry
//!   ([`crate::commands::TradeCmd`]).

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, Context, FontWeight, InteractiveElement, IntoElement, MouseButton,
    ParentElement, Styled, Subscription, Window, div, prelude::*,
};
use lines_core::{ContractKey, Price, Product, Qty};
use lines_feed::store::ChartStore;
use lines_trading::{AccountId, Command as TradeCommand, Money, Source};
use serde::{Deserialize, Serialize};

use crate::commands::TradeCmd;
use crate::dom_trade::{
    self, ArmEvent, Arming, BracketPreset, next_bracket, open_ticks, parse_qty, step_qty,
};
use crate::theme::Theme;
use crate::trading::{TradeCache, Trading};

/// How long a local refusal ("disarmed", "no account") stays.
const LOCAL_FLASH: Duration = Duration::from_secs(3);

/// A pane's saved trading choices (in its `DomState`/`ChartState`), the trading switch
/// included (optional fields: no version bump).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TradeDoc {
    /// Trading enabled on this pane (sticky across restarts; written only when on).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bracket: Option<BracketPreset>,
    /// Order hotkeys on for this pane ("Keys on"; sticky, written only when on).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hotkeys: bool,
}

/// One pane's trading state.
pub struct PaneTrade {
    pub account: Option<AccountId>,
    pub bracket: Option<BracketPreset>,
    pub arming: Arming,
    /// Order hotkeys ("Keys on / off", `TradeCmd::ToggleHotkeys`): order keys need it on top
    /// of the trading switch. Off in new panes; only the trader turns it on or off.
    pub hotkeys: bool,
    /// Typing a custom quantity (digits so far).
    pub qty_edit: Option<String>,
    /// Wheel travel over a size control, not yet a step.
    pub qty_scroll: f32,
    /// A working order's size being typed: the order and the digits so far.
    pub resize_edit: Option<(lines_trading::OrderId, String)>,
    /// The wheel may resize the order under the pointer: only after the pointer moved, so a zoom
    /// or ladder scroll that drifts a mark or tag under a still pointer never sends anything.
    pub resize_armed: bool,
    /// `SIZE_EPOCH` last drawn.
    seen_size: u64,
    pub account_menu: bool,
    /// The Qty or Bracket field's list, while open (one popover with the account list).
    pub panel_menu: Option<PanelMenu>,
    /// A refusal made in the UI (not by the core), with when.
    local_flash: Option<(String, Instant)>,
    /// The local message's word: NOT SENT for an order action, CONFIRM for a press waiting for
    /// its second (a paper reset), NOTICE otherwise.
    local_flash_label: &'static str,
    /// Whether the Keys chip is in view as last drawn. Order keys send only while it is.
    keys_shown: std::cell::Cell<bool>,
    cache: Option<Arc<TradeCache>>,
    seen: u64,
    /// The P&L display setting's epoch last drawn.
    seen_display: u64,
    /// The window the focus/activation subscriptions belong to.
    watched: Option<(gpui::WindowId, Subscription, Subscription)>,
    /// Narrow panel: the rule meters are open over what follows.
    pub meters_open: bool,
    /// The inline bracket-preset editor, while open.
    pub editor: Option<crate::brackets::Editor>,
    /// The editor's last refusal (Enter with an invalid cell).
    pub editor_msg: Option<String>,
    /// Position bracket: a mode change on its way to the core.
    bracket_ask: Option<BracketAsk>,
    /// Asking the person to replace working exits with one pair.
    pub bracket_confirm: Option<BracketConfirm>,
    /// The core refused the saved position mode for this account and contract (why): the Pos
    /// cell shows it until a switch or a change of account or contract.
    pub bracket_refused: Option<String>,
    /// What this pane last told the core for its account and contract.
    bracket_told: Option<(AccountId, ContractKey, lines_trading::BracketMode)>,
    /// Switch the bracket mode as the trader would, once drawn (screenshot fixture).
    pub switch_pending: bool,
}

/// The confirmation to replace working exits: what it was shown for, exactly.
#[derive(Clone, Debug)]
pub struct BracketConfirm {
    n: usize,
    mode: lines_trading::BracketMode,
    account: AccountId,
    contract: ContractKey,
}

/// A bracket-mode command sent, waiting for the core's answer.
struct BracketAsk {
    id: lines_trading::CommandId,
    sent: Instant,
    account: AccountId,
    contract: ContractKey,
    symbol: &'static str,
    mode: lines_trading::BracketMode,
    /// The person switched it (saved on success; a refusal asking to replace exits becomes
    /// the confirmation). Otherwise the saved mode is being applied (at start, a new preset).
    by_person: bool,
}

impl TradeDoc {
    /// The choices for a new pane made from this one (a split): everything but the trading
    /// switch and the order hotkeys, which start off in every new pane.
    pub fn copied(self) -> TradeDoc {
        TradeDoc {
            enabled: false,
            hotkeys: false,
            ..self
        }
    }
}

impl PaneTrade {
    pub fn new(saved: Option<&TradeDoc>, cx: &App) -> PaneTrade {
        let trading = Trading::try_global(cx);
        let account = saved
            .and_then(|s| s.account.as_ref())
            .map(|a| AccountId::new(a))
            // A broker account not listed yet (its broker connects after start) is kept; it
            // shows as waiting, read-only, until it is.
            .filter(|a| trading.is_some_and(|t| t.keeps_saved_account(a)))
            .or_else(|| trading.and_then(Trading::default_account));
        PaneTrade {
            account,
            bracket: saved.and_then(|s| s.bracket),
            // Sticky: restored as saved (new panes: disabled).
            arming: Arming::new(saved.is_some_and(|s| s.enabled)),
            hotkeys: saved.is_some_and(|s| s.hotkeys),
            qty_edit: None,
            qty_scroll: 0.0,
            resize_edit: None,
            resize_armed: false,
            seen_size: SIZE_EPOCH.load(Ordering::Relaxed),
            account_menu: trading.is_some_and(|t| t.start_account_menu),
            panel_menu: trading.and_then(|t| t.start_panel_menu),
            local_flash: None,
            local_flash_label: "NOT SENT",
            keys_shown: std::cell::Cell::new(true),
            cache: trading.map(|t| t.cache.clone()),
            seen: u64::MAX,
            seen_display: crate::pnl::display_epoch(),
            watched: None,
            meters_open: false,
            editor: None,
            editor_msg: None,
            bracket_ask: None,
            bracket_confirm: None,
            bracket_refused: None,
            bracket_told: None,
            switch_pending: false,
        }
    }

    pub fn doc(&self) -> Option<TradeDoc> {
        let d = TradeDoc {
            enabled: self.arming.is_armed(),
            account: self.account.as_ref().map(|a| a.0.clone()),
            bracket: self.bracket,
            hotkeys: self.hotkeys,
        };
        (d != TradeDoc::default()).then_some(d)
    }

    /// Read the trading cache without waiting (`None`: not trading, or busy this tick).
    pub fn try_cache(&self) -> Option<std::sync::MutexGuard<'_, crate::trading::TradeState>> {
        self.cache.as_ref()?.try_read()
    }

    /// The trading state moved for this pane's account (or a local flash expired).
    pub fn changed(&mut self) -> bool {
        let mut changed = false;
        let e = crate::pnl::display_epoch();
        if e != self.seen_display {
            self.seen_display = e;
            changed = true;
        }
        let e = SIZE_EPOCH.load(Ordering::Relaxed);
        if e != self.seen_size {
            self.seen_size = e;
            changed = true;
        }
        if let (Some(c), Some(a)) = (&self.cache, &self.account) {
            let r = c.revision(a);
            if r != self.seen {
                self.seen = r;
                changed = true;
            }
        }
        if self
            .local_flash
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() > LOCAL_FLASH)
        {
            self.local_flash = None;
            changed = true;
        }
        changed
    }

    /// Another account or contract: a confirmation or answer for the old one never applies.
    fn bracket_forget(&mut self) {
        self.bracket_confirm = None;
        self.bracket_ask = None;
        self.bracket_refused = None;
    }

    pub fn flash(&mut self, msg: impl Into<String>) {
        self.local_flash = Some((msg.into(), Instant::now()));
        self.local_flash_label = "NOT SENT";
    }

    /// A paper reset's first press (`crate::paper_reset`): its words, for as long as the second
    /// press is waited for.
    fn confirm(&mut self, msg: impl Into<String>) {
        // Shown from now until the wait ends (a flash shows for `LOCAL_FLASH` from its time).
        let wait = crate::paper_reset::CONFIRM.saturating_sub(LOCAL_FLASH);
        self.local_flash = Some((msg.into(), Instant::now() + wait));
        self.local_flash_label = "CONFIRM";
    }

    /// The press a notice asked for has come: it no longer applies.
    fn clear_notice(&mut self) {
        if self.local_flash_label == "NOTICE" {
            self.local_flash = None;
        }
    }

    /// A brief message about the panel itself, with no order involved.
    fn notice(&mut self, msg: impl Into<String>) {
        self.local_flash = Some((msg.into(), Instant::now()));
        self.local_flash_label = "NOTICE";
    }

    /// Whether one of the panel's lists is open (the account's, Qty's or Bracket's).
    pub fn menu_open(&self) -> bool {
        self.account_menu || self.panel_menu.is_some()
    }

    pub fn local_flash(&self) -> Option<&str> {
        self.local_flash
            .as_ref()
            .filter(|(_, at)| at.elapsed() <= LOCAL_FLASH)
            .map(|(m, _)| m.as_str())
    }

    /// Apply a switch event (the trader's toggle, or an opt-in auto-disable from
    /// `ArmPrefs`, all off by default). Returns whether the state changed.
    pub fn arm_event(&mut self, ev: ArmEvent, cx: &App) -> bool {
        let prefs = crate::picker_view::settings(cx).trading_prefs;
        self.arming.apply(ev, prefs)
    }

    /// The pane switched feed: let paper fills follow the new feed (`follow`: it is a running
    /// feed, not an unavailable one). The switch stays as it is unless opted in.
    pub fn feed_changed(&mut self, store: &Arc<ChartStore>, follow: bool, cx: &App) {
        self.arm_event(ArmEvent::ContractChanged, cx);
        self.editor = None;
        // A size being typed was for the old symbol: it never lands on this one.
        self.qty_edit = None;
        self.resize_edit = None;
        self.bracket_forget();
        self.account_menu = false;
        self.panel_menu = None;
        if follow && let Some(t) = Trading::try_global(cx) {
            t.follow(store);
        }
    }

    /// Disable trading when the pane loses focus or its window goes inactive, only if the
    /// trader opted in (`ArmPrefs`; off by default). Called from render: subscribes once per
    /// window.
    pub fn watch_focus<V: TradePane>(
        view: &mut V,
        focus: &gpui::FocusHandle,
        window: &mut Window,
        cx: &mut Context<V>,
    ) {
        // The rule for a size being typed, on every path to a frame: it is drawn only while its
        // control shows and the pane holds the keys, so a drawn field is always a live one.
        if view.trade_ref().qty_edit.is_some() && (!view.shows_size() || !focus.is_focused(window))
        {
            view.trade().qty_edit = None;
        }
        // The same for an order's size, while its order is open.
        if let Some((id, _)) = view.trade_ref().resize_edit
            && (!view.keys_free()
                || !view.resize_drawn(id)
                || !focus.is_focused(window)
                || !resize_order_open(view.trade_ref(), id))
        {
            view.trade().resize_edit = None;
        }
        let id = window.window_handle().window_id();
        if view.trade_ref().watched.as_ref().is_some_and(|w| w.0 == id) {
            return;
        }
        let blur = cx.on_focus_out(focus, window, |v: &mut V, _, _, cx| {
            // A size being typed needs the pane's keys: a drawn field always has them.
            let open = v.trade().qty_edit.take().is_some();
            if v.trade().resize_edit.take().is_some() || open {
                cx.notify();
            }
            if v.trade().arm_event(ArmEvent::FocusLost, cx) {
                cx.notify();
            }
        });
        let activation = cx.observe_window_activation(window, |v: &mut V, window, cx| {
            if !window.is_window_active() && v.trade().arm_event(ArmEvent::FocusLost, cx) {
                cx.notify();
            }
        });
        view.trade().watched = Some((id, blur, activation));
    }
}

/// What a pane with trading controls provides.
pub trait TradePane: Sized + 'static {
    fn trade(&mut self) -> &mut PaneTrade;
    fn trade_ref(&self) -> &PaneTrade;
    fn trade_store(&self) -> &Arc<ChartStore>;
    /// Nothing else holds the pane's keys (a picker, a menu, a bracket editor).
    fn keys_free(&self) -> bool;
    /// Whether this order's own mark or tag is on screen, standing for it alone.
    fn resize_drawn(&self, order: lines_trading::OrderId) -> bool;
    /// Resize a working order from this pane, or say why not (`order_resize`).
    fn send_resize(
        &mut self,
        order: lines_trading::OrderId,
        step: crate::order_resize::SizeStep,
        one_leg: bool,
        cx: &mut Context<Self>,
    );
    /// The product the pane shows: its order size is that symbol's.
    fn trade_product(&self) -> &Product;
    /// The pane's focus: a size being typed needs its keys.
    fn trade_focus(&self) -> &gpui::FocusHandle;
    /// Whether the size control shows and could take keys now: not while something else holds
    /// them (a picker, a menu), and a DOM's panel shows other things in its place during the
    /// bracket editor or confirmation; a chart also needs its strip.
    fn shows_size(&self) -> bool;
    /// The pane's feed can't serve its target (nothing to trade).
    fn trade_unavailable(&self) -> bool;
}

/// Bumped when a shared order size changes: every pane redraws its size and buttons.
static SIZE_EPOCH: AtomicU64 = AtomicU64::new(0);

/// An order size changed outside `set_size` (Settings' default): panes redraw.
pub fn size_changed() {
    SIZE_EPOCH.fetch_add(1, Ordering::Relaxed);
}

/// The pane's order size (the size of its product, shared by every DOM and chart on it).
pub fn pane_size<V: TradePane>(v: &V, cx: &App) -> i64 {
    let s = crate::picker_view::settings(cx);
    dom_trade::order_size(
        &s.order_sizes,
        v.trade_product().symbol,
        s.new_panes.default_qty,
    )
}

/// Set the order size of the pane's product, for every pane on it (saved).
pub fn set_size<V: TradePane>(v: &mut V, q: i64, cx: &mut Context<V>) {
    // An unavailable pane's product may be a stand-in: never change a real symbol's size for it.
    if v.trade_unavailable() {
        v.trade().qty_edit = None;
        cx.notify();
        return;
    }
    let symbol = v.trade_product().symbol.to_owned();
    crate::picker_view::update_settings(cx, |s| {
        s.order_sizes.insert(symbol, q);
    });
    size_changed();
    v.trade().qty_edit = None;
    cx.notify();
}

/// Step the size through the presets (`up`: larger).
pub fn step_size<V: TradePane>(v: &mut V, up: bool, cx: &mut Context<V>) {
    let q = step_qty(pane_size(v, cx), up);
    set_size(v, q, cx);
}

/// Wheel travel over a size control: step it (see `dom_trade::scroll_qty`).
pub fn scroll_size<V: TradePane>(
    v: &mut V,
    ev: &gpui::ScrollEvent,
    w: &Window,
    cx: &mut Context<V>,
) {
    let dy = f32::from(ev.delta.pixel_delta(w.line_height()).y);
    if let Some(up) = dom_trade::scroll_qty(&mut v.trade().qty_scroll, dy) {
        step_size(v, up, cx);
    }
    cx.stop_propagation();
}

/// The largest order the pane's account allows, as the core last reported (`None`: no limit,
/// or the cache is busy: the core still refuses).
pub fn max_order_qty<V: TradePane>(v: &V, cx: &App) -> Option<i64> {
    let t = Trading::try_global(cx)?;
    let account = v.trade_ref().account.as_ref()?;
    let s = t.cache.try_read()?;
    s.account(account)?.limits.max_order_qty
}

/// Why a built order is refused for its size, in the size control's words.
pub fn size_refusal<V: TradePane>(v: &V, cmd: &lines_trading::Command, cx: &App) -> Option<String> {
    dom_trade::command_over_max(cmd, max_order_qty(v, cx))
}

/// The contract a pane trades: its store's (a product's front month, or the contract shown).
pub fn contract_of(store: &ChartStore) -> Option<ContractKey> {
    store.try_read().and_then(|d| d.contract)
}

/// Why dragging order `o` to `to` is refused before anything is sent, as a rejected drag
/// shows it ("5812.25: a sell stop must stay below the market"): a stop moved across the
/// inside market of the pane's store (`dom_trade::stop_move_refusal`), or the market data
/// busy. `None`: the move may go.
pub fn stop_move_check(store: &ChartStore, o: &lines_trading::Order, to: Price) -> Option<String> {
    let Some(d) = store.try_read() else {
        return Some("market data busy; try again".to_owned());
    };
    let why = dom_trade::stop_move_refusal(
        o,
        to,
        d.book.best_bid().map(|b| b.0).or(d.bid),
        d.book.best_ask().map(|a| a.0).or(d.ask),
        d.last_trade,
    )?;
    Some(format!(
        "{}: {why}",
        d.product.price.format::<32>(to).as_str()
    ))
}

/// Entries refused while the pane's feed is stale: its prices may be ones the market has left.
pub const FEED_STALE: &str = "feed stale: entries wait for fresh market data";

/// Why the pane can't trade, if it can't.
pub fn cant_trade<V: TradePane>(v: &V, cx: &App) -> Option<&'static str> {
    if Trading::try_global(cx).is_none() {
        Some("trading is not running")
    } else if v.trade_unavailable() {
        Some("this pane has no market data")
    } else if v.trade_ref().account.is_none() {
        Some("no account")
    } else if contract_of(v.trade_store()).is_none() {
        Some("contract unknown: can't trade it")
    } else {
        None
    }
}

/// Whether the pane's account is read-only (no copy: for render paths).
pub fn is_read_only<V: TradePane>(v: &V, cx: &App) -> bool {
    Trading::try_global(cx)
        .zip(v.trade_ref().account.as_ref())
        .is_some_and(|(t, a)| t.is_read_only(a))
}

/// Why the pane's account can't be traded through Lines (a broker in observe mode), if so.
pub fn read_only<V: TradePane>(v: &V, cx: &App) -> Option<String> {
    let t = Trading::try_global(cx)?;
    t.read_only_reason(v.trade_ref().account.as_ref()?)
}

/// What an order from this pane needs: account, contract, quantity, position, bracket, the
/// inside market (ladder clicks, header buttons and their keys all build from this).
pub fn entry_ctx<V: TradePane>(v: &V, cx: &App) -> Result<dom_trade::EntryCtx, &'static str> {
    if let Some(why) = cant_trade(v, cx) {
        return Err(why);
    }
    // A stale feed: no entry at a price the market may have left (Flatten, Cancel all, moves
    // and cancels don't come through here and stay available).
    if v.trade_store().stale_for().is_some() {
        return Err(FEED_STALE);
    }
    let t = Trading::try_global(cx).ok_or("trading is not running")?;
    let pt = v.trade_ref();
    let account = pt.account.clone().ok_or("no account")?;
    let contract = contract_of(v.trade_store()).ok_or("contract unknown")?;
    let position = t
        .cache
        .read()
        .position(&account, contract)
        .map_or(0, |p| p.qty);
    let (best_bid, best_ask, last, tick, symbol) = v
        .trade_store()
        .try_read()
        .map(|d| {
            (
                d.book.best_bid().map(|b| b.0).or(d.bid),
                d.book.best_ask().map(|a| a.0).or(d.ask),
                d.last_trade,
                d.product.price.tick,
                d.product.symbol,
            )
        })
        .ok_or("market data busy; try again")?;
    let settings = crate::picker_view::settings(cx);
    Ok(dom_trade::EntryCtx {
        account,
        contract,
        // The traded contract's own product, read with its market.
        qty: Qty(dom_trade::order_size(
            &settings.order_sizes,
            symbol,
            settings.new_panes.default_qty,
        )),
        position,
        bracket: pt.bracket,
        tick,
        best_bid,
        best_ask,
        last,
    })
}

/// Whether a chart runs this trading command when focused. Charts show no trading switch yet
/// (chart trading is roadmap 8), so the switch, order entry, BE and the preset editor act only
/// in a DOM; Flatten, Cancel all, account, quantity, bracket and unit commands work from a
/// chart.
pub fn chart_runs(cmd: TradeCmd) -> bool {
    cmd.quick().is_none()
        && !matches!(
            cmd,
            TradeCmd::ToggleArm
                | TradeCmd::ToggleHotkeys
                | TradeCmd::Enable
                | TradeCmd::Disable
                | TradeCmd::Breakeven
                | TradeCmd::EditBrackets
                | TradeCmd::BracketMode
        )
}

/// Run a trading command from the palette or a menu. `issued`: the input time.
pub fn run<V: TradePane>(v: &mut V, cmd: TradeCmd, issued: Instant, cx: &mut Context<V>) {
    run_with(v, cmd, issued, Source::Palette, false, cx);
}

/// Run a trading command from its key, in the key-down handler (never a later frame). Order
/// keys also need the pane's order hotkeys on (`TradeCmd::needs_hotkeys`).
pub fn run_key<V: TradePane>(v: &mut V, cmd: TradeCmd, issued: Instant, cx: &mut Context<V>) {
    run_with(v, cmd, issued, Source::Hotkey, true, cx);
}

/// Run a trading command; `source`: where it came from (header buttons: `Source::Dom`).
pub fn run_from<V: TradePane>(
    v: &mut V,
    cmd: TradeCmd,
    issued: Instant,
    source: Source,
    cx: &mut Context<V>,
) {
    run_with(v, cmd, issued, source, false, cx);
}

fn run_with<V: TradePane>(
    v: &mut V,
    cmd: TradeCmd,
    issued: Instant,
    source: Source,
    key: bool,
    cx: &mut Context<V>,
) {
    if let Some(q) = cmd.quick() {
        // Header order buttons and their keys: gated by arming exactly like ladder clicks;
        // keys by the order hotkeys too.
        let arming = v.trade_ref().arming;
        let ctx = || entry_ctx(v, cx).map_err(dom_trade::Refusal);
        let built = if key && cmd.needs_hotkeys() {
            let pt = v.trade_ref();
            dom_trade::key_order(q, pt.hotkeys, pt.keys_shown.get(), arming, ctx)
        } else {
            ctx().and_then(|ctx| dom_trade::quick(q, arming, &ctx))
        };
        match built {
            Ok(c) => match size_refusal(v, &c, cx) {
                Some(why) => v.trade().flash(why),
                None => {
                    if let Some(t) = Trading::try_global(cx) {
                        t.submit(source, issued, c);
                    }
                }
            },
            Err(dom_trade::Refusal(why)) => v.trade().flash(why),
        }
        cx.notify();
        return;
    }
    let why = cant_trade(v, cx);
    let locked = if matches!(cmd, TradeCmd::Flatten | TradeCmd::CancelAll) {
        read_only(v, cx)
    } else {
        None
    };
    match cmd {
        TradeCmd::Flatten | TradeCmd::CancelAll if locked.is_some() => {
            // The read-only exception: never sent (Lines can't act on this account).
            let what = if cmd == TradeCmd::Flatten {
                "Flatten"
            } else {
                "Cancel all"
            };
            v.trade()
                .flash(format!("{what} off: {}", locked.unwrap_or_default()));
        }
        TradeCmd::ToggleArm => {
            // Always flips: a reason orders would be denied shows on the switch instead.
            v.trade().arm_event(ArmEvent::Toggle, cx);
        }
        TradeCmd::ToggleHotkeys => {
            let t = v.trade();
            t.hotkeys = !t.hotkeys;
        }
        TradeCmd::Enable | TradeCmd::Disable => {
            let want = cmd == TradeCmd::Enable;
            if v.trade_ref().arming.is_armed() != want {
                v.trade().arm_event(ArmEvent::Toggle, cx);
            }
        }
        TradeCmd::Breakeven => {
            let built = breakeven_for(v, cx);
            match built {
                Ok(cmds) => {
                    if let Some(t) = Trading::try_global(cx) {
                        for c in cmds {
                            t.submit(source, issued, c);
                        }
                    }
                }
                Err(w) => v.trade().flash(w),
            }
        }
        TradeCmd::BracketUnits => {
            let account = v.trade_ref().account.as_ref().map(|a| a.0.clone());
            if let Some(why) = v
                .trade_ref()
                .editor
                .as_ref()
                .and_then(|e| e.units_blocked())
            {
                v.trade().editor_msg = Some(why);
            } else if let Some(a) = account {
                let mut next = crate::brackets::BracketUnits::Ticks;
                crate::picker_view::update_settings(cx, |s| {
                    let e = s.bracket_presets.entry(a).or_default();
                    e.units = e.units.toggled();
                    next = e.units;
                });
                if let Some(ed) = v.trade().editor.as_mut() {
                    ed.set_units(next);
                }
            } else {
                v.trade().flash("no account");
            }
        }
        TradeCmd::BracketMode => {
            bracket_switch(v, cx);
        }
        TradeCmd::ResetPaper | TradeCmd::ResetPaperJournal => {
            let journal = cmd == TradeCmd::ResetPaperJournal;
            reset_press(v, journal, source, cx);
        }
        TradeCmd::EditBrackets => {
            if v.trade_ref().editor.is_some() {
                v.trade().editor = None;
            } else {
                match (
                    v.trade_ref().account.clone(),
                    v.trade_store().try_read().map(|d| d.product.clone()),
                ) {
                    (Some(a), Some(product)) => {
                        let s = &crate::picker_view::settings(cx).bracket_presets;
                        let presets = crate::brackets::presets_for(s, &a.0, product.symbol);
                        let units = crate::brackets::units_for(s, &a.0);
                        let t = v.trade();
                        t.editor = Some(crate::brackets::Editor::new(&product, units, &presets));
                        t.editor_msg = None;
                    }
                    (None, _) => v.trade().flash("no account"),
                    (_, None) => v.trade().flash("market data busy; try again"),
                }
            }
        }
        TradeCmd::Flatten | TradeCmd::CancelAll => {
            // Never gated by arming, risk or rules: only by having something to act on.
            if let Some(w) = why {
                v.trade().flash(w);
            } else if let (Some(account), Some(contract), Some(t)) = (
                v.trade_ref().account.clone(),
                contract_of(v.trade_store()),
                Trading::try_global(cx),
            ) {
                let c = if cmd == TradeCmd::Flatten {
                    TradeCommand::Flatten {
                        account,
                        contract: Some(contract),
                    }
                } else {
                    TradeCommand::CancelAll {
                        account,
                        contract: Some(contract),
                    }
                };
                t.submit(source, issued, c);
            }
        }
        TradeCmd::NextAccount => {
            let next = Trading::try_global(cx)
                .and_then(|t| t.next_account(v.trade_ref().account.as_ref()));
            set_account(v, next, cx);
        }
        TradeCmd::QtyUp | TradeCmd::QtyDown => {
            step_size(v, cmd == TradeCmd::QtyUp, cx);
            say_size(v, cx);
        }
        TradeCmd::QtyPreset(i) => {
            if let Some(&q) = dom_trade::QTY_PRESETS.get(i as usize) {
                set_size(v, q, cx);
                say_size(v, cx);
            }
        }
        TradeCmd::TypeQty => open_size_field(v),
        TradeCmd::NextBracket => {
            let b = v.trade_ref().bracket;
            let presets = match (v.trade_ref().account.as_ref(), v.trade_store().try_read()) {
                (Some(a), Some(d)) => Some(crate::brackets::presets_for(
                    &crate::picker_view::settings(cx).bracket_presets,
                    &a.0,
                    d.product.symbol,
                )),
                _ => None,
            };
            let mut next = match &presets {
                Some(p) => crate::brackets::next_choice(p, b),
                None => next_bracket(b),
            };
            // One bracket for the position needs a preset: Off is skipped.
            if next.is_none() && position_on_now(v, cx) {
                next = presets
                    .as_ref()
                    .and_then(|p| crate::brackets::next_choice(p, None));
            }
            v.trade().bracket = next;
        }
        TradeCmd::BuyMarket
        | TradeCmd::SellMarket
        | TradeCmd::JoinBid
        | TradeCmd::JoinAsk
        | TradeCmd::BuyAsk
        | TradeCmd::SellBid
        | TradeCmd::Reverse => {} // handled above
    }
    cx.notify();
}

pub fn set_account<V: TradePane>(v: &mut V, account: Option<AccountId>, cx: &mut Context<V>) {
    crate::popover::mark_moved_under_pointer();
    close_account_menu(v, cx);
    let t = v.trade();
    if account.is_some() && account != t.account {
        t.account = account;
        t.resize_edit = None;
        t.seen = u64::MAX;
        v.trade().arm_event(ArmEvent::AccountChanged, cx);
        v.trade().editor = None;
        v.trade().bracket_forget();
    }
    cx.notify();
}

/// Whether an order is live in the market (not held server-side, not pending, something
/// left), as the ladder draws it.
fn live(o: &lines_trading::Order) -> bool {
    matches!(
        o.state,
        lines_trading::OrderState::Working | lines_trading::OrderState::PartiallyFilled
    ) && o.remaining().0 > 0
}

/// Whether a working order could protect the open `position` (signed) on its own: a live
/// stop on the exit side that only ever closes it (a bracket's stop loss, a reduce-only stop,
/// or a single stop no larger than the position). Never a held exit (its entry hasn't
/// filled), an entry stop (it would flip or add) or a triggered stop-limit (a resting limit
/// now: at BE it could fill at once). Together they must not exceed the position either:
/// [`protective`].
pub fn protects(o: &lines_trading::Order, position: i64) -> bool {
    use lines_trading::{OrderKind, Role, Side};
    let stop = match o.kind {
        OrderKind::Stop { .. } => true,
        OrderKind::StopLimit { .. } => !o.triggered,
        _ => false,
    };
    let exit = match position.signum() {
        1 => o.side == Side::Sell,
        -1 => o.side == Side::Buy,
        _ => false,
    };
    let closes = o.role == Role::StopLoss
        || o.reduce_only
        || (o.role == Role::Single && o.remaining().0 <= position.abs());
    live(o) && stop && exit && closes
}

/// The position card's stop: the nearest live stop that [`protects`] the position (a bracket's
/// stop loss, an exit, or a plain stop on the closing side no larger than the position), its
/// price and what is left of it.
pub fn card_stop<'a>(
    orders: impl Iterator<Item = &'a lines_trading::Order>,
    position: i64,
    last: Option<Price>,
) -> Option<(Price, i64)> {
    orders
        // A bracket's stop-loss leg stays the stop once a stop-limit has triggered (it rests as
        // a limit): BE won't move it, but it still protects.
        .filter(|o| {
            protects(o, position)
                || (live(o)
                    && o.role == lines_trading::Role::StopLoss
                    && o.side.sign() == -position.signum())
        })
        .filter_map(dom_trade::order_mark)
        .map(|m| (m.price, m.qty.0 - m.filled.0))
        .min_by_key(|(pr, _)| (pr.0 - last.map_or(pr.0, |l| l.0)).abs())
}

/// The stops that protect the position, as far as it goes: those that [`protects`] accepts,
/// in the order the market reaches them (a long's highest sell stop first; at one price, a
/// reduce-only stop after the others, since it shrinks itself), each while their quantities
/// together stay within the position. BE moves only these (those not already at or past
/// breakeven), and only towards the market, so it keeps that order: with prices printing one
/// at a time, every stop it leaves behind is reached only after these have closed the
/// position, as without BE. Refused while a bracket's entry still works (see the body).
pub fn protective<'a>(
    orders: impl IntoIterator<Item = &'a lines_trading::Order>,
    position: i64,
) -> Result<Vec<&'a lines_trading::Order>, &'static str> {
    use lines_trading::Role;
    let open: Vec<&lines_trading::Order> = orders.into_iter().collect();
    let mut cands: Vec<_> = open
        .iter()
        .copied()
        .filter(|o| protects(o, position))
        .collect();
    // A bracket's stop while its entry still works: at BE (at or past the entry's own
    // price) it could fill before the rest of the entry, which would then have no stop.
    if cands
        .iter()
        .any(|o| o.parent.is_some_and(|p| open.iter().any(|e| e.id == p)))
    {
        return Err("the bracket's entry is still working: drag the stop instead");
    }
    // Reached first: a long's sell stops from the top, a short's buy stops from the bottom.
    let reached = |o: &lines_trading::Order| {
        let p = crate::dom_trade::order_price(o).map_or(0, |p| p.0);
        if position > 0 { -p } else { p }
    };
    let role = |o: &lines_trading::Order| match (o.reduce_only, o.role) {
        (false, Role::StopLoss) => 0,
        (false, _) => 1,
        (true, _) => 2,
    };
    cands.sort_by_key(|o| (reached(o), role(o), o.id.0));
    // Every other order on the exit side that may fill (live, or still pending: sent and not
    // yet acknowledged; unresolved orders live in another cache map and aren't seen here,
    // docs/known-gaps.md), held bracket exits aside (their entry adds as much
    // first): a plain limit or scale-out, a triggered stop-limit resting as a limit, an
    // oversized stop, a bracket's target. Each also closes the position when it fills, so it
    // uses the budget first.
    let exit = |o: &lines_trading::Order| match position.signum() {
        1 => o.side == lines_trading::Side::Sell,
        -1 => o.side == lines_trading::Side::Buy,
        _ => false,
    };
    let may_fill = |o: &lines_trading::Order| {
        use lines_trading::OrderState as S;
        matches!(
            o.state,
            S::Submitted | S::Working | S::PartiallyFilled | S::Unresolved
        )
    };
    let counted: Vec<&lines_trading::Order> = open
        .iter()
        .copied()
        .filter(|o| may_fill(o) && exit(o) && !cands.iter().any(|c| c.id == o.id))
        .collect();
    let left = |o: &lines_trading::Order| (o.qty.0 - o.filled.0).max(0);
    let others: i64 = counted.iter().map(|o| left(o)).sum();
    // In that order, up to the first that doesn't fit: nothing BE moves is reached after a
    // stop it leaves behind. A kept bracket stop and its counted target share the bracket's
    // quantity (one cancels the other): the pair counts once.
    let mut total = others;
    let kept: Vec<_> = cands
        .iter()
        .copied()
        .take_while(|o| {
            let partner = o
                .oco
                .and_then(|x| counted.iter().find(|t| t.id == x))
                .map_or(0, |t| left(t));
            total += (o.remaining().0 - partner).max(0);
            total <= position.abs()
        })
        .collect();
    if kept.is_empty() && !cands.is_empty() {
        return Err(if others > 0 {
            "other exit orders already cover the position: drag the stop instead"
        } else {
            "stops exceed the position: drag the stop instead"
        });
    }
    Ok(kept)
}

/// The commands that move the position's stop(s) to breakeven, from the cache. BE only moves
/// existing protective stops, like dragging them: it needs no switch.
fn breakeven_for<V: TradePane>(v: &V, cx: &App) -> Result<Vec<TradeCommand>, &'static str> {
    if let Some(why) = cant_trade(v, cx) {
        return Err(why);
    }
    let t = Trading::try_global(cx).ok_or("trading is not running")?;
    let account = v.trade_ref().account.clone().ok_or("no account")?;
    let contract = contract_of(v.trade_store()).ok_or("contract unknown")?;
    let (bid, ask, last, tick) = v
        .trade_store()
        .try_read()
        .map(|d| {
            (
                d.book.best_bid().map(|b| b.0).or(d.bid),
                d.book.best_ask().map(|a| a.0).or(d.ask),
                d.last_trade,
                d.product.price.tick,
            )
        })
        .ok_or("market data busy; try again")?;
    let s = t.cache.read();
    let pos = s.position(&account, contract);
    let (qty, avg) = pos.map_or((0, None), |p| (p.qty, p.avg_price));
    let stops: Vec<_> = protective(s.open_orders(&account, contract), qty)?
        .into_iter()
        .filter_map(|o| dom_trade::order_price(o).map(|p| (o.id, o.side, p)))
        .collect();
    // A long's stop is a sell: it must sit below where it would sell (the bid).
    let market = if qty > 0 { bid.or(last) } else { ask.or(last) };
    breakeven(&account, qty, avg, &stops, market, tick)
}

/// Keys while the bracket editor is open (it takes every key it uses; workspace commands
/// still run). Returns whether the key was used.
pub fn editor_key<V: TradePane>(v: &mut V, ev: &gpui::KeyDownEvent, cx: &mut Context<V>) -> bool {
    use crate::brackets::EditorKey;
    let k = &ev.keystroke;
    let m = k.modifiers;
    if v.trade_ref().editor.is_none() || m.alt || m.platform {
        return false;
    }
    let out = match v.trade().editor.as_mut() {
        Some(ed) => ed.key(k.key.as_str(), k.key_char.as_deref(), m.control, m.shift),
        None => return false,
    };
    match out {
        // A printable key the editor refused stays in the editor (never the DOM's letter
        // keys); other keys (commands) pass on.
        EditorKey::Ignored => return k.key_char.is_some() && !m.control,
        EditorKey::Typing => v.trade().editor_msg = None,
        EditorKey::Refused(why) => v.trade().editor_msg = Some(why),
        EditorKey::Cancel => {
            v.trade().editor = None;
            v.trade().editor_msg = None;
        }
        EditorKey::Save(presets) => {
            let account = v.trade_ref().account.as_ref().map(|a| a.0.clone());
            let symbol = v
                .trade_ref()
                .editor
                .as_ref()
                .map(|e| e.product.symbol.to_owned());
            if let (Some(a), Some(sym)) = (account, symbol) {
                crate::picker_view::update_settings(cx, |s| {
                    s.bracket_presets
                        .entry(a)
                        .or_default()
                        .products
                        .insert(sym, presets);
                });
            }
            v.trade().editor = None;
            v.trade().editor_msg = None;
        }
    }
    cx.notify();
    true
}

/// Whether order `id` is still open, as far as the pane's cache knows (busy: assume so).
fn resize_order_open(t: &PaneTrade, id: lines_trading::OrderId) -> bool {
    let Some(cache) = &t.cache else {
        return false;
    };
    cache
        .try_read()
        .is_none_or(|s| s.order(id).is_some_and(lines_trading::Order::is_open))
}

/// What resizing order `id` would send, from the cache's state (`order_resize::plan`).
pub fn resize_plan(
    id: lines_trading::OrderId,
    step: crate::order_resize::SizeStep,
    one_leg: bool,
    cx: &App,
) -> Result<crate::order_resize::Plan, String> {
    let t = Trading::try_global(cx).ok_or("trading is not running")?;
    let s = t.cache.read();
    let o = s.order(id).ok_or("the order is gone")?;
    let sibling = o.oco.and_then(|i| s.order(i));
    let entry_working = o
        .parent
        .and_then(|p| s.order(p))
        .is_some_and(lines_trading::Order::is_open);
    let position = s.position(&o.account, o.contract).map_or(0, |p| p.qty);
    // A position bracket sizes its exits to the position itself: a change would bounce back.
    let symbol = lines_feed::catalog::get(o.contract.product).map(|p| p.symbol);
    if o.reduce_only
        && symbol.is_some_and(|sym| {
            let brackets = &crate::picker_view::settings(cx).bracket_presets;
            crate::brackets::position_mode(brackets, &o.account.0, sym)
        })
    {
        return Err("the position bracket sizes its exits to the position".to_owned());
    }
    crate::order_resize::plan(o, sibling, entry_working, position, step, one_leg)
}

/// Open the field to type a working order's size.
pub fn open_resize_field<V: TradePane>(v: &mut V, id: lines_trading::OrderId) {
    v.trade().qty_edit = None;
    v.trade().resize_edit = Some((id, String::new()));
}

/// Keys while a working order's size is being typed. Enter sends it (Alt: one leg of a linked
/// pair). Returns whether the key was used.
pub fn resize_key<V: TradePane>(v: &mut V, key: &str, alt: bool, cx: &mut Context<V>) -> bool {
    if let Some((id, _)) = v.trade_ref().resize_edit
        && !(v.keys_free() && v.resize_drawn(id))
    {
        v.trade().resize_edit = None;
        return false;
    }
    // Alt only means something on Enter; other Alt keys are the pane's.
    if alt && key != "enter" {
        return false;
    }
    let t = v.trade();
    let Some((id, edit)) = t.resize_edit.as_mut() else {
        return false;
    };
    let id = *id;
    match key {
        "enter" => match parse_qty(edit) {
            Some(q) => {
                t.resize_edit = None;
                v.send_resize(id, crate::order_resize::SizeStep::To(q), alt, cx);
            }
            None => t.flash("quantity: whole number 1–9999"),
        },
        "escape" => t.resize_edit = None,
        "backspace" => {
            edit.pop();
        }
        k if k.len() == 1 && k.as_bytes()[0].is_ascii_digit() && edit.len() < 4 => edit.push_str(k),
        _ => {}
    }
    cx.notify();
    true
}

/// Wheel travel: resize the order under the pointer when the pointer moved there first (see
/// `resize_armed`). Returns whether the wheel was used; otherwise it scrolls or zooms as usual
/// and the wheel stays off orders until the pointer moves.
pub fn wheel_resize<V: TradePane>(
    v: &mut V,
    hovered: Option<lines_trading::OrderId>,
    ev: &gpui::ScrollEvent,
    w: &Window,
    cx: &mut Context<V>,
) -> bool {
    let dy = f32::from(ev.delta.pixel_delta(w.line_height()).y);
    // A sideways wheel, or Shift, pans: it never resizes.
    let dx = f32::from(ev.delta.pixel_delta(w.line_height()).x);
    if dy == 0.0 || dx.abs() > dy.abs() || ev.modifiers.shift {
        return false;
    }
    let Some(id) = hovered.filter(|_| v.trade_ref().resize_armed && !ev.modifiers.control) else {
        v.trade().resize_armed = false;
        return false;
    };
    if let Some(up) = dom_trade::scroll_qty(&mut v.trade().qty_scroll, dy) {
        let step = if up {
            crate::order_resize::SizeStep::Up
        } else {
            crate::order_resize::SizeStep::Down
        };
        v.send_resize(id, step, ev.modifiers.alt, cx);
    }
    cx.stop_propagation();
    true
}

/// Open the field to type a size, or say why not.
pub fn open_size_field<V: TradePane>(v: &mut V) {
    if v.trade_unavailable() {
        v.trade().flash("this pane has no market data");
    } else if v.shows_size() {
        v.trade().resize_edit = None;
        v.trade().qty_edit = Some(String::new());
    } else {
        v.trade().flash("Can't type a size now");
    }
}

/// A size changed by a key on a pane that doesn't show it (a chart without its strip): say so.
fn say_size<V: TradePane>(v: &mut V, cx: &App) {
    if !v.shows_size() && !v.trade_unavailable() {
        let q = pane_size(v, cx);
        let symbol = v.trade_product().symbol;
        v.trade().flash(format!("{symbol} order size {q}"));
    }
}

/// Keys while a custom quantity is being typed. Returns whether the key was used. A field
/// with nothing drawing it (`shows_size`) is dropped here, so a pane's keys never go dead with
/// nothing showing why.
pub fn qty_key<V: TradePane>(v: &mut V, key: &str, cx: &mut Context<V>) -> bool {
    if !v.shows_size() {
        v.trade().qty_edit = None;
        return false;
    }
    let t = v.trade();
    let Some(edit) = t.qty_edit.as_mut() else {
        return false;
    };
    match key {
        "enter" => match parse_qty(edit) {
            Some(q) => set_size(v, q, cx),
            None => t.flash("quantity: whole number 1–9999"),
        },
        "escape" => t.qty_edit = None,
        "backspace" => {
            edit.pop();
        }
        k if k.len() == 1 && k.as_bytes()[0].is_ascii_digit() && edit.len() < 4 => edit.push_str(k),
        _ => {}
    }
    cx.notify();
    true
}

// ---- rendering ---------------------------------------------------------------------------

/// What the panel shows, gathered under the cache lock (never waited for).
struct BarData {
    account_name: String,
    /// PAPER, SIM or LIVE.
    badge: &'static str,
    /// The account can't be traded through Lines: why.
    read_only: Option<String>,
    /// A broker account's connection, as words, and whether it needs attention.
    venue: Option<(String, bool)>,
    /// Signed qty, average price (price units), open P&L in ticks (when the mark is known)
    /// and money.
    position: Option<(i64, Option<f64>, Option<f64>, Money)>,
    /// The nearest working stop and target of the position (bracket exits and exits added on
    /// the chart): price and quantity left.
    stop: Option<(Price, i64)>,
    target: Option<(Price, i64)>,
    /// Realised + open P&L for the session trade date (the account's).
    day_pnl: Option<Money>,
    /// How P&L shows (app setting); off: no amounts anywhere in the panel.
    mode: crate::pnl::PnlDisplay,
    /// (label, left, limit) per rule.
    rules: Vec<(&'static str, Money, Money)>,
    /// The account's trading state, as words, when it isn't active.
    blocked: Option<String>,
    /// Short form of `blocked` for the switch's badge.
    blocked_short: Option<&'static str>,
    /// The guardrail lockout in force, as words (`guard_line`), and its state.
    guard: Option<(lines_trading::GuardState, String)>,
    /// The economic calendar (roadmap 8b): the chip's words and whether it warns (a news
    /// blackout window, or a calendar that can't answer), and the switch's reason when it
    /// refuses entries.
    news: Option<(bool, String)>,
    news_block: Option<&'static str>,
    /// (label, text): DENIED / REJECTED / WARNING / NOT SENT / NOTICE.
    flashes: Vec<(&'static str, String)>,
    unresolved: usize,
    working: usize,
    /// Market orders still working (a thin book's remainder): what is left to buy and to sell.
    market_left: Option<(i64, i64)>,
    /// Strategy instances armed on this account (the switch says so: it doesn't stop them).
    strategies_armed: usize,
}

/// What is left of the market orders working at the broker (they have no price row): to buy, to
/// sell. Not those still on their way (no flicker for a market order's latency).
fn market_left<'a>(orders: impl Iterator<Item = &'a lines_trading::Order>) -> Option<(i64, i64)> {
    use lines_trading::{OrderState, Side};
    let (mut buy, mut sell) = (0, 0);
    for o in orders
        .filter(|o| matches!(o.kind, lines_trading::OrderKind::Market))
        .filter(|o| matches!(o.state, OrderState::Working | OrderState::PartiallyFilled))
    {
        match o.side {
            Side::Buy => buy += o.remaining().0,
            Side::Sell => sell += o.remaining().0,
        }
    }
    (buy + sell > 0).then_some((buy, sell))
}

/// `market sell 3 left` (both sides, rare: `market buy 1, sell 3 left`).
fn market_words((buy, sell): (i64, i64)) -> String {
    match (buy, sell) {
        (b, 0) => format!("market buy {b} left"),
        (0, s) => format!("market sell {s} left"),
        (b, s) => format!("market buy {b}, sell {s} left"),
    }
}

fn empty_data(info: &crate::trading::AccountInfo, t: &Trading) -> BarData {
    BarData {
        account_name: info.name.clone(),
        badge: info.badge,
        read_only: info.read_only.clone(),
        venue: info.venue.and_then(|i| venue_words(t, i)),
        position: None,
        stop: None,
        target: None,
        day_pnl: None,
        mode: crate::pnl::PnlDisplay::default(),
        rules: Vec::new(),
        blocked: None,
        blocked_short: None,
        guard: None,
        news: None,
        news_block: None,
        flashes: Vec::new(),
        unresolved: 0,
        working: 0,
        market_left: None,
        strategies_armed: 0,
    }
}

fn gather<V: TradePane>(v: &V, product: &Product, cx: &App) -> Option<BarData> {
    let t = Trading::try_global(cx)?;
    let pt = v.trade_ref();
    let account = pt.account.as_ref()?;
    let info = t.account(account)?;
    let contract = contract_of(v.trade_store());
    let last = v.trade_store().try_read().and_then(|d| d.last_trade);
    let mut data = empty_data(&info, t);
    data.mode = cx
        .try_global::<crate::picker_view::AppSettings>()
        .map(|a| a.settings.pnl_display)
        .unwrap_or_default();
    if let Some(s) = t.cache.try_read() {
        if let Some(g) = s.guard(account) {
            // Time left on the core's clock (wall live, market time in replays).
            let now = t.now(v.trade_store());
            data.guard = guard_line(
                g.state,
                g.reason.as_deref().unwrap_or(""),
                g.until,
                now,
                data.mode.money(),
            )
            .map(|l| (g.state, l));
        }
        if let Some(k) = contract {
            data.working = s.open_orders(account, k).count();
            data.market_left = market_left(s.open_orders(account, k));
            if let Some(p) = s.position(account, k) {
                let ticks = match (p.avg_price, last) {
                    (Some(a), Some(m)) => open_ticks(p.qty, a, m, product.price.tick),
                    _ => None,
                };
                data.position = Some((p.qty, p.avg_price, ticks, p.unrealised));
                // The position's bracket exits: the nearest of each role.
                // Live ones only: a held exit (its entry not filled) protects nothing yet.
                let near = |role: dom_trade::MarkRole| {
                    s.open_orders(account, k)
                        .filter(|o| live(o))
                        .filter_map(dom_trade::order_mark)
                        .filter(|m| dom_trade::MarkRole::of(m) == role)
                        .map(|m| (m.price, m.qty.0 - m.filled.0))
                        .min_by_key(|(pr, _)| (pr.0 - last.map_or(pr.0, |l| l.0)).abs())
                };
                // The stop: any live stop that protects the position (`protects`, as BE
                // decides), a plain one placed on the ladder included, so "No stop" is never
                // said while one is working.
                data.stop = card_stop(s.open_orders(account, k), p.qty, last);
                data.target = near(dom_trade::MarkRole::Target);
            }
        }
        // The calendar: a release coming, or the account's news blackout, for this product.
        if let Some(now) = t.now(v.trade_store()) {
            let rule = s
                .account(account)
                .and_then(|a| crate::econ_ui::rule_of(&a.limits));
            let schedule = crate::econ_ui::schedule();
            if let Some(c) =
                crate::econ_ui::chip(Some(&schedule), rule.as_ref(), product.symbol, now)
            {
                data.news = Some((
                    crate::econ_ui::warns(&c),
                    crate::econ_ui::chip_text(&c, now),
                ));
                data.news_block = crate::econ_ui::blocker(&c);
            }
        }
        if let Some(a) = s.account(account) {
            data.day_pnl = Some(a.day_pnl);
            // The limits in force (they change from the Settings window), else the account's
            // starting preset.
            if let (Some(r), Some(preset)) = (
                &a.rules,
                a.limits.rules_in_force.as_ref().or(info.rules.as_ref()),
            ) {
                if let (Some(left), Some(limit)) = (r.daily_loss_left, preset.daily_loss_limit) {
                    data.rules.push(("Daily loss", left, Money::whole(limit)));
                }
                if let (Some(left), Some(dd)) = (r.to_drawdown, &preset.trailing_drawdown) {
                    data.rules.push(("Drawdown", left, Money::whole(dd.amount)));
                }
            }
            let why = a.blocked.map_or("", |b| match b {
                lines_trading::rules::Breach::TrailingDrawdown => "trailing drawdown breached",
                lines_trading::rules::Breach::DailyLoss => "daily loss limit reached",
            });
            (data.blocked, data.blocked_short) = match a.trading {
                lines_trading::rules::TradingState::Active => (None, None),
                lines_trading::rules::TradingState::ReducingOnly => (
                    Some(format!("Reducing only · {why} · orders may only close")),
                    Some("reducing only"),
                ),
                lines_trading::rules::TradingState::Halted => (
                    Some(format!("Halted · {why} · no new orders")),
                    Some("halted"),
                ),
            };
        }
        let now = Instant::now();
        data.strategies_armed = s.strategies_armed(account);
        data.flashes = s
            .flashes(account, contract, now)
            .map(|f| {
                // P&L off: refusals keep their reason, never an amount.
                let message = if data.mode.shown() {
                    std::borrow::Cow::Borrowed(f.message.as_str())
                } else {
                    crate::pnl::redact_amounts(&f.message)
                };
                let text = match f.price {
                    Some(p) => format!("{}: {message}", product.price.format::<32>(p).as_str()),
                    None => message.into_owned(),
                };
                (f.kind.label(), text)
            })
            .collect();
        data.unresolved = s.unresolved(account);
    }
    if let Some(m) = pt.local_flash() {
        data.flashes.push((pt.local_flash_label, m.to_owned()));
    }
    Some(data)
}

/// Why orders from this pane would be denied right now, in two or three words, for the
/// switch's badge ("Enabled · market closed"). The switch never flips for these.
fn blocker<V: TradePane>(
    v: &V,
    product: &Product,
    data: Option<&BarData>,
    cx: &App,
) -> Option<std::borrow::Cow<'static, str>> {
    if let Some(why) = cant_trade(v, cx) {
        return Some(
            match why {
                "this pane has no market data" => "no market data",
                "contract unknown: can't trade it" => "contract unknown",
                w => w,
            }
            .into(),
        );
    }
    if is_read_only(v, cx) {
        return Some("read-only".into());
    }
    if let Some(quiet) = v.trade_store().stale_for() {
        return Some(format!("feed stale {} s", quiet.as_secs()).into());
    }
    if let Some((state, _)) = data.and_then(|d| d.guard.as_ref()) {
        return Some(
            match state {
                lines_trading::GuardState::Cooldown => "cooldown",
                _ => "locked",
            }
            .into(),
        );
    }
    if let Some(b) = data.and_then(|d| d.blocked_short) {
        return Some(b.into());
    }
    if let Some(b) = data.and_then(|d| d.news_block) {
        return Some(b.into());
    }
    let t = Trading::try_global(cx)?;
    if t.check_sessions
        && let (Some(cal), Some(now)) = (
            lines_feed::catalog::sessions(product.id),
            t.now(v.trade_store()),
        )
        && !cal.is_open(now)
    {
        return Some("market closed".into());
    }
    None
}

/// The switch's text: `Trading: Enabled`, `Trading: Disabled`, and, when enabled but orders
/// would be denied, the reason (`Enabled · market closed`). Narrow panels drop "Trading:".
pub fn switch_text(enabled: bool, blocker: Option<&str>, narrow: bool) -> String {
    let state = if enabled { "Enabled" } else { "Disabled" };
    match (enabled, blocker, narrow) {
        // Narrow: the reason goes to the status line ([`blocked_line`]).
        (true, Some(_), true) => format!("{state} !"),
        (true, Some(b), false) => format!("{state} · {b}"),
        (_, _, true) => state.to_owned(),
        _ => format!("Trading: {state}"),
    }
}

/// From this width the switch says "2 strategies armed"; narrower, "2 strat".
pub const SWITCH_STRATEGIES: f32 = 380.0;
/// From this width it leads with "Trading:" too.
pub const SWITCH_WORDS: f32 = 600.0;

/// The switch's text with strategies armed on its account (a switch that says "Disabled"
/// doesn't stop them, decision 0025), as whole items that fit its row at each width: the state
/// always, and the count with its noun always (with a count the switch doesn't shrink: the
/// account chip gives way, so the number is never cut). A reason orders would be denied goes
/// to the status line ([`reason_on_status`]). No strategies armed: [`switch_text`].
pub fn switch_text_armed(enabled: bool, blocker: Option<&str>, armed: usize, width: f32) -> String {
    if armed == 0 {
        return switch_text(enabled, blocker, is_narrow(width));
    }
    let alert = if enabled && blocker.is_some() {
        "!"
    } else {
        ""
    };
    if is_narrow(width) {
        let state = if enabled { "On" } else { "Off" };
        return format!("{state}{alert} · {armed} strat");
    }
    let state = if enabled { "Enabled" } else { "Disabled" };
    let full = width == 0.0 || width >= SWITCH_WORDS;
    if full || width >= SWITCH_STRATEGIES {
        let lead = if full { "Trading: " } else { "" };
        format!("{lead}{state}{alert} · {}", strategies_armed_text(armed))
    } else {
        format!("{state}{alert} · {armed} strat")
    }
}

/// `1 strategy armed`, `2 strategies armed`.
fn strategies_armed_text(armed: usize) -> String {
    format!("{armed} strateg{} armed", plural_y(armed))
}

/// Whether why orders would be denied shows on the status line, not the switch: narrow panels,
/// and any panel whose switch is already showing a strategy count.
pub fn reason_on_status(narrow: bool, armed: usize) -> bool {
    narrow || armed > 0
}

/// The order hotkeys chip's text: the state in words (narrow: "Keys" and the knob).
pub fn keys_text(on: bool, narrow: bool) -> &'static str {
    match (on, narrow) {
        (_, true) => "Keys",
        (true, false) => "Keys on",
        (false, false) => "Keys off",
    }
}

/// The order hotkeys switch: a pill like the trading switch, its knob filled when on (shape
/// and words, never color alone), outlined in the warning color while keys can trade.
fn keys_chip<V: TradePane>(
    on: bool,
    narrow: bool,
    t: &Theme,
    cx: &mut Context<V>,
) -> gpui::Stateful<gpui::Div> {
    let knob = div().size(crate::zoom::ui(8.0)).rounded_full().border_1();
    let s = div()
        .id("keys-switch")
        .flex()
        .flex_row()
        .flex_shrink_0()
        .items_center()
        .gap_1()
        .h(crate::zoom::hit(MIN_HIT + 2.0))
        .px_2()
        .rounded_full()
        .border_1()
        .cursor_pointer()
        .whitespace_nowrap();
    let s = if on {
        s.border_color(t.warn)
            .text_color(t.text_main)
            .font_weight(FontWeight::BOLD)
            .child(knob.bg(t.warn).border_color(t.warn))
    } else {
        s.border_color(t.axis_line)
            .text_color(t.text_dim)
            .child(knob.border_color(t.text_dim))
    };
    let s = s.child(keys_text(on, narrow));
    let name = if on {
        "Order hotkeys on: turn off"
    } else {
        "Order hotkeys off: turn on"
    };
    run_on_click(
        tip(s, name, Some(TradeCmd::ToggleHotkeys), t),
        TradeCmd::ToggleHotkeys,
        cx,
    )
}

/// The status line for an enabled switch whose orders would be denied. `short` below
/// [`KEYS_IN_HEADER`], where the line may share its row with the Keys chip: the reason is what
/// matters, and the line's items are laid out whole, last ones left out.
pub fn blocked_line(blocker: &str, short: bool) -> String {
    if short {
        format!("Denied: {blocker}")
    } else {
        format!("Enabled, but orders are denied: {blocker}")
    }
}

/// The guardrail strip's text for a lockout: "Cooldown · 3 losses in a row · 12 min left",
/// "Locked · max 5 trades reached · until the next trade date"; `None` while active. Minutes
/// round up (as the core's denial does); without a clock the time left is left out. Amounts
/// (a daily loss limit) show only when the P&L display shows currency, like the rule strip.
pub fn guard_line(
    state: lines_trading::GuardState,
    reason: &str,
    until: Option<lines_core::UtcMillis>,
    now: Option<lines_core::UtcMillis>,
    money: bool,
) -> Option<String> {
    use lines_trading::GuardState as G;
    let reason = if money {
        std::borrow::Cow::Borrowed(reason)
    } else {
        crate::pnl::redact_amounts(reason)
    };
    let mut out = String::with_capacity(64);
    let _ = match state {
        G::Active => return None,
        G::Cooldown => write!(out, "Cooldown · {reason}"),
        G::Locked => write!(out, "Locked · {reason}"),
    };
    match (state, until, now) {
        (G::Cooldown, Some(u), Some(n)) => {
            let min = ((u.0 - n.0).max(0) + 59_999) / 60_000;
            let _ = write!(out, " · {min} min left");
        }
        (G::Locked, ..) => out.push_str(" · until the next trade date"),
        _ => {}
    }
    Some(out)
}

/// The panel's refusal line: the newest (label, text), led by the ladder tag's own word when
/// the message doesn't already say it (`5806.75: far · price is 400 ticks from the market`),
/// with `+N more` for the others still shown, so a burst never stacks rows and the header
/// and the ladder read the same.
pub fn flash_line(flashes: &[(&'static str, String)]) -> Option<(&'static str, String)> {
    let (label, text) = flashes.last()?;
    let (price, message) = match text.split_once(": ") {
        Some((p, m))
            if p.bytes()
                .all(|b| b.is_ascii_digit() || b == b'.' || b == b'\'' || b == b'-') =>
        {
            (Some(p), m)
        }
        _ => (None, text.as_str()),
    };
    let short = dom_trade::short_reason(message);
    let mut out = String::with_capacity(text.len() + 24);
    if let Some(p) = price {
        let _ = write!(out, "{p}: ");
    }
    if !short.is_empty() && !message.to_lowercase().contains(&short.to_lowercase()) {
        let _ = write!(out, "{short} · ");
    }
    out.push_str(message);
    if flashes.len() > 1 {
        let _ = write!(out, "  +{} more", flashes.len() - 1);
    }
    Some((*label, out))
}

/// A status text split into the items it is laid out as, each shown whole or not at all, so
/// no number is ever cut: at " · " and before "+N more" (the items' separators), and after
/// ": " and ", " (which stay with the item before). `(text, separated)`: whether a " · "
/// separator leads the item.
pub fn status_parts(text: &str) -> impl Iterator<Item = (&str, bool)> {
    text.split("  ")
        .flat_map(|p| p.split(" · ").enumerate())
        .flat_map(|(k, p)| {
            p.split_inclusive(": ")
                .flat_map(|q| q.split_inclusive(", "))
                .enumerate()
                .map(move |(m, q)| (q.trim(), k > 0 && m == 0))
        })
        .filter(|(p, _)| !p.is_empty())
}

/// `text` as whole items on one line of text (centred in its slot by the caller): an item
/// that doesn't fit wraps into the hidden overflow instead of being cut (a limit, minutes
/// left, a price), taking its leading " · " with it.
fn whole_items(text: &str, color: gpui::Hsla, bold: bool) -> gpui::Div {
    let mut row = div()
        .flex()
        .flex_row()
        .flex_wrap()
        .gap_x_1()
        .min_w_0()
        .h(crate::zoom::ui(ITEMS_LINE_H))
        .items_center()
        .overflow_hidden()
        .whitespace_nowrap();
    for (part, separated) in status_parts(text) {
        let mut item = div().flex_shrink_0().text_color(color);
        if bold {
            item = item.font_weight(FontWeight::BOLD);
        }
        let s = if separated {
            format!("· {part}")
        } else {
            part.to_owned()
        };
        row = row.child(item.child(s));
    }
    row
}

/// One line of 12 px text: the height `whole_items` keep, so a wrapped item never shows.
const ITEMS_LINE_H: f32 = 18.0;

/// An average price: exact on the tick grid, else one more decimal than the product shows.
pub fn avg_text(product: &Product, avg: f64) -> String {
    let mut s = String::new();
    let _ = write_avg(&mut s, product, avg);
    s
}

/// [`avg_text`] into a caller's buffer (paint paths: no allocation).
pub fn write_avg(out: &mut impl std::fmt::Write, product: &Product, avg: f64) -> std::fmt::Result {
    let fmt = product.price;
    let r = avg.round();
    if (avg - r).abs() < 1e-9 {
        return fmt.write(Price(r as i64), out);
    }
    let shown = fmt.format::<32>(Price(r as i64));
    let decimals = shown.as_str().split('.').nth(1).map_or(0, str::len) + 1;
    let v = avg * fmt.scale.num() as f64 / fmt.scale.den() as f64;
    write!(out, "{v:.decimals$}")
}

/// Money with thousands separators, no sign: `$1,000`, `$125.50` (cents only when there are
/// any). Exact: rounded to cents for display only.
pub fn money_grouped(m: Money) -> String {
    let cents = (m.0.unsigned_abs() + (Money::UNITS_PER_WHOLE as u128 / 200))
        / (Money::UNITS_PER_WHOLE as u128 / 100);
    let (whole, c) = (cents / 100, cents % 100);
    let mut out = String::with_capacity(16);
    out.push('$');
    let _ = crate::pnl::write_grouped(&mut out, whole);
    if c != 0 {
        let _ = write!(out, ".{c:02}");
    }
    out
}

/// A rule meter: how much of a limit is used, `used / limit` (`$125 / $1,000`), the share
/// (0..=1) and whether it is past the warning share.
pub struct Meter {
    pub text: String,
    pub share: f32,
    pub hot: bool,
    pub reached: bool,
}

/// Meters warn past this share of a limit.
pub const METER_WARN: f32 = 0.25;

pub fn meter(left: Money, limit: Money, money: bool) -> Meter {
    let used = Money((limit.0 - left.0).max(0));
    let share = if limit.0 > 0 {
        (used.0 as f64 / limit.0 as f64).clamp(0.0, 1.0) as f32
    } else {
        0.0
    };
    let level = dom_trade::rule_level(left.0, limit.0);
    let reached = level == dom_trade::RuleLevel::Breached;
    let text = match (money, level) {
        (true, _) => format!("{} / {}", money_grouped(used), money_grouped(limit)),
        // P&L not in currency: states, never an amount or a share of one.
        (false, dom_trade::RuleLevel::Breached) => "limit reached".into(),
        (false, dom_trade::RuleLevel::Near) => "near limit".into(),
        (false, dom_trade::RuleLevel::Ok) => "ok".into(),
    };
    Meter {
        text,
        share,
        hot: share > METER_WARN,
        reached,
    }
}

/// Move the position's stop(s) to breakeven: the average price, on the safe side of the tick
/// grid (a long's stop at or above it, a short's at or below), through the modify path. Only
/// when the market is already past that price (else the stop would fill at once).
pub fn breakeven(
    account: &AccountId,
    position: i64,
    avg: Option<f64>,
    stops: &[(lines_trading::OrderId, lines_trading::Side, Price)],
    market: Option<Price>,
    tick: i64,
) -> Result<Vec<TradeCommand>, &'static str> {
    if position == 0 {
        return Err("no position: nothing to move to breakeven");
    }
    let avg = avg.ok_or("average price unknown")?;
    let long = position > 0;
    let steps = avg / tick.max(1) as f64;
    let steps = if long {
        (steps - 1e-9).ceil()
    } else {
        (steps + 1e-9).floor()
    };
    let be = Price(steps as i64 * tick.max(1));
    let exit = if long {
        lines_trading::Side::Sell
    } else {
        lines_trading::Side::Buy
    };
    let mine: Vec<_> = stops.iter().filter(|s| s.1 == exit).collect();
    if mine.is_empty() {
        return Err("no stop working: place a bracket first");
    }
    let m = market.ok_or("no market price yet")?;
    if (long && m.0 <= be.0) || (!long && m.0 >= be.0) {
        return Err("price hasn't moved past breakeven yet");
    }
    // Only stops still on the losing side move: BE never loosens a stop already at or past it.
    let cmds: Vec<_> = mine
        .iter()
        .filter(|s| if long { s.2.0 < be.0 } else { s.2.0 > be.0 })
        .map(|s| TradeCommand::ModifyOrder {
            account: account.clone(),
            order: s.0,
            qty: None,
            price: Some(be),
        })
        .collect();
    if cmds.is_empty() {
        return Err("the stop is already at or past breakeven");
    }
    Ok(cmds)
}

// ---- layout (fixed height: the ladder never moves when state changes) -----------------
//
// Every size here is in 100 % units (`crate::zoom`): the DOM passes its width and height divided
// by the zoom, and the elements take `zoom::ui` of these constants, so at 150 % the panel is a
// 100 % panel at two-thirds of the width. Widths are measured at the panel's wide glyph estimate
// (7.5 px a character at 12 px, [`text_w`]): real text is narrower, never wider.

/// Below this width the panel collapses (meters behind ▸, short labels).
pub const NARROW_BELOW: f32 = 300.0;

/// From this width the Keys chip sits in the header beside the account and the switch (the
/// widest switch text included); narrower, it shows on the status line while keys are on, and on
/// the account row, where it fits, once a short pane's status line has gone.
pub const KEYS_IN_HEADER: f32 = 480.0;

/// Whether the Keys chip sits in the header (else it trails the status line while keys are
/// on). A width of 0 (not laid out yet) is the full layout, as for [`is_narrow`].
pub fn keys_in_header(width: f32) -> bool {
    width == 0.0 || width >= KEYS_IN_HEADER
}

/// The narrowest DOM the panel is laid out for (owner's ruling, trade panel mock 2; layout tests
/// below): every row fits whole from here.
pub const MIN_DOM_WIDTH: f32 = 165.0;

/// From this width the exits row holds BE and Reverse beside Cancel all and Flatten. Narrower, BE
/// and Reverse take a row of their own above the divider. (The Join buttons have already gone,
/// below [`NARROW_BELOW`].)
pub const EXITS_ONE_ROW: f32 = 240.0;

/// Whether a panel `width` is narrow (0: not laid out yet, full layout).
pub fn is_narrow(width: f32) -> bool {
    width > 0.0 && width < NARROW_BELOW
}

/// Whether a panel `width` is below [`EXITS_ONE_ROW`] (0: not laid out yet, full layout).
pub fn is_tight(width: f32) -> bool {
    width > 0.0 && width < EXITS_ONE_ROW
}

/// The top row: PAPER, the account, the trading switch.
const HDR_H: f32 = 28.0;
/// The position card's first line, its lower lines and the space under it.
const CARD_H: f32 = 58.0;
const CARD_BOX_H: f32 = 50.0;
/// One card line under the first.
const CARD_LINE_H: f32 = 16.0;

/// The position card's lines under the first at `width`: two, four in a tight panel. The orders
/// fill them by priority ([`card_place`]); the ticks sit on the first line's right, under the
/// open P&L, and the day's P&L on the last line's right.
pub fn card_lines(width: f32) -> usize {
    if is_tight(width) { 4 } else { 2 }
}

/// The position card's row height at `width`.
pub fn card_h(width: f32) -> f32 {
    CARD_H + (card_lines(width) - 1) as f32 * CARD_LINE_H
}

fn card_box_h(width: f32) -> f32 {
    CARD_BOX_H + (card_lines(width) - 1) as f32 * CARD_LINE_H
}
const STATUS_H: f32 = 24.0;
const METERS_H: f32 = 38.0;
const METERS_NARROW_H: f32 = 20.0;
/// The Qty and Bracket fields and the space under them.
const CTRL_H: f32 = 36.0;
const FIELD_H: f32 = 28.0;
/// Buy and Sell, and the space under them.
const TRADE_H: f32 = 44.0;
const TRADE_BUTTON_H: f32 = 40.0;
/// Join bid and Join ask, and the space under them.
const JOIN_H: f32 = 34.0;
const JOIN_BUTTON_H: f32 = 26.0;
/// BE and Reverse in a row of their own (tight panels).
const ACT_H: f32 = 30.0;
/// The exits row: Cancel all and Flatten (with BE and Reverse from [`EXITS_ONE_ROW`]).
const UTIL_H: f32 = 30.0;
const PAD_Y: f32 = 6.0;
/// The panel's side padding (`px_2`) and the gap between paired buttons and fields.
const SIDE: f32 = 8.0;
const PAIR_GAP: f32 = 8.0;
/// A button's or field's inner padding each side, and its border.
const BUTTON_PX: f32 = 8.0;
const BUTTON_TIGHT_PX: f32 = 6.0;
const FIELD_PX: f32 = 8.0;
const FIELD_TIGHT_PX: f32 = 6.0;
const FIELD_GAP: f32 = 6.0;
const FIELD_TIGHT_GAP: f32 = 4.0;
/// The ▾ on a field.
const CHEVRON_W: f32 = 8.0;
/// The trading switch's track.
const TRACK_W: f32 = 26.0;
const TRACK_H: f32 = 14.0;

/// Smallest hit target (logical px, both ways).
pub const MIN_HIT: f32 = 20.0;

/// The least height a split leaves a DOM: its header, the account row and the exits.
pub const MIN_DOM_PANE_H: f32 = 27.0 + HDR_H + UTIL_H + PAD_Y;

/// The least height a split leaves a pane of this kind.
pub fn min_pane_h(kind: crate::layout::PaneKind) -> f32 {
    match kind {
        crate::layout::PaneKind::Dom => MIN_DOM_PANE_H,
        _ => 0.0,
    }
}
/// The ladder's header and three rows, so it shows the inside market. Shorter, it doesn't show.
pub const LADDER_MIN_H: f32 =
    crate::pane_columns::HEADER_ROW_H + 3.0 * crate::dom_view::LADDER_ROW_H;

/// A text's width at `size` px, at the panel's wide glyph estimate.
pub fn text_w(s: &str, size: f32) -> f32 {
    s.chars().count() as f32 * 7.5 * size / 12.0
}

/// A word's width on these lines, at the panel's wide glyph estimate (12 px).
fn word_w(s: &str) -> f32 {
    text_w(s, 12.0)
}

/// An element's height at the zoom, never under the least hit target (`zoom::hit`'s rule: 80 %
/// shrinks text, never a target). Every such height fits its row's slot at 80 %.
fn tall(v: f32) -> gpui::Pixels {
    gpui::px(crate::zoom::f(v).max(MIN_HIT))
}

/// Which of the panel's rows a pane has room for. Cancel all and Flatten always come first
/// (docs/design.md §3); everything else goes whole, never cut through, in the order of
/// [`panel_rows`]. A function of the pane's size only (and the editor and the Join buttons'
/// setting, below), never of the trading state, so the ladder never shifts as a position opens or
/// a lockout starts (§7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PanelRows {
    /// The account and the switch. Only a window too short for its DOMs' minimum height goes
    /// without it (`MIN_DOM_PANE_H`): the exits stay, under the pane's header.
    pub account: bool,
    /// Without it, the position and its P&L take one line on the account row.
    pub card: bool,
    pub status: bool,
    pub meters: bool,
    /// Quantity and bracket: what a ladder click uses, so they go together with the ladder.
    pub entry: bool,
    /// Buy and Sell, and the Join buttons' row where it shows ([`PanelRows::joins_show`]).
    pub buttons: bool,
    /// Settings › DOM: the Join buttons are shown (absent setting: shown). They show only from
    /// [`EXITS_ONE_ROW`].
    pub joins: bool,
    /// BE and Reverse. From [`EXITS_ONE_ROW`] they sit in the exits row (no height of their own);
    /// narrower, in a row of their own that gives way after the card.
    pub actions: bool,
    pub ladder: bool,
}

impl PanelRows {
    const ALL: PanelRows = PanelRows {
        account: true,
        card: true,
        status: true,
        meters: true,
        entry: true,
        buttons: true,
        joins: true,
        actions: true,
        ladder: true,
    };

    /// Whether the Join buttons' row shows at `width`: with the buttons, the setting on, and a
    /// panel from [`NARROW_BELOW`] wide. Narrower they hide first, before anything else gives way
    /// (owner's ruling): their prices have already gone where they didn't fit ([`join_text`]).
    pub fn joins_show(&self, width: f32) -> bool {
        self.buttons && self.joins && !is_narrow(width)
    }

    /// Whether BE and Reverse take a row of their own at `width`.
    pub fn actions_row(&self, width: f32) -> bool {
        self.account && self.actions && is_tight(width)
    }

    /// The height of the rows the bracket editor takes: quantity and bracket, Buy and Sell, and the
    /// joins where they show.
    fn editor_h(&self, width: f32) -> f32 {
        CTRL_H
            + TRADE_H
            + if self.joins && !is_narrow(width) {
                JOIN_H
            } else {
                0.0
            }
    }

    /// These rows' height at `width`, the pane's header not included.
    pub fn height(&self, width: f32) -> f32 {
        if !self.account {
            return UTIL_H;
        }
        let meters = if is_narrow(width) {
            METERS_NARROW_H
        } else {
            METERS_H
        };
        let row = |shown: bool, h: f32| if shown { h } else { 0.0 };
        HDR_H
            + row(self.card, card_h(width))
            + row(self.status, STATUS_H)
            + row(self.meters, meters)
            + row(self.entry, CTRL_H)
            + row(self.buttons, TRADE_H)
            + row(self.joins_show(width), JOIN_H)
            + row(self.actions_row(width), ACT_H)
            + UTIL_H
            + PAD_Y
    }
}

fn pane_header_h() -> f32 {
    crate::pane::header_height(crate::layout::PaneKind::Dom)
}

/// Nothing measured yet. Not the same as a pane measured at no height, which a window too short
/// for its DOMs can leave: that one shows nothing, and its order keys send nothing.
fn not_laid_out(width: f32, pane_h: f32) -> bool {
    width <= 0.0 && pane_h <= 0.0
}

/// A pane's height as the rows reckon it: layout snaps to device pixels, so at 150% a pane held at
/// the minimum measures 90.67 and must still count as its 91.
fn reckoned_h(pane_h: f32) -> f32 {
    pane_h.ceil()
}

/// The rows with the bracket editor and confirmation closed. With three ladder rows kept: the order
/// buttons go first (the ladder and keys still enter). Then the ladder with quantity and bracket,
/// whole: a click on it uses them, so it never shows without them. Then the rule meters, the status
/// line, the position card (its line stays on the account row) and a tight panel's BE and Reverse
/// row. The account row and the exits remain at the DOM's minimum height (`MIN_DOM_PANE_H`), and
/// under that only the exits do.
fn closed_rows(width: f32, pane_h: f32, joins: bool) -> PanelRows {
    let mut r = PanelRows {
        joins,
        ..PanelRows::ALL
    };
    let fits = |r: &PanelRows, ladder: f32| pane_header_h() + r.height(width) + ladder <= pane_h;
    if !fits(&r, LADDER_MIN_H) {
        r.buttons = false;
    }
    if !fits(&r, LADDER_MIN_H) {
        (r.entry, r.ladder) = (false, false);
    }
    if !fits(&r, 0.0) {
        r.meters = false;
    }
    if !fits(&r, 0.0) {
        r.status = false;
    }
    if !fits(&r, 0.0) {
        r.card = false;
    }
    if !fits(&r, 0.0) {
        r.actions = false;
    }
    if !fits(&r, 0.0) {
        r.account = false;
    }
    r
}

/// The rows for a pane `pane_h` tall (nothing measured yet: all of them), with the Join buttons on
/// or off (Settings › DOM). `editing`: the bracket editor or confirmation is open and fits
/// ([`editor_fits`]): it takes the quantity, bracket and button rows' space, from the ladder where
/// those rows were gone (the one case where the panel's height follows state). Every other row is
/// as it is with it closed.
pub fn panel_rows(width: f32, pane_h: f32, editing: bool, joins: bool) -> PanelRows {
    if not_laid_out(width, pane_h) {
        return PanelRows {
            joins,
            ..PanelRows::ALL
        };
    }
    let pane_h = reckoned_h(pane_h);
    let mut r = closed_rows(width, pane_h, joins);
    if editing && editor_fits(width, pane_h, joins) {
        (r.entry, r.buttons) = (true, true);
        r.ladder = pane_header_h() + r.height(width) + LADDER_MIN_H <= pane_h;
    }
    r
}

/// Whether the bracket editor or confirmation has room: a pane too short closes it (never hidden
/// and still taking keys).
pub fn editor_fits(width: f32, pane_h: f32, joins: bool) -> bool {
    if not_laid_out(width, pane_h) {
        return true;
    }
    let pane_h = reckoned_h(pane_h);
    let mut r = closed_rows(width, pane_h, joins);
    (r.entry, r.buttons) = (true, true);
    r.account && pane_header_h() + r.height(width) <= pane_h
}

/// The bracket editor's preset rows in a slot `h` tall at `width` and zoom `z` (its head, foot
/// and padding take the rest): at least one, at most four. All in 100 % units, with the row that
/// holds the + − ↑ ↓ buttons (the header when tight, else the footer) a hit target tall, which
/// `zoom::hit` never shrinks: below 100 % it takes more units ([`hit_units`]).
fn editor_list_rows(h: f32, width: f32, z: f32) -> usize {
    let (head, foot) = editor_head_foot(width, z);
    (((h - head - foot - 8.0) / EDITOR_ROW_H).floor() as usize).clamp(1, 4)
}

/// The editor's header and footer heights in 100 % units at `width` and zoom `z`.
fn editor_head_foot(width: f32, z: f32) -> (f32, f32) {
    if is_tight(width) {
        (hit_units(EDITOR_HEAD_H, z), EDITOR_FOOT_H)
    } else {
        (EDITOR_HEAD_H, hit_units(EDITOR_FOOT_H, z))
    }
}

/// A hit target's size `v` (100 % units) at zoom `z`, in 100 % units: `zoom::hit` keeps it at
/// least `v` real pixels, so below 100 % it takes `v / z` units.
pub fn hit_units(v: f32, z: f32) -> f32 {
    if z < 1.0 { v / z } else { v }
}
/// The editor's header: as tall as the action buttons it holds in a tight panel.
const EDITOR_HEAD_H: f32 = MIN_HIT + 2.0;
const EDITOR_FOOT_H: f32 = 24.0;
const EDITOR_ROW_H: f32 = 20.0;

/// From this width, a pane without its account row names the position beside the exits.
const POSITION_BESIDE_EXITS: f32 = 400.0;

/// Whether a button of the utility row shows: BE and Reverse (`entry`) need the account row, because
/// they act on a position and an account the pane no longer names without it; in a tight panel
/// they sit in their own row instead ([`PanelRows::actions_row`]).
fn util_shows(entry: bool, rows: PanelRows, width: f32) -> bool {
    !entry || (rows.account && !is_tight(width))
}

/// Whether BE or Reverse is dimmed: both while flat (nothing to act on; a press still answers why),
/// Reverse also while entries are (it is an entry). Never Cancel all or Flatten.
pub fn exit_dims(cmd: TradeCmd, flat: bool, entries_dim: bool) -> bool {
    match cmd {
        TradeCmd::Breakeven => flat,
        TradeCmd::Reverse => flat || entries_dim,
        _ => false,
    }
}

/// Whether the bracket editor or confirmation is open: see [`panel_rows`].
pub fn panel_editing(pt: &PaneTrade) -> bool {
    pt.bracket_confirm.is_some() || pt.editor.is_some()
}

/// Close the bracket editor or confirmation in a pane too short for it, and say so. A confirmation
/// closes as its Keep does: nothing changes.
pub fn close_editor_too_short(pt: &mut PaneTrade) {
    pt.editor = None;
    pt.bracket_confirm = None;
    pt.notice("Bracket editor closed: pane too short");
}

/// The position once the card has gone, as whole items: the side and quantity (or "Flat"), the
/// average, and the P&L as the display allows with its sign.
fn position_items(
    position: Option<(i64, Option<f64>, Option<f64>, Money)>,
    mode: crate::pnl::PnlDisplay,
    product: &Product,
    narrow: bool,
) -> (String, Option<String>, Option<(String, f64)>) {
    let Some((q, avg, ticks, money)) = position else {
        return ("Flat".to_owned(), None, None);
    };
    let arrow = if q > 0 { "▲" } else { "▼" };
    let side = if narrow {
        format!("{arrow} {}", q.abs())
    } else {
        let word = if q > 0 { "LONG" } else { "SHORT" };
        format!("{arrow} {word} {}", q.abs())
    };
    let avg = avg.map(|a| format!("@ {}", avg_text(product, a)));
    let mut text = String::new();
    let pnl = if mode.money() {
        let _ = crate::pnl::write_money(&mut text, money);
        Some((text, money.0 as f64))
    } else if let Some(tk) = ticks.filter(|_| mode.ticks()) {
        let _ = crate::pnl::write_ticks(&mut text, tk);
        Some((text, tk))
    } else {
        None
    };
    (side, avg, pnl)
}

/// A short line's items in priority order, by width: which fit in `room`, whole (a number is never
/// cut, and a button is never pressed for room). One too wide is left out; later ones may still fit.
fn fit_items(room: f32, widths: &[f32]) -> Vec<bool> {
    let mut left = room;
    widths
        .iter()
        .map(|w| {
            // An item this line doesn't have takes no room.
            let fits = *w > 0.0 && *w + 8.0 <= left;
            if fits {
                left -= *w + 8.0;
            }
            fits
        })
        .collect()
}

/// Whether the Keys chip is in view while the account row shows: in the header, on the status line
/// while that shows, or on this row where it fit.
fn keys_chip_on_account_row(width: f32, status: bool, keys_here: bool) -> bool {
    keys_in_header(width) || status || keys_here
}

/// Rules exist on this account, but a short pane has no room for their meters: never reads the
/// same as an account with no rules.
const RULES_HIDDEN: &str = "rules hidden";

/// The Keys chip's room wherever it is laid out by estimate: its words, knob, gap and padding. A chip
/// counted as in view is whole, so order keys are never live behind a cut one.
const KEYS_CHIP_W: f32 = 90.0;

/// The trading switch's width with this text at `width`: the words, the gap, the track and its
/// padding.
fn switch_w(text: &str, width: f32) -> f32 {
    text_w(text, switch_px(width)) + SWITCH_GAP + TRACK_W + 2.0 * SWITCH_PX
}
const SWITCH_GAP: f32 = 4.0;
const SWITCH_PX: f32 = 2.0;

/// The switch's text size: 12 px, 11 in a tight panel (a two-digit strategy count fits beside
/// PAPER).
fn switch_px(width: f32) -> f32 {
    if is_tight(width) { 11.0 } else { 12.0 }
}

/// The PAPER / SIM / LIVE badge's room on the account row (by estimate).
const BADGE_W: f32 = 52.0;

/// The room the switch may take on the account row at `width`: past the badge, the account's
/// least room (a hit target), the Keys chip where it sits in the header, and their gaps.
fn switch_room(width: f32) -> f32 {
    let keys = if keys_in_header(width) {
        KEYS_CHIP_W + 8.0
    } else {
        0.0
    };
    width - 2.0 * SIDE - BADGE_W - 4.0 - 8.0 - MIN_HIT - keys
}

/// What's left on the account row for the position, past the badge, the least of the account's
/// name, the switch with this text, the Keys chip if it shows, and their gaps.
fn account_row_room(width: f32, switch: &str, keys: bool) -> f32 {
    let (keys_w, gaps) = if keys { (KEYS_CHIP_W, 3.0) } else { (0.0, 2.0) };
    width - 2.0 * SIDE - BADGE_W - MIN_HIT - switch_w(switch, width) - keys_w - gaps * 8.0
}

/// The panel's height at `width` with every row: a function of the width only, never of the
/// trading state (position, lockout, flashes, the editor, enabled or not), so the ladder below
/// never shifts (docs/design.md §7).
#[cfg_attr(not(test), allow(dead_code))]
pub fn panel_height(width: f32) -> f32 {
    PanelRows::ALL.height(width)
}

/// Below this width the disabled panel's line drops to its short form (the long one would be
/// cut: 50 characters at the width test's 7.5 px).
pub const LOCKED_HINT_FULL: f32 = 400.0;
/// From this width that line, with strategies armed, also says how to enable.
pub const LOCKED_HINT_ARMED_FULL: f32 = 520.0;

/// What a disabled panel says in the joins' place: the lock that binds. With nothing else
/// in the way, how to enable; with a lockout or another blocker too, that first (enabling
/// alone would not unlock entries).
///
/// With strategies armed on the account the count leads, because "entries locked" alone would
/// read as if nothing can enter: `2 strategies still armed · DOM locked`. "Still" only when
/// this switch is the only lock: a blocker (a closed market, a blackout, a lockout) may bind
/// them too, and the core decides. Laid out by [`whole_items`], so a narrow panel leaves out
/// what follows the count, whole, and never cuts the number.
pub fn locked_hint(blocker: Option<&str>, armed: usize, width: f32) -> String {
    let key = crate::commands::hint(crate::commands::Command::Trade(TradeCmd::ToggleArm));
    let at = |w: f32| width == 0.0 || width >= w;
    if armed > 0 {
        let strategies = strategies_armed_text(armed);
        return match blocker {
            Some(b) if at(LOCKED_HINT_ARMED_FULL) => {
                format!("{strategies} · Entries locked: {b}")
            }
            Some(b) => format!("{strategies} · {}", capitalized(b)),
            None => {
                let still = format!("{armed} strateg{} still armed", plural_y(armed));
                if at(LOCKED_HINT_ARMED_FULL) {
                    format!("{still} · DOM entries locked ({key})")
                } else {
                    format!("{still} · DOM locked")
                }
            }
        };
    }
    match (blocker, at(LOCKED_HINT_FULL)) {
        (Some(b), true) => format!("Entries locked: {b} · trading off"),
        (None, true) => format!("Entries locked: enable trading ({key})"),
        (Some(b), false) => format!("Locked: {b}"),
        (None, false) => format!("Locked: {key}"),
    }
}

/// First letter upper case (a blocker's words start lower case).
fn capitalized(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

/// `strategy`/`strategies`'s ending.
fn plural_y(n: usize) -> &'static str {
    if n == 1 { "y" } else { "ies" }
}

/// The utility row's labels: words, short in a narrow panel ("Cancel all" keeps its object
/// at every width). In a tight panel BE and Reverse have a row of their own, with room for the
/// word.
pub fn util_labels(width: f32) -> [&'static str; 4] {
    if is_narrow(width) && !is_tight(width) {
        ["BE", "Rev", "Cancel all", "Flatten"]
    } else {
        ["BE", "Reverse", "Cancel all", "Flatten"]
    }
}

/// Whether a read-only account's "Cancel all off" and "Flatten off" fit the exits row at `width`
/// (beside BE and Reverse where they share it). Where they don't, one cell says "Exits off": the
/// word that nothing will be sent is never cut.
pub fn off_exits_fit(width: f32) -> bool {
    if width == 0.0 {
        return true;
    }
    let size = util_text(width);
    // A cell: its words, `px_1` each side and its border.
    let cell = |s: &str| (text_w(s, size) + 2.0 * 4.0 + 2.0).max(MIN_HIT);
    let mut room = width - 2.0 * SIDE;
    if !is_tight(width) {
        let l = util_labels(width);
        room -= cell(l[0]) + cell(l[1]) + 2.0 * 4.0;
    }
    cell("Cancel all off") + 4.0 + cell("Flatten off") <= room
}

/// How the bracket editor's preset rows are laid out at `width`: the row marker, the Stop and
/// Target cells' width (`None`: they share the room), the name cell, and the text size.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EditorCells {
    pub marker: bool,
    pub value: Option<f32>,
    pub name: bool,
    pub text: f32,
}

/// The longest distance a cell holds while typed, at the wide glyph estimate: six digits
/// ([`crate::brackets::MAX_TICKS`]) or a point distance like "100.00", and the cursor.
#[cfg(test)]
const EDITOR_VALUE_SAMPLE: &str = "100000▏";

/// A tight panel drops the marker (the active cell has its accent border) and the name (it still
/// saves; wider panels edit it), and Stop and Target share the row: a distance is never cut.
pub fn editor_cells(width: f32) -> EditorCells {
    if is_tight(width) {
        EditorCells {
            marker: false,
            value: None,
            name: false,
            text: 11.0,
        }
    } else {
        EditorCells {
            marker: true,
            value: Some(64.0),
            name: true,
            text: 12.0,
        }
    }
}

/// The exits row's text size: 12 px, 11 in a tight panel (Cancel all and Flatten keep their words).
fn util_text(width: f32) -> f32 {
    if is_tight(width) { 11.0 } else { 12.0 }
}

/// The unresolved-orders notice: with its advice where there is room, short when narrow.
pub fn unresolved_label(n: usize, width: f32) -> String {
    if is_narrow(width) {
        format!("{n} unresolved")
    } else {
        format!("{n} unresolved · check the broker")
    }
}

/// The room inside one of two paired buttons or fields at `width`: half the panel less the gap,
/// its padding and border.
fn pair_inner(width: f32, pad: f32) -> f32 {
    (width - 2.0 * SIDE - PAIR_GAP) / 2.0 - 2.0 * pad - 2.0
}

/// What a Buy or Sell button says, by the room it has.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MarketText {
    /// "Buy 10" over "Market · 7572.00".
    Full(String, String),
    /// "Buy 10": the price line is the first to go (owner's ruling: the size is never dropped).
    Size(String),
    /// "Buy" over "10": a size too long for one line keeps it whole, on the second.
    Stacked(&'static str, String),
}

/// The Buy and Sell buttons' padding each side, and their first line's text size.
fn market_px(width: f32) -> (f32, f32) {
    if is_tight(width) {
        (BUTTON_TIGHT_PX, 12.0)
    } else {
        (BUTTON_PX, 13.0)
    }
}

/// A Buy or Sell button's text at `width` for `size` and the price it would hit (`None`: no
/// market, shown as "—"). Both buttons decide by Sell's longer word, so they always match.
/// `locked` (entries locked): the second line says "Locked" instead of the price, wherever it
/// fits, and the room allows for the dimmed button's 2 px edge.
pub fn market_text(
    buy: bool,
    size: i64,
    price: Option<&str>,
    width: f32,
    locked: bool,
) -> MarketText {
    let word = if buy { "Buy" } else { "Sell" };
    let title = format!("{word} {size}");
    let (pad, title_px) = market_px(width);
    let inner = if width == 0.0 {
        f32::MAX
    } else {
        // A dimmed button's edge is 2 px, not 1: a pixel less each side.
        pair_inner(width, pad) - if locked { 2.0 } else { 0.0 }
    };
    if text_w(&format!("Sell {size}"), title_px) > inner {
        return MarketText::Stacked(word, size.to_string());
    }
    let line = if locked {
        "Locked".to_owned()
    } else {
        format!("Market · {}", price.unwrap_or("—"))
    };
    if text_w(&line, 11.0) <= inner {
        MarketText::Full(title, line)
    } else {
        MarketText::Size(title)
    }
}

/// What a Join button says, by the room it has: the words and the price, then the words alone,
/// then just "Bid" / "Ask" (the joins show only from [`NARROW_BELOW`]).
pub fn join_text(bid: bool, price: Option<&str>, width: f32) -> (&'static str, Option<String>) {
    let (long, short) = if bid {
        ("Join bid", "Bid")
    } else {
        ("Join ask", "Ask")
    };
    let inner = if width == 0.0 {
        f32::MAX
    } else {
        pair_inner(width, BUTTON_PX)
    };
    let price = price.unwrap_or("—");
    if word_w(long) + FIELD_GAP + word_w(price) <= inner {
        (long, Some(price.to_owned()))
    } else if word_w(long) <= inner {
        (long, None)
    } else {
        (short, None)
    }
}

/// What the Qty and Bracket fields show besides their values, which always show: the Qty field's
/// "Qty" (or "max N", which always shows: a size the account refuses is said before the click),
/// the Bracket field's "Bracket", and position mode's word (long or short; always shown while on).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FieldFit {
    pub qty_key: bool,
    pub bracket_key: bool,
    pub long_mode: bool,
}

/// The fields' parts that fit at `width`, given their texts: the Bracket field's "Bracket" goes
/// first, then position mode's long word, then the Qty field's "Qty".
pub fn field_fit(
    width: f32,
    qty_key: &str,
    qty: &str,
    bracket: &str,
    mode: Option<(&str, &str)>,
) -> FieldFit {
    let (pad, gap) = if is_tight(width) {
        (FIELD_TIGHT_PX, FIELD_TIGHT_GAP)
    } else {
        (FIELD_PX, FIELD_GAP)
    };
    let room = if width == 0.0 {
        f32::MAX
    } else {
        width - 2.0 * SIDE - PAIR_GAP
    };
    let need = |f: FieldFit| fields_w(pad, gap, f, qty_key, qty, bracket, mode);
    let mut f = FieldFit {
        qty_key: true,
        bracket_key: true,
        long_mode: true,
    };
    for step in 0..3 {
        if need(f) <= room {
            break;
        }
        match step {
            0 => f.bracket_key = false,
            1 => f.long_mode = false,
            _ => f.qty_key = false,
        }
    }
    f
}

/// The two fields' width with these parts shown (by estimate), their padding and borders included.
fn fields_w(
    pad: f32,
    gap: f32,
    f: FieldFit,
    qty_key: &str,
    qty: &str,
    bracket: &str,
    mode: Option<(&str, &str)>,
) -> f32 {
    let part = |s: &str, size: f32| text_w(s, size) + gap;
    let q = 2.0 * pad
        + 2.0
        + if f.qty_key || qty_key != "Qty" {
            part(qty_key, 11.0)
        } else {
            0.0
        }
        + part(qty, 12.0)
        + CHEVRON_W;
    let b = 2.0 * pad
        + 2.0
        + if f.bracket_key {
            part("Bracket", 11.0)
        } else {
            0.0
        }
        + part(bracket, 12.0)
        + mode.map_or(0.0, |(long, short)| {
            part(if f.long_mode { long } else { short }, 11.0)
        })
        + CHEVRON_W;
    q + b
}

/// The position card's large text size: 15 px where the side and the P&L fit beside each other,
/// else 13, else 12, else 11. Neither is cut up to a six-figure P&L at the narrowest DOM: the
/// average gives way first ([`card_line_fit`]).
pub fn card_big_px(width: f32, side: &str, pnl: Option<&str>) -> f32 {
    let room = card_inner(width);
    // 11 px last (the DOM's small text): a six-figure P&L beside a three-digit position, with its
    // thousands separators, still fits the narrowest DOM whole.
    [15.0, 13.0, 12.0]
        .into_iter()
        .find(|&s| text_w(side, s) + 8.0 + pnl.map_or(0.0, |p| text_w(p, s)) <= room)
        .unwrap_or(11.0)
}

/// The room inside the position card at `width`.
fn card_inner(width: f32) -> f32 {
    if width == 0.0 {
        f32::MAX
    } else {
        width - 2.0 * SIDE - 2.0 * 8.0
    }
}

/// Whether the average fits on the card's first line beside the side and the P&L at `big` px.
pub fn card_avg_fits(width: f32, side: &str, avg: &str, pnl: Option<&str>, big: f32) -> bool {
    text_w(side, big) + 6.0 + word_w(avg) + 8.0 + pnl.map_or(0.0, |p| text_w(p, big))
        <= card_inner(width)
}

/// Where a flat card shows the day's P&L: on its first line beside "Flat" where it fits (the mock),
/// else on the last line's right, as in a position.
pub fn flat_day_on_first_line(width: f32, day: &str) -> bool {
    text_w("Flat", 13.0) + 8.0 + text_w("Day ", 12.0) + text_w(day, 13.0) <= card_inner(width)
}

/// The trading switch's words below [`EXITS_ONE_ROW`]: none (its track shows the state by the
/// knob's place and fill, never color alone, and its tooltip names it), "!" when orders would be
/// denied, or with strategies armed their count, so the account's name keeps its room.
pub fn switch_text_tight(enabled: bool, blocker: Option<&str>, armed: usize) -> String {
    if armed > 0 {
        return format!("{armed} strat");
    }
    match (enabled, blocker) {
        (true, Some(_)) => "!".to_owned(),
        _ => String::new(),
    }
}

/// The switch's words at `width`: [`switch_text_tight`] below [`EXITS_ONE_ROW`], else
/// [`switch_text_armed`], in its narrow form ("Enabled !") where a reason doesn't fit beside the
/// account ([`reason_below`] then puts it on the status line).
pub fn switch_words(enabled: bool, blocker: Option<&str>, armed: usize, width: f32) -> String {
    if is_tight(width) {
        return switch_text_tight(enabled, blocker, armed);
    }
    let text = switch_text_armed(enabled, blocker, armed, width);
    if armed == 0 && width > 0.0 && switch_w(&text, width) > switch_room(width) {
        switch_text(enabled, blocker, true)
    } else {
        text
    }
}

/// Whether why orders would be denied shows on the status line, not the switch: a narrow panel,
/// a switch showing a strategy count, or one with no room for the reason ([`switch_words`]).
pub fn reason_below(enabled: bool, blocker: Option<&str>, armed: usize, width: f32) -> bool {
    reason_on_status(is_narrow(width), armed)
        || switch_words(enabled, blocker, armed, width)
            != switch_text_armed(enabled, blocker, armed, width)
}

// ---- elements ---------------------------------------------------------------------------
//
// The panel's design language (docs/design.md, Trade panel): fields are quiet boxes on `panel`
// with an `axis_line` edge that name what they hold and show its value, and open a list under
// them; primary actions (Buy, Sell) are solid in the side's color; secondary ones (the joins) are
// outlined in it; exits sit under a divider, Flatten outlined in the warning color and bold.

fn cell(id: impl Into<gpui::ElementId>, t: &Theme) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .min_w(crate::zoom::hit(MIN_HIT))
        .h(crate::zoom::hit(MIN_HIT + 2.0))
        .px_1()
        .rounded_sm()
        .border_1()
        .border_color(t.axis_line)
        .text_color(t.text_main)
        .cursor_pointer()
}

fn run_on_click<V: TradePane>(
    el: gpui::Stateful<gpui::Div>,
    cmd: TradeCmd,
    cx: &mut Context<V>,
) -> gpui::Stateful<gpui::Div> {
    el.on_mouse_down(
        MouseButton::Left,
        cx.listener(move |v: &mut V, ev: &gpui::MouseDownEvent, _, cx| {
            cx.stop_propagation();
            // A popover open anywhere (another window's): this press only closes it.
            if crate::popover::press_closes_popover(cx) {
                cx.notify();
                return;
            }
            // Aimed at what has moved (the bracket editor closing, a list over this row).
            let exit = matches!(cmd, TradeCmd::Flatten | TradeCmd::CancelAll);
            let refused = if exit {
                crate::popover::second_press_exit(ev.click_count, ev.position)
            } else {
                crate::popover::second_press(ev.click_count, ev.position)
            };
            if refused {
                if exit {
                    // An exit is never refused in silence: the next press acts.
                    v.trade()
                        .notice("Press again: that press followed a change here");
                    cx.notify();
                }
                return;
            }
            if exit {
                v.trade().clear_notice();
            }
            run_from(v, cmd, Instant::now(), Source::Dom, cx);
        }),
    )
}

fn tip(
    el: gpui::Stateful<gpui::Div>,
    name: &str,
    cmd: Option<TradeCmd>,
    t: &Theme,
) -> gpui::Stateful<gpui::Div> {
    crate::tooltip::tip_div(el, name, cmd.map(crate::commands::Command::Trade), t)
}

/// The trading switch's track: filled in the warning color with its knob at the right when on, an
/// outline with its knob at the left when off (the knob's place is the state's shape, never the
/// color alone; the switch's words say it too where there is room).
fn track(on: bool, t: &Theme) -> gpui::Div {
    let knob = div()
        .absolute()
        .top(crate::zoom::ui(2.0))
        .size(crate::zoom::ui(TRACK_H - 6.0))
        .rounded_full();
    let base = div()
        .relative()
        .flex_shrink_0()
        .w(crate::zoom::ui(TRACK_W))
        .h(crate::zoom::ui(TRACK_H))
        .rounded_full()
        .border_1();
    if on {
        base.bg(t.warn)
            .border_color(t.warn)
            .child(knob.right(crate::zoom::ui(2.0)).bg(t.on_warn))
    } else {
        base.border_color(t.control_edge)
            .child(knob.left(crate::zoom::ui(2.0)).bg(t.text_dim))
    }
}

/// A field: what it holds and its value, with ▾ (it opens a list under it). `open`: its list shows.
fn field(id: &'static str, open: bool, tight: bool, t: &Theme) -> gpui::Stateful<gpui::Div> {
    let accent = t.accent;
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .gap(crate::zoom::ui(if tight {
            FIELD_TIGHT_GAP
        } else {
            FIELD_GAP
        }))
        .h(tall(FIELD_H))
        .px(crate::zoom::ui(if tight {
            FIELD_TIGHT_PX
        } else {
            FIELD_PX
        }))
        .min_w_0()
        .overflow_hidden()
        .whitespace_nowrap()
        .rounded_md()
        .bg(t.panel)
        .border_1()
        .border_color(if open { t.accent } else { t.axis_line })
        .cursor_pointer()
        .hover(move |s| s.border_color(accent))
}

/// A field's name ("Qty", "Bracket"): small and quiet.
fn field_key(text: impl Into<gpui::SharedString>, t: &Theme) -> gpui::Div {
    div()
        .flex_shrink_0()
        .text_size(crate::zoom::ui(11.0))
        .text_color(t.text_dim)
        .child(text.into())
}

fn chevron(t: &Theme) -> gpui::Div {
    div()
        .flex_shrink_0()
        .ml_auto()
        .text_size(crate::zoom::ui(11.0))
        .text_color(t.text_dim)
        .child("▾")
}

/// The panel's two lists (Qty, Bracket) open one at a time, like the account list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelMenu {
    Qty,
    Bracket,
}

/// What a row of a panel list shows on its left: a check for the one in force, a box for a switch
/// (checked when on), or nothing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mark {
    None,
    Check,
    Box(bool),
}

/// A panel list: under its field, in the menus' look (`crate::menu`).
fn menu_list(id: &'static str, t: &Theme) -> gpui::Stateful<gpui::Div> {
    // The wheel goes on to the dropdown's scroll (a list held to the room scrolls).
    div()
        .id(id)
        .block_mouse_except_scroll()
        .min_w(crate::zoom::ui(150.0))
        .flex()
        .flex_col()
        .py_1()
        .bg(t.panel)
        .border_1()
        .border_color(t.axis_line)
        .rounded_md()
        .whitespace_nowrap()
        .text_size(crate::zoom::ui(12.0))
}

fn menu_sep(t: &Theme) -> gpui::Div {
    div().my_1().h(gpui::px(1.0)).flex_none().bg(t.axis_line)
}

/// One row of a panel list; a press closes the list and runs `act`.
fn menu_row<V: TradePane>(
    id: (&'static str, u64),
    mark: Mark,
    label: String,
    key: Option<TradeCmd>,
    t: &Theme,
    cx: &mut Context<V>,
    act: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let (accent, lit) = (t.accent, t.ghost_selected);
    let mark = match mark {
        Mark::Box(on) => div()
            .size(crate::zoom::ui(11.0))
            .flex()
            .items_center()
            .justify_center()
            .border_1()
            .border_color(t.control_edge)
            .rounded_sm()
            .text_size(crate::zoom::ui(10.0))
            .text_color(t.text_main)
            .child(if on { "✓" } else { "" }),
        Mark::Check => div().text_color(t.text_main).child("✓"),
        Mark::None => div(),
    };
    div()
        .id(id)
        .h(tall(22.0))
        .flex_none()
        .px_2()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .border_l_2()
        .border_color(gpui::transparent_black())
        .cursor_pointer()
        .hover(move |s| s.bg(lit).border_color(accent))
        .child(div().w(crate::zoom::ui(12.0)).flex_none().child(mark))
        // A row with a key may cut its label before the caps (a narrow window); a row without
        // one (a bracket preset: money-relevant) is named in full.
        .child(
            div()
                .flex_1()
                .when(key.is_some(), |d| d.min_w_0().truncate())
                .text_color(t.text_main)
                .child(label),
        )
        .children(key.and_then(|k| crate::keycap::caps(crate::commands::Command::Trade(k), t)))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |v: &mut V, _: &gpui::MouseDownEvent, window, cx| {
                cx.stop_propagation();
                close_account_menu(v, cx);
                act(v, window, cx);
                cx.notify();
            }),
        )
}

/// Open or close one of the panel's lists (the Qty or Bracket field).
pub fn toggle_panel_menu<V: TradePane>(v: &mut V, which: PanelMenu, cx: &mut Context<V>) {
    if v.trade_ref().panel_menu == Some(which) {
        return close_account_menu(v, cx);
    }
    let t = v.trade();
    t.account_menu = false;
    t.panel_menu = Some(which);
    // One popover kind for the panel's lists: the account's, Qty and Bracket open one at a time.
    crate::popover::opened(
        cx,
        crate::popover::Kind::Account,
        |v: &mut V, cx| close_account_menu(v, cx),
        |v: &V| v.trade_ref().menu_open(),
    );
    cx.notify();
}

/// The DOM's trading panel (trade panel v2, mock 2): PAPER, the account and the trading switch;
/// the position card; the lockout or refusal line; rule meters; the Qty and Bracket fields; Buy
/// and Sell; Join bid and Join ask; the exits under a divider. Its height depends only on the
/// pane's size and the Join buttons' setting ([`panel_rows`]). Nothing when trading isn't running.
pub fn dom_bar<V: TradePane>(
    v: &V,
    product: &Product,
    width: f32,
    rows: PanelRows,
    t: &Theme,
    cx: &mut Context<V>,
) -> Option<AnyElement> {
    Trading::try_global(cx)?;
    if bracket_needs_sync(v, cx) {
        let pane = cx.entity();
        cx.defer(move |cx| pane.update(cx, |v, cx| bracket_sync(v, cx)));
    }
    let data = gather(v, product, cx);
    let pt = v.trade_ref();
    let narrow = is_narrow(width);
    let enabled = pt.arming.is_armed();
    let mode = data.as_ref().map(|d| d.mode).unwrap_or_default();
    // A guardrail lockout dims entries too (the core still decides; a click is denied with
    // the reason). Flatten and Cancel all never dim.
    let guarded = data
        .as_ref()
        .is_some_and(|d| d.guard.is_some() || d.news_block.is_some());
    let entries_dim = !enabled || guarded;
    let blocked = blocker(v, product, data.as_ref(), cx);
    let blocker = blocked.as_deref();
    let strategies_armed = data.as_ref().map_or(0, |d| d.strategies_armed);
    let reason_on_line = reason_below(enabled, blocker, strategies_armed, width);
    let flat = data
        .as_ref()
        .is_some_and(|d| d.position.is_none_or(|p| p.0 == 0));

    let mut panel = div()
        .relative()
        .flex()
        .flex_col()
        .flex_shrink_0()
        .h(crate::zoom::ui(rows.height(width)))
        .overflow_hidden()
        .px(crate::zoom::ui(SIDE))
        .when(rows.account, |d| d.pb(crate::zoom::ui(PAD_Y)))
        .bg(t.bg)
        .border_b_1()
        .border_color(t.axis_line)
        .text_size(crate::zoom::ui(12.0));

    // 1. PAPER, the account ▾, and the trading switch on the right (words and the track's knob,
    //    never color alone).
    let switch_words = switch_words(enabled, blocker, strategies_armed, width);
    let switch = {
        // Never cut: its words are the form that fits (`switch_words`), and the account chip gives
        // way to them.
        let words = (!switch_words.is_empty()).then(|| {
            div()
                .flex_shrink_0()
                .text_size(crate::zoom::ui(switch_px(width)))
                .child(switch_words.clone())
        });
        let s = div()
            .id("trading-switch")
            .flex()
            .flex_row()
            .flex_shrink_0()
            .items_center()
            .gap(crate::zoom::ui(SWITCH_GAP))
            .h(crate::zoom::hit(MIN_HIT + 2.0))
            .px(crate::zoom::ui(SWITCH_PX))
            .rounded_sm()
            .cursor_pointer()
            .whitespace_nowrap()
            // Disabled reads quiet, unless strategies are armed: that part mustn't be missed.
            .text_color(if enabled || strategies_armed > 0 {
                t.text_main
            } else {
                t.text_dim
            })
            .when(enabled, |d| d.font_weight(FontWeight::BOLD))
            .children(words)
            .child(track(enabled, t));
        let verb = if enabled {
            "Disable trading"
        } else {
            "Enable trading"
        };
        let name = match (strategies_armed, blocker) {
            (0, None) => verb.to_owned(),
            (0, Some(b)) => format!("{verb}. Orders are denied: {b}."),
            (n, why) => {
                let it = if n == 1 { "it" } else { "them" };
                let why = why.map_or_else(String::new, |b| format!(" Orders are denied: {b}."));
                format!(
                    "{verb}. {}: this switch doesn't stop {it} (Stop all does).{why}",
                    strategies_armed_text(n)
                )
            }
        };
        run_on_click(
            tip(s, &name, Some(TradeCmd::ToggleArm), t),
            TradeCmd::ToggleArm,
            cx,
        )
    };
    if !rows.account {
        // A window too short for its DOMs: the chip is out of sight, so order keys send nothing.
        pt.keys_shown.set(false);
    }
    if rows.account {
        // Keys on with the status line gone: here, unless it would cut the switch or the account.
        let keys_here = !keys_in_header(width)
            && pt.hotkeys
            && !rows.status
            && account_row_room(width, &switch_words, true) >= 0.0;
        pt.keys_shown
            .set(keys_chip_on_account_row(width, rows.status, keys_here));
        let rules_hidden = !rows.meters && data.as_ref().is_some_and(|d| !d.rules.is_empty());
        panel = panel.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .h(crate::zoom::ui(HDR_H))
                .child(account_chip(v, data.as_ref(), t, cx))
                .when(!rows.card || rules_hidden, |row| {
                    row.child({
                        let mut gap = div().flex_shrink_0().flex().items_center().gap_2();
                        if let Some(d) = data.as_ref() {
                            // With the card showing, only the rules' token belongs here.
                            let (side, avg, pnl) = if rows.card {
                                (String::new(), None, None)
                            } else {
                                position_items(d.position, mode, product, narrow)
                            };
                            // Beside the account (which gives way) and the switch.
                            let room = account_row_room(
                                width,
                                &switch_words,
                                keys_in_header(width) || keys_here,
                            );
                            let fit = fit_items(
                                room,
                                &[
                                    word_w(&side),
                                    pnl.as_ref().map_or(0.0, |(s, _)| word_w(s)),
                                    if rules_hidden {
                                        word_w(RULES_HIDDEN)
                                    } else {
                                        0.0
                                    },
                                    avg.as_deref().map_or(0.0, word_w),
                                ],
                            );
                            gap = gap
                                .when(fit[0], |d| {
                                    d.child(
                                        div()
                                            .flex_shrink_0()
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(t.text_main)
                                            .child(side),
                                    )
                                })
                                .when_some(avg.filter(|_| fit[3]), |d, a| {
                                    d.child(div().flex_shrink_0().text_color(t.text_dim).child(a))
                                })
                                .when_some(pnl.filter(|_| fit[1]), |d, (s, v)| {
                                    d.child(
                                        div()
                                            .flex_shrink_0()
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(if v > 0.0 {
                                                t.up
                                            } else if v < 0.0 {
                                                t.down
                                            } else {
                                                t.text_main
                                            })
                                            .child(s),
                                    )
                                })
                                .when(fit[2], |d| {
                                    d.child(
                                        div()
                                            .flex_shrink_0()
                                            .text_color(t.text_main)
                                            .child(RULES_HIDDEN),
                                    )
                                });
                        }
                        gap
                    })
                })
                // On the right. Narrower than KEYS_IN_HEADER the header has no room beside the
                // account and the switch; the Keys chip shows on the status line while keys are on
                // (off is the default), and here once that line has gone.
                .child(
                    div()
                        .ml_auto()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .flex_shrink_0()
                        .when(keys_in_header(width), |d| {
                            d.child(keys_chip(pt.hotkeys, false, t, cx))
                        })
                        .when(keys_here, |d| d.child(keys_chip(true, narrow, t, cx)))
                        .child(switch),
                ),
        );
    }

    // 2. The position card.
    if rows.card {
        panel = panel.child(
            div()
                .h(crate::zoom::ui(card_h(width)))
                .flex_shrink_0()
                .child(position_card(data.as_ref(), product, width, t)),
        );
    }

    // 3. One status line: a refusal (brief) or the lockout / account state (lasting).
    let status = div()
        .h(crate::zoom::ui(STATUS_H))
        .flex()
        .flex_row()
        .items_center();
    let locked = read_only(v, cx);
    let flashing = data
        .as_ref()
        .is_some_and(|d| flash_line(&d.flashes).is_some());
    // Where the joins don't show, a disabled panel's line (what locks entries) comes here when
    // nothing else needs the line.
    let lock_line = (!enabled && rows.buttons && !rows.joins_show(width) && !panel_editing(pt))
        .then(|| locked_hint(blocker, strategies_armed, width));
    let status = match data.as_ref() {
        _ if locked.is_some() && !flashing => status.child(lock_strip(
            &format!("Read-only · {}", locked.as_deref().unwrap_or_default()),
            "",
            narrow,
            t,
        )),
        Some(d) => match (flash_line(&d.flashes), &d.guard, &d.blocked) {
            (Some((label, f)), ..) => status.gap_2().child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .min_w_0()
                    .child(
                        div()
                            .px_1()
                            .flex_shrink_0()
                            .bg(t.warn)
                            .text_color(t.on_warn)
                            .font_weight(FontWeight::BOLD)
                            .child(label),
                    )
                    .child(whole_items(&f, t.text_main, false)),
            ),
            (None, Some((_, g)), _) => status.child(lock_strip(
                g,
                "exits, Flatten and Cancel all still work",
                narrow,
                t,
            )),
            (None, None, Some(b)) => status.child(lock_strip(
                b,
                "Flatten and Cancel all still work",
                narrow,
                t,
            )),
            // The calendar: a blackout (or a calendar that can't answer) as a strip; a
            // release coming as a quiet line, unless the panel needs the line for why
            // orders are denied.
            (None, None, None)
                if d.news.as_ref().is_some_and(|(warns, _)| {
                    *warns || !(reason_on_line && enabled && blocker.is_some())
                }) =>
            {
                let (warns, text) = d.news.as_ref().expect("checked");
                if *warns {
                    let tail = if d.news_block.is_some() {
                        "exits, Flatten and Cancel all still work"
                    } else {
                        ""
                    };
                    status.child(lock_strip(text, tail, narrow, t))
                } else {
                    // Whole words only: a countdown is never cut.
                    let line = if narrow {
                        text.clone()
                    } else {
                        format!("Scheduled release: {text}")
                    };
                    status.child(whole_items(&line, t.text_dim, false))
                }
            }
            // Narrow, or a strategy count on the switch: it shows only "!", the reason is here.
            _ => match blocker.filter(|_| reason_on_line && enabled) {
                Some(b) => status.child(lock_strip(
                    &blocked_line(b, !keys_in_header(width)),
                    "",
                    true,
                    t,
                )),
                None => match &lock_line {
                    Some(l) => status.child(whole_items(l, t.text_main, false)),
                    None => status,
                },
            },
        },
        None => status,
    };
    // Keys on while the header has no room for the chip: it trails the status line beside
    // whatever it says (a flash, a lockout, a release), never replaced by it.
    // Narrow, the chip says just "Keys" (its knob filled: on), so a flash's price still fits
    // beside it (fit test).
    let status = if !keys_in_header(width) && pt.hotkeys {
        status
            .overflow_hidden()
            .gap_2()
            .child(keys_chip(true, narrow, t, cx).ml_auto())
    } else {
        status
    };
    if rows.status {
        panel = panel.child(status);
    }

    // 4. Rule meters (narrow: one line behind ▸; open, they lie over what follows).
    if rows.meters {
        panel = panel.child(meters(v, data.as_ref(), mode, narrow, t, cx));
    }

    // 5.–7. Qty and Bracket, Buy and Sell, the joins; or the bracket editor in their place.
    if panel_editing(pt) {
        if rows.entry && rows.buttons {
            let h = rows.editor_h(width);
            if let Some(c) = &pt.bracket_confirm {
                panel = panel.child(bracket_confirm_view(c, v, product, h, width, t, cx));
            } else if let Some(ed) = &pt.editor {
                panel = panel.child(editor_view(
                    ed,
                    pt.editor_msg.as_deref(),
                    v,
                    h,
                    width,
                    t,
                    cx,
                ));
            }
        }
    } else {
        if rows.entry {
            panel = panel.child(controls(v, product, width, t, cx));
        }
        if rows.buttons {
            panel = panel.child(market_buttons(v, product, width, entries_dim, t, cx));
            if rows.joins_show(width) {
                panel = panel.child(if enabled {
                    join_buttons(v, product, width, t, cx)
                } else {
                    // Disabled: one line says what locks entries, in the joins' place.
                    div()
                        .h(crate::zoom::ui(JOIN_H))
                        .pb(crate::zoom::ui(JOIN_H - JOIN_BUTTON_H))
                        .flex()
                        .items_center()
                        .justify_center()
                        .min_w_0()
                        .overflow_hidden()
                        // Whole items, the count first: what doesn't fit is left out, never cut.
                        .child(whole_items(
                            &locked_hint(blocker, strategies_armed, width),
                            t.text_main,
                            false,
                        ))
                        .into_any_element()
                });
            }
        }
    }

    // 8. Exits: BE and Reverse (dim while flat; Reverse also while entries are; a tight panel
    //    gives them a row of their own), then under a divider Cancel all and Flatten (never dim;
    //    Flatten outlined in the warning color, bold).
    let labels = util_labels(width);
    let size = util_text(width);
    let util = [
        (
            TradeCmd::Breakeven,
            "Move the stop to breakeven",
            labels[0],
            true,
        ),
        (TradeCmd::Reverse, "Reverse the position", labels[1], true),
        (TradeCmd::CancelAll, "Cancel all orders", labels[2], false),
        (TradeCmd::Flatten, "Flatten", labels[3], false),
    ];
    let button = |cmd: TradeCmd,
                  name: &str,
                  label: &'static str,
                  entry: bool,
                  grow: bool,
                  cx: &mut Context<V>|
     -> AnyElement {
        // On a read-only account Cancel all and Flatten read "off" and refuse with the reason.
        let off = !entry && locked.is_some();
        let label: std::borrow::Cow<'static, str> = if off {
            format!("{label} off").into()
        } else {
            label.into()
        };
        let name = if off {
            "Read-only: Lines can't act on this account"
        } else {
            name
        };
        let id: u64 = match cmd {
            TradeCmd::Breakeven => 0,
            TradeCmd::Reverse => 1,
            TradeCmd::CancelAll => 2,
            _ => 3,
        };
        let mut b = cell(("util", id), t)
            .text_size(crate::zoom::ui(size))
            .whitespace_nowrap()
            .child(label);
        if off {
            b = b.text_color(t.text_dim);
        }
        if grow {
            b = b.flex_1();
        }
        if cmd == TradeCmd::Flatten && !off {
            b = b
                .border_color(t.warn)
                .border_2()
                .font_weight(FontWeight::BOLD);
        }
        if exit_dims(cmd, flat, entries_dim) {
            // Dimmed as Buy and Sell are: a 2 px dashed edge and dim text (readable, not faded).
            b = b
                .border_2()
                .border_dashed()
                .border_color(t.control_edge)
                .text_color(t.text_dim);
        }
        let b = run_on_click(tip(b, name, Some(cmd), t), cmd, cx);
        b.into_any_element()
    };
    if rows.actions_row(width) {
        let mut row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .h(crate::zoom::ui(ACT_H));
        for (cmd, name, label, _) in util.iter().take(2) {
            row = row.child(button(*cmd, name, label, true, true, cx));
        }
        panel = panel.child(row);
    }
    let mut row = div()
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .h(crate::zoom::ui(UTIL_H))
        .when(rows.account, |d| d.border_t_1().border_color(t.axis_line));
    if !rows.account {
        // Paper or live is always in view (docs/design.md §3): the account row's badge moves here.
        let waiting = pt.account.is_some();
        let word = data
            .as_ref()
            .map_or(if waiting { "LIVE" } else { "PAPER" }, |d| d.badge);
        row = row.child(account_badge(word, t).flex_shrink_0());
        // The position too, with room for it, so this pane can't read as flat. It says only what
        // it knows: with no account data there's no side to name.
        if width >= POSITION_BESIDE_EXITS
            && let Some(d) = data.as_ref()
        {
            let (side, _, _) = position_items(d.position, mode, product, false);
            row = row.child(
                div()
                    .flex_shrink_0()
                    .font_weight(FontWeight::BOLD)
                    .text_color(t.text_main)
                    .child(side),
            );
        }
    }
    // A read-only account where "Cancel all off" and "Flatten off" don't fit: one "Exits off".
    let merged_off = locked.is_some() && !off_exits_fit(width);
    for (cmd, name, label, entry) in util {
        if !util_shows(entry, rows, width) {
            continue;
        }
        if merged_off && !entry {
            if cmd == TradeCmd::Flatten {
                row = row.child(button(cmd, name, "Exits", entry, true, cx));
            }
            continue;
        }
        // BE and Reverse size to their words; Cancel all and Flatten share the rest.
        row = row.child(button(cmd, name, label, entry, !entry, cx));
    }
    panel = panel.child(row);
    Some(panel.into_any_element())
}

fn lock_strip(text: &str, after: &str, narrow: bool, t: &Theme) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .gap_2()
        .min_w_0()
        .px_1()
        .rounded_sm()
        .border_1()
        .border_color(t.warn)
        .child(whole_items(text, t.text_main, true))
        .when(!narrow, |d| {
            // The hint gives way first, so the lockout's reason keeps its room.
            d.child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_color(t.text_dim)
                    .child(after.to_owned()),
            )
        })
}

/// An exit on the position card (mock 2): "SL 7562.00" for the whole position, "SL 1 @ 7562.00"
/// when it closes only part of it (never read as the whole), never a bare "S1" (an index on one
/// pane, a quantity on another, a short position on the ladder).
pub fn card_exit(role: &str, qty: i64, position: i64, price: &str) -> String {
    if qty == position.abs() {
        format!("{role} {price}")
    } else {
        format!("{role} {qty} @ {price}")
    }
}

/// The position card (mock 2): the side by shape and size with the average, open P&L large on the
/// right; under them the stop (SL, the down color, or "No stop") and target (TP, the target
/// color), working orders and anything unresolved as whole items, the ticks under the open P&L
/// and the day's P&L on the last line's right. Flat:
/// "Flat", and the day's P&L beside it where it fits. P&L off: no amounts at all.
fn position_card(d: Option<&BarData>, product: &Product, width: f32, t: &Theme) -> gpui::Div {
    let mode = d.map(|d| d.mode).unwrap_or_default();
    let tone = |m: Money| match crate::pnl::tone(m) {
        crate::pnl::Tone::Up => t.up,
        crate::pnl::Tone::Down => t.down,
        crate::pnl::Tone::Flat => t.text_dim,
    };
    let line = |h: f32| {
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_2()
            .h(crate::zoom::ui(h))
            .min_w_0()
            .whitespace_nowrap()
    };
    let fmt = product.price;
    let card = div()
        .flex()
        .flex_col()
        .justify_center()
        .gap(crate::zoom::ui(2.0))
        .h(crate::zoom::ui(card_box_h(width)))
        .px_2()
        .rounded_md()
        .bg(t.panel);
    let day = d.and_then(|d| d.day_pnl).filter(|_| mode.money());
    let day_el = |m: Money, big: Option<f32>| {
        let mut s = String::new();
        let _ = crate::pnl::write_day(&mut s, mode, m);
        // "Day" quiet, the amount in its tone.
        let s = s.split_once(' ').map_or(s.clone(), |(_, a)| a.to_owned());
        div()
            .flex()
            .flex_row()
            .flex_shrink_0()
            .items_baseline()
            .gap_1()
            .child(div().text_color(t.text_dim).child("Day"))
            .child(
                div()
                    .when_some(big, |e, px| e.text_size(crate::zoom::ui(px)))
                    .font_weight(FontWeight::BOLD)
                    .text_color(tone(m))
                    // A number: the number font (`crate::fonts`), every digit one width.
                    .font_family(crate::fonts::MONO)
                    .child(s),
            )
    };
    let position = d.and_then(|d| d.position).filter(|p| p.0 != 0);
    let mut day_below = position.is_some();
    // The average where it doesn't fit beside the side and the P&L: first on the lines below.
    let mut avg_below: Option<String> = None;
    let first = match position {
        Some((q, avg, ticks, money)) => {
            let long = q > 0;
            // Direction by shape as well as color: ▲ / ▼, and the size.
            let side = format!("{} {}", if long { "▲" } else { "▼" }, q.abs());
            // The loudest number: open P&L in currency, else ticks; nothing when off.
            let big = if mode.money() {
                let mut s = String::new();
                let _ = crate::pnl::write_money(&mut s, money);
                Some((s, tone(money)))
            } else if mode.ticks() {
                ticks.map(|tk| {
                    let mut s = String::new();
                    let _ = crate::pnl::write_ticks(&mut s, tk);
                    let c = if tk > 0.0 {
                        t.up
                    } else if tk < 0.0 {
                        t.down
                    } else {
                        t.text_main
                    };
                    (s, c)
                })
            } else {
                None
            };
            let pnl_text = big.as_ref().map(|(s, _)| s.as_str());
            let px = card_big_px(width, &side, pnl_text);
            let avg = avg.map(|a| format!("@ {}", avg_text(product, a)));
            let avg = match avg {
                Some(a) if card_avg_fits(width, &side, &a, pnl_text, px) => Some(a),
                a => {
                    avg_below = a;
                    None
                }
            };
            line(22.0)
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_baseline()
                        .gap(crate::zoom::ui(6.0))
                        .min_w_0()
                        .child(
                            div()
                                .flex_shrink_0()
                                .text_size(crate::zoom::ui(px))
                                .font_weight(FontWeight::BOLD)
                                .text_color(if long { t.up } else { t.down })
                                .child(side),
                        )
                        .children(avg.map(|a| div().text_color(t.text_dim).child(a))),
                )
                .children(big.map(|(s, c)| {
                    // Open P&L: a number in the number font; with a unit ("ticks") it is words
                    // (`text::is_words`).
                    let family = if crate::text::is_words(&s) {
                        crate::fonts::ui_family()
                    } else {
                        crate::fonts::MONO.into()
                    };
                    div()
                        .flex_shrink_0()
                        .text_size(crate::zoom::ui(px))
                        .font_weight(FontWeight::BOLD)
                        .text_color(c)
                        .font_family(family)
                        .child(s)
                }))
        }
        None => {
            let mut text = String::new();
            let here = day.filter(|m| {
                text.clear();
                let _ = crate::pnl::write_money(&mut text, *m);
                flat_day_on_first_line(width, &text)
            });
            day_below = day.is_some() && here.is_none();
            line(22.0)
                .child(
                    div()
                        .text_size(crate::zoom::ui(13.0))
                        .font_weight(FontWeight::BOLD)
                        .text_color(t.text_dim)
                        .child("Flat"),
                )
                .children(here.map(|m| day_el(m, Some(13.0))))
        }
    };
    // The lines below, as whole items in order (what doesn't fit is left out whole, never cut,
    // lowest priority first; [`card_place`]): a market order's remainder, the stop, the target,
    // the average when it isn't above, working orders (never left out).
    let pos_qty = position.map_or(0, |p| p.0);
    let mut items: Vec<(String, gpui::Hsla, bool, u8)> = Vec::new();
    if let Some(m) = d.and_then(|d| d.market_left) {
        items.push((market_words(m), t.text_main, false, 4));
    }
    if position.is_some() {
        match d.and_then(|d| d.stop) {
            Some((p, q)) => items.push((
                card_exit("SL", q, pos_qty, fmt.format::<32>(p).as_str()),
                t.down,
                true,
                5,
            )),
            // Unprotected is a state, said in words, not a blank line.
            None => items.push(("No stop".to_owned(), t.text_dim, false, 5)),
        }
        if let Some((p, q)) = d.and_then(|d| d.target) {
            items.push((
                card_exit("TP", q, pos_qty, fmt.format::<32>(p).as_str()),
                t.order_target,
                true,
                5,
            ));
        }
    }
    if let Some(a) = avg_below {
        items.push((a, t.text_dim, false, 3));
    }
    let working = d.map_or(0, |d| d.working);
    items.push((
        match working {
            0 => "No working orders".to_owned(),
            n => format!("{n} working"),
        },
        t.text_dim,
        false,
        CARD_KEEP,
    ));
    // Ticks (P&L both ways) sit under the open P&L on the right, never among the orders, where
    // "SL 6006.00 0.0 t" would read as the stop's distance.
    let ticks = position
        .and_then(|p| p.2)
        .filter(|_| mode.money() && mode.ticks())
        .map(|tk| {
            let mut s = String::new();
            let _ = crate::pnl::write_ticks(&mut s, tk);
            s
        });
    let unresolved = d.map_or(0, |d| d.unresolved);
    let day_on_line = day.filter(|_| day_below);
    let day_w = day_on_line.map_or(0.0, |m| {
        let mut s = String::new();
        let _ = crate::pnl::write_money(&mut s, m);
        text_w("Day ", 11.0) + text_w(&s, 11.0) + 4.0 + 8.0
    });
    // The unresolved chip keeps its advice only where the working count still fits beside it.
    let working_w = items
        .iter()
        .find(|i| i.3 == CARD_KEEP)
        .map_or(0.0, |i| text_w(&i.0, 11.0));
    let ticks_w = ticks.as_ref().map_or(0.0, |s| text_w(s, 11.0) + 8.0);
    let chip = (unresolved > 0).then(|| {
        let long = unresolved_label(unresolved, width);
        // It shares the first line with the ticks; the working count must still fit beside it.
        if text_w(&long, 11.0) + 16.0 + 8.0 + working_w + ticks_w <= card_inner(width) {
            long
        } else {
            format!("{unresolved} unresolved")
        }
    });
    let chip_w = chip.as_ref().map_or(0.0, |c| text_w(c, 11.0) + 8.0 + 8.0);
    let inner = card_inner(width);
    let lines = card_lines(width);
    // The day's P&L on the last line; the ticks under the open P&L, on the first.
    let day_line = lines - 1;
    let mut rooms = [inner; 4];
    rooms[0] -= chip_w + ticks_w;
    rooms[day_line] -= day_w;
    let rooms = rooms.map(|r| r.max(0.0));
    let widths: Vec<(f32, u8)> = items
        .iter()
        .map(|(s, _, priced, k)| (text_w(s, 11.0) + if *priced { 4.0 } else { 0.0 }, *k))
        .collect();
    let place = card_place(&rooms[..lines], &widths);
    let lower = || {
        div()
            .flex()
            .flex_row()
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .items_center()
            .gap_x_2()
            .text_size(crate::zoom::ui(11.0))
    };
    let mut rows = [
        lower().when_some(chip, |e, c| {
            e.child(
                div()
                    .flex_shrink_0()
                    .px_1()
                    .bg(t.warn)
                    .text_color(t.on_warn)
                    .font_weight(FontWeight::BOLD)
                    .child(c),
            )
        }),
        lower(),
        lower(),
        lower(),
    ];
    for ((text, color, priced, _), at) in items.into_iter().zip(place) {
        let Some(at) = at else { continue };
        // "SL 7562.00": the role quiet, the price in its color.
        let item = if priced {
            let (role, rest) = text.split_once(' ').unwrap_or(("", text.as_str()));
            div()
                .flex()
                .flex_row()
                .flex_shrink_0()
                .gap_1()
                .child(div().text_color(t.text_dim).child(role.to_owned()))
                .child(
                    div()
                        .font_weight(FontWeight::BOLD)
                        .text_color(color)
                        .child(rest.to_owned()),
                )
        } else {
            div().flex_shrink_0().text_color(color).child(text)
        };
        let r = std::mem::replace(&mut rows[at], div());
        rows[at] = r.child(item);
    }
    let mut card = card.child(first);
    for (i, row) in rows.into_iter().enumerate().take(lines) {
        card = card.child(
            line(16.0)
                .text_size(crate::zoom::ui(11.0))
                .child(row)
                .children(
                    ticks
                        .clone()
                        .filter(|_| i == 0)
                        .map(|s| div().flex_shrink_0().text_color(t.text_dim).child(s)),
                )
                .children(
                    day_on_line
                        .filter(|_| i == day_line)
                        .map(|m| day_el(m, None)),
                ),
        );
    }
    card
}

/// The working count's priority on the card: never left out.
pub const CARD_KEEP: u8 = u8::MAX;

/// Where the card's lower items go: each item `(width, priority)`, in order, to the first line
/// with room for it (an index into `rooms`, the room on each line) or nowhere. While they don't
/// all fit, the lowest priority (the last of equals) is left out whole. [`CARD_KEEP`] is never
/// left out: where only those remain and still don't fit, they go on the first line.
pub fn card_place(rooms: &[f32], items: &[(f32, u8)]) -> Vec<Option<usize>> {
    let mut shown: Vec<bool> = vec![true; items.len()];
    loop {
        let mut out = vec![None; items.len()];
        let mut used = vec![0.0f32; rooms.len()];
        let mut fits = true;
        for (i, (w, _)) in items.iter().enumerate() {
            if !shown[i] {
                continue;
            }
            let at = (0..rooms.len()).find(|&l| {
                let need = if used[l] > 0.0 { w + 8.0 } else { *w };
                used[l] + need <= rooms[l]
            });
            match at {
                Some(l) => {
                    used[l] += if used[l] > 0.0 { w + 8.0 } else { *w };
                    out[i] = Some(l);
                }
                None => {
                    fits = false;
                    break;
                }
            }
        }
        if fits {
            return out;
        }
        let drop = items
            .iter()
            .enumerate()
            .filter(|(i, (_, k))| shown[*i] && *k != CARD_KEEP)
            .min_by_key(|(i, (_, k))| (*k, std::cmp::Reverse(*i)))
            .map(|(i, _)| i);
        match drop {
            Some(i) => shown[i] = false,
            None => {
                return shown.iter().map(|s| s.then_some(0)).collect();
            }
        }
    }
}

/// The rule meters: a thin bar per limit with `used / limit`. Narrow: one line behind ▸.
fn meters<V: TradePane>(
    v: &V,
    d: Option<&BarData>,
    mode: crate::pnl::PnlDisplay,
    narrow: bool,
    t: &Theme,
    cx: &mut Context<V>,
) -> gpui::Div {
    let rules: Vec<(&'static str, Meter)> = d
        .map(|d| {
            d.rules
                .iter()
                .map(|(l, left, limit)| (*l, meter(*left, *limit, mode.money())))
                .collect()
        })
        .unwrap_or_default();
    let bar_row = |label: &'static str, m: &Meter| {
        let fill = if m.hot || m.reached {
            t.warn
        } else {
            t.text_dim
        };
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .h(crate::zoom::ui(16.0))
            .child(
                div()
                    .w(crate::zoom::ui(64.0))
                    .flex_shrink_0()
                    .text_color(t.text_dim)
                    .child(label),
            )
            .child(
                div()
                    .flex_1()
                    .h(crate::zoom::ui(4.0))
                    .rounded_full()
                    .bg(t.axis_line)
                    .child(
                        div()
                            .h_full()
                            .rounded_full()
                            .bg(fill)
                            // P&L off or ticks: no fill (it would show an amount's share).
                            .w(gpui::relative(if mode.money() { m.share } else { 0.0 })),
                    ),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_color(if m.reached { t.text_main } else { t.text_dim })
                    .when(m.reached || (m.hot && !mode.money()), |e| {
                        e.font_weight(FontWeight::BOLD)
                    })
                    .child(m.text.clone()),
            )
    };
    let h = if narrow { METERS_NARROW_H } else { METERS_H };
    let slot = div()
        .h(crate::zoom::ui(h))
        .flex()
        .flex_col()
        .justify_center();
    if rules.is_empty() {
        return slot.child(div().text_color(t.text_dim).child(if narrow {
            "No prop rules"
        } else {
            "No prop rules on this account"
        }));
    }
    if !narrow {
        return slot.children(rules.iter().map(|(l, m)| bar_row(l, m)));
    }
    // Narrow: a one-line summary that opens the meters over what follows.
    let open = v.trade_ref().meters_open;
    let worst = rules
        .iter()
        .map(|(_, m)| m)
        .max_by(|a, b| a.share.total_cmp(&b.share));
    let summary = match worst {
        Some(m) if m.reached => "limit reached".to_owned(),
        Some(m) if m.hot => "near a limit".to_owned(),
        _ => format!("{} limits ok", rules.len()),
    };
    let toggle = div()
        .id("meters-toggle")
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .h(crate::zoom::hit(MIN_HIT))
        .cursor_pointer()
        .text_color(t.text_dim)
        .child(if open { "▾" } else { "▸" })
        .child(summary)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|v: &mut V, _, _, cx| {
                cx.stop_propagation();
                let o = v.trade_ref().meters_open;
                v.trade().meters_open = !o;
                cx.notify();
            }),
        );
    let toggle = crate::tooltip::tip_div(
        toggle,
        if open { "Hide limits" } else { "Show limits" },
        None,
        t,
    );
    let mut slot = slot.relative().child(toggle);
    if open {
        slot = slot.child(
            div()
                .absolute()
                .top(crate::zoom::ui(h))
                .left_0()
                .right_0()
                .px_1()
                .py_1()
                .bg(t.panel)
                .border_1()
                .border_color(t.axis_line)
                .rounded_sm()
                .children(rules.iter().map(|(l, m)| bar_row(l, m))),
        );
    }
    slot
}

/// The account's presets for the pane's product and the display unit.
fn presets_and_units<V: TradePane>(
    v: &V,
    product: &Product,
    cx: &App,
) -> (
    Vec<crate::brackets::NamedBracket>,
    crate::brackets::BracketUnits,
) {
    let s = &crate::picker_view::settings(cx).bracket_presets;
    let account = v.trade_ref().account.as_ref().map_or("", |a| a.0.as_str());
    (
        crate::brackets::presets_for(s, account, product.symbol),
        crate::brackets::units_for(s, account),
    )
}

/// A bracket preset as the Bracket field and its list name it: its name, else its distances.
fn preset_text(
    p: &crate::brackets::NamedBracket,
    product: &Product,
    units: crate::brackets::BracketUnits,
) -> String {
    if p.name.is_empty() {
        crate::brackets::label(p.preset(), product, units)
    } else {
        p.name.clone()
    }
}

/// The Qty and Bracket fields (mock 2): each names what it holds and shows its value, and opens its
/// list. Qty: the presets with their keys and "Custom…" (typing shows in the field); Bracket: None
/// and the presets, one bracket for the position (a switch), the units, "Edit presets…".
fn controls<V: TradePane>(
    v: &V,
    product: &Product,
    width: f32,
    t: &Theme,
    cx: &mut Context<V>,
) -> gpui::Div {
    let pt = v.trade_ref();
    let tight = is_tight(width);
    let size = pane_size(v, cx);
    // Over the account's maximum: said here, before the click (shape: the words, not color).
    let over = dom_trade::over_max(size, max_order_qty(v, cx));
    let custom = !dom_trade::QTY_PRESETS.contains(&size);
    let qty_value = match &pt.qty_edit {
        Some(s) => format!("{s}_"),
        None => size.to_string(),
    };
    let (presets, units) = presets_and_units(v, product, cx);
    let position = pt
        .account
        .as_ref()
        .is_some_and(|a| saved_on(cx, a, product.symbol));
    // In position mode the pair uses a preset (the first one when none is picked).
    let current = pt.bracket.or_else(|| {
        position
            .then(|| presets.first().map(|p| p.preset()))
            .flatten()
    });
    let bracket_value = match current {
        None => "None".to_owned(),
        Some(c) => presets.iter().find(|p| p.preset() == c).map_or_else(
            || crate::brackets::label(c, product, units),
            |p| preset_text(p, product, units),
        ),
    };
    let refused = pt.bracket_refused.clone();
    // Saved on but refused: "position !" in the warning color (never the color alone).
    let mode_words = position.then(|| {
        if refused.is_some() {
            ("· position !", "pos !")
        } else {
            ("· position", "pos")
        }
    });
    let qty_key = over.clone().unwrap_or_else(|| "Qty".to_owned());
    let fit = field_fit(width, &qty_key, &qty_value, &bracket_value, mode_words);
    let open = pt.panel_menu;

    // Qty ▾
    let qty = field("qty-field", open == Some(PanelMenu::Qty), tight, t)
        .flex_shrink_0()
        .on_scroll(
            cx.listener(|v: &mut V, ev: &gpui::ScrollEvent, w, cx| scroll_size(v, ev, w, cx)),
        )
        .when(over.is_some() || fit.qty_key, |f| {
            f.child(match &over {
                Some(text) => field_key(text.clone(), t)
                    .text_color(t.warn)
                    .font_weight(FontWeight::BOLD),
                None => field_key("Qty", t),
            })
        })
        .child(
            div()
                .flex_shrink_0()
                .font_weight(FontWeight::BOLD)
                .text_color(t.text_main)
                .child(qty_value),
        )
        .child(chevron(t));
    let qty = tip(qty, "Quantity", Some(TradeCmd::TypeQty), t).on_mouse_down(
        MouseButton::Left,
        cx.listener(|v: &mut V, ev: &gpui::MouseDownEvent, _, cx| {
            cx.stop_propagation();
            if crate::popover::second_press(ev.click_count, ev.position) {
                return;
            }
            toggle_panel_menu(v, PanelMenu::Qty, cx);
        }),
    );
    let qty_menu = (open == Some(PanelMenu::Qty)).then(|| {
        let mut list = menu_list("qty-list", t);
        for (i, q) in dom_trade::QTY_PRESETS.into_iter().enumerate() {
            let on = size == q && pt.qty_edit.is_none();
            list = list.child(menu_row(
                ("qty-item", i as u64),
                if on { Mark::Check } else { Mark::None },
                q.to_string(),
                Some(TradeCmd::QtyPreset(i as u8)),
                t,
                cx,
                move |v, _, cx| set_size(v, q, cx),
            ));
        }
        list = list.child(menu_sep(t)).child(menu_row(
            ("qty-item", 99),
            if custom { Mark::Check } else { Mark::None },
            if custom {
                format!("Custom… ({size})")
            } else {
                "Custom…".to_owned()
            },
            Some(TradeCmd::TypeQty),
            t,
            cx,
            |v, window, cx| {
                // The keys must reach the pane: in a popped-out window focus may sit on its root.
                v.trade_focus().clone().focus(window, cx);
                open_size_field(v);
            },
        ));
        list.into_any_element()
    });

    // Bracket ▾
    let bracket = field("bracket-field", open == Some(PanelMenu::Bracket), tight, t)
        .flex_1()
        .when(fit.bracket_key, |f| f.child(field_key("Bracket", t)))
        // The one part that gives way (with "…") where nothing else is left to drop: the list
        // names it in full.
        .child(
            div()
                .min_w_0()
                .truncate()
                .font_weight(FontWeight::BOLD)
                .text_color(t.text_main)
                .child(bracket_value),
        )
        .when_some(mode_words, |f, (long, short)| {
            let warn = refused.is_some();
            f.child(
                field_key(if fit.long_mode { long } else { short }, t)
                    .when(warn, |e| e.text_color(t.warn).font_weight(FontWeight::BOLD)),
            )
        })
        .child(chevron(t));
    let bracket_tip = match (&refused, position) {
        (Some(why), true) => format!(
            "Bracket. One bracket for the position isn't in force (the core refused it: {why})"
        ),
        (_, true) => "Bracket: one stop and one target for the whole position".to_owned(),
        _ => "Bracket for each entry".to_owned(),
    };
    let bracket = tip(bracket, &bracket_tip, Some(TradeCmd::NextBracket), t).on_mouse_down(
        MouseButton::Left,
        cx.listener(|v: &mut V, ev: &gpui::MouseDownEvent, _, cx| {
            cx.stop_propagation();
            if crate::popover::second_press(ev.click_count, ev.position) {
                return;
            }
            toggle_panel_menu(v, PanelMenu::Bracket, cx);
        }),
    );
    let bracket_menu = (open == Some(PanelMenu::Bracket)).then(|| {
        let mut list = menu_list("bracket-list", t);
        list = list.child(menu_row(
            ("bracket-item", 0),
            if current.is_none() {
                Mark::Check
            } else {
                Mark::None
            },
            "None".to_owned(),
            None,
            t,
            cx,
            |v, _, cx| {
                if position_on_now(v, cx) {
                    v.trade().flash(
                        "None isn't available with one bracket for the position: turn it off first",
                    );
                } else {
                    v.trade().bracket = None;
                }
            },
        ));
        for (i, p) in presets.iter().enumerate() {
            let b = p.preset();
            list = list.child(menu_row(
                ("bracket-item", i as u64 + 1),
                if current == Some(b) {
                    Mark::Check
                } else {
                    Mark::None
                },
                if p.name.is_empty() {
                    crate::brackets::label(b, product, units)
                } else {
                    format!("{} · {}", p.name, crate::brackets::label(b, product, units))
                },
                None,
                t,
                cx,
                move |v, _, _| v.trade().bracket = Some(b),
            ));
        }
        // A custom bracket in force that no preset names: shown, checked.
        if let Some(c) = current.filter(|c| !presets.iter().any(|p| p.preset() == *c)) {
            list = list.child(menu_row(
                ("bracket-item", 98),
                Mark::Check,
                crate::brackets::label(c, product, units),
                None,
                t,
                cx,
                move |v, _, _| v.trade().bracket = Some(c),
            ));
        }
        let pos_label = if refused.is_some() && position {
            "One bracket for the position (refused)"
        } else {
            "One bracket for the position"
        };
        list = list
            .child(menu_sep(t))
            .child(menu_row(
                ("bracket-item", 100),
                Mark::Box(position),
                pos_label.to_owned(),
                Some(TradeCmd::BracketMode),
                t,
                cx,
                |v, _, cx| run_from(v, TradeCmd::BracketMode, Instant::now(), Source::Dom, cx),
            ))
            .child(menu_row(
                ("bracket-item", 101),
                Mark::Box(units == crate::brackets::BracketUnits::Points),
                "Distances in points".to_owned(),
                Some(TradeCmd::BracketUnits),
                t,
                cx,
                |v, _, cx| run_from(v, TradeCmd::BracketUnits, Instant::now(), Source::Dom, cx),
            ))
            .child(menu_row(
                ("bracket-item", 102),
                Mark::None,
                "Edit presets…".to_owned(),
                Some(TradeCmd::EditBrackets),
                t,
                cx,
                |v, _, cx| run_from(v, TradeCmd::EditBrackets, Instant::now(), Source::Dom, cx),
            ));
        list.into_any_element()
    });
    let dismiss = |cx: &mut Context<V>| -> crate::popover::OnDismiss {
        Box::new(
            cx.listener(|v: &mut V, _: &gpui::MouseDownEvent, _, cx| close_account_menu(v, cx)),
        )
    };
    // Each list opens under its field (`dropdown::fit`) and covers what is under it, Flatten and
    // Cancel all included (owner, 2026-11-10).
    let qty = crate::dropdown::dropdown(div().flex().flex_shrink_0().child(qty), qty_menu)
        .on_dismiss(dismiss(cx));
    let bracket =
        crate::dropdown::dropdown(div().flex().flex_1().min_w_0().child(bracket), bracket_menu)
            .on_dismiss(dismiss(cx));
    div()
        .flex()
        .flex_row()
        .items_start()
        .gap(crate::zoom::ui(PAIR_GAP))
        .h(crate::zoom::ui(CTRL_H))
        // Never dimmed: a size and a bracket can be chosen while entries are locked.
        .child(qty)
        .child(bracket)
}

/// The best bid and ask the joins and the market buttons quote (the book's, else the quote's).
fn inside<V: TradePane>(v: &V) -> (Option<Price>, Option<Price>) {
    v.trade_store()
        .try_read()
        .map(|d| {
            (
                d.book.best_bid().map(|b| b.0).or(d.bid),
                d.book.best_ask().map(|a| a.0).or(d.ask),
            )
        })
        .unwrap_or_default()
}

/// Buy and Sell (mock 2): solid in the side's color, "Buy 10" over "Market · 7572.00" (the price it
/// would hit); Sell's text to the right. While entries are locked: unfilled, dashed, "Locked".
fn market_buttons<V: TradePane>(
    v: &V,
    product: &Product,
    width: f32,
    dim: bool,
    t: &Theme,
    cx: &mut Context<V>,
) -> gpui::Div {
    let (bid, ask) = inside(v);
    let size = pane_size(v, cx);
    let button = |id: &'static str, buy: bool, p: Option<Price>| {
        let price = p.map(|p| product.price.format::<32>(p));
        let text = market_text(buy, size, price.as_ref().map(|p| p.as_str()), width, dim);
        // The second line: the market and its price (small), or a size too long for the first
        // line (as loud as the first).
        let (top, below, stacked_size): (String, Option<String>, bool) = match text {
            MarketText::Full(a, b) => (a, Some(b), false),
            MarketText::Size(a) => (a, None, false),
            MarketText::Stacked(w, s) => (w.to_owned(), Some(s), true),
        };
        // Dimmed (entries locked): no fill, a 2 px dashed edge in the side's color (GPUI's dash
        // is twice the width: 4 on, 2 off) and dim text saying "Locked", so it never looks
        // armed: solid fill means it would send.
        let side = if buy { t.up } else { t.down };
        let (fill, edge, text) = if dim {
            (t.panel, side, t.text_dim)
        } else {
            (side, side, t.bg)
        };
        let (pad, title_px) = market_px(width);
        div()
            .id(id)
            .flex_1()
            .flex()
            .flex_col()
            .justify_center()
            .when(buy, |d| d.items_start())
            .when(!buy, |d| d.items_end())
            .h(tall(TRADE_BUTTON_H))
            .px(crate::zoom::ui(pad))
            .rounded_md()
            .bg(fill)
            .border_1()
            .when(dim, |d| d.border_2().border_dashed())
            .border_color(edge)
            // Text in the background color on the side's (AA both ways); dimmed, the theme's.
            .text_color(text)
            .cursor_pointer()
            .whitespace_nowrap()
            .overflow_hidden()
            .child(
                div()
                    .text_size(crate::zoom::ui(title_px))
                    .font_weight(FontWeight::BOLD)
                    .child(top),
            )
            .children(below.map(|b| {
                div()
                    .text_size(crate::zoom::ui(if stacked_size { title_px } else { 11.0 }))
                    .when(stacked_size, |d| d.font_weight(FontWeight::BOLD))
                    .child(b)
            }))
    };
    let buy = tip(
        button("buy-mkt", true, ask),
        "Buy market",
        Some(TradeCmd::BuyMarket),
        t,
    );
    let sell = tip(
        button("sell-mkt", false, bid),
        "Sell market",
        Some(TradeCmd::SellMarket),
        t,
    );
    div()
        .flex()
        .flex_row()
        .items_start()
        .gap(crate::zoom::ui(PAIR_GAP))
        .h(crate::zoom::ui(TRADE_H))
        .child(run_on_click(buy, TradeCmd::BuyMarket, cx))
        .child(run_on_click(sell, TradeCmd::SellMarket, cx))
}

/// Join bid and Join ask (mock 2): outlined in the side's color, the words and the price (bold, in
/// the side's color) apart; prices give way first, at narrow widths ([`join_text`]).
fn join_buttons<V: TradePane>(
    v: &V,
    product: &Product,
    width: f32,
    t: &Theme,
    cx: &mut Context<V>,
) -> AnyElement {
    let (bid, ask) = inside(v);
    let button = |id: &'static str, is_bid: bool, p: Option<Price>| {
        let price = p.map(|p| product.price.format::<32>(p));
        let (words, price) = join_text(is_bid, price.as_ref().map(|p| p.as_str()), width);
        let line = if is_bid { t.up } else { t.down };
        // Raised to the panel, not a state token: the price is in the side's color, AA on
        // `panel` but not on the light themes' tinted states.
        let panel = t.panel;
        div()
            .id(id)
            .flex_1()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .when(price.is_none(), |d| d.justify_center())
            .gap(crate::zoom::ui(FIELD_GAP))
            .h(tall(JOIN_BUTTON_H))
            .px(crate::zoom::ui(BUTTON_PX))
            .rounded_md()
            .bg(t.bg)
            .border_1()
            .border_color(line)
            .cursor_pointer()
            .whitespace_nowrap()
            .overflow_hidden()
            .hover(move |s| s.bg(panel))
            .child(div().text_color(t.text_main).child(words))
            .children(price.map(|p| {
                div()
                    .font_weight(FontWeight::BOLD)
                    .text_color(line)
                    .child(p)
            }))
    };
    let jb = tip(
        button("join-bid", true, bid),
        "Join bid (buy limit at the best bid)",
        Some(TradeCmd::JoinBid),
        t,
    );
    let ja = tip(
        button("join-ask", false, ask),
        "Join ask (sell limit at the best ask)",
        Some(TradeCmd::JoinAsk),
        t,
    );
    div()
        .flex()
        .flex_row()
        .items_start()
        .gap(crate::zoom::ui(PAIR_GAP))
        .h(crate::zoom::ui(JOIN_H))
        .child(run_on_click(jb, TradeCmd::JoinBid, cx))
        .child(run_on_click(ja, TradeCmd::JoinAsk, cx))
        .into_any_element()
}

/// The inline bracket-preset editor, in the place of the fields and the order buttons: `h` tall
/// ([`PanelRows::editor_h`]), with as many preset rows as fit (at least one; it scrolls).
fn editor_view<V: TradePane>(
    ed: &crate::brackets::Editor,
    msg: Option<&str>,
    v: &V,
    h: f32,
    width: f32,
    t: &Theme,
    cx: &mut Context<V>,
) -> gpui::Div {
    use crate::brackets::Field;
    let rows_shown = editor_list_rows(h, width, crate::zoom::get());
    let first = ed.row.saturating_sub(rows_shown - 1);
    let tight = is_tight(width);
    let unit = ed.units.word();
    let head = div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        // The row with the actions is a hit target tall at every zoom ([`editor_head_foot`]).
        .h(if tight {
            crate::zoom::hit(EDITOR_HEAD_H)
        } else {
            crate::zoom::ui(EDITOR_HEAD_H)
        })
        .flex_shrink_0()
        .whitespace_nowrap()
        .overflow_hidden()
        .when(tight, |d| d.gap_1())
        .when(!tight, |d| {
            d.child(
                div()
                    .flex_shrink_0()
                    .font_weight(FontWeight::BOLD)
                    .text_color(t.text_main)
                    .child(format!("Bracket presets · {}", ed.product.symbol)),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_color(t.text_dim)
                    .child(format!("in {unit}")),
            )
        })
        // Tight: the unit names the distances ("Ticks"), and the actions sit beside it so the
        // footer keeps its room for the prices.
        .when(tight, |d| {
            let mut w = unit.chars();
            let title: String = w
                .next()
                .map(|c| c.to_uppercase().chain(w).collect())
                .unwrap_or_default();
            d.child(
                div()
                    .flex_shrink_0()
                    .font_weight(FontWeight::BOLD)
                    .text_color(t.text_main)
                    .child(title),
            )
        })
        .child(div().flex_1())
        .when(!tight, |d| {
            d.child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_color(t.text_dim)
                    .child("Enter save · Esc cancel"),
            )
        });
    let fit = editor_cells(width);
    let cell_el = |r: usize, f: Field, text: &str, w: Option<f32>| {
        let active = r == ed.row && f == ed.field;
        let bad = ed.cell_problem(r, f).is_some();
        let shown = if active {
            format!("{text}▏")
        } else {
            text.to_owned()
        };
        div()
            .id(("bracket-cell", (r * 3 + f as usize) as u64))
            .map(|d| match w {
                Some(w) => d.w(crate::zoom::ui(w)).flex_shrink_0(),
                None => d.flex_1().min_w_0(),
            })
            .h(crate::zoom::ui(18.0))
            .px_1()
            .flex()
            .items_center()
            .rounded_sm()
            .border_1()
            .border_color(if active {
                t.accent
            } else if bad {
                t.warn
            } else {
                t.axis_line
            })
            .text_color(t.text_main)
            .cursor_text()
            .overflow_hidden()
            .child(shown)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |v: &mut V, _, _, cx| {
                    cx.stop_propagation();
                    if let Some(e) = v.trade().editor.as_mut() {
                        e.row = r;
                        e.field = f;
                    }
                    cx.notify();
                }),
            )
    };
    let mut list = div().flex().flex_col().gap(crate::zoom::ui(2.0));
    for (r, row) in ed.rows.iter().enumerate().skip(first).take(rows_shown) {
        list = list.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .text_size(crate::zoom::ui(fit.text))
                .when(fit.marker, |d| {
                    d.child(
                        div()
                            .w(crate::zoom::ui(14.0))
                            .flex_shrink_0()
                            .text_color(if r == ed.row { t.accent } else { t.text_dim })
                            .child(if r == ed.row { "›" } else { " " }),
                    )
                })
                .child(div().flex_shrink_0().text_color(t.text_dim).child("S"))
                .child(cell_el(r, Field::Stop, &row.stop, fit.value))
                .child(div().flex_shrink_0().text_color(t.text_dim).child("T"))
                .child(cell_el(r, Field::Target, &row.target, fit.value))
                .when(fit.name, |d| {
                    d.child(cell_el(r, Field::Name, &row.name, None))
                }),
        );
    }
    if ed.rows.is_empty() {
        list = list.child(
            div()
                .text_color(t.text_dim)
                .child("No presets: Insert adds one"),
        );
    }
    // Footer: a problem (refused save, off-grid cell), else the live preview.
    let preview = ed.preview(preview_entry(v));
    let problem = msg
        .map(str::to_owned)
        .or_else(|| ed.cell_problem(ed.row, ed.field));
    let action = |id: &'static str,
                  label: &'static str,
                  name: &'static str,
                  keys: &[&str],
                  f: fn(&mut crate::brackets::Editor)| {
        let keys = keys.iter().map(|k| (*k).to_owned()).collect();
        crate::tooltip::tip_keys(cell(id, t).flex_shrink_0().child(label), name, keys, t)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |v: &mut V, _, _, cx| {
                    cx.stop_propagation();
                    if let Some(e) = v.trade().editor.as_mut() {
                        f(e);
                    }
                    cx.notify();
                }),
            )
    };
    let foot = div()
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .h(if tight {
            crate::zoom::ui(EDITOR_FOOT_H)
        } else {
            crate::zoom::hit(EDITOR_FOOT_H)
        })
        .flex_shrink_0()
        // Tight: 11 px, so both prices fit whole ("S 21450.25  T 21460.25" in 149 px).
        .when(tight, |d| d.text_size(crate::zoom::ui(11.0)))
        .child(match problem {
            Some(p) => div()
                .min_w_0()
                .truncate()
                .px_1()
                .bg(t.warn)
                .text_color(t.on_warn)
                .child(p),
            // The preview's prices are never cut: whole items, what doesn't fit left out whole
            // (a tight panel gives the footer to them alone).
            None => whole_items(
                &preview.unwrap_or_default().replace(" · ", "  "),
                t.text_main,
                false,
            ),
        });
    let actions = div()
        .flex()
        .flex_row()
        .flex_shrink_0()
        .items_center()
        .gap_1()
        .child(action(
            "bracket-add",
            "+",
            "Add a preset",
            &["Insert"],
            crate::brackets::Editor::add,
        ))
        .child(action(
            "bracket-del",
            "−",
            "Delete the preset",
            &["Delete"],
            crate::brackets::Editor::delete,
        ))
        .child(action(
            "bracket-up",
            "↑",
            "Move up",
            &["Ctrl", "↑"],
            |e| e.reorder(-1),
        ))
        .child(action(
            "bracket-down",
            "↓",
            "Move down",
            &["Ctrl", "↓"],
            |e| e.reorder(1),
        ));
    let (head, foot) = if tight {
        (head.child(div().flex_1()).child(actions), foot)
    } else {
        (head, foot.child(div().flex_1()).child(actions))
    };
    div()
        .flex()
        .flex_col()
        .justify_between()
        .h(crate::zoom::ui(h))
        .py_1()
        .overflow_hidden()
        .child(head)
        .child(list)
        .child(foot)
}

/// Where the editor's preview measures from: a buy at the ask (else the last trade).
fn preview_entry<V: TradePane>(v: &V) -> Option<(Price, bool)> {
    let d = v.trade_store().try_read()?;
    let ask = d.book.best_ask().map(|a| a.0).or(d.ask).or(d.last_trade);
    ask.map(|p| (p, true))
}

/// The account name (click: choose another) and the PAPER badge.
fn account_chip<V: TradePane>(
    v: &V,
    data: Option<&BarData>,
    t: &Theme,
    cx: &mut Context<V>,
) -> AnyElement {
    // An account set but not listed is a broker's, waiting for it: LIVE until the broker
    // says otherwise (never shown as simulated), read-only.
    let fallback = if v.trade_ref().account.is_some() {
        ("broker account (waiting)".to_owned(), "LIVE", true, None)
    } else {
        ("no account".to_owned(), "PAPER", false, None)
    };
    let (name, badge_word, locked, venue) = data.map_or(fallback, |d| {
        (
            d.account_name.clone(),
            d.badge,
            d.read_only.is_some(),
            d.venue.clone(),
        )
    });
    let accent = t.accent;
    let open = v.trade_ref().account_menu;
    let menu = account_menu(v, t, cx);
    // Quiet until pointed at (mock 2): the badge beside it and the ▾ say what it is.
    let c = cell("account", t)
        .border_color(gpui::transparent_black())
        .min_w_0()
        .overflow_hidden()
        .whitespace_nowrap()
        // Gives way with "…", never cut bare.
        .child(div().min_w_0().truncate().child(format!("{name} ▾")))
        .when(open, |d| d.border_color(accent))
        .hover(move |s| s.border_color(accent));
    let c = tip(c, "Choose the account", None, t);
    // The badge first: at narrow widths the account chip gives way, never the badge.
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .min_w_0()
        .child(account_badge(badge_word, t))
        .child(
            crate::dropdown::dropdown(
                // The chip is recorded from a wrapper: the chip clips its own children.
                div()
                    .relative()
                    .min_w_0()
                    .flex()
                    .child(c.on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|v: &mut V, _, _, cx| {
                            cx.stop_propagation();
                            toggle_account_menu(v, cx);
                        }),
                    ))
                    .children(
                        crate::layout_rec::on()
                            .then(|| {
                                crate::layout_rec::marker(
                                    lines_core::layout_lint::Kind::Chip,
                                    format!("{name} ▾"),
                                    0,
                                    "",
                                )
                            })
                            .flatten(),
                    ),
                menu,
            )
            .on_dismiss(Box::new(cx.listener(
                |v: &mut V, _: &gpui::MouseDownEvent, _, cx| close_account_menu(v, cx),
            ))),
        )
        .when(locked, |d| {
            d.child(
                div()
                    .px_1()
                    .flex_shrink_0()
                    .rounded_sm()
                    .border_1()
                    .border_color(t.text_dim)
                    .text_color(t.text_main)
                    .font_weight(FontWeight::BOLD)
                    .child("READ-ONLY"),
            )
        })
        .when_some(venue, |d, (words, attention)| {
            d.child(if attention {
                div()
                    .px_1()
                    .min_w_0()
                    .truncate()
                    .bg(t.warn)
                    .text_color(t.on_warn)
                    .font_weight(FontWeight::BOLD)
                    .child(words)
            } else {
                div()
                    .min_w_0()
                    .truncate()
                    .text_color(t.text_dim)
                    .child(words)
            })
        })
        .into_any_element()
}

/// A broker's connection as the chip says it: "Broker · connected", "… · stale 12 s",
/// "… · disconnected: why"; `true` when it needs attention.
fn venue_words(t: &Trading, venue: usize) -> Option<(String, bool)> {
    let (label, status) = t.venue_status(venue)?;
    let Some(s) = status else {
        return Some((label, false));
    };
    if !s.connected {
        return Some((format!("{label} · {}", s.detail), true));
    }
    let silent_s = s
        .last_message
        .map(|m| (lines_feed::now_utc().0 - m.0) / 1000)
        .unwrap_or(0);
    if silent_s >= crate::trading::VENUE_STALE_AFTER_S {
        return Some((format!("{label} · stale {silent_s} s"), true));
    }
    Some((format!("{label} · {}", s.detail), false))
}

/// PAPER: an inverted chip with the word (never colour alone).
pub fn paper_badge(t: &Theme) -> gpui::Div {
    account_badge("PAPER", t)
}

/// The chart header's account, for the fit model: its chip text ("Name ▾") and whether it's
/// paper.
pub fn chart_account<V: TradePane>(v: &V, cx: &gpui::App) -> Option<(String, bool)> {
    let trading = Trading::try_global(cx)?;
    let info = v
        .trade_ref()
        .account
        .as_ref()
        .and_then(|a| trading.account(a))?;
    Some((format!("{} ▾", info.name), info.paper))
}

/// The account's badge as a recorded header word on a chip (never colour alone): PAPER and
/// SIM inverted in the paper colours, LIVE outlined.
pub fn account_badge(word: &'static str, t: &Theme) -> gpui::Div {
    badge(word, t)
        .relative()
        .children(crate::layout_rec::text_marker(word))
}

/// The badge chip itself (in menus: not recorded as header text).
fn badge(word: &'static str, t: &Theme) -> gpui::Div {
    let chip = div()
        .flex_shrink_0()
        .px_1()
        .rounded_sm()
        .font_weight(FontWeight::BOLD);
    if word == "LIVE" {
        chip.border_1()
            .border_color(t.text_main)
            .text_color(t.text_main)
            .child(word)
    } else {
        chip.bg(t.paper_badge)
            .text_color(t.on_paper_badge)
            .child(word)
    }
}

/// A press of Reset paper account on this pane's account: the first shows what it will do
/// (CONFIRM), the second sends it; a broker account is refused (NOT SENT).
fn reset_press<V: TradePane>(v: &mut V, journal: bool, source: Source, cx: &mut Context<V>) {
    let Some(account) = v.trade_ref().account.clone() else {
        v.trade().flash("no account");
        return;
    };
    let Some(t) = Trading::try_global(cx) else {
        return;
    };
    match t.press_reset(&account, journal, source) {
        Ok(Some(text)) => v.trade().confirm(format!("{text}. Press again to reset")),
        Ok(None) => {
            let name = t.account(&account).map(|a| a.name).unwrap_or_default();
            v.trade().notice(format!(
                "Resetting {}: anything open is flattened first",
                crate::paper_reset::account_words(&name, &t.source)
            ));
        }
        Err(why) => v.trade().flash(why),
    }
    cx.notify();
}

/// Open or close the account list (the account chip).
pub fn toggle_account_menu<V: TradePane>(v: &mut V, cx: &mut Context<V>) {
    if v.trade_ref().account_menu {
        return close_account_menu(v, cx);
    }
    v.trade().account_menu = true;
    v.trade().panel_menu = None;
    crate::popover::opened(
        cx,
        crate::popover::Kind::Account,
        |v: &mut V, cx| close_account_menu(v, cx),
        |v: &V| v.trade_ref().menu_open(),
    );
    cx.notify();
}

/// Close the account list, or the Qty or Bracket list, if open (an outside click, Esc, another
/// popover opening, a row picked).
pub fn close_account_menu<V: TradePane>(v: &mut V, cx: &mut Context<V>) {
    let account = std::mem::take(&mut v.trade().account_menu);
    if v.trade().panel_menu.take().is_some() || account {
        crate::popover::closed(cx, crate::popover::Kind::Account);
        cx.notify();
    }
}

/// The account list, while the selector is open: under the account chip (`dropdown`).
fn account_menu<V: TradePane>(v: &V, t: &Theme, cx: &mut Context<V>) -> Option<AnyElement> {
    if !v.trade_ref().account_menu {
        return None;
    }
    let trading = Trading::try_global(cx)?;
    let current = v.trade_ref().account.clone();
    let lit = t.ghost_selected;
    // The trigger's label, only while recording (and only with a current account).
    let trigger = crate::layout_rec::on()
        .then(|| current.as_ref().and_then(|a| trading.account(a)))
        .flatten()
        .map(|a| format!("{} ▾", a.name));
    let mut list = div()
        .id("account-list")
        .block_mouse_except_scroll()
        .relative()
        .children(trigger.and_then(|trigger| {
            crate::layout_rec::bordered(
                lines_core::layout_lint::Kind::Popup { trigger },
                "account list",
                crate::layout_rec::POPUP,
                "",
                1.0,
            )
        }))
        // A fixed width: the rules line wraps rather than widening the list over the header's
        // other controls (the DOM's arm state).
        .w(crate::zoom::ui(320.0))
        // Not the header's no-wrap text style.
        .whitespace_normal()
        .flex()
        .flex_col()
        .py_1()
        .bg(t.panel)
        .border_1()
        .border_color(t.accent)
        .rounded_sm()
        .text_size(crate::zoom::ui(12.0));
    for (i, a) in trading.accounts().iter().enumerate() {
        let id = a.id.clone();
        let on = current.as_ref() == Some(&a.id);
        let detail = match (&a.read_only, &a.rules) {
            (Some(why), _) => format!("read-only: {why}"),
            (None, Some(r)) => format!("rules: {} (approximation)", r.name),
            (None, None) => "no prop rules".into(),
        };
        list = list.child(
            div()
                .id(("acct", i as u64))
                .px_2()
                .py_1()
                .flex()
                .flex_col()
                .cursor_pointer()
                .hover(move |s| s.bg(lit))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_2()
                        .child(
                            div()
                                .text_color(t.text_main)
                                .font_weight(if on {
                                    FontWeight::BOLD
                                } else {
                                    FontWeight::NORMAL
                                })
                                .child(if on {
                                    format!("✓ {}", a.name)
                                } else {
                                    a.name.clone()
                                }),
                        )
                        .child(badge(a.badge, t)),
                )
                .child(
                    div()
                        .text_color(t.text_dim)
                        .text_size(crate::zoom::ui(11.0))
                        .child(detail),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |v: &mut V, _, _, cx| {
                        cx.stop_propagation();
                        set_account(v, Some(id.clone()), cx);
                    }),
                ),
        );
    }
    // The current account, when paper: reset it (a second press confirms; `paper_reset`).
    if let Some(info) = current.as_ref().and_then(|a| trading.account(a))
        && info.paper
    {
        let who = crate::paper_reset::account_words(&info.name, &trading.source);
        let waiting = trading.cache.read().resetting(&info.id);
        list = list.child(div().my_1().h(crate::zoom::ui(1.0)).bg(t.axis_line));
        if waiting {
            // In place of the reset rows: one is on its way, and a second can't be started.
            list = list.child(
                div()
                    .px_2()
                    .py_1()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .items_center()
                    .text_color(t.text_main)
                    .child(
                        // Pending is outlined (design.md §3), never color alone.
                        div()
                            .flex_shrink_0()
                            .px_1()
                            .rounded_sm()
                            .border_1()
                            .border_color(t.text_main)
                            .font_weight(FontWeight::BOLD)
                            .child("PENDING"),
                    )
                    // Wraps inside the list's width (never past its edge).
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(format!("Resetting {who}: waiting for the flatten to fill")),
                    ),
            );
        } else {
            let mut armed_text = None;
            for (k, journal) in [(0u64, false), (1, true)] {
                let armed = trading.reset_armed(&info.id, journal);
                // One line whatever the state: nothing below it moves under the pointer.
                let label = if armed {
                    armed_text = Some(crate::paper_reset::confirm_text(
                        &info.name,
                        &trading.source,
                        info.start_balance,
                        journal,
                    ));
                    crate::paper_reset::armed_label(&info.name, &trading.source, journal)
                } else if journal {
                    format!("Reset {who} and clear its journal…")
                } else {
                    format!("Reset {who}…")
                };
                let mut item = div()
                    .id(("acct-reset", k))
                    .px_2()
                    .py_1()
                    .cursor_pointer()
                    .hover(move |s| s.bg(lit))
                    // Destructive rows go last, in their token (design.md §6); the words say it.
                    .text_color(t.destructive)
                    .whitespace_nowrap()
                    .truncate()
                    .child(label);
                if armed {
                    // Waiting for the second press: its words and a border, not color alone.
                    item = item
                        .border_1()
                        .border_color(t.warn)
                        .font_weight(FontWeight::BOLD);
                }
                list = list.child(item.on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |v: &mut V, _, _, cx| {
                        cx.stop_propagation();
                        reset_press(v, journal, Source::Dom, cx);
                        // Sent: the list closes (the pane says what happens next).
                        let sent = Trading::try_global(cx).is_some_and(|t| {
                            v.trade_ref()
                                .account
                                .as_ref()
                                .is_some_and(|a| !t.reset_armed(a, journal))
                        });
                        if sent {
                            close_account_menu(v, cx);
                        }
                    }),
                ));
            }
            // What the second press does, under the rows (last: nothing above it moves).
            if let Some(text) = armed_text {
                list = list.child(
                    div()
                        .px_2()
                        .pb_1()
                        .text_size(crate::zoom::ui(11.0))
                        .text_color(t.text_main)
                        .child(text),
                );
            }
        }
    }
    Some(list.into_any_element())
}

/// A chart's header: its account and the PAPER badge (chart trading is roadmap 8).
/// `fits`: whether the header has room for the account chip (`pane::chart_header_fit`).
/// Without room only the PAPER or LIVE badge shows: paper vs live is never dropped, and the
/// chip is dropped whole, never drawn cut (the account stays reachable from the palette).
pub fn chart_chip<V: TradePane>(
    v: &V,
    t: &Theme,
    fits: bool,
    cx: &mut Context<V>,
) -> Option<AnyElement> {
    let trading = Trading::try_global(cx)?;
    let pt = v.trade_ref();
    let info = pt.account.as_ref().and_then(|a| trading.account(a))?;
    if !fits {
        return Some(account_badge(info.badge, t).into_any_element());
    }
    // Name and badge only: the header's fit model measures exactly these.
    let mut data = empty_data(&info, trading);
    data.read_only = None;
    data.venue = None;
    // Whole or not at all (the header's fit decides): never shrunk and cut.
    Some(
        div()
            .flex_shrink_0()
            .child(account_chip(v, Some(&data), t, cx))
            .into_any_element(),
    )
}

// ---- position bracket (one stop and one target for the whole position) ---------------------

/// Whether the pane's account trades its product with one bracket for the whole position
/// (saved per account and product symbol).
fn position_on<V: TradePane>(v: &V, cx: &App) -> bool {
    position_known(v, cx) == Some(true)
}

/// The saved mode as render can know it: `None` while the store is busy (unknown, never
/// "off": nothing is sent on it) or without an account.
fn position_known<V: TradePane>(v: &V, cx: &App) -> Option<bool> {
    let a = v.trade_ref().account.as_ref()?;
    let d = v.trade_store().try_read()?;
    Some(saved_on(cx, a, d.product.symbol))
}

/// `position_on` for a click: waits for the store instead of reading "off" while it is busy.
fn position_on_now<V: TradePane>(v: &V, cx: &App) -> bool {
    let Some(a) = v.trade_ref().account.as_ref() else {
        return false;
    };
    let symbol = v.trade_store().read().product.symbol;
    saved_on(cx, a, symbol)
}

fn saved_on(cx: &App, account: &AccountId, symbol: &str) -> bool {
    crate::brackets::position_mode(
        &crate::picker_view::settings(cx).bracket_presets,
        &account.0,
        symbol,
    )
}

/// Pos is saved on but no preset is there for its distances (the list was emptied).
fn no_preset<V: TradePane>(v: &V, cx: &App) -> bool {
    position_on(v, cx)
        && v.trade_ref().bracket.is_none()
        && first_preset(v, cx).is_none()
        && v.trade_ref().account.is_some()
}

/// What this pane asked the core to hold that its setting no longer wants: Position told
/// for its account and contract while Pos is now off (another pane turned it off).
fn told_off<V: TradePane>(v: &V, cx: &App) -> Option<(AccountId, ContractKey)> {
    let (a, k, m) = v.trade_ref().bracket_told.as_ref()?;
    let here =
        v.trade_ref().account.as_ref() == Some(a) && contract_of(v.trade_store()) == Some(*k);
    (here
        && matches!(m, lines_trading::BracketMode::Position { .. })
        && position_known(v, cx) == Some(false))
    .then(|| (a.clone(), *k))
}

/// The position mode this pane's saved setting wants the core to hold, with the pane's
/// preset (or the first one): `None` when it is per entry or nothing is known yet.
fn want_mode<V: TradePane>(
    v: &V,
    cx: &App,
) -> Option<(AccountId, ContractKey, lines_trading::BracketMode)> {
    if !position_on(v, cx) {
        return None;
    }
    let account = v.trade_ref().account.clone()?;
    let contract = contract_of(v.trade_store())?;
    let preset = v.trade_ref().bracket.or_else(|| first_preset(v, cx))?;
    Some((
        account,
        contract,
        lines_trading::BracketMode::Position {
            stop_ticks: preset.stop_ticks,
            target_ticks: preset.target_ticks,
        },
    ))
}

fn first_preset<V: TradePane>(v: &V, cx: &App) -> Option<BracketPreset> {
    let a = v.trade_ref().account.as_ref()?;
    let d = v.trade_store().try_read()?;
    crate::brackets::presets_for(
        &crate::picker_view::settings(cx).bracket_presets,
        &a.0,
        d.product.symbol,
    )
    .first()
    .map(|p| p.preset())
}

/// Whether [`bracket_sync`] has something to do: an answer to collect, or the saved mode (or
/// a new preset) not yet told to the core for this account and contract.
pub fn bracket_needs_sync<V: TradePane>(v: &V, cx: &App) -> bool {
    let pt = v.trade_ref();
    if pt.switch_pending {
        // After the fixture's script (screenshot mode), so its exits are there to replace.
        return Trading::try_global(cx)
            .is_some_and(|t| t.settled.load(std::sync::atomic::Ordering::Acquire));
    }
    if let Some(ask) = &pt.bracket_ask {
        // Never blocks the frame (try_read); an answer lost from the cache times out.
        return ask.sent.elapsed() > ASK_TIMEOUT
            || pt
                .cache
                .as_ref()
                .is_some_and(|c| c.try_read().is_some_and(|s| s.result(ask.id).is_some()));
    }
    want_mode(v, cx).is_some_and(|w| pt.bracket_told.as_ref() != Some(&w))
        || (no_preset(v, cx) && pt.bracket_refused.is_none())
        || told_off(v, cx).is_some()
}

/// Collect the core's answer to a mode change, or tell it the saved mode (at start, after a
/// new preset, on another account or contract). The core keeps an existing pair (adopted)
/// and applies new offsets to the next position.
pub fn bracket_sync<V: TradePane>(v: &mut V, cx: &mut Context<V>) {
    if v.trade_ref().switch_pending {
        v.trade().switch_pending = false;
        bracket_switch(v, cx);
        // Screenshot fixture only: wait for the core's answer, so the frame shows it.
        #[cfg(any(test, feature = "screenshot"))]
        if let Some(t) = Trading::try_global(cx) {
            t.settle();
        }
    }
    if let Some(ask) = v.trade_ref().bracket_ask.as_ref() {
        let r = v
            .trade_ref()
            .cache
            .as_ref()
            .and_then(|c| c.try_read().and_then(|s| s.result(ask.id).cloned()));
        let r = match r {
            Some(r) => r,
            None if ask.sent.elapsed() > ASK_TIMEOUT => Err(lines_trading::Reject::new(
                lines_trading::RejectCode::Invalid,
                "no answer from the trading core",
            )),
            None => return,
        };
        let Some(ask) = v.trade().bracket_ask.take() else {
            return;
        };
        let position = matches!(ask.mode, lines_trading::BracketMode::Position { .. });
        match r {
            Ok(_) => {
                v.trade().bracket_refused = None;
                if let lines_trading::BracketMode::Position {
                    stop_ticks,
                    target_ticks,
                } = ask.mode
                    && v.trade_ref().bracket.is_none()
                {
                    // The preset the pair uses is the pane's (entries carry it if the core
                    // ever holds per entry again).
                    v.trade().bracket = Some(BracketPreset {
                        stop_ticks,
                        target_ticks,
                    });
                }
                if ask.by_person {
                    let (account, symbol) = (ask.account.0.clone(), ask.symbol.to_owned());
                    crate::picker_view::update_settings(cx, |s| {
                        let e = s.bracket_presets.entry(account).or_default();
                        if position {
                            e.position.insert(symbol);
                        } else {
                            e.position.remove(&symbol);
                        }
                    });
                }
            }
            Err(rej) => {
                // Saved on, so Pos shows: until the core holds it, the cell says it isn't in
                // force (whoever asked). Turning it on from off: only the person's flash.
                let saved = saved_on(cx, &ask.account, ask.symbol);
                match crate::brackets::replace_count(&rej.message) {
                    // Exits to replace: the person confirms (also when the saved mode met them).
                    Some(n) if position => {
                        v.trade().bracket_confirm = Some(BracketConfirm {
                            n,
                            mode: ask.mode,
                            account: ask.account.clone(),
                            contract: ask.contract,
                        });
                    }
                    _ if ask.by_person => v.trade().flash(rej.message.clone()),
                    _ => {}
                }
                if saved {
                    v.trade().bracket_refused = Some(rej.message);
                }
            }
        }
        // Told, whatever the answer: a refusal isn't sent again until something changes.
        v.trade().bracket_told = Some((ask.account, ask.contract, ask.mode));
        cx.notify();
        return;
    }
    if let Some((account, contract)) = told_off(v, cx) {
        send_mode(
            v,
            account,
            contract,
            lines_trading::BracketMode::PerEntry,
            false,
            false,
            cx,
        );
        return;
    }
    if no_preset(v, cx) && v.trade_ref().bracket_refused.is_none() {
        v.trade().bracket_refused =
            Some("no bracket preset for its distances: add one with Edit…".into());
        cx.notify();
        return;
    }
    if let Some((account, contract, mode)) = want_mode(v, cx)
        && v.trade_ref().bracket_told.as_ref() != Some(&(account.clone(), contract, mode))
    {
        // The pair's preset is the pane's from now on (what the row highlights is what an
        // entry carries, should the core hold per entry).
        if v.trade_ref().bracket.is_none() {
            v.trade().bracket = first_preset(v, cx);
        }
        send_mode(v, account, contract, mode, false, false, cx);
    }
}

/// How long a bracket-mode command waits for its answer before it counts as lost.
const ASK_TIMEOUT: Duration = Duration::from_secs(10);

fn send_mode<V: TradePane>(
    v: &mut V,
    account: AccountId,
    contract: ContractKey,
    mode: lines_trading::BracketMode,
    replace: bool,
    by_person: bool,
    cx: &mut Context<V>,
) {
    let Some(t) = Trading::try_global(cx) else {
        return;
    };
    let symbol = v.trade_store().read().product.symbol;
    let id = t.submit(
        Source::Dom,
        Instant::now(),
        TradeCommand::SetBracketMode {
            account: account.clone(),
            contract,
            mode,
            replace,
        },
    );
    v.trade().bracket_ask = Some(BracketAsk {
        id,
        sent: Instant::now(),
        account,
        contract,
        symbol,
        mode,
        by_person,
    });
    cx.notify();
}

/// `TradeCmd::BracketMode`: per entry ↔ one bracket for the whole position.
fn bracket_switch<V: TradePane>(v: &mut V, cx: &mut Context<V>) {
    let (Some(account), Some(contract)) =
        (v.trade_ref().account.clone(), contract_of(v.trade_store()))
    else {
        v.trade().flash("no account or contract");
        return;
    };
    if v.trade_ref().bracket_ask.is_some() {
        v.trade()
            .flash("the last bracket change is still on its way");
        return;
    }
    v.trade().bracket_confirm = None;
    let on = position_on_now(v, cx);
    // On (in force or refused): a click turns it off. Per entry never needs anything of the
    // core, so this always works, refused or not (a way out of "Pos !").
    if on {
        send_mode(
            v,
            account,
            contract,
            lines_trading::BracketMode::PerEntry,
            false,
            true,
            cx,
        );
        return;
    }
    let Some(preset) = v.trade_ref().bracket.or_else(|| first_preset(v, cx)) else {
        v.trade()
            .flash("add a bracket preset first: the position's bracket uses its distances");
        return;
    };
    let mode = lines_trading::BracketMode::Position {
        stop_ticks: preset.stop_ticks,
        target_ticks: preset.target_ticks,
    };
    send_mode(v, account, contract, mode, false, true, cx);
}

/// The confirmation, in the place of quantity, bracket and order buttons: switching to one
/// bracket for the position cancels `n` working exits.
fn bracket_confirm_view<V: TradePane>(
    c: &BracketConfirm,
    v: &V,
    product: &Product,
    h: f32,
    width: f32,
    t: &Theme,
    cx: &mut Context<V>,
) -> gpui::Div {
    let (n, mode) = (c.n, c.mode);
    let (_, units) = presets_and_units(v, product, cx);
    let preset = match mode {
        lines_trading::BracketMode::Position {
            stop_ticks,
            target_ticks,
        } => crate::brackets::label(
            BracketPreset {
                stop_ticks,
                target_ticks,
            },
            product,
            units,
        ),
        lines_trading::BracketMode::PerEntry => String::new(),
    };
    // One compact question everywhere (the rows it replaces are 80 to 114 px): what is cancelled
    // and what replaces it, at most three lines ([`confirm_lines`]), over Keep and Replace.
    let label = c.contract.label(product);
    let what = confirm_text(n, &preset, label.as_str(), width);
    let (keep, replace_text) = ("Keep", format!("Replace {n}"));
    let cancel = tip(
        cell("bracket-confirm-cancel", t)
            .flex_shrink_0()
            .px_2()
            .child(keep),
        "Keep the exits as they are (per entry)",
        None,
        t,
    )
    .on_mouse_down(
        MouseButton::Left,
        cx.listener(|v: &mut V, _, _, cx| {
            cx.stop_propagation();
            v.trade().bracket_confirm = None;
            cx.notify();
        }),
    );
    let replace = tip(
        cell("bracket-confirm-replace", t)
            .flex_shrink_0()
            .px_2()
            .bg(t.accent)
            .border_color(t.accent)
            .text_color(t.on_last)
            .font_weight(FontWeight::BOLD)
            .child(replace_text),
        "Cancel them and place one bracket for the position",
        None,
        t,
    )
    .on_mouse_down(
        MouseButton::Left,
        cx.listener(move |v: &mut V, ev: &gpui::MouseDownEvent, _, cx| {
            cx.stop_propagation();
            if crate::popover::second_press(ev.click_count, ev.position) {
                return;
            }
            // Exactly what it was shown for: this account and contract (a pane that moved
            // drops the confirmation).
            if v.trade_ref().bracket_ask.is_some() {
                v.trade()
                    .flash("the last bracket change is still on its way");
            } else if let Some(c) = v.trade().bracket_confirm.take() {
                send_mode(v, c.account, c.contract, c.mode, true, true, cx);
            }
            cx.notify();
        }),
    );
    div()
        .flex()
        .flex_col()
        .gap_1()
        .h(crate::zoom::ui(h))
        .overflow_hidden()
        .text_size(crate::zoom::ui(11.0))
        .line_height(crate::zoom::ui(CONFIRM_LINE_H))
        // The text gives way (clipped) before the buttons ever do: they sit at the bottom.
        .child(
            div()
                .flex_1()
                .min_h_0()
                .overflow_hidden()
                .text_color(t.text_main)
                .child(what),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .flex_shrink_0()
                .gap_2()
                .mt_1()
                .justify_end()
                .child(cancel)
                .child(replace),
        )
}

/// The confirmation's line height (11 px text).
const CONFIRM_LINE_H: f32 = 14.0;

/// How many lines the confirmation's question takes at `width`, words wrapped whole at the
/// panel's wide glyph estimate: it must be at most three in the rows it replaces.
pub fn confirm_lines(text: &str, width: f32) -> usize {
    let room = width - 2.0 * SIDE;
    let (mut lines, mut used) = (1, 0.0f32);
    for word in text.split(' ') {
        let w = text_w(word, 11.0);
        let need = if used > 0.0 { w + text_w(" ", 11.0) } else { w };
        if used > 0.0 && used + need > room {
            lines += 1;
            used = w;
        } else {
            used += need;
        }
    }
    lines
}

/// The confirmation's question at `width`: the longest of its forms that fits three lines
/// ([`confirm_lines`]): with the contract and "for the whole position", with the contract, or
/// (the narrowest panels) what is cancelled and what replaces it alone.
pub fn confirm_text(n: usize, preset: &str, contract: &str, width: f32) -> String {
    let s = if n == 1 { "" } else { "s" };
    let forms = [
        format!(
            "Replace {n} {contract} exit{s} with one bracket ({preset}) for the whole position?"
        ),
        format!("Replace {n} {contract} exit{s} with one bracket ({preset})?"),
        format!("Replace {n} exit{s} with one bracket ({preset})?"),
    ];
    let last = forms.len() - 1;
    forms
        .into_iter()
        .enumerate()
        .find(|(i, f)| *i == last || confirm_lines(f, width) <= 3)
        .map(|(_, f)| f)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lines_trading::{OrderId, Side};

    /// A chart shows no trading switch, so the switch, order keys, BE and the preset editor
    /// never act from a focused chart (the workspace refuses them); commands that never send
    /// an entry still work there.
    #[test]
    fn charts_never_enable_or_enter_orders() {
        for cmd in [
            TradeCmd::ToggleArm,
            TradeCmd::Enable,
            TradeCmd::Disable,
            TradeCmd::BuyMarket,
            TradeCmd::SellMarket,
            TradeCmd::JoinBid,
            TradeCmd::JoinAsk,
            TradeCmd::Reverse,
            TradeCmd::Breakeven,
            TradeCmd::EditBrackets,
            TradeCmd::BracketMode,
        ] {
            assert!(!chart_runs(cmd), "{cmd:?}");
        }
        for cmd in [
            TradeCmd::Flatten,
            TradeCmd::CancelAll,
            TradeCmd::NextAccount,
            TradeCmd::QtyUp,
            TradeCmd::QtyDown,
            TradeCmd::NextBracket,
            TradeCmd::BracketUnits,
        ] {
            assert!(chart_runs(cmd), "{cmd:?}");
        }
    }

    /// The guardrail strip: state, reason and time left in words; minutes round up; P&L
    /// off (or ticks) keeps the reason but never a loss amount.
    #[test]
    fn guard_strip_text() {
        use lines_core::UtcMillis as T;
        use lines_trading::GuardState as G;
        assert_eq!(guard_line(G::Active, "x", None, None, true), None);
        let now = T(1_000_000);
        let until = T(now.0 + 11 * 60_000 + 1);
        assert_eq!(
            guard_line(
                G::Cooldown,
                "3 losses in a row",
                Some(until),
                Some(now),
                true
            )
            .unwrap(),
            "Cooldown · 3 losses in a row · 12 min left"
        );
        assert_eq!(
            guard_line(G::Cooldown, "3 losses in a row", Some(until), None, true).unwrap(),
            "Cooldown · 3 losses in a row",
            "no clock: no time left rather than a wrong one"
        );
        let loss = "daily loss limit of 1000 reached";
        let off = guard_line(G::Locked, loss, None, Some(now), false).unwrap();
        assert_eq!(
            off,
            "Locked · daily loss limit reached · until the next trade date"
        );
        assert!(!off.bytes().any(|b| b.is_ascii_digit()));
        assert_eq!(
            guard_line(G::Locked, "max 5 trades reached", None, None, false).unwrap(),
            "Locked · max 5 trades reached · until the next trade date"
        );
    }

    #[test]
    fn money_and_average_text() {
        let es = lines_feed::catalog::get(lines_feed::catalog::ES).unwrap();
        assert_eq!(avg_text(&es, 599_950.0), "5999.50");
        // Between ticks: one more decimal than the product shows.
        assert_eq!(avg_text(&es, 599_962.5), "5999.625");
        // Meters: thousands separators, cents only when there are any.
        assert_eq!(money_grouped(Money::whole(1_000)), "$1,000");
        assert_eq!(money_grouped(Money::whole(125)), "$125");
        assert_eq!(money_grouped(Money::cents(12_550)), "$125.50");
        assert_eq!(money_grouped(Money::whole(1_234_567)), "$1,234,567");
        assert_eq!(money_grouped(Money::whole(-340)), "$340");
    }

    /// Rule meters read `used / limit`, warn past 25 %, and show no amount when P&L isn't in
    /// currency.
    #[test]
    fn meters_show_used_of_limit() {
        let limit = Money::whole(1_000);
        let m = meter(Money::whole(875), limit, true);
        assert_eq!(m.text, "$125 / $1,000");
        assert!(!m.hot && (m.share - 0.125).abs() < 1e-6);
        let m = meter(Money::whole(660), limit, true);
        assert_eq!(m.text, "$340 / $1,000");
        assert!(m.hot, "34 % is past the warning share");
        let m = meter(Money::ZERO, limit, true);
        assert!(m.reached && m.share == 1.0);
        // P&L off / ticks: states only.
        assert_eq!(meter(Money::whole(875), limit, false).text, "ok");
        assert_eq!(meter(Money::whole(200), limit, false).text, "near limit");
        assert_eq!(meter(Money::ZERO, limit, false).text, "limit reached");
        assert!(!meter(Money::whole(660), limit, false).text.contains('$'));
    }

    /// BE moves the exit-side stop to the average, on the safe side of the tick grid, only
    /// once the market is past it; never for a missing stop or position.
    #[test]
    fn breakeven_moves_the_stop_to_the_average() {
        let a = AccountId::new("paper");
        let stop = |id: u64, side, p: i64| (OrderId(id), side, Price(p));
        // Long 2 @ 5999.625 (between ticks): BE rounds up to 5999.75, never below the average.
        let cmds = breakeven(
            &a,
            2,
            Some(599_962.5),
            &[stop(7, Side::Sell, 599_750), stop(8, Side::Buy, 600_500)],
            Some(Price(600_100)),
            25,
        )
        .unwrap();
        assert_eq!(
            cmds,
            vec![TradeCommand::ModifyOrder {
                account: a.clone(),
                order: OrderId(7),
                qty: None,
                price: Some(Price(599_975)),
            }],
            "only the sell stop, to the next tick at or above the average"
        );
        // Short 1 @ 6000.10: rounds down.
        let cmds = breakeven(
            &a,
            -1,
            Some(600_010.0),
            &[stop(9, Side::Buy, 600_500)],
            Some(Price(599_500)),
            25,
        )
        .unwrap();
        assert!(matches!(
            &cmds[0],
            TradeCommand::ModifyOrder {
                price: Some(Price(600_000)),
                ..
            }
        ));
        // Refusals.
        assert_eq!(
            breakeven(&a, 0, None, &[], None, 25),
            Err("no position: nothing to move to breakeven")
        );
        assert_eq!(
            breakeven(
                &a,
                1,
                Some(600_000.0),
                &[stop(1, Side::Buy, 1)],
                Some(Price(600_100)),
                25
            ),
            Err("no stop working: place a bracket first"),
            "a buy stop doesn't protect a long"
        );
        assert_eq!(
            breakeven(
                &a,
                1,
                Some(600_000.0),
                &[stop(1, Side::Sell, 599_000)],
                Some(Price(600_000)),
                25
            ),
            Err("price hasn't moved past breakeven yet")
        );
        assert_eq!(
            breakeven(
                &a,
                1,
                Some(600_000.0),
                &[stop(1, Side::Sell, 600_000)],
                Some(Price(600_100)),
                25
            ),
            Err("the stop is already at or past breakeven")
        );
        // Never loosens: a long's stop already trailed past BE stays; only the one below moves.
        let cmds = breakeven(
            &a,
            1,
            Some(600_000.0),
            &[stop(1, Side::Sell, 600_500), stop(2, Side::Sell, 599_000)],
            Some(Price(601_000)),
            25,
        )
        .unwrap();
        assert!(
            matches!(
                &cmds[..],
                [TradeCommand::ModifyOrder {
                    order: OrderId(2),
                    price: Some(Price(600_000)),
                    ..
                }]
            ),
            "{cmds:?}"
        );
        assert_eq!(
            breakeven(
                &a,
                -1,
                Some(600_000.0),
                &[stop(1, Side::Buy, 599_500)],
                Some(Price(599_000)),
                25
            ),
            Err("the stop is already at or past breakeven"),
            "a short's stop below BE stays"
        );
    }

    /// BE's candidates: live protective stops only.
    #[test]
    fn only_live_closing_stops_protect() {
        use lines_core::UtcMillis;
        use lines_trading::{Order, OrderKind, OrderState, Role};
        let es = lines_trading::types::ContractDoc {
            product: 1,
            year: Some(2026),
            month: Some(12),
            equity: false,
            option: None,
        }
        .to_key()
        .unwrap();
        let stop = |side, qty: i64, role, state| Order {
            id: OrderId(1),
            account: AccountId::new("paper"),
            contract: es,
            side,
            qty: Qty(qty),
            kind: OrderKind::Stop {
                trigger: Price(599_000),
            },
            state,
            filled: Qty(0),
            fill_value: 0,
            role,
            parent: None,
            oco: None,
            triggered: false,
            created: UtcMillis(0),
            updated: UtcMillis(0),
            reduce_only: false,
            overfill: Qty(0),
        };
        // Long 1.
        assert!(protects(
            &stop(Side::Sell, 1, Role::StopLoss, OrderState::Working),
            1
        ));
        assert!(protects(
            &stop(Side::Sell, 1, Role::Single, OrderState::Working),
            1
        ));
        assert!(
            !protects(&stop(Side::Sell, 1, Role::StopLoss, OrderState::Acked), 1),
            "held: its entry hasn't filled"
        );
        assert!(
            !protects(&stop(Side::Sell, 3, Role::Single, OrderState::Working), 1),
            "an entry stop that would flip the position"
        );
        assert!(!protects(
            &stop(Side::Sell, 1, Role::Entry, OrderState::Working),
            1
        ));
        assert!(
            !protects(&stop(Side::Buy, 1, Role::StopLoss, OrderState::Working), 1),
            "wrong side"
        );
        assert!(
            !protects(&stop(Side::Sell, 1, Role::StopLoss, OrderState::Working), 0),
            "flat"
        );
        let mut t = stop(Side::Buy, 1, Role::StopLoss, OrderState::Working);
        t.kind = OrderKind::StopLimit {
            trigger: Price(601_000),
            limit: Price(601_000),
        };
        assert!(protects(&t, -1));
        t.triggered = true;
        assert!(!protects(&t, -1), "a triggered stop-limit rests as a limit");
        let mut done = stop(Side::Sell, 1, Role::StopLoss, OrderState::Working);
        done.filled = Qty(1);
        assert!(!protects(&done, 1), "nothing left: drawn as held");

        // Together: long 1 with its stop loss and a breakdown stop of 1: only the stop loss
        // moves (both at BE would trigger together and flip the position).
        let mut sl = stop(Side::Sell, 1, Role::StopLoss, OrderState::Working);
        sl.id = OrderId(5);
        let mut single = stop(Side::Sell, 1, Role::Single, OrderState::Working);
        single.id = OrderId(2);
        let picked: Vec<_> = protective([&single, &sl], 1)
            .unwrap()
            .iter()
            .map(|o| o.id)
            .collect();
        assert_eq!(picked, vec![OrderId(5)]);
        // Two single stops of 1 against long 2: both; against long 1: the first only.
        let mut a = stop(Side::Sell, 1, Role::Single, OrderState::Working);
        a.id = OrderId(3);
        let mut b = stop(Side::Sell, 1, Role::Single, OrderState::Working);
        b.id = OrderId(4);
        assert_eq!(protective([&a, &b], 2).unwrap().len(), 2);
        assert_eq!(
            protective([&b, &a], 1)
                .unwrap()
                .iter()
                .map(|o| o.id)
                .collect::<Vec<_>>(),
            vec![OrderId(3)]
        );
        // A reduce-only stop counts too, and comes after the stop loss: left behind, it
        // cancels itself when the position goes flat (a stop loss left behind would flip it).
        let mut r = stop(Side::Sell, 1, Role::Single, OrderState::Working);
        r.id = OrderId(1);
        r.reduce_only = true;
        assert_eq!(
            protective([&r, &sl], 1)
                .unwrap()
                .iter()
                .map(|o| o.id)
                .collect::<Vec<_>>(),
            vec![sl.id]
        );
        assert_eq!(protective([&r, &sl], 2).unwrap().len(), 2);
        // Taken in the order the market reaches them: a reduce-only stop already past BE
        // (higher, for a long) fills first and uses the budget, so the stop loss below it is
        // not moved (moved to BE, it would fill after the reduce-only one and flip).
        let mut high = r.clone();
        high.kind = OrderKind::Stop {
            trigger: Price(600_200),
        };
        assert_eq!(
            protective([&sl, &high], 1)
                .unwrap()
                .iter()
                .map(|o| o.id)
                .collect::<Vec<_>>(),
            vec![high.id]
        );
        // And BE then refuses: nothing left to move.
        assert_eq!(
            breakeven(
                &AccountId::new("paper"),
                1,
                Some(600_000.0),
                &[(high.id, Side::Sell, Price(600_200))],
                Some(Price(600_300)),
                25
            ),
            Err("the stop is already at or past breakeven")
        );
        // A bracket stop loss whose entry still works: refused (moved to BE, it could fill
        // before the rest of the entry, which would then be left without a stop).
        let mut entry = stop(Side::Buy, 3, Role::Entry, OrderState::PartiallyFilled);
        entry.id = OrderId(20);
        entry.kind = OrderKind::Limit {
            price: Price(599_500),
        };
        entry.filled = Qty(1);
        let mut child = stop(Side::Sell, 1, Role::StopLoss, OrderState::Working);
        child.id = OrderId(21);
        child.parent = Some(entry.id);
        assert_eq!(
            protective([&entry, &child], 1).map(|v| v.len()),
            Err("the bracket's entry is still working: drag the stop instead")
        );
        // Its entry done (not open): fine.
        assert_eq!(protective([&child], 1).unwrap().len(), 1);
        // The budget stops at the first stop that doesn't fit (long 2: 1 @ high, a stop
        // loss of 2 below it, 1 lower still: only the first).
        let mut s1 = stop(Side::Sell, 1, Role::Single, OrderState::Working);
        s1.id = OrderId(30);
        s1.kind = OrderKind::Stop {
            trigger: Price(601_000),
        };
        let mut s2 = stop(Side::Sell, 2, Role::StopLoss, OrderState::Working);
        s2.id = OrderId(31);
        let mut s3 = stop(Side::Sell, 1, Role::Single, OrderState::Working);
        s3.id = OrderId(32);
        s3.kind = OrderKind::Stop {
            trigger: Price(597_000),
        };
        assert_eq!(
            protective([&s1, &s2, &s3], 2)
                .unwrap()
                .iter()
                .map(|o| o.id)
                .collect::<Vec<_>>(),
            vec![s1.id]
        );
        // A stop and a separate (non-bracket) sell limit for long 1: the limit already closes
        // the position, so moving the stop to BE could flip it after the limit fills.
        let mut tp = stop(Side::Sell, 1, Role::Single, OrderState::Working);
        tp.id = OrderId(40);
        tp.kind = OrderKind::Limit {
            price: Price(600_500),
        };
        let mut st = stop(Side::Sell, 1, Role::Single, OrderState::Working);
        st.id = OrderId(41);
        assert_eq!(
            protective([&tp, &st], 1).map(|v| v.len()),
            Err("other exit orders already cover the position: drag the stop instead")
        );
        // A bracket's own target (one-cancels-other with its stop) doesn't: it shrinks the stop.
        let mut bt = tp.clone();
        bt.role = Role::TakeProfit;
        let mut bs = st.clone();
        bs.role = Role::StopLoss;
        bt.oco = Some(bs.id);
        bs.oco = Some(bt.id);
        assert_eq!(protective([&bt, &bs], 1).unwrap().len(), 1);
        // A bracket of 2 (target and stop) plus a single stop above its stop, long 2: the
        // target already covers the position, so the single stop isn't moved (moved to BE it
        // would flip the position after the target fills and cancels the bracket stop).
        let mut t2 = bt.clone();
        t2.id = OrderId(50);
        t2.qty = Qty(2);
        let mut s2 = bs.clone();
        s2.id = OrderId(51);
        s2.qty = Qty(2);
        t2.oco = Some(s2.id);
        s2.oco = Some(t2.id);
        let mut s3 = st.clone();
        s3.id = OrderId(52);
        s3.kind = OrderKind::Stop {
            trigger: Price(599_700),
        };
        assert_eq!(
            protective([&t2, &s2, &s3], 2).map(|v| v.len()),
            Err("other exit orders already cover the position: drag the stop instead")
        );
        // An exit still pending (sent, not acknowledged) counts too.
        let mut pend = tp.clone();
        pend.state = OrderState::Submitted;
        assert_eq!(
            protective([&pend, &st], 1).map(|v| v.len()),
            Err("other exit orders already cover the position: drag the stop instead")
        );
        // Scale-in: two filled brackets of 1 each, long 2: both stops move.
        let mut ta = bt.clone();
        ta.id = OrderId(60);
        let mut sa = bs.clone();
        sa.id = OrderId(61);
        ta.oco = Some(sa.id);
        sa.oco = Some(ta.id);
        let mut tb = bt.clone();
        tb.id = OrderId(62);
        let mut sb = bs.clone();
        sb.id = OrderId(63);
        tb.oco = Some(sb.id);
        sb.oco = Some(tb.id);
        assert_eq!(protective([&ta, &sa, &tb, &sb], 2).unwrap().len(), 2);
        // A filled bracket plus a second entry still working with its exits held (acked):
        // the first bracket's stop moves; the held exits neither count nor move.
        let mut entry2 = stop(Side::Buy, 1, Role::Entry, OrderState::Working);
        entry2.id = OrderId(70);
        entry2.kind = OrderKind::Limit {
            price: Price(599_000),
        };
        let mut held_t = bt.clone();
        held_t.id = OrderId(71);
        held_t.state = OrderState::Acked;
        held_t.parent = Some(entry2.id);
        let mut held_s = bs.clone();
        held_s.id = OrderId(72);
        held_s.state = OrderState::Acked;
        held_s.parent = Some(entry2.id);
        held_t.oco = Some(held_s.id);
        held_s.oco = Some(held_t.id);
        assert_eq!(
            protective([&bt, &bs, &entry2, &held_t, &held_s], 1)
                .unwrap()
                .iter()
                .map(|o| o.id)
                .collect::<Vec<_>>(),
            vec![bs.id]
        );
        // An oversized stop counts too.
        let mut x = stop(Side::Sell, 2, Role::Single, OrderState::Working);
        x.id = OrderId(42);
        assert!(protective([&x, &st], 1).is_err());
        // A short: the lowest buy stop is reached first.
        let mut lo = stop(Side::Buy, 1, Role::StopLoss, OrderState::Working);
        lo.id = OrderId(8);
        lo.kind = OrderKind::Stop {
            trigger: Price(600_100),
        };
        let mut hi = stop(Side::Buy, 1, Role::StopLoss, OrderState::Working);
        hi.id = OrderId(9);
        hi.kind = OrderKind::Stop {
            trigger: Price(600_500),
        };
        assert_eq!(
            protective([&hi, &lo], -1)
                .unwrap()
                .iter()
                .map(|o| o.id)
                .collect::<Vec<_>>(),
            vec![lo.id]
        );
        // Short 2: a partly filled buy stop for 3 with 2 left fits (the remainder counts).
        let mut p = stop(Side::Buy, 3, Role::StopLoss, OrderState::PartiallyFilled);
        p.filled = Qty(1);
        assert_eq!(protective([&p], -2).unwrap().len(), 1);
        // A bracket stop larger than what's left of the position: none.
        let big = stop(Side::Sell, 2, Role::StopLoss, OrderState::Working);
        assert!(protects(&big, 1));
        assert_eq!(
            protective([&big], 1).map(|v| v.len()),
            Err("stops exceed the position: drag the stop instead")
        );
    }

    #[test]
    fn a_split_never_copies_the_switch() {
        let doc = TradeDoc {
            enabled: true,
            account: Some("paper".into()),
            bracket: None,
            hotkeys: true,
        };
        let c = doc.clone().copied();
        assert!(
            !c.enabled && !c.hotkeys,
            "a split starts with trading and keys off"
        );
        assert_eq!(c.account, doc.account);
    }

    #[test]
    fn the_locked_hint_names_the_lock_that_binds() {
        assert!(locked_hint(None, 0, 800.0).starts_with("Entries locked: enable trading ("));
        assert_eq!(
            locked_hint(Some("cooldown"), 0, 800.0),
            "Entries locked: cooldown · trading off"
        );
        // A narrow panel's short form (the long one would be cut).
        assert!(locked_hint(None, 0, 240.0).starts_with("Locked: "));
        assert_eq!(locked_hint(Some("cooldown"), 0, 240.0), "Locked: cooldown");
        // Strategies armed, and this switch the only lock: the count first, "still armed".
        let wide = locked_hint(None, 2, 800.0);
        assert!(
            wide.starts_with("2 strategies still armed · DOM entries locked ("),
            "{wide}"
        );
        assert_eq!(
            locked_hint(None, 1, 420.0),
            "1 strategy still armed · DOM locked"
        );
        assert_eq!(
            locked_hint(None, 2, 240.0),
            "2 strategies still armed · DOM locked"
        );
        // Another lock binds too: never "still" (the core may be refusing their entries as
        // well), and the count stays first.
        for width in [240.0, 420.0, 800.0] {
            let line = locked_hint(Some("news blackout"), 2, width);
            assert!(line.starts_with("2 strategies armed · "), "{line}");
            assert!(line.to_lowercase().contains("news blackout"), "{line}");
            assert!(!line.contains("still"), "{line}");
        }
        assert_eq!(
            locked_hint(Some("market closed"), 2, 800.0),
            "2 strategies armed · Entries locked: market closed"
        );
        assert_eq!(
            locked_hint(Some("market closed"), 1, 240.0),
            "1 strategy armed · Market closed"
        );
    }

    #[test]
    fn refusals_are_one_line_with_a_count() {
        assert_eq!(flash_line(&[]), None);
        let three = vec![
            ("REJECTED", "6000.00: ES market is closed".to_owned()),
            (
                "REJECTED",
                "6004.00: bracket entry not placed: ES market is closed".to_owned(),
            ),
            (
                "REJECTED",
                "5998.00: bracket entry not placed: ES market is closed".to_owned(),
            ),
        ];
        // The newest, its price, and how many more; "rejected" only in the label; the ladder's
        // word ("closed") isn't repeated when the message says it.
        let (label, text) = flash_line(&three).unwrap();
        assert_eq!(label, "REJECTED");
        assert_eq!(
            text,
            "5998.00: bracket entry not placed: ES market is closed  +2 more"
        );
        assert!(!text.to_lowercase().contains("rejected"));
        // A message that doesn't say the ladder's word gets it first.
        let far = [(
            "DENIED",
            "6100.00: price is 400 ticks from the market".to_owned(),
        )];
        assert_eq!(
            flash_line(&far).unwrap().1,
            "6100.00: far · price is 400 ticks from the market"
        );
    }

    /// Review round 3: a narrow panel cut the lockout mid-number ("limit of 10" for 1,000,
    /// "· 1" for 12 min left). Status text is laid out as whole items instead.
    #[test]
    fn status_text_splits_into_whole_items() {
        let parts = |t| status_parts(t).collect::<Vec<_>>();
        assert_eq!(
            parts("Cooldown · 3 losses in a row · 12 min left"),
            [
                ("Cooldown", false),
                ("3 losses in a row", true),
                ("12 min left", true)
            ]
        );
        assert_eq!(
            parts("5830.00: ES market is closed  +2 more"),
            [
                ("5830.00:", false),
                ("ES market is closed", false),
                ("+2 more", false)
            ]
        );
        // Focused review: a single long refusal with numbers is several short items, so the
        // minutes left and the limit are never cut.
        assert_eq!(
            parts("6000.00: Cooldown: 3 losses in a row, 12 min left"),
            [
                ("6000.00:", false),
                ("Cooldown:", false),
                ("3 losses in a row,", false),
                ("12 min left", false)
            ]
        );
        for (p, _) in parts("6000.00: day loss · daily loss limit of 1000 reached") {
            assert!(
                p.chars().count() <= 32,
                "{p:?} is short enough for the narrowest panel"
            );
        }
    }

    /// The switch reads Enabled / Disabled in words; when orders would be denied anyway it
    /// says why in a badge, and stays enabled.
    #[test]
    fn switch_says_enabled_disabled_and_why() {
        assert_eq!(switch_text(true, None, false), "Trading: Enabled");
        assert_eq!(switch_text(false, None, false), "Trading: Disabled");
        assert_eq!(
            switch_text(true, Some("market closed"), false),
            "Enabled · market closed"
        );
        assert_eq!(
            switch_text(false, Some("market closed"), false),
            "Trading: Disabled"
        );
        assert_eq!(switch_text(true, None, true), "Enabled");
        assert_eq!(switch_text(true, Some("market closed"), true), "Enabled !");
        // Strategies armed on the account: the state and the count at every width.
        let closed = Some("market closed");
        assert_eq!(switch_text_armed(false, None, 0, 240.0), "Disabled");
        assert_eq!(switch_text_armed(false, None, 1, 240.0), "Off · 1 strat");
        assert_eq!(switch_text_armed(true, closed, 2, 240.0), "On! · 2 strat");
        assert_eq!(
            switch_text_armed(false, None, 2, 320.0),
            "Disabled · 2 strat"
        );
        assert_eq!(
            switch_text_armed(true, closed, 2, 320.0),
            "Enabled! · 2 strat"
        );
        assert_eq!(
            switch_text_armed(false, None, 1, 420.0),
            "Disabled · 1 strategy armed"
        );
        assert_eq!(
            switch_text_armed(true, closed, 2, 420.0),
            "Enabled! · 2 strategies armed"
        );
        assert_eq!(
            switch_text_armed(false, None, 1, 800.0),
            "Trading: Disabled · 1 strategy armed"
        );
        assert_eq!(
            switch_text_armed(false, closed, 2, 800.0),
            "Trading: Disabled · 2 strategies armed"
        );
        assert_eq!(
            switch_text_armed(true, closed, 2, 800.0),
            "Trading: Enabled! · 2 strategies armed"
        );
        // Not laid out yet: the full layout.
        assert_eq!(
            switch_text_armed(true, None, 2, 0.0),
            "Trading: Enabled · 2 strategies armed"
        );
        // The reason moves below whenever a count is on the switch.
        assert!(reason_on_status(true, 0) && reason_on_status(false, 1));
        assert!(!reason_on_status(false, 0));
        assert_eq!(
            blocked_line("market closed", false),
            "Enabled, but orders are denied: market closed"
        );
        assert_eq!(blocked_line("market closed", true), "Denied: market closed");
    }

    /// The owner's widths (trade panel mock 2), in 100 % units: at 150 % zoom a pane 1.5 times as
    /// wide lays out the same.
    const WIDTHS: [f32; 5] = [MIN_DOM_WIDTH, 200.0, EXITS_ONE_ROW, 320.0, 420.0];

    /// The panel's height is a function of its width (and the Join buttons' setting) only: no
    /// trading state can change it, so the ladder below never shifts. Narrow panels are shorter
    /// (meters behind ▸); tight ones trade the joins' row for BE and Reverse's.
    #[test]
    fn panel_height_depends_on_width_only() {
        let wide = panel_height(420.0);
        assert_eq!(wide, 314.0);
        assert_eq!(wide, panel_height(800.0));
        assert_eq!(
            wide,
            panel_height(0.0),
            "before the first layout: the wide panel"
        );
        let narrow = panel_height(EXITS_ONE_ROW);
        assert_eq!(narrow, panel_height(NARROW_BELOW - 1.0));
        assert!(narrow < wide);
        let tight = panel_height(MIN_DOM_WIDTH);
        assert_eq!(tight, panel_height(EXITS_ONE_ROW - 1.0));
        // Tight: BE and Reverse's row, and the card's two more lines.
        assert_eq!(tight, narrow + ACT_H + 2.0 * CARD_LINE_H);
        // Settings › DOM without the joins: their row shorter, where they'd show.
        let off = PanelRows {
            joins: false,
            ..PanelRows::ALL
        };
        assert_eq!(off.height(420.0), wide - JOIN_H);
        assert_eq!(
            off.height(MIN_DOM_WIDTH),
            tight,
            "a tight panel has none either way"
        );
        // The editor fits in the rows it replaces, with at least one preset row (it scrolls).
        for (rows, w) in [
            (PanelRows::ALL, 420.0),
            (off, 420.0),
            (PanelRows::ALL, MIN_DOM_WIDTH),
        ] {
            let h = rows.editor_h(w);
            // At 80 %, 100 % and 150 %: the hit-target row takes more units below 100 %.
            for z in [0.8, 1.0, 1.5] {
                let n = editor_list_rows(h, w, z);
                let (head, foot) = editor_head_foot(w, z);
                assert!(n >= 1, "{w} at {z}");
                assert!(
                    head + n as f32 * EDITOR_ROW_H + foot + 8.0 <= h + 0.01,
                    "{w} at {z}: {head} + {n} rows + {foot} in {h}"
                );
            }
        }
        assert_eq!(
            editor_list_rows(PanelRows::ALL.editor_h(420.0), 420.0, 1.0),
            3
        );
    }

    /// Every pane height a split leaves a DOM, at every width the owner named and the joins on and
    /// off: what shows always fits the pane with the exits and the account row, a row that went
    /// never comes back in a shorter pane, and a ladder never shows without quantity. A window too
    /// short for that keeps the exits whole down to 53 px.
    #[test]
    fn the_exits_fit_at_every_pane_height() {
        let header = pane_header_h();
        for width in [
            MIN_DOM_WIDTH,
            200.0,
            EXITS_ONE_ROW,
            NARROW_BELOW,
            420.0,
            1400.0,
        ] {
            for joins in [true, false] {
                let mut last = panel_rows(width, 2000.0, false, joins);
                assert_eq!(
                    last,
                    PanelRows {
                        joins,
                        ..PanelRows::ALL
                    }
                );
                for h in (MIN_DOM_PANE_H as u32..2000).rev() {
                    let pane_h = h as f32;
                    let r = panel_rows(width, pane_h, false, joins);
                    assert!(r.account, "{width} x {h}: the minimum holds them");
                    assert!(header + r.height(width) <= pane_h, "{width} x {h}: {r:?}");
                    if r.ladder {
                        assert!(r.entry, "{width} x {h}");
                        assert!(
                            header + r.height(width) + LADDER_MIN_H <= pane_h,
                            "{width} x {h}: {r:?}"
                        );
                    }
                    let kept = |new: bool, old: bool| !new || old;
                    assert!(
                        kept(r.card, last.card)
                            && kept(r.status, last.status)
                            && kept(r.meters, last.meters)
                            && kept(r.entry, last.entry)
                            && kept(r.buttons, last.buttons)
                            && kept(r.actions, last.actions)
                            && kept(r.ladder, last.ladder),
                        "{width} x {h}: {r:?} after {last:?}"
                    );
                    assert_eq!(r.joins, joins, "the setting, never the height");
                    last = r;
                }
                let short = panel_rows(width, MIN_DOM_PANE_H - 1.0, false, joins);
                assert!(!short.account);
                assert!(header + short.height(width) <= 57.0);
                // Without the account row, BE and Reverse go: they'd act on what nothing names.
                assert!(util_shows(false, short, width) && !util_shows(true, short, width));
                assert!(!short.actions_row(width));
                let min = panel_rows(width, MIN_DOM_PANE_H, false, joins);
                if is_tight(width) {
                    // Their own row gives way before the account row.
                    assert!(!min.actions_row(width));
                    let r = panel_rows(width, MIN_DOM_PANE_H + ACT_H, false, joins);
                    assert!(r.actions_row(width) && !util_shows(true, r, width));
                } else {
                    assert!(util_shows(true, min, width));
                }
                // Snapped to device pixels at 150% and 175%, a pane held at the minimum still is
                // one.
                assert!(panel_rows(width, 136.0 / 1.5, false, joins).account);
                assert!(panel_rows(width, 159.0 / 1.75, false, joins).account);
            }
        }
        assert_eq!(pane_header_h(), 27.0, "MIN_DOM_PANE_H counts it");
        // The exits' buttons, centered in their row under the header, are whole from 53 px.
        assert_eq!(
            pane_header_h() + (UTIL_H - (MIN_HIT + 2.0)) / 2.0 + MIN_HIT + 2.0,
            53.0
        );
    }

    /// Where each row goes, in order: a wide panel with the joins and without, a narrow one whose
    /// meters are 18 px shorter, and a tight one whose BE and Reverse row gives way after the card.
    /// Each number is the least pane height at which that row still shows.
    #[test]
    fn rows_give_way_in_order() {
        for (width, joins, full, ladder, meters, status, card, actions) in [
            (420.0, true, 413.0, 335.0, 227.0, 189.0, 165.0, 91.0),
            (420.0, false, 379.0, 335.0, 227.0, 189.0, 165.0, 91.0),
            // Narrow: no joins (they hide first, below NARROW_BELOW).
            (EXITS_ONE_ROW, true, 361.0, 317.0, 209.0, 189.0, 165.0, 91.0),
            (
                MIN_DOM_WIDTH,
                true,
                423.0,
                379.0,
                271.0,
                251.0,
                227.0,
                121.0,
            ),
            (
                MIN_DOM_WIDTH,
                false,
                423.0,
                379.0,
                271.0,
                251.0,
                227.0,
                121.0,
            ),
        ] {
            let at = |h: f32| panel_rows(width, h, false, joins);
            let all = PanelRows {
                joins,
                ..PanelRows::ALL
            };
            assert_eq!(at(full), all, "{width}: all fit");
            let no_buttons = PanelRows {
                buttons: false,
                ..all
            };
            for h in [full - 1.0, ladder] {
                assert_eq!(at(h), no_buttons, "{width} x {h}");
            }
            let no_ladder = PanelRows {
                entry: false,
                ladder: false,
                ..no_buttons
            };
            for h in [ladder - 1.0, meters] {
                assert_eq!(at(h), no_ladder, "{width} x {h}");
            }
            let no_meters = PanelRows {
                meters: false,
                ..no_ladder
            };
            for h in [meters - 1.0, status] {
                assert_eq!(at(h), no_meters, "{width} x {h}");
            }
            let no_status = PanelRows {
                status: false,
                ..no_meters
            };
            for h in [status - 1.0, card] {
                assert_eq!(at(h), no_status, "{width} x {h}");
            }
            let no_card = PanelRows {
                card: false,
                ..no_status
            };
            for h in [card - 1.0, actions] {
                assert_eq!(at(h), no_card, "{width} x {h}");
            }
            let no_actions = PanelRows {
                actions: false,
                ..no_card
            };
            if actions > MIN_DOM_PANE_H {
                for h in [actions - 1.0, MIN_DOM_PANE_H] {
                    assert_eq!(at(h), no_actions, "{width} x {h}");
                }
            }
            let no_account = PanelRows {
                account: false,
                ..no_actions
            };
            for h in [MIN_DOM_PANE_H - 1.0, 1.0] {
                assert_eq!(at(h), no_account, "{width} x {h}");
            }
        }
        assert_eq!(
            panel_rows(0.0, 0.0, false, true),
            PanelRows::ALL,
            "not laid out yet"
        );
        let squeezed = panel_rows(420.0, 0.0, false, true);
        assert!(
            !squeezed.account && !squeezed.ladder,
            "measured at no height"
        );
    }

    /// The editor takes only the rows it replaces: nothing else changes, and where its rows were
    /// showing the panel doesn't move at all. A pane too short closes it rather than hide it.
    #[test]
    fn the_editor_changes_only_its_own_rows() {
        for width in [420.0, MIN_DOM_WIDTH] {
            for joins in [true, false] {
                for h in 26..600 {
                    let pane_h = h as f32;
                    let closed = panel_rows(width, pane_h, false, joins);
                    let open = panel_rows(width, pane_h, true, joins);
                    if !editor_fits(width, pane_h, joins) {
                        assert_eq!(open, closed, "{width} x {h}");
                        continue;
                    }
                    assert!(open.entry && open.buttons, "{width} x {h}");
                    assert_eq!(
                        (
                            open.account,
                            open.card,
                            open.status,
                            open.meters,
                            open.actions,
                            open.joins
                        ),
                        (
                            closed.account,
                            closed.card,
                            closed.status,
                            closed.meters,
                            closed.actions,
                            closed.joins
                        ),
                        "{width} x {h}"
                    );
                    assert!(pane_header_h() + open.height(width) <= pane_h, "{h}");
                    if closed.entry && closed.buttons {
                        assert_eq!(open, closed, "{width} x {h}: the ladder stays put");
                    }
                }
            }
        }
        assert!(editor_fits(0.0, 0.0, true), "not laid out yet");
        assert!(!editor_fits(420.0, 0.0, true), "measured at no height");
        assert!(!editor_fits(420.0, 150.0, true));
    }

    /// The position once the card has gone: direction by shape and word, only what the P&L display
    /// allows, and the P&L apart from the average it can be misread with.
    #[test]
    fn the_position_items_follow_the_display() {
        use crate::pnl::PnlDisplay as P;
        let es = lines_feed::catalog::get(lines_feed::catalog::ES).unwrap();
        let long = Some((2, Some(599_425.0), Some(-17.0), Money::cents(-42_500)));
        let (side, avg, pnl) = position_items(long, P::Both, &es, false);
        assert_eq!(side, "▲ LONG 2");
        assert!(avg.unwrap().starts_with("@ "));
        assert_eq!(pnl.unwrap().0, "−$425.00");
        assert_eq!(position_items(long, P::Both, &es, true).0, "▲ 2", "narrow");
        assert_eq!(position_items(long, P::Off, &es, false).2, None);
        assert_eq!(
            position_items(long, P::Ticks, &es, false).2.unwrap().0,
            "−17.0 t"
        );
        let short = Some((-3, None, None, Money::cents(5_000)));
        assert_eq!(position_items(short, P::Both, &es, false).0, "▼ SHORT 3");
        assert_eq!(position_items(None, P::Both, &es, false).0, "Flat");
    }

    /// The account row never cuts the switch's count or takes the account's last hit target: Keys
    /// shows there only with room, armed strategies and all.
    #[test]
    fn the_account_row_keeps_keys_only_with_room() {
        let armed = |w: f32| switch_words(true, None, 1, w);
        for width in [MIN_DOM_WIDTH, EXITS_ONE_ROW, NARROW_BELOW, 380.0, 479.0] {
            let room = account_row_room(width, &armed(width), true);
            // Everything on the row, at this estimate, in the panel's width.
            let used = BADGE_W + MIN_HIT + switch_w(&armed(width), width) + KEYS_CHIP_W + 3.0 * 8.0;
            assert!((width - 2.0 * SIDE - used - room).abs() < 0.01, "{width}");
        }
        assert!(account_row_room(MIN_DOM_WIDTH, &armed(MIN_DOM_WIDTH), true) < 0.0);
        assert!(account_row_room(479.0, "Enabled", true) >= 0.0);
        // In view: the header, the status line, or this row with room. None of them: order keys
        // send nothing (`dom_trade::key_order`).
        assert!(keys_chip_on_account_row(480.0, false, false));
        assert!(
            keys_chip_on_account_row(0.0, false, false),
            "not laid out yet"
        );
        assert!(keys_chip_on_account_row(400.0, true, false));
        assert!(keys_chip_on_account_row(400.0, false, true));
        assert!(!keys_chip_on_account_row(400.0, false, false));
    }

    /// A short line keeps whole items in priority order: one too wide is left out, never cut, and
    /// a later one that fits still shows. No room at all leaves the buttons their room.
    #[test]
    fn a_short_line_keeps_whole_items_by_priority() {
        assert_eq!(fit_items(100.0, &[40.0, 80.0, 30.0]), [true, false, true]);
        assert_eq!(fit_items(0.0, &[40.0]), [false]);
        assert_eq!(fit_items(-50.0, &[1.0]), [false]);
        assert_eq!(
            fit_items(100.0, &[0.0, 92.0]),
            [false, true],
            "nothing there costs nothing"
        );
    }

    /// Owner's ruling (mock 2): Buy and Sell keep the size and drop the price line first; a size
    /// too long for one line goes whole to the second. Both buttons always read alike.
    #[test]
    fn buy_and_sell_keep_the_size_and_drop_the_price_first() {
        let price = Some("7572.00");
        let full = |w: f32| matches!(market_text(true, 10, price, w, false), MarketText::Full(..));
        assert!(!full(MIN_DOM_WIDTH) && !full(200.0) && !full(EXITS_ONE_ROW));
        assert!(full(320.0) && full(420.0) && full(0.0));
        assert_eq!(
            market_text(true, 10, price, 420.0, false),
            MarketText::Full("Buy 10".into(), "Market · 7572.00".into())
        );
        assert_eq!(
            market_text(false, 10, None, 420.0, false),
            MarketText::Full("Sell 10".into(), "Market · —".into())
        );
        for width in WIDTHS {
            for size in [1, 10, 100, 9999] {
                for (buy, locked) in [(true, false), (false, false), (true, true), (false, true)] {
                    let t = market_text(buy, size, price, width, locked);
                    let other = market_text(!buy, size, price, width, locked);
                    assert_eq!(
                        std::mem::discriminant(&t),
                        std::mem::discriminant(&other),
                        "{width} {size}: both buttons alike"
                    );
                    // The size always shows, whole, in its room (a locked button's 2 px edge
                    // takes a pixel more each side).
                    let (pad, title_px) = market_px(width);
                    let inner = pair_inner(width, pad) - if locked { 2.0 } else { 0.0 };
                    match t {
                        MarketText::Full(a, b) => {
                            assert!(a.ends_with(&size.to_string()));
                            assert!(text_w(&a, title_px) <= inner && text_w(&b, 11.0) <= inner);
                            assert_eq!(locked, b == "Locked", "{width}: {b}");
                        }
                        MarketText::Size(a) => {
                            assert!(a.ends_with(&size.to_string()));
                            assert!(text_w(&a, title_px) <= inner, "{width}: {a}");
                        }
                        MarketText::Stacked(w, s) => {
                            assert_eq!(s, size.to_string());
                            assert!(text_w(w, title_px) <= inner && text_w(&s, title_px) <= inner);
                        }
                    }
                }
            }
        }
        // The narrowest panel keeps a two-digit size on one line, a four-digit one stacked.
        assert_eq!(
            market_text(true, 10, price, MIN_DOM_WIDTH, false),
            MarketText::Size("Buy 10".into())
        );
        assert_eq!(
            market_text(false, 9999, price, MIN_DOM_WIDTH, false),
            MarketText::Stacked("Sell", "9999".into())
        );
    }

    /// Owner's ruling (mock 2): the Join buttons drop their prices first, and narrower still they
    /// are hidden before anything else gives way; Settings › DOM hides them at any width.
    #[test]
    fn the_joins_drop_their_prices_then_hide() {
        let price = Some("7571.75");
        assert_eq!(
            join_text(true, price, 420.0),
            ("Join bid", Some("7571.75".into()))
        );
        assert_eq!(
            join_text(false, price, 320.0),
            ("Join ask", Some("7571.75".into()))
        );
        // Prices go first: still shown, without them (a long price at the narrowest width that
        // shows the joins).
        let long = Some("21450.25");
        assert_eq!(join_text(true, long, NARROW_BELOW), ("Join bid", None));
        assert!(PanelRows::ALL.joins_show(NARROW_BELOW));
        // Then the row: hidden below NARROW_BELOW, before anything else gives way (Reverse's
        // short word, the meters behind ▸ and the short switch words all start there).
        assert_eq!(util_labels(NARROW_BELOW)[1], "Reverse");
        assert!(!is_narrow(NARROW_BELOW));
        for width in WIDTHS.into_iter().chain([260.0, NARROW_BELOW - 1.0]) {
            assert_eq!(
                PanelRows::ALL.joins_show(width),
                !is_narrow(width),
                "{width}"
            );
            assert_eq!(
                PanelRows::ALL.actions_row(width),
                is_tight(width),
                "{width}"
            );
            // Hidden by the setting at every width.
            let off = PanelRows {
                joins: false,
                ..PanelRows::ALL
            };
            assert!(!off.joins_show(width));
            if PanelRows::ALL.joins_show(width) {
                let (words, p) = join_text(true, price, width);
                let inner = pair_inner(width, BUTTON_PX);
                let need = word_w(words) + p.as_deref().map_or(0.0, |p| FIELD_GAP + word_w(p));
                assert!(need <= inner, "{width}: {words} {p:?}");
            }
        }
        assert!(!PanelRows::ALL.joins_show(NARROW_BELOW - 1.0));
        // Short words only where "Join bid" itself has no room (never at a width that shows them).
        assert_eq!(join_text(true, price, 100.0), ("Bid", None));
    }

    /// Mock 2: BE and Reverse are dimmed while flat (Reverse also while entries are), Cancel all
    /// and Flatten never (docs/design.md §3).
    #[test]
    fn be_and_reverse_dim_while_flat_and_exits_never() {
        assert!(exit_dims(TradeCmd::Breakeven, true, false));
        assert!(
            !exit_dims(TradeCmd::Breakeven, false, true),
            "BE only moves a stop"
        );
        assert!(exit_dims(TradeCmd::Reverse, true, false));
        assert!(exit_dims(TradeCmd::Reverse, false, true), "an entry");
        assert!(!exit_dims(TradeCmd::Reverse, false, false));
        for flat in [true, false] {
            for dim in [true, false] {
                assert!(!exit_dims(TradeCmd::Flatten, flat, dim));
                assert!(!exit_dims(TradeCmd::CancelAll, flat, dim));
            }
        }
    }

    /// The card names an exit by role and price, with its quantity when it closes only part of the
    /// position, so a partial stop never reads as the whole (an earlier review: "S1" read as an
    /// index on one pane and a quantity on another).
    #[test]
    fn card_exits_say_a_partial_quantity() {
        assert_eq!(card_exit("SL", 2, 2, "7562.00"), "SL 7562.00");
        assert_eq!(card_exit("TP", 2, -2, "7577.00"), "TP 7577.00");
        assert_eq!(card_exit("SL", 1, 2, "7562.00"), "SL 1 @ 7562.00");
    }

    /// The bracket editor's preset row fits every width without cutting a distance being typed.
    #[test]
    fn the_editor_never_cuts_a_distance() {
        for width in WIDTHS {
            let fit = editor_cells(width);
            let room = width - 2.0 * SIDE;
            let label = text_w("S", fit.text) + text_w("T", fit.text);
            let cell_pad = 2.0 * 4.0 + 2.0;
            let need = text_w(EDITOR_VALUE_SAMPLE, fit.text) + cell_pad;
            match fit.value {
                Some(w) => {
                    assert!(need <= w, "{width}: a distance fits its {w} px cell");
                    let fixed = if fit.marker { 14.0 } else { 0.0 } + label + 2.0 * w;
                    let gaps = 4.0 * if fit.name { 5.0 } else { 4.0 };
                    assert!(fixed + gaps <= room, "{width}: the row fits");
                }
                None => {
                    let gaps = 4.0 * 3.0;
                    let each = (room - label - gaps) / 2.0;
                    assert!(need <= each, "{width}: {need} in {each}");
                }
            }
        }
        assert!(
            !editor_cells(MIN_DOM_WIDTH).name,
            "a tight panel leaves the name out"
        );
        // Tight: the footer holds both preview prices whole at 11 px (an NQ-sized price), and
        // the header the unit word and the four actions.
        let room = MIN_DOM_WIDTH - 2.0 * SIDE;
        assert!(2.0 * text_w("S 21450.25", 11.0) + 4.0 <= room);
        assert!(text_w("Points", 12.0) + 4.0 + 4.0 + 4.0 * MIN_HIT + 3.0 * 4.0 <= room);
    }

    /// The bracket confirmation's question fits the rows it replaces at every width: at most
    /// three lines over the buttons, with ten or more exits and a points preset.
    #[test]
    fn the_confirmation_is_never_cut() {
        for width in WIDTHS {
            let h = PanelRows {
                joins: false,
                ..PanelRows::ALL
            }
            .editor_h(width);
            for (n, preset, contract) in [(1, "8/16 t", "ESZ26"), (12, "100/200 pt", "MNQZ26")] {
                let text = confirm_text(n, preset, contract, width);
                let lines = confirm_lines(&text, width);
                assert!(lines <= 3, "{width}: {text} takes {lines} lines");
                // At 80 %, 100 % and 150 %: the lines, the gap, the buttons' margin and the
                // buttons, a hit target tall (more units below 100 %).
                for z in [0.8, 1.0, 1.5] {
                    let need =
                        lines as f32 * CONFIRM_LINE_H + 4.0 + 4.0 + hit_units(MIN_HIT + 2.0, z);
                    assert!(need <= h, "{width} at {z}: {need} in {h}");
                }
            }
        }
        // The contract and "the whole position" where they fit: "the whole position" goes first,
        // and the contract still shows at the narrowest width.
        let wide = confirm_text(2, "8/16 t", "ESZ26", 320.0);
        assert!(
            wide.contains("ESZ26") && wide.contains("whole position"),
            "{wide}"
        );
        assert_eq!(
            confirm_text(2, "8/16 t", "ESZ26", MIN_DOM_WIDTH),
            "Replace 2 ESZ26 exits with one bracket (8/16 t)?"
        );
        // Where nothing longer fits three lines, the plain question.
        assert_eq!(
            confirm_text(2, "8/16 t", "ESZ26", 60.0),
            "Replace 2 exits with one bracket (8/16 t)?"
        );
    }

    /// A read-only account's exits say "off" whole: both where they fit, else one "Exits off".
    #[test]
    fn read_only_exits_are_never_cut() {
        assert!(off_exits_fit(0.0));
        assert!(off_exits_fit(480.0));
        for width in WIDTHS.into_iter().chain([260.0, NARROW_BELOW]) {
            let size = util_text(width);
            let room = width - 2.0 * SIDE;
            if !off_exits_fit(width) {
                let need = text_w("Exits off", size) + 2.0 * 4.0 + 2.0;
                let others = if is_tight(width) {
                    0.0
                } else {
                    let l = util_labels(width);
                    text_w(l[0], size).max(MIN_HIT - 10.0)
                        + text_w(l[1], size)
                        + 2.0 * (2.0 * 4.0 + 2.0)
                        + 2.0 * 4.0
                };
                assert!(need + others <= room, "{width}: Exits off fits");
            }
        }
        assert!(!off_exits_fit(MIN_DOM_WIDTH), "165 px merges them");
    }

    /// The card's lower items: the working count is never left out; ticks go first, then the
    /// target, then the stop, then the average. Tight panels have one more line for them.
    #[test]
    fn the_card_never_drops_the_working_count() {
        assert_eq!(card_h(165.0), card_h(200.0));
        assert!(
            card_h(165.0) > card_h(240.0),
            "tight panels have more card lines"
        );
        assert_eq!(card_h(240.0), card_h(420.0));
        let w = |s: &str| text_w(s, 11.0);
        // In the card's order: the stop, the target, the average (when not above), working. The
        // ticks sit under the open P&L on the first line's right, the day's P&L on the last.
        let sl = (w("SL 6006.00") + 4.0, 5);
        let tp = (w("TP 6014.00") + 4.0, 5);
        let avg = (w("@ 6010.25"), 3);
        let working = (w("2 working"), CARD_KEEP);
        let items = [sl, tp, avg, working];
        let rooms_at = |width: f32, day: &str| {
            let inner = card_inner(width);
            let lines = card_lines(width);
            let mut r = vec![inner; lines];
            r[0] -= w("−12.5 t") + 8.0;
            r[lines - 1] -= w("Day ") + w(day) + 12.0;
            r
        };
        for width in WIDTHS {
            for day in ["$0.00", "+$125.00", "−$13,408.75"] {
                let rooms = rooms_at(width, day);
                let place = card_place(&rooms, &items);
                assert!(place[3].is_some(), "{width} {day}: the working count shows");
                // The stop and the target show at every width the owner lays out.
                assert!(
                    place[0].is_some() && place[1].is_some(),
                    "{width} {day}: {place:?}"
                );
                // What shows fits its line.
                for (l, room) in rooms.iter().enumerate() {
                    let on: Vec<f32> = items
                        .iter()
                        .zip(&place)
                        .filter(|(_, p)| **p == Some(l))
                        .map(|(i, _)| i.0)
                        .collect();
                    let used = on.iter().sum::<f32>() + 8.0 * on.len().saturating_sub(1) as f32;
                    assert!(used <= *room, "{width}: line {l} holds {used} in {room}");
                }
                // Left out in order: the average, then the target, then the stop.
                let order = [2, 1, 0];
                let dropped: Vec<usize> =
                    order.into_iter().filter(|i| place[*i].is_none()).collect();
                assert_eq!(dropped, order[..dropped.len()], "{width} {day}: in order");
            }
        }
        // At the narrowest everything shows with a modest day.
        let place = card_place(&rooms_at(MIN_DOM_WIDTH, "$0.00"), &items);
        assert!(place.iter().all(Option::is_some), "{place:?}");
        // Nothing fits: the working count still shows, on the first line.
        assert_eq!(card_place(&[0.0], &items), vec![None, None, None, Some(0)]);
    }

    /// The card's side and P&L are never cut: the average goes first, then the large text steps
    /// down; a flat card's day P&L moves under "Flat" where it doesn't fit beside it.
    #[test]
    fn the_card_keeps_side_and_pnl_whole() {
        for width in WIDTHS {
            for (side, pnl) in [
                ("▲ 200", "+$50,000.00"),
                ("▼ 2", "−$13,408.75"),
                ("▲ 10", "+$0.00"),
                ("▲ 200", "−$123,408.75"),
            ] {
                let px = card_big_px(width, side, Some(pnl));
                assert!(
                    text_w(side, px) + 8.0 + text_w(pnl, px) <= card_inner(width),
                    "{width}: {side} {pnl} at {px}"
                );
                if card_avg_fits(width, side, "@ 7567.00", Some(pnl), px) {
                    assert!(
                        text_w(side, px) + 6.0 + word_w("@ 7567.00") + 8.0 + text_w(pnl, px)
                            <= card_inner(width)
                    );
                }
            }
        }
        assert_eq!(card_big_px(420.0, "▲ 200", Some("−$13,408.75")), 15.0);
        assert!(card_big_px(MIN_DOM_WIDTH, "▲ 200", Some("−$13,408.75")) < 15.0);
        assert!(card_avg_fits(
            420.0,
            "▲ 200",
            "@ 7567.00",
            Some("+$50,000.00"),
            15.0
        ));
        assert!(!card_avg_fits(
            MIN_DOM_WIDTH,
            "▲ 200",
            "@ 7567.00",
            Some("+$50,000.00"),
            13.0
        ));
        assert!(flat_day_on_first_line(420.0, "−$13,408.75"));
        assert!(!flat_day_on_first_line(MIN_DOM_WIDTH, "−$13,408.75"));
    }

    /// The Qty and Bracket fields always show their values, a size over the account's maximum
    /// always says so, and position mode always shows while on; the rest gives way in order.
    #[test]
    fn the_fields_keep_their_values() {
        let pos = Some(("· position", "pos"));
        for width in WIDTHS {
            let (pad, gap) = if is_tight(width) {
                (FIELD_TIGHT_PX, FIELD_TIGHT_GAP)
            } else {
                (FIELD_PX, FIELD_GAP)
            };
            let room = width - 2.0 * SIDE - PAIR_GAP;
            for (key, qty, bracket, mode) in [
                ("Qty", "10", "8/16 t", None),
                ("Qty", "10", "8/16 t", pos),
                ("Qty", "999", "20/40 pt", pos),
                ("max 5", "10", "8/16 t", None),
            ] {
                let f = field_fit(width, key, qty, bracket, mode);
                let need = fields_w(pad, gap, f, key, qty, bracket, mode);
                // In a tight panel, over the maximum or with a long size or bracket: the bracket's
                // value may give way with "…" (its list names it in full); nothing else ever does.
                if !is_tight(width) || (key == "Qty" && qty.len() <= 2 && bracket.len() <= 6) {
                    assert!(
                        need <= room,
                        "{width}: {key} {qty} {bracket} {mode:?}: {need}"
                    );
                }
            }
        }
        // The order things give way: "Bracket", the long mode word, "Qty".
        let all = field_fit(420.0, "Qty", "10", "8/16 t", pos);
        assert!(all.qty_key && all.bracket_key && all.long_mode);
        let tight = field_fit(MIN_DOM_WIDTH, "Qty", "10", "8/16 t", pos);
        assert!(!tight.bracket_key && !tight.long_mode && !tight.qty_key);
        let mid = field_fit(200.0, "Qty", "10", "8/16 t", None);
        assert!(!mid.bracket_key && mid.qty_key, "{mid:?}");
        let both = field_fit(EXITS_ONE_ROW, "Qty", "10", "8/16 t", None);
        assert!(both.bracket_key && both.qty_key, "{both:?}");
    }

    /// The panel at every width the owner named: every row's items fit, at a wide 12 px glyph (7.5
    /// px a character), with ≥ 20 px hit targets.
    #[test]
    fn panel_fits_the_narrowest_dom() {
        const CHAR: f32 = 7.5;
        const PAD: f32 = 2.0 * 4.0 + 2.0; // px_1 + border
        let w = |s: &str| (s.chars().count() as f32 * CHAR + PAD).max(MIN_HIT);
        for width in WIDTHS {
            let room = width - 2.0 * SIDE;
            // Exits: Cancel all and Flatten keep their words at every width.
            let util = util_labels(width);
            assert_eq!(util[2], "Cancel all");
            assert_eq!(util[3], "Flatten", "Flatten keeps its word");
            let px = util_text(width);
            let b = |s: &str| (text_w(s, px) + PAD).max(MIN_HIT);
            let flatten = b(util[3]) + 2.0;
            if is_tight(width) {
                assert!(b(util[2]) + flatten + 4.0 <= room, "{width}: exits");
                assert!(
                    b(util[0]) + b(util[1]) + 4.0 <= room,
                    "{width}: BE, Reverse"
                );
            } else {
                let need = b(util[0]) + b(util[1]) + b(util[2]) + flatten + 3.0 * 4.0;
                assert!(need <= room, "{width}: util row {need} > {room}");
            }
            // The account row: the switch's words in every state without strategies leave the
            // account a hit target beside PAPER.
            for (enabled, blocker) in [
                (false, None),
                (true, None),
                (true, Some("market closed")),
                (true, Some("contract unknown")),
                (true, Some("feed stale 1234 s")),
            ] {
                let words = switch_words(enabled, blocker, 0, width);
                assert!(
                    switch_w(&words, width) <= switch_room(width),
                    "{width}: {words}"
                );
                // A reason that isn't on the switch is on the status line.
                if enabled && blocker.is_some() && !words.contains(blocker.unwrap()) {
                    assert!(reason_below(enabled, blocker, 0, width), "{width}: {words}");
                }
            }
            // With strategies armed, a two-digit count and a blocker: the count is never cut,
            // beside the badge (the account gives way).
            let words = switch_words(true, Some("market closed"), 12, width);
            assert!(words.contains("12 strat"), "{words}");
            assert!(
                w("PAPER") + 4.0 + 8.0 + switch_w(&words, width) <= room,
                "{width}: {words}"
            );
            // The position-bracket confirmation's buttons (narrow labels, a two-digit count).
            let confirm = w("Keep") + w("Replace 99") + 8.0;
            assert!(confirm <= room, "confirmation buttons {confirm}");
        }
        // The tight switch is its track alone, "!" when orders would be denied, or the count.
        assert_eq!(switch_words(false, None, 0, MIN_DOM_WIDTH), "");
        assert_eq!(switch_words(true, None, 0, MIN_DOM_WIDTH), "");
        assert_eq!(
            switch_words(true, Some("market closed"), 0, MIN_DOM_WIDTH),
            "!"
        );
        assert_eq!(switch_words(true, None, 2, MIN_DOM_WIDTH), "2 strat");
        assert!(reason_below(true, Some("market closed"), 0, MIN_DOM_WIDTH));
        // From NARROW_BELOW the switch says why, where it fits.
        assert_eq!(
            switch_words(true, Some("market closed"), 0, 420.0),
            "Enabled · market closed"
        );
        assert!(!reason_below(true, Some("market closed"), 0, 420.0));
        // Lines the narrowest panel that shows the joins' place keeps whole (a tight panel lays them
        // out the same way, as whole items: what doesn't fit is left out, never cut).
        let room = EXITS_ONE_ROW - 2.0 * SIDE;
        // The disabled panel's line: whole at each width's form, and with strategies armed its
        // first item (the count) is always whole, with a two-digit count, at the narrowest
        // (`whole_items` leaves out whatever follows it, whole).
        let line = |s: &str| s.chars().count() as f32 * CHAR;
        let long = Some("feed stale 1234 s");
        for (hint, w, what) in [
            (
                locked_hint(None, 0, EXITS_ONE_ROW),
                EXITS_ONE_ROW,
                "no count",
            ),
            (
                locked_hint(long, 0, EXITS_ONE_ROW),
                EXITS_ONE_ROW,
                "no count, reason",
            ),
            (
                locked_hint(None, 0, LOCKED_HINT_FULL),
                LOCKED_HINT_FULL,
                "no count at 400",
            ),
            (locked_hint(None, 12, 320.0), 320.0, "count at 320"),
            (locked_hint(long, 12, 340.0), 340.0, "count, reason at 340"),
            (
                locked_hint(None, 12, LOCKED_HINT_ARMED_FULL),
                LOCKED_HINT_ARMED_FULL,
                "count at 520",
            ),
            (
                locked_hint(long, 12, LOCKED_HINT_ARMED_FULL),
                LOCKED_HINT_ARMED_FULL,
                "count, reason at 520",
            ),
        ] {
            assert!(line(&hint) <= w - 16.0, "{what}: {hint}");
        }
        for hint in [
            locked_hint(None, 12, EXITS_ONE_ROW),
            locked_hint(long, 12, EXITS_ONE_ROW),
        ] {
            let first = status_parts(&hint).next().expect("items").0;
            assert!(first.contains("12 strateg"), "{hint}");
            assert!(line(first) <= room, "{first}");
        }
        // The denied line keeps its longest reason, whole (with the Keys chip on the line too,
        // a long reason is left out whole: the switch's tooltip has it).
        assert!(line(&blocked_line("feed stale 1234 s", true)) + 12.0 <= room);
        // From KEYS_IN_HEADER: the account, the Keys chip (knob and px_2 on top of `w`) and
        // the widest wide switch text fit one row.
        let room_k = KEYS_IN_HEADER - 2.0 * SIDE;
        let keys = w(keys_text(false, false)).max(w(keys_text(true, false))) + 12.0 + 8.0;
        let widest = [
            "contract unknown",
            "market closed",
            "feed stale 1234 s",
            "news blackout",
        ]
        .iter()
        .map(|b| {
            switch_w(
                &switch_words(true, Some(b), 0, KEYS_IN_HEADER),
                KEYS_IN_HEADER,
            )
        })
        .fold(
            switch_w(
                &switch_words(false, None, 0, KEYS_IN_HEADER),
                KEYS_IN_HEADER,
            ),
            f32::max,
        );
        // Keys on in a narrow DOM: the chip trails the status line beside a flash whose
        // label is the longest and whose first item a 10-character price ("104523.50:").
        let flash = ["DENIED", "REJECTED", "WARNING", "NOT SENT"]
            .iter()
            .map(|l| w(l) - 2.0)
            .fold(0.0, f32::max)
            + 8.0
            + 10.0 * CHAR;
        let chip = w(keys_text(true, true)) + 20.0;
        assert!(
            flash + 8.0 + chip <= room,
            "status line with Keys {} > {room}",
            flash + 8.0 + chip
        );
        assert!(keys_in_header(0.0) && keys_in_header(KEYS_IN_HEADER));
        assert!(!keys_in_header(KEYS_IN_HEADER - 1.0) && !keys_in_header(MIN_DOM_WIDTH));
        let header = BADGE_W + MIN_HIT + keys + widest + 3.0 * 8.0;
        assert!(header <= room_k, "header with Keys {header} > {room_k}");
        const { assert!(MIN_HIT >= 20.0) };
        assert!(is_narrow(MIN_DOM_WIDTH) && !is_narrow(NARROW_BELOW) && !is_narrow(0.0));
        assert!(is_tight(MIN_DOM_WIDTH) && !is_tight(EXITS_ONE_ROW) && !is_tight(0.0));
    }

    /// The card's stop is any live stop that protects the position, as BE decides: "No stop"
    /// is never said while one is working.
    #[test]
    fn the_card_stop_is_any_stop_that_protects() {
        use lines_core::{ContractKey, Month, UtcMillis};
        use lines_trading::{Order, OrderId, OrderKind, OrderState, Role};
        let k = ContractKey::monthly(lines_feed::catalog::ES, 2026, Month::new(12).unwrap());
        let stop = |id: u64, side: Side, at: i64, qty: i64, reduce_only: bool| Order {
            id: OrderId(id),
            account: AccountId::new("paper-1"),
            contract: k,
            side,
            qty: Qty(qty),
            kind: OrderKind::Stop { trigger: Price(at) },
            state: OrderState::Working,
            filled: Qty(0),
            fill_value: 0,
            role: Role::Single,
            parent: None,
            oco: None,
            triggered: false,
            created: UtcMillis(0),
            updated: UtcMillis(0),
            reduce_only,
            overfill: Qty(0),
        };
        let last = Some(Price(601_025));
        // A plain sell stop placed on the ladder protects a long 2: the card names it.
        let plain = [stop(1, Side::Sell, 600_600, 2, false)];
        assert_eq!(card_stop(plain.iter(), 2, last), Some((Price(600_600), 2)));
        // An exit (reduce-only) too.
        let exit = [stop(2, Side::Sell, 600_600, 2, true)];
        assert_eq!(card_stop(exit.iter(), 2, last), Some((Price(600_600), 2)));
        // A buy stop adds to a long: no stop, so the card says "No stop".
        let entry = [stop(3, Side::Buy, 601_200, 1, false)];
        assert_eq!(card_stop(entry.iter(), 2, last), None);
        // A plain stop larger than the position would flip it: not protection.
        let flips = [stop(4, Side::Sell, 600_600, 5, false)];
        assert_eq!(card_stop(flips.iter(), 2, last), None);
        // The nearest of two.
        let two = [
            stop(5, Side::Sell, 600_000, 1, true),
            stop(6, Side::Sell, 600_800, 1, true),
        ];
        assert_eq!(card_stop(two.iter(), 2, last), Some((Price(600_800), 1)));
        // A bracket's stop-loss leg stays the stop once its stop-limit has triggered.
        let mut leg = stop(7, Side::Sell, 600_600, 2, false);
        leg.role = Role::StopLoss;
        leg.kind = OrderKind::StopLimit {
            trigger: Price(600_600),
            limit: Price(600_575),
        };
        leg.triggered = true;
        // Triggered, it rests at its limit: that is the price shown.
        assert_eq!(
            card_stop([leg.clone()].iter(), 2, last),
            Some((Price(600_575), 2))
        );
        // A stop-loss leg on the wrong side (left from an earlier short) protects nothing.
        let mut wrong = leg;
        wrong.side = Side::Buy;
        assert_eq!(card_stop([wrong].iter(), 2, last), None);
        // A short's buy stop protects it.
        let short = [stop(8, Side::Buy, 601_400, 1, false)];
        assert_eq!(card_stop(short.iter(), -2, last), Some((Price(601_400), 1)));
    }

    /// A market order still working (a thin book's remainder) says so, with what is left.
    #[test]
    fn a_working_market_order_says_what_is_left() {
        use lines_core::{ContractKey, Month, UtcMillis};
        use lines_trading::{Order, OrderId, OrderKind, OrderState, Role};
        let k = ContractKey::monthly(lines_feed::catalog::ES, 2026, Month::new(12).unwrap());
        let order = |id: u64, side: Side, kind: OrderKind, qty: i64, filled: i64| Order {
            id: OrderId(id),
            account: AccountId::new("paper-1"),
            contract: k,
            side,
            qty: Qty(qty),
            kind,
            state: OrderState::PartiallyFilled,
            filled: Qty(filled),
            fill_value: 0,
            role: Role::Flatten,
            parent: None,
            oco: None,
            triggered: false,
            created: UtcMillis(0),
            updated: UtcMillis(0),
            reduce_only: true,
            overfill: Qty(0),
        };
        let orders = [
            order(1, Side::Sell, OrderKind::Market, 5, 2),
            order(
                2,
                Side::Sell,
                OrderKind::Limit {
                    price: Price(600_000),
                },
                1,
                0,
            ),
        ];
        let left = market_left(orders.iter());
        assert_eq!(left, Some((0, 3)));
        assert_eq!(market_words(left.unwrap()), "market sell 3 left");
        assert_eq!(market_left(orders[1..].iter()), None);
        let mut orders = orders.to_vec();
        // On its way (no answer yet): not shown.
        let mut sent = order(3, Side::Buy, OrderKind::Market, 1, 0);
        sent.state = OrderState::Submitted;
        orders.push(sent);
        assert_eq!(market_left(orders.iter()), Some((0, 3)));
        // Both sides: both said.
        orders.push(order(4, Side::Buy, OrderKind::Market, 2, 1));
        assert_eq!(
            market_words(market_left(orders.iter()).unwrap()),
            "market buy 1, sell 3 left"
        );
    }

    /// The switch is saved with the pane and restored (sticky across a restart); disabled
    /// writes nothing.
    #[test]
    fn trade_doc_saves_the_switch_and_choices() {
        assert_eq!(serde_json::to_string(&TradeDoc::default()).unwrap(), "{}");
        let d = TradeDoc {
            enabled: true,
            account: Some("paper".into()),
            bracket: Some(BracketPreset {
                stop_ticks: 8,
                target_ticks: 16,
            }),
            hotkeys: true,
        };
        let json = serde_json::to_string(&d).unwrap();
        assert!(json.starts_with(r#"{"enabled":true"#), "{json}");
        let back: TradeDoc = serde_json::from_str(&json).unwrap();
        assert_eq!(back, d);
        // A file from before the switch was saved: disabled. Its per-pane `qty` (from before
        // the shared size) still loads and is ignored.
        let old: TradeDoc = serde_json::from_str(r#"{"qty":2}"#).unwrap();
        assert!(!old.enabled);
        assert!(
            !old.hotkeys,
            "a pane saved before order hotkeys has them off"
        );
        // Restored state is the saved one, and the events that used to disable it don't.
        let mut a = Arming::new(back.enabled);
        for ev in [
            ArmEvent::FocusLost,
            ArmEvent::AccountChanged,
            ArmEvent::ContractChanged,
        ] {
            a.apply(ev, dom_trade::ArmPrefs::default());
        }
        assert!(
            a.is_armed(),
            "sticky across focus, account and contract changes"
        );
    }

    /// Impossible trading denies the order with its reason and leaves the switch on: the
    /// order builder takes the switch by value and never changes it.
    #[test]
    fn impossible_trading_denies_without_flipping_the_switch() {
        let mut a = Arming::default();
        a.apply(ArmEvent::Toggle, dom_trade::ArmPrefs::default());
        let ctx = dom_trade::EntryCtx {
            account: AccountId::new("paper"),
            contract: lines_core::ContractKey::monthly(
                lines_feed::catalog::ES,
                2026,
                lines_core::Month::new(12).unwrap(),
            ),
            qty: Qty(1),
            // No position: Reverse is refused.
            position: 0,
            bracket: None,
            tick: 25,
            best_bid: None,
            best_ask: None,
            last: None,
        };
        assert!(dom_trade::quick(dom_trade::QuickOrder::Reverse, a, &ctx).is_err());
        assert!(a.is_armed(), "denied, still enabled");
    }
}
