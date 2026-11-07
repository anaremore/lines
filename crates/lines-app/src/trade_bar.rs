//! A pane's trading controls (roadmap 7; docs/design.md §3): which account it trades, the
//! order quantity, the bracket preset, one-click arming, Flatten and Cancel All, the position
//! and the prop-rule strip. The DOM shows all of it in its header; a chart shows its account.
//!
//! Trust rules this module keeps:
//! - The account is always named, with a **PAPER** badge (text and an inverted chip, not only
//!   a colour: decision 0006). Live accounts will carry a different badge.
//! - **Flatten** and **Cancel All** are always visible and always sent: neither is gated by
//!   arming, risk or prop rules (the core lets them through).
//! - Arming is explicit and shown as words (ARMED / DISARMED) and by shape (filled vs
//!   outlined). Refusals appear inline, briefly, never as dialogs.
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
    self, ArmEvent, Arming, BracketPreset, RuleLevel, next_bracket, open_ticks, parse_qty,
    rule_level, step_qty,
};
use crate::theme::Theme;
use crate::trading::{TradeCache, Trading};

/// How long a local refusal ("disarmed", "no account") stays.
const LOCAL_FLASH: Duration = Duration::from_secs(3);

/// A pane's saved trading choices (in its `DomState`/`ChartState`). Arming is never saved.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TradeDoc {
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
}

impl PaneTrade {
    pub fn new(saved: Option<&TradeDoc>, cx: &App) -> PaneTrade {
        let trading = Trading::try_global(cx);
        let account = saved
            .and_then(|s| s.account.as_ref())
            .map(|a| AccountId::new(a))
            .filter(|a| trading.is_some_and(|t| t.account(a).is_some()))
            .or_else(|| trading.and_then(Trading::default_account));
        PaneTrade {
            account,
            qty: saved.and_then(|s| s.qty).filter(|&q| q > 0).unwrap_or(1),
            bracket: saved.and_then(|s| s.bracket),
            arming: Arming::default(),
            qty_edit: None,
            account_menu: trading.is_some_and(|t| t.start_account_menu),
            local_flash: None,
            cache: trading.map(|t| t.cache.clone()),
            seen: u64::MAX,
            seen_display: crate::pnl::display_epoch(),
            watched: None,
        }
    }

