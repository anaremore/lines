//! The strategy panel (decision 0025, phase c): every strategy instance with its state,
//! account and contract, position, today's trades and P&L, and its newest decision; Start
//! and Stop per instance and a prominent Stop all.
//!
//! - **Model:** `crate::strategy_panel` (pure). The pane formats rows only when something
//!   they show moved: the source, the core's events for the instances' accounts, the
//!   journal, the P&L display or the trade date.
//! - **Commands** go straight to the trading core (`Trading::submit`). Stop all is never
//!   gated and stays one press, even with a popover open (`popover::always_live`). Its key
//!   is in `commands::BINDINGS`; it is also in the palette.
//! - **Paper only:** strategies trade the paper broker only (the core refuses anything else),
//!   and the header says so.
//! - **Refusals:** a Start or Stop the core refuses shows inline on its row ("DENIED: …").

use std::sync::Arc;
use std::time::Instant;

use gpui::{
    App, Context, EventEmitter, FocusHandle, FontWeight, IntoElement, KeyDownEvent, MouseButton,
    MouseDownEvent, ParentElement, Render, Styled, Window, div, prelude::*, px,
};
use lines_core::CivilDate;
use lines_feed::store::ChartStore;
use lines_trading::strategy::StrategyId;
use lines_trading::{AccountId, CommandId, JournalShared, Source};

use crate::commands::{Command, StrategyCmd};
use crate::feeds::PaneFeed;
use crate::pane::{self, FeedChanged, Pane, PaneCommon};
use crate::pnl::{self, PnlDisplay};
use crate::strategy_panel::{
    self as model, Phase, Row, RowCtx, Strategies, StrategyInfo, StrategySource,
};
use crate::theme::Theme;
use crate::trading::{TradeCache, Trading};
use crate::wsdoc::PaneDoc;

/// Smallest hit target (docs/design.md: 20 × 20 px).
const MIN_HIT: f32 = 20.0;

pub struct StrategyView {
    feed: PaneFeed,
    common: PaneCommon,
    focus: FocusHandle,
    store: Arc<ChartStore>,
    theme: Theme,
    frozen: bool,
    source: Option<Arc<dyn StrategySource>>,
    cache: Option<Arc<TradeCache>>,
    journal: Option<Arc<JournalShared>>,
    seen_revision: u64,
    /// The inputs' revision and the trade date the rows were built from.
    built: Option<(u64, Option<CivilDate>)>,
    today: Option<CivilDate>,
    /// When `today` was last checked (about once a second).
    date_checked: Option<Instant>,
    mode: PnlDisplay,
    infos: Vec<StrategyInfo>,
    /// The instances' accounts (their trading revisions move the rows).
    accounts: Vec<AccountId>,
    rows: Vec<Row>,
    selected: Option<StrategyId>,
    /// Commands sent and not answered yet; a refusal becomes the row's note.
    pending: Vec<(StrategyId, CommandId)>,
    /// Refusals shown on their rows, with the row's state when shown (cleared when it moves).
    denied: Vec<(StrategyId, String, Option<Phase>)>,
}

impl EventEmitter<FeedChanged> for StrategyView {}

impl Pane for StrategyView {
    fn store(&self) -> &Arc<ChartStore> {
        &self.store
    }

    fn feed(&self) -> &PaneFeed {
        &self.feed
    }

    fn common(&mut self) -> &mut PaneCommon {
        &mut self.common
    }

    fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    fn save_state(&self, _: &mut PaneDoc) {}

    fn seen_revision(&mut self) -> &mut u64 {
        &mut self.seen_revision
    }

    /// The source's, the instances' accounts', the journal's and the P&L display's
    /// revisions (atomics only), never the market's.
    fn revision(&self) -> u64 {
        let mut r = self.source.as_ref().map_or(0, |s| s.revision());
        if let Some(c) = &self.cache {
            for a in &self.accounts {
                r = r.wrapping_add(c.revision(a).wrapping_mul(31));
            }
        }
        r = r.wrapping_add(self.journal.as_ref().map_or(0, |j| j.revision()) << 20);
        r.wrapping_add(pnl::display_epoch() << 44)
    }

