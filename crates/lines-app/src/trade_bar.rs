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
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, Context, FontWeight, InteractiveElement, IntoElement, MouseButton,
    ParentElement, Styled, Subscription, Window, div, prelude::*, px,
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
    pub qty: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bracket: Option<BracketPreset>,
}

/// One pane's trading state.
pub struct PaneTrade {
    pub account: Option<AccountId>,
    pub qty: i64,
    pub bracket: Option<BracketPreset>,
    pub arming: Arming,
    /// Typing a custom quantity (digits so far).
    pub qty_edit: Option<String>,
    pub account_menu: bool,
    /// A refusal made in the UI (not by the core), with when.
    local_flash: Option<(String, Instant)>,
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
}

impl TradeDoc {
    /// The choices for a new pane made from this one (a split): everything but the trading
    /// switch, which starts disabled in every new pane.
    pub fn copied(self) -> TradeDoc {
        TradeDoc {
            enabled: false,
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
            // A new pane: Settings › Trading's default quantity.
            qty: saved
                .and_then(|s| s.qty)
                .filter(|&q| q > 0)
                .unwrap_or_else(|| {
                    crate::picker_view::settings(cx)
                        .new_panes
                        .default_qty
                        .map_or(1, i64::from)
                }),
            bracket: saved.and_then(|s| s.bracket),
            // Sticky: restored as saved (new panes: disabled).
            arming: Arming::new(saved.is_some_and(|s| s.enabled)),
            qty_edit: None,
            account_menu: trading.is_some_and(|t| t.start_account_menu),
            local_flash: None,
            cache: trading.map(|t| t.cache.clone()),
            seen: u64::MAX,
            seen_display: crate::pnl::display_epoch(),
            watched: None,
            meters_open: false,
            editor: None,
            editor_msg: None,
        }
    }

    pub fn doc(&self) -> Option<TradeDoc> {
        let d = TradeDoc {
            enabled: self.arming.is_armed(),
            account: self.account.as_ref().map(|a| a.0.clone()),
            qty: (self.qty != 1).then_some(self.qty),
            bracket: self.bracket,
        };
        (d != TradeDoc::default()).then_some(d)
    }