    pub fn doc(&self) -> Option<TradeDoc> {
        let d = TradeDoc {
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

    pub fn arm_event(&mut self, ev: ArmEvent, can_trade: bool, cx: &App) -> bool {
        let prefs = Trading::try_global(cx).map(|t| t.prefs).unwrap_or_default();
        self.arming.apply(ev, prefs, can_trade)
    }

    /// The pane switched feed: disarm, and let paper fills follow the new feed (`follow`:
    /// it is a running feed, not an unavailable one).
    pub fn feed_changed(&mut self, store: &Arc<ChartStore>, follow: bool, cx: &App) {
        self.arm_event(ArmEvent::ContractChanged, false, cx);
        self.account_menu = false;
        if follow && let Some(t) = Trading::try_global(cx) {
            t.follow(store);
        }
    }

    /// Disarm when the pane loses focus or its window goes inactive (if configured). Called
    /// from render: subscribes once per window.
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
            if v.trade().arm_event(ArmEvent::FocusLost, true, cx) {
                cx.notify();
            }
        });
        let activation = cx.observe_window_activation(window, |v: &mut V, window, cx| {
            if !window.is_window_active() && v.trade().arm_event(ArmEvent::FocusLost, true, cx) {
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

/// What an order from this pane needs: account, contract, quantity, position, bracket, the
/// inside market (ladder clicks, header buttons and their keys all build from this).
pub fn entry_ctx<V: TradePane>(v: &V, cx: &App) -> Result<dom_trade::EntryCtx, &'static str> {
    if let Some(why) = cant_trade(v, cx) {
        return Err(why);
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
        bracket: pt.bracket,
        tick,
        best_bid,
        best_ask,
        last,
    })
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
    match cmd {
        TradeCmd::ToggleArm => {
            let ok = why.is_none();
            if !v.trade().arm_event(ArmEvent::Toggle, ok, cx)
                && let Some(w) = why
            {
                v.trade().flash(format!("can't arm: {w}"));
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
            v.trade().bracket = next_bracket(b);
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
    let t = v.trade();
    t.account_menu = false;
    if account.is_some() && account != t.account {
        t.account = account;
        t.seen = u64::MAX;
        v.trade().arm_event(ArmEvent::AccountChanged, true, cx);
    }
    cx.notify();
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

/// What the header shows, gathered under the cache lock (never waited for).
struct BarData {
    account_name: String,
    paper: bool,
    /// Signed qty, average price text, open P&L in ticks (when the mark is known) and money.
    position: Option<(i64, String, Option<f64>, Money)>,
    /// Realised + open P&L for the session trade date (the account's).
    day_pnl: Option<Money>,
    /// How P&L shows (app setting); off: no amounts anywhere in the header.
    mode: crate::pnl::PnlDisplay,
    /// (label, left, limit) per rule.
    rules: Vec<(&'static str, Money, Money)>,
    /// The account's trading state, as words, when it isn't active.
    blocked: Option<String>,
    /// (label, text): DENIED / REJECTED / WARNING / NOT SENT.
    flashes: Vec<(&'static str, String)>,
    unresolved: usize,
    working: usize,
}

fn gather<V: TradePane>(v: &V, product: &Product, cx: &App) -> Option<BarData> {
    let t = Trading::try_global(cx)?;
    let pt = v.trade_ref();
    let account = pt.account.as_ref()?;
    let info = t.account(account)?;
    let contract = contract_of(v.trade_store());
    let last = v.trade_store().try_read().and_then(|d| d.last_trade);
    let mut data = BarData {
        account_name: info.name.clone(),
        paper: info.paper,
        position: None,
        day_pnl: None,
        mode: cx
            .try_global::<crate::picker_view::AppSettings>()
            .map(|a| a.settings.pnl_display)
            .unwrap_or_default(),
        rules: Vec::new(),
        blocked: None,
        flashes: Vec::new(),
        unresolved: 0,
        working: 0,
    };
    if let Some(s) = t.cache.try_read() {
        if let Some(k) = contract {
            data.working = s.open_orders(account, k).count();
            if let Some(p) = s.position(account, k) {
                let avg = p
                    .avg_price
                    .map_or_else(String::new, |a| avg_text(product, a));
                let ticks = match (p.avg_price, last) {
                    (Some(a), Some(m)) => open_ticks(p.qty, a, m, product.price.tick),
                    _ => None,
                };
                data.position = Some((p.qty, avg, ticks, p.unrealised));
            }
        }
        if let Some(a) = s.account(account) {
            data.day_pnl = Some(a.day_pnl);
            if let (Some(r), Some(preset)) = (&a.rules, &info.rules) {
                if let (Some(left), Some(limit)) = (r.daily_loss_left, preset.daily_loss_limit) {
                    data.rules.push(("daily loss", left, Money::whole(limit)));
                }
                if let (Some(left), Some(dd)) = (r.to_drawdown, &preset.trailing_drawdown) {
                    data.rules.push(("drawdown", left, Money::whole(dd.amount)));
                }
            }
            let why = a.blocked.map_or("", |b| match b {
                lines_trading::rules::Breach::TrailingDrawdown => "trailing drawdown breached",
                lines_trading::rules::Breach::DailyLoss => "daily loss limit reached",
            });
            data.blocked = match a.trading {
                lines_trading::rules::TradingState::Active => None,
                lines_trading::rules::TradingState::ReducingOnly => Some(format!(
                    "REDUCING ONLY · {why} · orders may only close the position"
                )),
                lines_trading::rules::TradingState::Halted => {
                    Some(format!("HALTED · {why} · no new orders"))
                }
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

/// The header's refusal line: the newest (label, text), with `×N` when there were N
/// refusals in the last few seconds (like the footer's notices), so a burst never stacks rows.
pub fn flash_line(flashes: &[(&'static str, String)]) -> Option<(&'static str, String)> {
    let (label, text) = flashes.last()?;
    Some(match flashes.len() {
        1 => (*label, text.clone()),
        n => (*label, format!("{text} ×{n}")),
    })
}

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

fn chip(text: impl Into<gpui::SharedString>, t: &Theme) -> gpui::Div {
    div()
        .px_1()
        .rounded_sm()
        .border_1()
        .border_color(t.axis_line)
        .text_color(t.text_main)
        .child(text.into())
}

fn button<V: TradePane>(
    id: &'static str,
    label: String,
    t: &Theme,
    cmd: TradeCmd,
    cx: &mut Context<V>,
) -> gpui::Stateful<gpui::Div> {
    let accent = t.accent;
    chip(label, t)
        .id(id)
        .cursor_pointer()
        .hover(move |s| s.border_color(accent))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |v: &mut V, _, _, cx| {
                cx.stop_propagation();
                run(v, cmd, Instant::now(), cx);
            }),
        )
}

/// The DOM's trading header: account, position, quantity, bracket, arming, Flatten / Cancel
/// All, the rule strip and recent refusals. Nothing when trading isn't running.
pub fn dom_bar<V: TradePane>(
    v: &V,
    product: &Product,
    width: f32,
    t: &Theme,
    cx: &mut Context<V>,
) -> Option<AnyElement> {
    let data = gather(v, product, cx);
    Trading::try_global(cx)?;
    let pt = v.trade_ref();
    let armed = pt.arming.is_armed();
    let mut rows = div()
        .flex()
        .flex_col()
        .bg(t.panel)
        .border_b_1()
        .border_color(t.axis_line)
        .text_size(px(12.0));

    // Row 1: account (selector) + PAPER, position, Flatten / Cancel All.
    let mut r1 = div()
        .flex()
        .flex_row()
        .flex_wrap()
        .items_center()
        .gap_2()
        .px_2()
        .py_1();
    r1 = r1.child(account_chip(v, data.as_ref(), t, cx));
    let mode = data.as_ref().map(|d| d.mode).unwrap_or_default();
    // P&L in the up/down colour, always with its sign (never colour alone).
    let tone = |m: Money| match crate::pnl::tone(m) {
        crate::pnl::Tone::Up => t.up,
        crate::pnl::Tone::Down => t.down,
        crate::pnl::Tone::Flat => t.text_main,
    };
    match data.as_ref().and_then(|d| d.position.clone()) {
        Some((q, avg, ticks, money)) => {
            // Side, quantity and average always show: P&L off hides only the P&L.
            let side = if q > 0 { "LONG" } else { "SHORT" };
            r1 = r1.child(
                div()
                    .font_weight(FontWeight::BOLD)
                    .text_color(t.text_main)
                    .child(format!("{side} {} @ {avg}", q.abs())),
            );
            let mut pnl = String::new();
            if crate::pnl::write_pnl(&mut pnl, mode, ticks, money).unwrap_or(false) {
                r1 = r1.child(
                    div()
                        .font_weight(FontWeight::BOLD)
                        .text_color(tone(money))
                        .child(pnl),
                );
            }
        }
        None => {
            r1 = r1.child(div().text_color(t.text_dim).child("flat"));
        }
    }
    // The account's day P&L (realised + open, session trade date): currency modes only.
    if let Some(day) = data.as_ref().and_then(|d| d.day_pnl) {
        let mut text = String::new();
        if crate::pnl::write_day(&mut text, mode, day).unwrap_or(false) {
            r1 = r1.child(div().text_color(tone(day)).child(text));
        }
    }
    if let Some(n) = data.as_ref().map(|d| d.working).filter(|&n| n > 0) {
        r1 = r1.child(div().text_color(t.text_dim).child(format!("{n} open")));
    }
    // Sent before a restart or reconnect with no answer: listed, never shown as working.
    if let Some(n) = data.as_ref().map(|d| d.unresolved).filter(|&n| n > 0) {
        r1 = r1.child(
            div()
                .px_1()
                .bg(t.warn)
                .text_color(t.on_warn)
                .font_weight(FontWeight::BOLD)
                .child(unresolved_label(n, width)),
        );
    }
    r1 = r1.child(div().flex_1());
    // Together and never squeezed out: both always visible (decision 0006).
    r1 = r1.child(
        div()
            .flex()
            .flex_row()
            .flex_shrink_0()
            .gap_1()
            .child(button(
                "flatten",
                "Flatten".into(),
                t,
                TradeCmd::Flatten,
                cx,
            ))
            .child(button(
                "cancel-all",
                "Cancel all".into(),
                t,
                TradeCmd::CancelAll,
                cx,
            )),
    );
    rows = rows.child(r1);

    // Row 2: quantity, bracket, arming.
    let mut r2 = div()
        .flex()
        .flex_row()
        .flex_wrap()
        .items_center()
        .gap_1()
        .px_2()
        .pb_1();
    r2 = r2.child(div().text_color(t.text_dim).child("Qty"));
    for q in dom_trade::QTY_PRESETS {
        let on = pt.qty == q && pt.qty_edit.is_none();
        let accent = t.accent;
        let mut c = chip(q.to_string(), t)
            .id(("qty", q as u64))
            .cursor_pointer()
            .hover(move |s| s.border_color(accent));
        if on {
            c = c
                .bg(t.accent)
                .border_color(t.accent)
                .text_color(t.on_last)
                .font_weight(FontWeight::BOLD);
        }
        r2 = r2.child(c.on_mouse_down(
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
    let editing = pt.qty_edit.is_some() || !dom_trade::QTY_PRESETS.contains(&pt.qty);
    let mut cc = chip(custom, t).id("qty-custom").cursor_pointer();
    if editing {
        cc = cc.border_color(t.accent).font_weight(FontWeight::BOLD);
    }
    r2 = r2.child(cc.on_mouse_down(
        MouseButton::Left,
        cx.listener(|v: &mut V, _, _, cx| {
            cx.stop_propagation();
            v.trade().qty_edit = Some(String::new());
            cx.notify();
        }),
    ));
    let bracket = match pt.bracket {
        Some(b) => format!("Bracket {}/{} t", b.stop_ticks, b.target_ticks),
        None => "No bracket".into(),
    };
    r2 = r2
        .child(div().w(px(6.0)))
        .child(button("bracket", bracket, t, TradeCmd::NextBracket, cx));
    // Order buttons: market buy/sell (up/down colours, each with its word), join bid/ask,
    // reverse. Dimmed while disarmed; a click then says "Arm to trade" and sends nothing.
    let labels = order_button_labels(width);
    for (i, (cmd, tone)) in [
        (TradeCmd::BuyMarket, Some(t.up)),
        (TradeCmd::SellMarket, Some(t.down)),
        (TradeCmd::JoinBid, None),
        (TradeCmd::JoinAsk, None),
        (TradeCmd::Reverse, None),
    ]
    .into_iter()
    .enumerate()
    {
        let mut b = div()
            .id(("order-button", i))
            .px_1()
            .rounded_sm()
            .border_1()
            .cursor_pointer()
            .font_weight(FontWeight::BOLD)
            .child(labels[i]);
        b = match (tone, armed) {
            // Armed market buttons: filled in the side's colour.
            (Some(c), true) => b.bg(c).border_color(c).text_color(t.bg),
            (Some(c), false) => b.border_color(c).text_color(c),
            (None, _) => b.border_color(t.axis_line).text_color(t.text_main),
        };
        if !armed {
            b = b.opacity(0.45);
        }
        r2 = r2.child(b.on_mouse_down(
            MouseButton::Left,
            cx.listener(move |v: &mut V, _, _, cx| {
                cx.stop_propagation();
                run_from(v, cmd, Instant::now(), Source::Dom, cx);
            }),
        ));
    }
    r2 = r2.child(div().flex_1());
    let mut arm = div()
        .id("arm")
        .px_2()
        .rounded_sm()
        .border_1()
        .cursor_pointer()
        .font_weight(FontWeight::BOLD);
    arm = if armed {
        // Filled and named: unmistakable.
        arm.bg(t.accent)
            .border_color(t.accent)
            .text_color(t.on_last)
            .child("ARMED · click trades")
    } else {
        arm.border_color(t.axis_line)
            .text_color(t.text_dim)
            .child("DISARMED")
    };
    r2 = r2.child(arm.on_mouse_down(
        MouseButton::Left,
        cx.listener(|v: &mut V, _, _, cx| {
            cx.stop_propagation();
            run(v, TradeCmd::ToggleArm, Instant::now(), cx);
        }),
    ));
    rows = rows.child(r2);

    // Row 3: the prop-rule strip (accounts with a preset).
    if let Some(d) = &data
        && (!d.rules.is_empty() || d.blocked.is_some())
    {
        rows = rows.child(rule_strip(d, t, mode));
    }
    // Refusals and rejections, inline and brief: one line, the newest, with a count.
    if let Some(d) = &data
        && let Some((label, f)) = flash_line(&d.flashes)
    {
        rows = rows.child(
            div()
                .px_2()
                .pb_1()
                .flex()
                .flex_row()
                .gap_2()
                .child(
                    div()
                        .px_1()
                        .bg(t.warn)
                        .text_color(t.on_warn)
                        .font_weight(FontWeight::BOLD)
                        .child(label),
                )
                .child(div().text_color(t.text_main).overflow_hidden().child(f)),
        );
    }
    Some(rows.into_any_element())
}

/// Prop-rule progress. Amounts are currency, so they show only when the P&L display shows
/// currency; otherwise (ticks, off) states without amounts (`daily loss: ok`, `NEAR LIMIT ·
/// daily loss`, `daily loss: LIMIT REACHED`).
fn rule_strip(d: &BarData, t: &Theme, mode: crate::pnl::PnlDisplay) -> gpui::Div {
    let mut strip = div().flex().flex_row().flex_wrap().gap_2().px_2().pb_1();
    if let Some(b) = &d.blocked {
        strip = strip.child(
            div()
                .px_1()
                .bg(t.warn)
                .text_color(t.on_warn)
                .font_weight(FontWeight::BOLD)
                .child(format!("{b} · Flatten and Cancel all still work")),
        );
    }
    for (label, left, limit) in &d.rules {
        let level = rule_level(left.0, limit.0);
        let mut text = String::new();
        if mode.money() {
            let _ = write!(
                text,
                "{label}: ${} left of ${}",
                money_abs(*left),
                money_abs(*limit)
            );
        } else {
            let _ = write!(text, "{label}: ok");
        }
        let cell = div().px_1().rounded_sm();
        strip = strip.child(match level {
            RuleLevel::Ok => cell.text_color(t.text_dim).child(text),
            // Near and breached: the warning colour, and words, not only colour.
            RuleLevel::Near => cell
                .bg(t.warn)
                .text_color(t.on_warn)
                .font_weight(FontWeight::BOLD)
                .child(if mode.money() {
                    format!("NEAR LIMIT · {text}")
                } else {
                    format!("NEAR LIMIT · {label}")
                }),
            RuleLevel::Breached => cell
                .bg(t.warn)
                .text_color(t.on_warn)
                .font_weight(FontWeight::BOLD)
                .child(format!("{label}: LIMIT REACHED")),
        });
    }
    strip
}

fn money_abs(m: Money) -> String {
    let s = m.to_string();
    s.trim_start_matches('-').to_owned()
}

/// Below this header width the order buttons use short labels.
pub const SHORT_LABELS_BELOW: f32 = 360.0;

/// The narrowest DOM the trading header is designed for: every header item fits on a line of
/// its own (rows wrap), so nothing overlaps (layout test below).
#[cfg_attr(not(test), allow(dead_code))]
pub const MIN_DOM_WIDTH: f32 = 240.0;

/// Whether a header `width` calls for the short labels (0: not laid out yet, full labels).
pub fn is_narrow(width: f32) -> bool {
    width > 0.0 && width < SHORT_LABELS_BELOW
}

/// The order buttons' labels at a header `width` (0: not laid out yet, the full labels).
pub fn order_button_labels(width: f32) -> [&'static str; 5] {
    if is_narrow(width) {
        ["B MKT", "S MKT", "J BID", "J ASK", "REV"]
    } else {
        ["Buy Mkt", "Sell Mkt", "Join Bid", "Join Ask", "Reverse"]
    }
}

/// The unresolved-orders notice: with its advice where there is room, short in a narrow DOM.
pub fn unresolved_label(n: usize, width: f32) -> String {
    if is_narrow(width) {
        format!("{n} UNRESOLVED")
    } else {
        format!("{n} UNRESOLVED · check the broker")
    }
}

/// The account name (click: choose another) and the PAPER badge.
fn account_chip<V: TradePane>(
    v: &V,
    data: Option<&BarData>,
    t: &Theme,
    cx: &mut Context<V>,
) -> AnyElement {
    let (name, paper) = data.map_or(("no account".to_owned(), true), |d| {
        (d.account_name.clone(), d.paper)
    });
    let accent = t.accent;
    let open = v.trade_ref().account_menu;
    let menu = account_menu(v, t, cx);
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .child(crate::dropdown::dropdown(
            chip(format!("{name} ▾"), t)
                .id("account")
                .cursor_pointer()
                .font_weight(FontWeight::BOLD)
                .when(open, |d| d.border_color(accent))
                .hover(move |s| s.border_color(accent))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|v: &mut V, _, _, cx| {
                        cx.stop_propagation();
                        let t = v.trade();
                        t.account_menu = !t.account_menu;
                        cx.notify();
                    }),
                ),
            menu,
        ))
        .when(paper, |d| d.child(paper_badge(t)))
        .into_any_element()
}

/// PAPER: an inverted chip with the word (never colour alone).
pub fn paper_badge(t: &Theme) -> gpui::Div {
    div()
        .px_1()
        .rounded_sm()
        .bg(t.paper_badge)
        .text_color(t.on_paper_badge)
        .font_weight(FontWeight::BOLD)
        .child("PAPER")
}

/// The account list, while the selector is open: under the account chip (`dropdown`).
fn account_menu<V: TradePane>(v: &V, t: &Theme, cx: &mut Context<V>) -> Option<AnyElement> {
    if !v.trade_ref().account_menu {
        return None;
    }
    let trading = Trading::try_global(cx)?;
    let current = v.trade_ref().account.clone();
    let accent = t.accent;
    let mut list = div()
        .id("account-list")
        .occlude()
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
    for (i, a) in trading.accounts.iter().enumerate() {
        let id = a.id.clone();
        let on = current.as_ref() == Some(&a.id);
        let detail = match &a.rules {
            Some(r) => format!("rules: {} (approximation)", r.name),
            None => "no prop rules".into(),
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
                        .when(a.paper, |d| d.child(paper_badge(t))),
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
pub fn chart_chip<V: TradePane>(v: &V, t: &Theme, cx: &mut Context<V>) -> Option<AnyElement> {
    let trading = Trading::try_global(cx)?;
    let pt = v.trade_ref();
    let info = pt.account.as_ref().and_then(|a| trading.account(a))?;
    let data = BarData {
        account_name: info.name.clone(),
        paper: info.paper,
        position: None,
        day_pnl: None,
        mode: crate::pnl::PnlDisplay::default(),
        rules: Vec::new(),
        blocked: None,
        flashes: Vec::new(),
        unresolved: 0,
        working: 0,
    };
    Some(account_chip(v, Some(&data), t, cx))
}

/// The quantity the pane trades.
pub fn qty(v: &PaneTrade) -> Qty {
    Qty(v.qty)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn money_and_average_text() {
        let es = lines_feed::catalog::get(lines_feed::catalog::ES).unwrap();
        assert_eq!(avg_text(&es, 599_950.0), "5999.50");
        // Between ticks: one more decimal than the product shows.
        assert_eq!(avg_text(&es, 599_962.5), "5999.625");
    }

    #[test]
    fn refusals_are_one_line_with_a_count() {
        assert_eq!(flash_line(&[]), None);
        let one = vec![("DENIED", "6000.00: ES market is closed".to_owned())];
        assert_eq!(
            flash_line(&one),
            Some(("DENIED", "6000.00: ES market is closed".to_owned()))
        );
        // A rejected bracket: entry and both exits refused at once: one line, ×3.
        let three = vec![
            ("REJECTED", "6000.00: ES market is closed".to_owned()),
            (
                "REJECTED",
                "6004.00: bracket entry rejected: ES market is closed".to_owned(),
            ),
            (
                "REJECTED",
                "5998.00: bracket entry rejected: ES market is closed".to_owned(),
            ),
        ];
        assert_eq!(
            flash_line(&three),
            Some((
                "REJECTED",
                "5998.00: bracket entry rejected: ES market is closed ×3".to_owned()
            ))
        );
    }

    /// The header at its narrowest (MIN_DOM_WIDTH): its rows wrap, so it never overlaps as
    /// long as every item fits a line of its own. Widths at a wide 12 px glyph (7.5 px a
    /// character, bold), plus each item's padding and border; worst-case dynamic text.
    #[test]
    fn header_fits_the_narrowest_dom() {
        const CHAR: f32 = 7.5;
        const ITEM_PAD: f32 = 2.0 * 8.0 + 2.0; // px_2 (the widest) + border
        const ROW_PAD: f32 = 2.0 * 8.0; // px_2
        let w = |s: &str| s.chars().count() as f32 * CHAR + ITEM_PAD;
        let room = MIN_DOM_WIDTH - ROW_PAD;
        let mut items: Vec<String> = vec![
            "Paper 50K eval ▾".into(),
            "PAPER".into(),
            "SHORT 9999 @ 99999.125".into(),
            "+999.9 t  -$99999.99".into(),
            "Qty".into(),
            "…".into(),
            "9999_".into(),
            "Bracket 20/40 t".into(),
            "No bracket".into(),
            "ARMED · click trades".into(),
            "DISARMED".into(),
        ];
        items.push(unresolved_label(9, MIN_DOM_WIDTH));
        items.extend(order_button_labels(MIN_DOM_WIDTH).map(String::from));
        for i in &items {
            assert!(w(i) <= room, "{i:?} ({} px) doesn't fit {room} px", w(i));
        }
        // Flatten and Cancel all stay together (one group that never wraps apart).
        let group = w("Flatten") + 4.0 + w("Cancel all");
        assert!(group <= room, "Flatten + Cancel all: {group} > {room}");
        // Narrow DOMs use the short labels, wide ones the words.
        assert_eq!(order_button_labels(MIN_DOM_WIDTH)[0], "B MKT");
        assert_eq!(order_button_labels(SHORT_LABELS_BELOW - 1.0)[1], "S MKT");
        assert_eq!(order_button_labels(SHORT_LABELS_BELOW)[0], "Buy Mkt");
        assert_eq!(
            order_button_labels(0.0)[4],
            "Reverse",
            "before the first layout"
        );
        // Both label sets keep the side's word, never colour alone.
        for ws in [MIN_DOM_WIDTH, 800.0] {
            let l = order_button_labels(ws);
            assert!(l[0].to_lowercase().starts_with('b') && l[1].to_lowercase().starts_with('s'));
        }
    }

    #[test]
    fn trade_doc_saves_only_choices() {
        let d = TradeDoc::default();
        assert_eq!(serde_json::to_string(&d).unwrap(), "{}");
        let d = TradeDoc {
            account: Some("paper".into()),
            qty: Some(3),
            bracket: Some(BracketPreset {
                stop_ticks: 8,
                target_ticks: 16,
            }),
        };
        let back: TradeDoc = serde_json::from_str(&serde_json::to_string(&d).unwrap()).unwrap();
        assert_eq!(back, d);
    }
}