    /// A new trade date (checked about once a second) restarts "today".
    fn changed(&mut self) -> bool {
        if self
            .date_checked
            .is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(1))
        {
            return false;
        }
        self.date_checked = Some(Instant::now());
        let before = self.today;
        before.is_some() && self.today() != before
    }

    fn set_feed(&mut self, feed: PaneFeed, cx: &mut Context<Self>) {
        self.store = feed.store().clone();
        self.feed = feed;
        self.built = None;
        cx.notify();
    }
}

impl StrategyView {
    pub fn new(feed: PaneFeed, theme: Theme, frozen: bool, cx: &mut Context<Self>) -> Self {
        let store = feed.store().clone();
        pane::watch_store(cx);
        let trading = Trading::try_global(cx);
        StrategyView {
            feed,
            common: PaneCommon::default(),
            focus: cx.focus_handle(),
            store,
            theme,
            frozen,
            source: cx.try_global::<Strategies>().map(|s| s.0.clone()),
            cache: trading.map(|t| t.cache.clone()),
            journal: trading.map(|t| t.journal.clone()),
            seen_revision: u64::MAX,
            built: None,
            today: None,
            date_checked: None,
            mode: PnlDisplay::default(),
            infos: Vec::new(),
            accounts: Vec::new(),
            rows: Vec::new(),
            selected: None,
            pending: Vec::new(),
            denied: Vec::new(),
        }
    }

    /// The panel never takes typed text.
    pub fn takes_text(&self) -> bool {
        false
    }