    /// The trading state moved for this pane's account (or a local flash expired).
    pub fn changed(&mut self) -> bool {
        let mut changed = false;
        let e = crate::pnl::display_epoch();
        if e != self.seen_display {
            self.seen_display = e;
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

    pub fn flash(&mut self, msg: impl Into<String>) {
        self.local_flash = Some((msg.into(), Instant::now()));
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
        self.account_menu = false;
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
        let id = window.window_handle().window_id();
        if view.trade_ref().watched.as_ref().is_some_and(|w| w.0 == id) {
            return;
        }
        let blur = cx.on_focus_out(focus, window, |v: &mut V, _, _, cx| {
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
    /// The pane's feed can't serve its target (nothing to trade).
    fn trade_unavailable(&self) -> bool;
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
    let (position, working) = {
        let s = t.cache.read();
        (
            s.position(&account, contract).map_or(0, |p| p.qty),
            s.open_orders(&account, contract).count(),
        )
    };
    let (best_bid, best_ask, last, tick) = v
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
    Ok(dom_trade::EntryCtx {
        account,
        contract,
        qty: qty(pt),
        position,
        working,
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
                | TradeCmd::Enable
                | TradeCmd::Disable
                | TradeCmd::Breakeven
                | TradeCmd::EditBrackets
        )
}

/// Run a trading command from a hotkey or the palette. `issued`: the input time.
pub fn run<V: TradePane>(v: &mut V, cmd: TradeCmd, issued: Instant, cx: &mut Context<V>) {
    run_from(v, cmd, issued, Source::Hotkey, cx);
}

/// Run a trading command; `source`: where it came from (header buttons: `Source::Dom`).
pub fn run_from<V: TradePane>(
    v: &mut V,
    cmd: TradeCmd,
    issued: Instant,
    source: Source,
    cx: &mut Context<V>,
) {
    if let Some(q) = cmd.quick() {
        // Header order buttons and their keys: gated by arming exactly like ladder clicks.
        let built = entry_ctx(v, cx)
            .map_err(dom_trade::Refusal)
            .and_then(|ctx| dom_trade::quick(q, v.trade_ref().arming, &ctx));
        match built {
            Ok(c) => {
                if let Some(t) = Trading::try_global(cx) {
                    t.submit(source, issued, c);
                }
            }
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
            let q = v.trade_ref().qty;
            v.trade().qty = step_qty(q, cmd == TradeCmd::QtyUp);
            v.trade().qty_edit = None;
        }
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
            v.trade().bracket = match presets {
                Some(p) => crate::brackets::next_choice(&p, b),
                None => next_bracket(b),
            };
        }
        TradeCmd::BuyMarket
        | TradeCmd::SellMarket
        | TradeCmd::JoinBid
        | TradeCmd::JoinAsk
        | TradeCmd::Reverse => {} // handled above
    }
    cx.notify();
}

pub fn set_account<V: TradePane>(v: &mut V, account: Option<AccountId>, cx: &mut Context<V>) {
    close_account_menu(v, cx);
    let t = v.trade();
    if account.is_some() && account != t.account {
        t.account = account;
        t.seen = u64::MAX;
        v.trade().arm_event(ArmEvent::AccountChanged, cx);
        v.trade().editor = None;
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

/// Keys while a custom quantity is being typed. Returns whether the key was used.
pub fn qty_key<V: TradePane>(v: &mut V, key: &str, cx: &mut Context<V>) -> bool {
    let t = v.trade();
    let Some(edit) = t.qty_edit.as_mut() else {
        return false;
    };
    match key {
        "enter" => match parse_qty(edit) {
            Some(q) => {
                t.qty = q;
                t.qty_edit = None;
            }
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
    /// (label, text): DENIED / REJECTED / WARNING / NOT SENT.
    flashes: Vec<(&'static str, String)>,
    unresolved: usize,
    working: usize,
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
                data.stop = near(dom_trade::MarkRole::Stop);
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
        data.flashes.push(("NOT SENT", m.to_owned()));
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
        return Some(b);
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

/// A narrow panel's status line for an enabled switch whose orders would be denied.
pub fn blocked_line(blocker: &str) -> String {
    format!("Enabled, but orders are denied: {blocker}")
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
        .h(px(ITEMS_LINE_H))
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
    let digits = whole.to_string();
    let mut out = String::with_capacity(digits.len() + 6);
    out.push('$');
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
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

/// Below this width the panel collapses (meters behind ▸, short labels).
pub const NARROW_BELOW: f32 = 300.0;

/// The narrowest DOM the panel is designed for (layout test below).
#[cfg_attr(not(test), allow(dead_code))]
pub const MIN_DOM_WIDTH: f32 = 240.0;

/// Whether a panel `width` is narrow (0: not laid out yet, full layout).
pub fn is_narrow(width: f32) -> bool {
    width > 0.0 && width < NARROW_BELOW
}

const HDR_H: f32 = 28.0;
const CARD_H: f32 = 58.0;
const STATUS_H: f32 = 24.0;
const METERS_H: f32 = 38.0;
const METERS_NARROW_H: f32 = 20.0;
const SEG_H: f32 = 26.0;
const GRID_H: f32 = 78.0;
const UTIL_H: f32 = 30.0;
const PAD_Y: f32 = 6.0;
/// The quantity and bracket rows' label column (narrow: "Qty" only, no bracket label).
const LABEL_W: f32 = 52.0;
const LABEL_NARROW_W: f32 = 36.0;
/// Presets the bracket row shows before "…" (narrow: one).
fn presets_shown(narrow: bool) -> usize {
    if narrow {
        1
    } else {
        crate::brackets::MAX_SHOWN
    }
}
/// Smallest hit target (logical px, both ways).
pub const MIN_HIT: f32 = 20.0;

/// The panel's height at `width`: a function of the width only, never of the trading state
/// (position, lockout, flashes, the editor, enabled or not), so the ladder below never shifts
/// (docs/design.md §7).
pub fn panel_height(width: f32) -> f32 {
    let meters = if is_narrow(width) {
        METERS_NARROW_H
    } else {
        METERS_H
    };
    HDR_H + CARD_H + STATUS_H + meters + 2.0 * SEG_H + GRID_H + UTIL_H + PAD_Y
}

/// What a disabled panel says in the joins' place: the lock that binds. With nothing else
/// in the way, how to enable; with a lockout or another blocker too, that first (enabling
/// alone would not unlock entries).
pub fn locked_hint(blocker: Option<&str>) -> String {
    match blocker {
        Some(b) => format!("Entries locked: {b} · trading off"),
        None => format!(
            "Entries locked: enable trading ({})",
            crate::commands::hint(crate::commands::Command::Trade(TradeCmd::ToggleArm))
        ),
    }
}

/// The utility row's labels: words, short in a narrow panel ("Cancel all" keeps its object
/// at every width).
pub fn util_labels(width: f32) -> [&'static str; 4] {
    if is_narrow(width) {
        ["BE", "Rev", "Cancel all", "Flatten"]
    } else {
        ["BE", "Reverse", "Cancel all", "Flatten"]
    }
}

/// The unresolved-orders notice: with its advice where there is room, short when narrow.
pub fn unresolved_label(n: usize, width: f32) -> String {
    if is_narrow(width) {
        format!("{n} unresolved")
    } else {
        format!("{n} unresolved · check the broker")
    }
}

// ---- elements ---------------------------------------------------------------------------

fn cell(id: impl Into<gpui::ElementId>, t: &Theme) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .min_w(px(MIN_HIT))
        .h(px(MIN_HIT + 2.0))
        .px_1()
        .rounded_sm()
        .border_1()
        .border_color(t.axis_line)
        .text_color(t.text_main)
        .cursor_pointer()
}

/// A segment of a segmented control (`on`: selected, filled with the accent).
fn segment(
    id: impl Into<gpui::ElementId>,
    text: String,
    on: bool,
    t: &Theme,
) -> gpui::Stateful<gpui::Div> {
    let accent = t.accent;
    let c = cell(id, t).child(text);
    if on {
        c.bg(t.accent)
            .border_color(t.accent)
            .text_color(t.on_last)
            .font_weight(FontWeight::BOLD)
    } else {
        c.hover(move |s| s.border_color(accent))
    }
}

fn run_on_click<V: TradePane>(
    el: gpui::Stateful<gpui::Div>,
    cmd: TradeCmd,
    cx: &mut Context<V>,
) -> gpui::Stateful<gpui::Div> {
    el.on_mouse_down(
        MouseButton::Left,
        cx.listener(move |v: &mut V, _, _, cx| {
            cx.stop_propagation();
            if matches!(cmd, TradeCmd::Flatten | TradeCmd::CancelAll) {
                // Drawn over an open popover's backdrop (`popover::always_live`): it closes.
                crate::popover::close_any(cx);
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

/// The DOM's trading panel (trade panel v2): account and the trading switch; the position
/// card; the lockout or refusal line; rule meters; quantity and bracket; order buttons; the
/// utility row. Its height depends only on its width ([`panel_height`]). Nothing when
/// trading isn't running.
pub fn dom_bar<V: TradePane>(
    v: &V,
    product: &Product,
    width: f32,
    t: &Theme,
    cx: &mut Context<V>,
) -> Option<AnyElement> {
    Trading::try_global(cx)?;
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
    let gate = |el: gpui::Stateful<gpui::Div>| {
        if !enabled || guarded {
            el.opacity(0.4)
        } else {
            el
        }
    };
    let blocked = blocker(v, product, data.as_ref(), cx);
    let blocker = blocked.as_deref();

    let mut panel = div()
        .relative()
        .flex()
        .flex_col()
        .flex_shrink_0()
        .h(px(panel_height(width)))
        .overflow_hidden()
        .px_2()
        .pb(px(PAD_Y))
        .bg(t.bg)
        .border_b_1()
        .border_color(t.axis_line)
        .text_size(px(12.0));

    // 1. Account ▾ + PAPER, then the trading switch (shape and words, not color alone).
    let switch = {
        let s = div()
            .id("trading-switch")
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .h(px(MIN_HIT + 2.0))
            .px_2()
            .rounded_full()
            .border_1()
            .cursor_pointer()
            .font_weight(FontWeight::BOLD)
            .whitespace_nowrap()
            .overflow_hidden();
        // The knob: filled when enabled, a hollow ring when disabled.
        let knob = div().size(px(8.0)).rounded_full().border_1();
        let s = if enabled {
            s.bg(t.warn)
                .border_color(t.warn)
                .text_color(t.on_warn)
                .child(knob.bg(t.on_warn).border_color(t.on_warn))
        } else {
            s.border_color(t.axis_line)
                .text_color(t.text_dim)
                .child(knob.border_color(t.text_dim))
        };
        let s = s.child(switch_text(enabled, blocker, narrow));
        let name = if enabled {
            "Disable trading"
        } else {
            "Enable trading"
        };
        run_on_click(
            tip(s, name, Some(TradeCmd::ToggleArm), t),
            TradeCmd::ToggleArm,
            cx,
        )
    };
    panel = panel.child(
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .h(px(HDR_H))
            .child(account_chip(v, data.as_ref(), t, cx))
            .child(div().flex_1())
            .child(switch),
    );

    // 2. The position card.
    panel = panel.child(position_card(data.as_ref(), product, width, t));

    // 3. One status line: a refusal (brief) or the lockout / account state (lasting).
    let status = div().h(px(STATUS_H)).flex().flex_row().items_center();
    let locked = read_only(v, cx);
    let flashing = data
        .as_ref()
        .is_some_and(|d| flash_line(&d.flashes).is_some());
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
            // release coming as a quiet line.
            (None, None, None) if d.news.is_some() => {
                let (warns, text) = d.news.as_ref().expect("checked");
                if *warns {
                    let tail = if d.news_block.is_some() {
                        "exits, Flatten and Cancel all still work"
                    } else {
                        ""
                    };
                    status.child(lock_strip(text, tail, narrow, t))
                } else {
                    status.child(
                        div()
                            .min_w_0()
                            .text_color(t.text_dim)
                            .child(format!("Scheduled release: {text}")),
                    )
                }
            }
            // Narrow: the switch shows only "!", the reason is here.
            _ => match blocker.filter(|_| narrow && enabled) {
                Some(b) => status.child(lock_strip(&blocked_line(b), "", true, t)),
                None => status,
            },
        },
        None => status,
    };
    panel = panel.child(status);

    // 4. Rule meters (narrow: one line behind ▸; open, they lie over what follows).
    panel = panel.child(meters(v, data.as_ref(), mode, narrow, t, cx));

    // 5.–6. Quantity and bracket, or the bracket editor in their place (with the buttons).
    if let Some(ed) = &pt.editor {
        panel = panel.child(editor_view(ed, pt.editor_msg.as_deref(), v, t, cx));
    } else {
        panel = panel
            .child(gate(qty_row(v, narrow, t, cx)))
            .child(gate(bracket_row(v, product, narrow, t, cx)));
        // 7. Order buttons: market (filled) with the price they'd hit, joins (outlined).
        panel = panel.child(order_grid(v, product, enabled, guarded, blocker, t, cx));
    }

    // 8. Utility row: BE and Reverse (Reverse dims while disabled), Cancel all and Flatten
    //    (never dim; Flatten outlined in the warning color).
    let labels = util_labels(width);
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
    let mut row = div().flex().flex_row().items_center().gap_1().h(px(UTIL_H));
    for (i, (cmd, name, label, entry)) in util.into_iter().enumerate() {
        // BE and Reverse size to their words; Cancel all and Flatten take the rest. On a
        // read-only account those two read "off" and refuse with the reason when clicked.
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
        let mut b = cell(("util", i as u64), t).child(label);
        if off {
            b = b.text_color(t.text_dim);
        }
        if !entry {
            b = b.flex_1();
        }
        if cmd == TradeCmd::Flatten && !off {
            b = b
                .border_color(t.warn)
                .border_2()
                .font_weight(FontWeight::BOLD);
        }
        // Reverse is an entry (dims while disabled); BE only moves a stop.
        if cmd == TradeCmd::Reverse {
            b = gate(b);
        }
        let b = run_on_click(tip(b, name, Some(cmd), t), cmd, cx);
        row = if entry {
            row.child(b)
        } else {
            // Cancel all and Flatten: one press even with a popover open (design.md §3).
            row.child(crate::popover::always_live(b))
        };
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

/// A position exit as the panel and the chart's tags both write it: the role in words, the
/// quantity apart from it, the price ("Stop 2 @ 5812.25"), never a bare "S1" (an index on one
/// pane, a quantity on another, a short position on the ladder).
pub fn exit_label(role: &str, qty: i64, price: &str) -> String {
    format!("{role} {qty} @ {price}")
}

/// The position card: side by shape and word, size and average, open P&L large; the bracket
/// exits ("Stop 2 @ …" / "Target 2 @ …" in their role colors) and ticks; working orders and today's P&L. Flat:
/// "Flat" and today's P&L. P&L off: no amounts at all.
fn position_card(d: Option<&BarData>, product: &Product, width: f32, t: &Theme) -> gpui::Div {
    let mode = d.map(|d| d.mode).unwrap_or_default();
    let tone = |m: Money| match crate::pnl::tone(m) {
        crate::pnl::Tone::Up => t.up,
        crate::pnl::Tone::Down => t.down,
        crate::pnl::Tone::Flat => t.text_dim,
    };
    let line = || {
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_2()
            .h(px(16.0))
            .whitespace_nowrap()
    };
    let fmt = product.price;
    let mut card = div()
        .flex()
        .flex_col()
        .justify_center()
        .h(px(CARD_H))
        .px_2()
        .rounded_sm()
        .bg(t.panel);
    let day = d.and_then(|d| d.day_pnl).filter(|_| mode.money());
    let today = |m: Money| {
        let mut s = String::new();
        let _ = crate::pnl::write_day(&mut s, mode, m);
        s
    };
    match d.and_then(|d| d.position) {
        Some((q, avg, ticks, money)) => {
            let long = q > 0;
            let side = div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .font_weight(FontWeight::BOLD)
                .text_color(t.text_main)
                // Direction by shape as well as word.
                .child(if long { "▲" } else { "▼" })
                // Narrow: the arrow and the quantity, so the P&L beside them never cuts.
                .child(if is_narrow(width) {
                    q.abs().to_string()
                } else {
                    format!("{} {}", if long { "LONG" } else { "SHORT" }, q.abs())
                })
                .child(
                    div()
                        .font_weight(FontWeight::NORMAL)
                        .text_color(t.text_dim)
                        .child(
                            avg.map_or_else(String::new, |a| format!("@ {}", avg_text(product, a))),
                        ),
                );
            // The loudest number: open P&L in currency, else ticks; nothing when off.
            let big = if mode.money() {
                let mut s = String::new();
                let _ = crate::pnl::write_money(&mut s, money);
                Some((s, tone(money)))
            } else if mode.ticks() {
                ticks.map(|tk| {
                    let mut s = String::new();
                    let _ = crate::pnl::write_ticks(&mut s, tk);
                    (
                        s,
                        if tk > 0.0 {
                            t.up
                        } else if tk < 0.0 {
                            t.down
                        } else {
                            t.text_main
                        },
                    )
                })
            } else {
                None
            };
            card = card.child(line().child(side).children(big.map(|(s, c)| {
                div()
                    .flex_shrink_0()
                    .text_size(px(15.0))
                    .font_weight(FontWeight::BOLD)
                    .text_color(c)
                    .child(s)
            })));
            // Stop / Target in their role colors and in words, with the quantity apart from the
            // role ("Stop 2 @ 5812.25", as the chart's tags read), and the ticks when both show.
            let exits = div()
                .flex()
                .flex_row()
                .gap_2()
                .children(d.and_then(|d| d.stop).map(|(p, q)| {
                    div().text_color(t.order_stop).child(exit_label(
                        "Stop",
                        q,
                        fmt.format::<32>(p).as_str(),
                    ))
                }))
                .children(d.and_then(|d| d.target).map(|(p, q)| {
                    div().text_color(t.order_target).child(exit_label(
                        "Target",
                        q,
                        fmt.format::<32>(p).as_str(),
                    ))
                }));
            let tick_text = ticks.filter(|_| mode.money() && mode.ticks()).map(|tk| {
                let mut s = String::new();
                let _ = crate::pnl::write_ticks(&mut s, tk);
                s
            });
            card = card.child(
                line()
                    .child(exits)
                    .children(tick_text.map(|s| div().text_color(t.text_dim).child(s))),
            );
        }
        None => {
            card = card.child(
                line()
                    .child(
                        div()
                            .font_weight(FontWeight::BOLD)
                            .text_color(t.text_dim)
                            .child("Flat"),
                    )
                    .children(day.map(|m| {
                        div()
                            .text_size(px(15.0))
                            .font_weight(FontWeight::BOLD)
                            .text_color(tone(m))
                            .child(today(m))
                    })),
            );
            card = card.child(line());
        }
    }
    // Third line: working orders (and unresolved), today's P&L while in a position.
    let working = d.map_or(0, |d| d.working);
    let unresolved = d.map_or(0, |d| d.unresolved);
    let left = div()
        .flex()
        .flex_row()
        .gap_2()
        .text_color(t.text_dim)
        .child(match working {
            0 => "No working orders".to_owned(),
            n => format!("{n} working"),
        })
        .when(unresolved > 0, |e| {
            e.child(
                div()
                    .px_1()
                    .bg(t.warn)
                    .text_color(t.on_warn)
                    .font_weight(FontWeight::BOLD)
                    .child(unresolved_label(unresolved, width)),
            )
        });
    let in_position = d.and_then(|d| d.position).is_some();
    card.child(
        line().child(left).children(
            day.filter(|_| in_position)
                .map(|m| div().text_color(tone(m)).child(today(m))),
        ),
    )
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
            .h(px(16.0))
            .child(
                div()
                    .w(px(64.0))
                    .flex_shrink_0()
                    .text_color(t.text_dim)
                    .child(label),
            )
            .child(
                div()
                    .flex_1()
                    .h(px(4.0))
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
    let slot = div().h(px(h)).flex().flex_col().justify_center();
    if rules.is_empty() {
        return slot.child(
            div()
                .text_color(t.text_dim)
                .child("No prop rules on this account"),
        );
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
        .h(px(MIN_HIT))
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
                .top(px(h))
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

fn qty_row<V: TradePane>(
    v: &V,
    narrow: bool,
    t: &Theme,
    cx: &mut Context<V>,
) -> gpui::Stateful<gpui::Div> {
    let pt = v.trade_ref();
    let mut row = div()
        .id("qty-row")
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .h(px(SEG_H))
        .child(
            div()
                .w(px(if narrow { LABEL_NARROW_W } else { LABEL_W }))
                .flex_shrink_0()
                .text_color(t.text_dim)
                .child("Qty"),
        );
    for q in dom_trade::QTY_PRESETS {
        let on = pt.qty == q && pt.qty_edit.is_none();
        let s = segment(("qty", q as u64), q.to_string(), on, t).flex_1();
        let s = tip(s, &format!("Quantity {q}"), None, t);
        row = row.child(s.on_mouse_down(
            MouseButton::Left,
            cx.listener(move |v: &mut V, _, _, cx| {
                cx.stop_propagation();
                v.trade().qty = q;
                v.trade().qty_edit = None;
                cx.notify();
            }),
        ));
    }
    // Custom: the current value if it's not a preset, or the field being typed.
    let custom = match &pt.qty_edit {
        Some(s) => format!("{s}_"),
        None if !dom_trade::QTY_PRESETS.contains(&pt.qty) => pt.qty.to_string(),
        None => "…".into(),
    };
    let on = pt.qty_edit.is_some() || !dom_trade::QTY_PRESETS.contains(&pt.qty);
    let s = segment("qty-custom", custom, on, t).flex_1();
    let s = tip(s, "Type a quantity", None, t);
    row.child(s.on_mouse_down(
        MouseButton::Left,
        cx.listener(|v: &mut V, _, _, cx| {
            cx.stop_propagation();
            v.trade().qty_edit = Some(String::new());
            cx.notify();
        }),
    ))
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

fn bracket_row<V: TradePane>(
    v: &V,
    product: &Product,
    narrow: bool,
    t: &Theme,
    cx: &mut Context<V>,
) -> gpui::Stateful<gpui::Div> {
    use crate::brackets::label;
    let (presets, units) = presets_and_units(v, product, cx);
    let current = v.trade_ref().bracket;
    let shown = presets_shown(narrow);
    let mut row = div()
        .id("bracket-row")
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .h(px(SEG_H))
        .when(!narrow, |r| {
            r.child(
                div()
                    .w(px(LABEL_W))
                    .flex_shrink_0()
                    .text_color(t.text_dim)
                    .child("Bracket"),
            )
        });
    let off = tip(
        segment("bracket-off", "Off".into(), current.is_none(), t),
        "No bracket",
        None,
        t,
    );
    row = row.child(off.on_mouse_down(
        MouseButton::Left,
        cx.listener(|v: &mut V, _, _, cx| {
            cx.stop_propagation();
            v.trade().bracket = None;
            cx.notify();
        }),
    ));
    for (i, p) in presets.iter().take(shown).enumerate() {
        let b = p.preset();
        let text = if p.name.is_empty() {
            // Narrow: the numbers with the unit's letters attached ("8/16t").
            if narrow {
                format!(
                    "{}/{}{}",
                    crate::brackets::amount_text(b.stop_ticks, product, units),
                    crate::brackets::amount_text(b.target_ticks, product, units),
                    units.suffix()
                )
            } else {
                label(b, product, units)
            }
        } else {
            p.name.clone()
        };
        let s = segment(("bracket", i as u64), text, current == Some(b), t);
        let s = tip(s, &format!("Bracket {}", label(b, product, units)), None, t);
        row = row.child(s.on_mouse_down(
            MouseButton::Left,
            cx.listener(move |v: &mut V, _, _, cx| {
                cx.stop_propagation();
                v.trade().bracket = Some(b);
                cx.notify();
            }),
        ));
    }
    // More presets than shown, or a custom one selected: "…" cycles through them all.
    let hidden_selected =
        current.is_some_and(|c| !presets.iter().take(shown).any(|p| p.preset() == c));
    if presets.len() > shown || hidden_selected {
        let text = match current.filter(|_| hidden_selected) {
            Some(c) => label(c, product, units),
            None => "…".into(),
        };
        let s = tip(
            segment("bracket-more", text, hidden_selected, t),
            "More presets",
            Some(TradeCmd::NextBracket),
            t,
        );
        row = row.child(run_on_click(s, TradeCmd::NextBracket, cx));
    }
    row = row.child(div().flex_1());
    let u = tip(
        cell("bracket-units", t).child(units.suffix()),
        "Bracket units: ticks / points",
        Some(TradeCmd::BracketUnits),
        t,
    );
    row = row.child(run_on_click(u, TradeCmd::BracketUnits, cx));
    let e = tip(
        cell("bracket-edit", t).child(if narrow { "Edit" } else { "Edit…" }),
        "Edit bracket presets",
        Some(TradeCmd::EditBrackets),
        t,
    );
    row.child(run_on_click(e, TradeCmd::EditBrackets, cx))
}

/// The 2×2 order buttons: BUY MKT / SELL MKT filled in the side's color with the price they'd
/// hit, Join bid / Join ask outlined. Disabled: dimmed, and the second row says how to enable.
fn order_grid<V: TradePane>(
    v: &V,
    product: &Product,
    enabled: bool,
    guarded: bool,
    blocker: Option<&str>,
    t: &Theme,
    cx: &mut Context<V>,
) -> gpui::Div {
    let (bid, ask) = v
        .trade_store()
        .try_read()
        .map(|d| {
            (
                d.book.best_bid().map(|b| b.0).or(d.bid),
                d.book.best_ask().map(|a| a.0).or(d.ask),
            )
        })
        .unwrap_or_default();
    let price = |p: Option<Price>| {
        p.map_or_else(
            || "—".to_owned(),
            |p| product.price.format::<32>(p).as_str().to_owned(),
        )
    };
    let dim = !enabled || guarded;
    let button = |id: &'static str,
                  title: &'static str,
                  p: Option<Price>,
                  fill: Option<gpui::Hsla>,
                  line: gpui::Hsla| {
        let b = div()
            .id(id)
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .h(px(36.0))
            .rounded_sm()
            .border_1()
            .border_color(line)
            .cursor_pointer()
            .child(div().font_weight(FontWeight::BOLD).child(title))
            .child(div().text_size(px(11.0)).child(price(p)));
        let b = match fill {
            // Filled in the side's color, text in the background color (AA both ways).
            Some(c) => b.bg(c).text_color(t.bg),
            None => b.text_color(t.text_main),
        };
        if dim { b.opacity(0.4) } else { b }
    };
    let row = || div().flex().flex_row().gap_1();
    let buy = tip(
        button("buy-mkt", "BUY MKT", ask, Some(t.up), t.up),
        "Buy market",
        Some(TradeCmd::BuyMarket),
        t,
    );
    let sell = tip(
        button("sell-mkt", "SELL MKT", bid, Some(t.down), t.down),
        "Sell market",
        Some(TradeCmd::SellMarket),
        t,
    );
    let mut grid = div()
        .flex()
        .flex_col()
        .justify_center()
        .gap_1()
        .h(px(GRID_H))
        .child(
            row()
                .child(run_on_click(buy, TradeCmd::BuyMarket, cx))
                .child(run_on_click(sell, TradeCmd::SellMarket, cx)),
        );
    if enabled {
        let jb = tip(
            button("join-bid", "Join bid", bid, None, t.up),
            "Join bid (buy limit at the best bid)",
            Some(TradeCmd::JoinBid),
            t,
        );
        let ja = tip(
            button("join-ask", "Join ask", ask, None, t.down),
            "Join ask (sell limit at the best ask)",
            Some(TradeCmd::JoinAsk),
            t,
        );
        grid = grid.child(
            row()
                .child(run_on_click(jb, TradeCmd::JoinBid, cx))
                .child(run_on_click(ja, TradeCmd::JoinAsk, cx)),
        );
    } else {
        // Disabled: one line says what locks entries, in the joins' place (same height).
        grid = grid.child(
            div()
                .h(px(36.0))
                .flex()
                .items_center()
                .justify_center()
                .text_color(t.text_main)
                .child(locked_hint(blocker)),
        );
    }
    grid
}

/// The inline bracket-preset editor, in the place of quantity, bracket and order buttons.
fn editor_view<V: TradePane>(
    ed: &crate::brackets::Editor,
    msg: Option<&str>,
    v: &V,
    t: &Theme,
    cx: &mut Context<V>,
) -> gpui::Div {
    use crate::brackets::Field;
    const ROWS: usize = 4;
    let h = 2.0 * SEG_H + GRID_H;
    let first = ed.row.saturating_sub(ROWS - 1);
    let head = div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .h(px(20.0))
        .child(
            div()
                .font_weight(FontWeight::BOLD)
                .text_color(t.text_main)
                .child(format!("Bracket presets · {}", ed.product.symbol)),
        )
        .child(
            div()
                .text_color(t.text_dim)
                .child(format!("in {}", ed.units.word())),
        )
        .child(div().flex_1())
        .child(
            div()
                .text_color(t.text_dim)
                .child("Enter save · Esc cancel"),
        );
    let cell_el = |r: usize, f: Field, text: &str, w: f32| {
        let active = r == ed.row && f == ed.field;
        let bad = ed.cell_problem(r, f).is_some();
        let shown = if active {
            format!("{text}▏")
        } else {
            text.to_owned()
        };
        div()
            .id(("bracket-cell", (r * 3 + f as usize) as u64))
            .w(px(w))
            .h(px(18.0))
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
    let mut list = div().flex().flex_col().gap(px(2.0));
    for (r, row) in ed.rows.iter().enumerate().skip(first).take(ROWS) {
        list = list.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .child(
                    div()
                        .w(px(14.0))
                        .text_color(if r == ed.row { t.accent } else { t.text_dim })
                        .child(if r == ed.row { "›" } else { " " }),
                )
                .child(div().text_color(t.text_dim).child("S"))
                .child(cell_el(r, Field::Stop, &row.stop, 64.0))
                .child(div().text_color(t.text_dim).child("T"))
                .child(cell_el(r, Field::Target, &row.target, 64.0))
                .child(cell_el(r, Field::Name, &row.name, 80.0)),
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
                  f: fn(&mut crate::brackets::Editor)| {
        tip(cell(id, t).child(label), name, None, t).on_mouse_down(
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
        .h(px(24.0))
        .child(match problem {
            Some(p) => div()
                .min_w_0()
                .truncate()
                .px_1()
                .bg(t.warn)
                .text_color(t.on_warn)
                .child(p),
            None => div()
                .min_w_0()
                .truncate()
                .text_color(t.text_main)
                .child(preview.unwrap_or_default()),
        })
        .child(div().flex_1())
        .child(action(
            "bracket-add",
            "+",
            "Add a preset (Insert)",
            crate::brackets::Editor::add,
        ))
        .child(action(
            "bracket-del",
            "−",
            "Delete the preset (Delete)",
            crate::brackets::Editor::delete,
        ))
        .child(action("bracket-up", "↑", "Move up (Ctrl+Up)", |e| {
            e.reorder(-1)
        }))
        .child(action(
            "bracket-down",
            "↓",
            "Move down (Ctrl+Down)",
            |e| e.reorder(1),
        ));
    div()
        .flex()
        .flex_col()
        .justify_between()
        .h(px(h))
        .py_1()
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
    let c = cell("account", t)
        .font_weight(FontWeight::BOLD)
        .min_w_0()
        .overflow_hidden()
        .whitespace_nowrap()
        .child(format!("{name} ▾"))
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

/// Open or close the account list (the account chip).
pub fn toggle_account_menu<V: TradePane>(v: &mut V, cx: &mut Context<V>) {
    if v.trade_ref().account_menu {
        return close_account_menu(v, cx);
    }
    v.trade().account_menu = true;
    crate::popover::opened(cx, crate::popover::Kind::Account, |v: &mut V, cx| {
        close_account_menu(v, cx)
    });
    cx.notify();
}

/// Close the account list if open (an outside click, Esc, another popover opening).
pub fn close_account_menu<V: TradePane>(v: &mut V, cx: &mut Context<V>) {
    if std::mem::take(&mut v.trade().account_menu) {
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
    let accent = t.accent;
    // The trigger's label, only while recording (and only with a current account).
    let trigger = crate::layout_rec::on()
        .then(|| current.as_ref().and_then(|a| trading.account(a)))
        .flatten()
        .map(|a| format!("{} ▾", a.name));
    let mut list = div()
        .id("account-list")
        .occlude()
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
        .w(px(320.0))
        // Not the header's no-wrap text style.
        .whitespace_normal()
        .flex()
        .flex_col()
        .py_1()
        .bg(t.panel)
        .border_1()
        .border_color(t.accent)
        .rounded_sm()
        .text_size(px(12.0));
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
                .hover(move |s| s.bg(accent.opacity(0.18)))
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
                        .text_size(px(11.0))
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

/// The quantity the pane trades.
pub fn qty(v: &PaneTrade) -> Qty {
    Qty(v.qty)
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
            qty: Some(3),
            bracket: None,
        };
        let c = doc.clone().copied();
        assert!(!c.enabled);
        assert_eq!((c.account, c.qty), (doc.account, doc.qty));
    }

    /// Review: the panel said "S1" for a stop the chart tag called "Stop 2": one grammar for
    /// both, the role in words and the quantity apart from it.
    #[test]
    fn exit_labels_match_the_chart_tags() {
        assert_eq!(exit_label("Stop", 2, "5812.25"), "Stop 2 @ 5812.25");
        assert_eq!(exit_label("Target", 1, "5820.00"), "Target 1 @ 5820.00");
    }

    #[test]
    fn the_locked_hint_names_the_lock_that_binds() {
        assert!(locked_hint(None).starts_with("Entries locked: enable trading ("));
        assert_eq!(
            locked_hint(Some("cooldown")),
            "Entries locked: cooldown · trading off"
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
        assert_eq!(
            blocked_line("market closed"),
            "Enabled, but orders are denied: market closed"
        );
    }

    /// The panel's height is a function of its width only: no trading state can change it, so
    /// the ladder below never shifts. Narrow panels are shorter (meters behind ▸).
    #[test]
    fn panel_height_depends_on_width_only() {
        let wide = panel_height(420.0);
        assert_eq!(wide, panel_height(800.0));
        assert_eq!(
            wide,
            panel_height(0.0),
            "before the first layout: the wide panel"
        );
        let narrow = panel_height(MIN_DOM_WIDTH);
        assert_eq!(narrow, panel_height(NARROW_BELOW - 1.0));
        assert!(narrow < wide);
        // The editor fits in the slots it replaces (quantity, bracket, order buttons).
        const { assert!(2.0 * SEG_H + GRID_H >= 20.0 + 4.0 * 20.0 + 24.0) };
    }

    /// The panel at its narrowest (MIN_DOM_WIDTH): every row's items fit, at a wide 12 px
    /// glyph (7.5 px a character), with ≥ 20 px hit targets.
    #[test]
    fn panel_fits_the_narrowest_dom() {
        const CHAR: f32 = 7.5;
        const PAD: f32 = 2.0 * 4.0 + 2.0; // px_1 + border
        let room = MIN_DOM_WIDTH - 2.0 * 8.0; // the panel's px_2
        let w = |s: &str| (s.chars().count() as f32 * CHAR + PAD).max(MIN_HIT);
        // Utility row: BE and Rev at their size, Cancel and Flatten sharing the rest.
        let util = util_labels(MIN_DOM_WIDTH);
        let need: f32 = util.iter().map(|l| w(l)).sum::<f32>() + 3.0 * 4.0;
        assert!(need <= room, "util row: {need} > {room}");
        assert_eq!(util[3], "Flatten", "Flatten keeps its word");
        // Quantity: label, 5 presets and the custom segment.
        let qty = LABEL_NARROW_W + 6.0 * w("10") + 6.0 * 4.0;
        assert!(qty <= room, "qty row {qty}");
        // Bracket (narrow, no label): Off, the presets shown, "…", units and Edit.
        let n = presets_shown(true) as f32;
        let bracket = w("Off") + n * w("20/40") + w("…") + w("pt") + w("Edit") + (n + 4.0) * 4.0;
        assert!(bracket <= room, "bracket row {bracket}");
        // Header: the switch (its widest narrow text) beside an account name that shrinks.
        let switch = w(&switch_text(true, Some("contract unknown"), true)) + 2.0 * 8.0;
        assert!(
            60.0 + w("PAPER") + switch + 2.0 * 8.0 <= room,
            "header {switch}"
        );
        const { assert!(MIN_HIT >= 20.0) };
        assert!(is_narrow(MIN_DOM_WIDTH) && !is_narrow(NARROW_BELOW) && !is_narrow(0.0));
    }

    /// The switch is saved with the pane and restored (sticky across a restart); disabled
    /// writes nothing.
    #[test]
    fn trade_doc_saves_the_switch_and_choices() {
        assert_eq!(serde_json::to_string(&TradeDoc::default()).unwrap(), "{}");
        let d = TradeDoc {
            enabled: true,
            account: Some("paper".into()),
            qty: Some(3),
            bracket: Some(BracketPreset {
                stop_ticks: 8,
                target_ticks: 16,
            }),
        };
        let json = serde_json::to_string(&d).unwrap();
        assert!(json.starts_with(r#"{"enabled":true"#), "{json}");
        let back: TradeDoc = serde_json::from_str(&json).unwrap();
        assert_eq!(back, d);
        // A file from before the switch was saved: disabled.
        let old: TradeDoc = serde_json::from_str(r#"{"qty":2}"#).unwrap();
        assert!(!old.enabled);
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
            // Reverse is refused while orders work on the contract.
            position: 1,
            working: 1,
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