    /// A new look (the Settings window's appearance).
    pub(crate) fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
    }

    /// The current trade date of the pane's market (a replay's market time, else the wall
    /// clock); the last known one while the store is busy.
    fn today(&mut self) -> Option<CivilDate> {
        let replay =
            self.frozen || matches!(self.feed.config.source, lines_feed::Source::Replay { .. });
        if let Some(d) = self.store.try_read() {
            let now = d
                .market_time
                .filter(|_| replay)
                .unwrap_or_else(lines_feed::now_utc);
            self.today = Some(lines_feed::disk::partition::trade_date(&d.product, now));
        }
        self.today
    }

    /// Rebuild the rows if an input moved. False if the trading cache was busy this instant
    /// (try again on the next poll).
    fn rebuild(&mut self, cx: &App) -> bool {
        let today = self.today();
        // Read the revision once, before anything it covers: a change landing while the rows
        // are built moves it past this value, so the next poll builds again.
        let rev = (self.revision(), today);
        if self.built == Some(rev) {
            return true;
        }
        if let Some(s) = &self.source {
            self.infos = s.snapshot();
        }
        let mut accounts: Vec<AccountId> = self.infos.iter().map(|i| i.account.clone()).collect();
        accounts.sort_by(|a, b| a.0.cmp(&b.0));
        accounts.dedup();
        // New accounts: their revisions weren't in `rev`; build again on the next poll.
        let same_accounts = accounts == self.accounts;
        self.accounts = accounts;
        let Some(trading) = Trading::try_global(cx) else {
            self.rows.clear();
            self.built = Some(rev);
            return true;
        };
        let Some(state) = trading.cache.try_read() else {
            return false;
        };
        // Answers to commands this pane sent: a refusal is shown on its row.
        self.pending.retain(|(id, cmd)| match state.result(*cmd) {
            Some(Err(r)) => {
                self.denied.retain(|(d, _, _)| d != id);
                self.denied
                    .push((*id, format!("DENIED: {}", r.message), None));
                false
            }
            Some(Ok(_)) => {
                self.denied.retain(|(d, _, _)| d != id);
                false
            }
            None => true,
        });
        let snap = self.journal.as_ref().and_then(|j| j.snapshot());
        let trades = snap.as_ref().map_or(&[][..], |s| &s.trades[..]);
        let names = trading.accounts();
        let mode = self.mode;
        self.rows = self
            .infos
            .iter()
            .map(|i| {
                let name = names
                    .iter()
                    .find(|a| a.id == i.account)
                    .map_or(i.account.0.as_str(), |a| a.name.as_str());
                // One instance per account and contract (0025): orders there are its own
                // while it is armed.
                let working = state.open_orders(&i.account, i.contract).next().is_some();
                model::row(
                    i,
                    &RowCtx {
                        core: state.strategy(i.id),
                        position: state.position(&i.account, i.contract),
                        working,
                        trades,
                        today,
                        account_name: name,
                        mode,
                    },
                )
            })
            .collect();
        drop(state);
        // A refusal shows until the row's state changes (then its reason is the news).
        for (id, _, phase) in &mut self.denied {
            let now = self.rows.iter().find(|r| r.id == *id).map(|r| r.phase);
            phase.get_or_insert(now.unwrap_or(Phase::Stopped));
        }
        let rows = &self.rows;
        self.denied
            .retain(|(id, _, phase)| rows.iter().find(|r| r.id == *id).map(|r| r.phase) == *phase);
        self.built = same_accounts.then_some(rev);
        true
    }

    /// Run a strategy command from this pane (buttons, keys, the palette).
    pub fn run(
        &mut self,
        cmd: StrategyCmd,
        source: Source,
        cx: &mut Context<Self>,
    ) -> Result<(), &'static str> {
        let trading = Trading::try_global(cx).ok_or("trading isn't running")?;
        let issued = Instant::now();
        let (id, c) = match cmd {
            StrategyCmd::StopAll | StrategyCmd::StopAllKeep => {
                model::submit_stop_all(trading, cmd == StrategyCmd::StopAll, source);
                self.built = None;
                cx.notify();
                return Ok(());
            }
            StrategyCmd::Start | StrategyCmd::Stop => {
                let row = self
                    .selected
                    .and_then(|s| self.rows.iter().find(|r| r.id == s))
                    .ok_or("select a strategy first (click its row)")?;
                match cmd {
                    StrategyCmd::Start if row.can_start => (Some(row.id), model::start(row.id)),
                    StrategyCmd::Start => return Err("this strategy can't be started now"),
                    _ if row.can_stop => (Some(row.id), model::stop(row.id)),
                    _ => return Err("this strategy isn't running"),
                }
            }
            StrategyCmd::Open => return Ok(()),
            StrategyCmd::ToggleMarks => return Err("focus a chart first"),
        };
        let sent = trading.submit(source, issued, c);
        if let Some(id) = id {
            self.pending.push((id, sent));
        }
        self.built = None;
        cx.notify();
        Ok(())
    }

    fn click(&mut self, id: StrategyId, cmd: StrategyCmd, cx: &mut Context<Self>) {
        self.selected = Some(id);
        if let Err(e) = self.run(cmd, Source::Palette, cx) {
            self.denied.retain(|(d, _, _)| *d != id);
            self.denied.push((id, e.to_owned(), None));
            self.built = None;
        }
        cx.notify();
    }

    fn on_key_down(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        let m = k.modifiers;
        if m.control || m.alt || m.platform || self.rows.is_empty() {
            return;
        }
        let i = self
            .selected
            .and_then(|s| self.rows.iter().position(|r| r.id == s));
        let n = self.rows.len();
        let next = match k.key.as_str() {
            "down" => i.map_or(0, |i| (i + 1).min(n - 1)),
            "up" => i.map_or(0, |i| i.saturating_sub(1)),
            _ => return,
        };
        self.selected = Some(self.rows[next].id);
        cx.stop_propagation();
        cx.notify();
    }

    fn header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let any = !self.rows.is_empty();
        let stop_all = crate::tooltip::tip_div(
            div()
                .id("stop-all")
                .flex()
                .items_center()
                .justify_center()
                .min_w(px(MIN_HIT))
                .h(px(MIN_HIT))
                .flex_shrink_0()
                .px_2()
                .rounded_sm()
                .border_2()
                .border_color(t.warn)
                .text_color(t.text_main)
                .font_weight(FontWeight::BOLD)
                .cursor_pointer()
                .child("Stop all")
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        // One press even over a popover, which it closes (as Flatten does).
                        crate::popover::close_any(cx);
                        let _ = this.run(StrategyCmd::StopAll, Source::Palette, cx);
                    }),
                ),
            "Stop all strategies and flatten their positions",
            Some(Command::Strategy(StrategyCmd::StopAll)),
            &t,
        );
        div()
            .flex()
            .flex_row()
            .whitespace_nowrap()
            .overflow_hidden()
            .px_2()
            .pr(px(pane::controls_gutter(
                crate::layout::PaneKind::Strategies,
            )))
            .py_1()
            .bg(t.panel)
            .border_b_1()
            .border_t_1()
            .border_color(t.axis_line)
            .text_size(px(12.0))
            .child(
                pane::header_content()
                    // The left group gives way (its note truncates) so Stop all always fits.
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .items_center()
                            .min_w_0()
                            .overflow_hidden()
                            .children(pane::link_chip(self.common.link, &t))
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_color(t.text_main)
                                    .child("Strategies"),
                            )
                            .child(crate::trade_bar::paper_badge(&t).flex_shrink_0())
                            // Strategies don't use a DOM's trading switch: a chip of its own.
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .px_1()
                                    .rounded_sm()
                                    .border_1()
                                    .border_color(t.control_edge)
                                    .text_color(t.text_main)
                                    .child("Not gated by the DOM's trading switch"),
                            ),
                    )
                    // Always visible while any instance exists (decision 0025).
                    .when(any, |d| d.child(crate::popover::always_live(stop_all))),
            )
    }

    fn chip(&self, phase: Phase) -> gpui::Div {
        let t = self.theme;
        let loud = matches!(phase, Phase::Halted | Phase::Faulted | Phase::Paused);
        // A lockout (halted, faulted) is a filled chip; a pause is outlined: never the same look.
        if matches!(phase, Phase::Halted | Phase::Faulted) {
            return div()
                .flex_shrink_0()
                .px_1()
                .rounded_sm()
                .border_1()
                .border_color(t.warn)
                .bg(t.warn)
                .text_color(t.on_warn)
                .font_weight(FontWeight::BOLD)
                .child(phase.word());
        }
        div()
            .flex_shrink_0()
            .px_1()
            .rounded_sm()
            .border_1()
            .border_color(if loud {
                t.warn
            } else if phase == Phase::Stopped {
                t.axis_line
            } else {
                t.accent
            })
            .text_color(if phase == Phase::Stopped {
                t.text_dim
            } else {
                t.text_main
            })
            .font_weight(FontWeight::SEMIBOLD)
            .child(phase.word())
    }

    fn button(
        &self,
        id: StrategyId,
        cmd: StrategyCmd,
        label: &'static str,
        name: &'static str,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let t = self.theme;
        // Each action keeps its slot on every row (nothing moves under the pointer); one that
        // doesn't apply is drawn dim and does nothing. Stop has Flatten's warning outline.
        let b = div()
            .id((label, u64::from(id.0)))
            .flex()
            .items_center()
            .justify_center()
            .min_w(px(MIN_HIT * 2.5))
            .h(px(MIN_HIT + 2.0))
            .px_2()
            .rounded_sm()
            .border_1()
            .child(label);
        if !enabled {
            // Not available: no outline at all (shape, not only a lighter gray).
            return b
                .border_color(gpui::transparent_black())
                .text_color(t.text_dim);
        }
        let b = if cmd == StrategyCmd::Stop {
            b.border_color(t.warn).font_weight(FontWeight::SEMIBOLD)
        } else {
            b.border_color(t.control_edge)
        };
        crate::tooltip::tip_div(
            b.text_color(t.text_main).cursor_pointer().on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.click(id, cmd, cx);
                }),
            ),
            name,
            None,
            &t,
        )
    }

    fn row_el(&self, r: &Row, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let selected = self.selected == Some(r.id);
        let pnl_color = match r.pnl_sign {
            1 => t.up,
            -1 => t.down,
            _ => t.text_main,
        };
        let denied = self
            .denied
            .iter()
            .find(|(d, _, _)| *d == r.id)
            .map(|(_, m, _)| m.clone());
        let id = r.id;
        let line1 = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .min_w_0()
            .child(self.chip(r.phase))
            .child(
                div()
                    .flex_shrink_0()
                    .text_color(t.text_main)
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(r.label.clone()),
            )
            // One text, so a narrow pane truncates its end with "…": the symbol stays, the
            // version goes first.
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_color(t.text_main)
                    .child(format!("{} · {}", r.scope, r.strategy)),
            );
        let mut line2 = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .min_w_0()
            .child(
                div()
                    .flex_shrink_0()
                    .text_color(t.text_main)
                    .child(r.position.clone()),
            );
        let limits = div()
            .truncate()
            .text_color(t.text_dim)
            .child(r.limits.clone());
        if let Some(p) = &r.pnl {
            // Labeled by scope: this instance's P&L today (closed trades plus open).
            // The number is never cut: its label gives way first.
            line2 = line2.child(
                div()
                    .flex()
                    .flex_row()
                    .gap_1()
                    .min_w_0()
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_color(t.text_dim)
                            .child("P&L today"),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_color(pnl_color)
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(p.clone()),
                    ),
            );
        }
        let line2 = line2.child(
            div()
                .min_w_0()
                .truncate()
                .text_color(t.text_main)
                .child(r.trades.clone()),
        );
        // Every row has the same lines (blank when empty), so rows never change height and
        // nothing below moves.
        const BLANK: &str = "\u{a0}";
        // The reason and the newest decision, in that order, then blank lines up to two:
        // every row has the same height (nothing below moves) and its slack at the bottom.
        let mut lines: Vec<(String, gpui::Hsla)> = Vec::with_capacity(2);
        if let Some(w) = denied.or(r.why.clone()) {
            lines.push((w, t.text_main));
        }
        if let Some(l) = r.last.clone() {
            lines.push((l, t.text_dim));
        }
        while lines.len() < 2 {
            lines.push((BLANK.to_owned(), t.text_dim));
        }
        let notes = div().flex().flex_col().min_w_0().children(
            lines
                .into_iter()
                .map(|(text, color)| div().truncate().text_color(color).child(text)),
        );
        let buttons = div()
            .flex()
            .flex_row()
            .gap_1()
            .flex_shrink_0()
            .child(self.button(
                id,
                StrategyCmd::Start,
                "Start",
                "Start this strategy (paper)",
                r.can_start,
                cx,
            ))
            .child(self.button(
                id,
                StrategyCmd::Stop,
                "Stop",
                "Stop this strategy and flatten its position",
                r.can_stop,
                cx,
            ));
        div()
            .id(("strategy-row", u64::from(id.0)))
            .flex()
            .flex_row()
            .items_start()
            .gap_2()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(t.grid)
            .when(selected, |d| d.border_l_2().border_color(t.accent))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                    this.selected = Some(id);
                    cx.notify();
                }),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .gap_1()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(line1)
                    .child(line2)
                    .child(limits)
                    .child(notes),
            )
            .child(buttons)
    }
}

impl Render for StrategyView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mode = cx
            .try_global::<crate::picker_view::AppSettings>()
            .map(|a| a.settings.pnl_display)
            .unwrap_or_default();
        if mode != self.mode {
            self.mode = mode;
            self.built = None;
        }
        if !self.rebuild(cx) {
            // The trading cache was busy this instant: draw again on the next poll.
            self.seen_revision = u64::MAX;
        }
        self.common.paints += 1;
        let _ = window;
        let t = self.theme;
        let rows: Vec<_> = self
            .rows
            .clone()
            .iter()
            .map(|r| self.row_el(r, cx).into_any_element())
            .collect();
        let problem = self.source.as_ref().and_then(|s| s.problem());
        let empty = if self.rows.is_empty() {
            let msg = match &problem {
                Some(p) => format!("The strategy couldn't start: {p}"),
                None if self.source.is_some() => "Starting the strategy…".to_owned(),
                None => "No strategies. Run Lines with --strategy NAME --backtest-config FILE; strategies trade paper only."
                    .to_owned(),
            };
            Some(div().px_2().py_2().text_color(t.text_dim).child(msg))
        } else {
            problem.map(|p| div().px_2().py_1().text_color(t.text_main).child(p))
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(t.bg)
            .text_size(px(12.0))
            .child(self.header(cx))
            .child(
                div()
                    .id("strategies")
                    .flex_1()
                    .flex()
                    .flex_col()
                    .overflow_y_scroll()
                    .track_focus(&self.focus)
                    .on_key_down(cx.listener(Self::on_key_down))
                    .children(empty)
                    .children(rows),
            )
    }
}
