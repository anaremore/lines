//! The replay bar (owner-approved mock, 2026-11-09): at the bottom of the main window while a
//! recording or stored session plays. REPLAY tag, previous / play-pause / next session, the
//! speeds, a timeline with the regular session's open and close, the replay's time in the
//! display time zone, and Go to….
//!
//! The control itself is `lines_feed::replay_control`: the feed paces frames by it, and the
//! store's and the trading core's market clocks read it, so a paused replay stands still
//! everywhere. Every jump ends paused at its target. A jump back restarts the file (the feed
//! rewinds its store); before it, the paper accounts' working orders and triggers are
//! canceled, since they were placed later in the replay than where it lands, and the bar says
//! how many.

use std::cell::Cell;
use std::fmt::Write as _;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    App, Context, FontWeight, Global, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ParentElement, Render, SharedString, Styled, Window, canvas, div, prelude::*, px,
};
use lines_core::display_tz::{self, DisplayTz};
use lines_core::sessions::{SessionCalendar, SessionKind};
use lines_core::{CivilDate, Timeframe, UtcMillis};
use lines_feed::replay_control::{ReplayControl, SPEEDS, Snapshot};

use crate::commands::{Command, ReplayCmd};
use crate::icons::{self, Icon};
use crate::theme::Theme;

/// The replay this run plays, for the bar (a GPUI global; only with `--source replay`).
pub struct Replay {
    pub control: Arc<ReplayControl>,
    /// The recorded product's trading hours (session jumps, the timeline's marks).
    pub calendar: Option<&'static SessionCalendar>,
    /// What the last jump back canceled, for the bar to show, from whichever window ran it.
    pub note: Option<String>,
}

impl Global for Replay {}

/// The bar's height at 100 % UI zoom.
pub const BAR_H_100: f32 = 36.0;

/// Below this width (at 100 % UI zoom) the speed chips collapse into one dropdown.
pub const NARROW_100: f32 = 980.0;

/// Whether the bar, `width` logical px wide, shows its speeds as one dropdown.
pub fn speeds_collapse(width: f32) -> bool {
    width > 0.0 && width < crate::zoom::f(NARROW_100)
}

/// The bar's height (logical px at the current UI zoom).
pub fn bar_h() -> f32 {
    crate::zoom::f(BAR_H_100)
}

/// Whether a replay key goes to the focused pane instead: no replay plays, the pane takes
/// typed text, or it is the option chain, whose Left/Right step its expiries.
pub fn key_goes_to_pane(cmd: ReplayCmd, replaying: bool, typing: bool, chain: bool) -> bool {
    !replaying || typing || (chain && matches!(cmd, ReplayCmd::StepBack | ReplayCmd::StepForward))
}

/// A chart timeframe's bar length for stepping: tick, volume and range bars have none, so
/// they step a minute.
pub fn step_ms(tf: Option<Timeframe>) -> i64 {
    match tf {
        Some(Timeframe::Seconds(n)) => i64::from(n) * 1000,
        Some(Timeframe::Minutes(n)) => i64::from(n) * UtcMillis::MINUTE,
        Some(Timeframe::Hours(n)) => i64::from(n) * 60 * UtcMillis::MINUTE,
        Some(Timeframe::Day) => 24 * 60 * UtcMillis::MINUTE,
        Some(Timeframe::Week) => 7 * 24 * 60 * UtcMillis::MINUTE,
        _ => UtcMillis::MINUTE,
    }
}

/// One bar on from `clock`: forward completes the current bar (its end); back goes to the
/// current bar's start (the bar before it complete), or a whole bar back from a boundary.
/// `None`: no such time (a corrupt recording's clock at the edge of the range): refused, never
/// wrapped (a wrapped step forward would read as a jump back and cancel the orders).
pub fn step_target(clock: UtcMillis, period: i64, forward: bool) -> Option<UtcMillis> {
    let p = period.max(1000);
    let start = clock.0.div_euclid(p).checked_mul(p)?;
    Some(UtcMillis(match (forward, clock.0 > start) {
        (true, _) => start.checked_add(p)?,
        (false, true) => start,
        (false, false) => start.checked_sub(p)?,
    }))
}

/// The previous or next session open from `clock`: the regular session's where the product
/// has one (the 09:30 open), else the electronic session's. `None`: no such open (a market
/// that never closes, or a calendar that ends).
pub fn session_target(cal: &SessionCalendar, clock: UtcMillis, forward: bool) -> Option<UtcMillis> {
    let kind = if cal.has_rth() {
        SessionKind::Rth
    } else {
        SessionKind::Eth
    };
    if forward {
        let mut from = clock;
        // The session `clock` is in has opened already: the next one's open.
        for _ in 0..16 {
            let s = cal.next_session(from, kind)?;
            if s.open > clock {
                return Some(s.open);
            }
            from = s.close;
        }
        None
    } else {
        // Two weeks back covers any holiday run.
        cal.sessions(
            UtcMillis(clock.0.checked_sub(14 * 24 * 60 * UtcMillis::MINUTE)?),
            clock,
            kind,
        )
        .filter(|s| s.open < clock)
        .map(|s| s.open)
        .last()
    }
}

/// "Go to…": `HH:MM`, `HH:MM:SS`, or either after a date `YYYY-MM-DD`, in `zone`. A time
/// alone is on the replay's current day (in `zone`).
pub fn parse_go_to(
    text: &str,
    now: UtcMillis,
    zone: &DisplayTz,
) -> Result<UtcMillis, &'static str> {
    const BAD: &str = "use HH:MM, HH:MM:SS or YYYY-MM-DD HH:MM";
    let mut parts = text.split_whitespace();
    let (date, time) = match (parts.next(), parts.next(), parts.next()) {
        (Some(t), None, None) => (None, t),
        (Some(d), Some(t), None) => (Some(d), t),
        _ => return Err(BAD),
    };
    let date = match date {
        None => {
            let (y, m, d, ..) = zone.civil(now);
            CivilDate::new(u16::try_from(y).map_err(|_| BAD)?, m as u8, d as u8)
        }
        Some(d) => {
            let mut f = d.split('-');
            let (y, m, d) = (f.next(), f.next(), f.next());
            if f.next().is_some() {
                return Err(BAD);
            }
            let n = |s: Option<&str>| s.and_then(|s| s.parse::<u16>().ok());
            match (n(y), n(m), n(d)) {
                (Some(y), Some(m), Some(d)) if m <= 12 && d <= 31 => {
                    CivilDate::new(y, m as u8, d as u8)
                }
                _ => None,
            }
        }
    }
    .ok_or(BAD)?;
    let mut f = time.split(':');
    let n = |s: Option<&str>| s.and_then(|s| s.parse::<u32>().ok());
    let (h, m, s) = match (f.next(), f.next(), f.next(), f.next()) {
        (h, Some(m), None, None) => (n(h), n(Some(m)), Some(0)),
        (h, Some(m), Some(s), None) => (n(h), n(Some(m)), n(Some(s))),
        _ => return Err(BAD),
    };
    let (Some(h), Some(m), Some(s)) = (h, m, s) else {
        return Err(BAD);
    };
    if h > 23 || m > 59 || s > 59 {
        return Err(BAD);
    }
    // The zone's wall clock as if it were UTC; its offset found where that lands (twice, for
    // a time beside a DST change).
    let local = date.start_utc().0 + i64::from(h * 3600 + m * 60 + s) * 1000;
    let guess = local - zone.offset_ms(UtcMillis(local));
    Ok(UtcMillis(local - zone.offset_ms(UtcMillis(guess))))
}

/// A speed's label on the bar.
pub fn speed_label(s: f64) -> &'static str {
    match s {
        s if s <= 0.0 => "Max",
        s if s <= 1.0 => "1×",
        s if s <= 2.0 => "2×",
        s if s <= 10.0 => "10×",
        _ => "60×",
    }
}

/// Where `t` sits on the timeline, 0..=1.
pub fn fraction(t: UtcMillis, first: UtcMillis, last: UtcMillis) -> f32 {
    if last.0 <= first.0 {
        return 1.0;
    }
    // In i128: the span of a corrupt recording can exceed i64.
    let (t, a, b) = (i128::from(t.0), i128::from(first.0), i128::from(last.0));
    ((t - a) as f64 / (b - a) as f64).clamp(0.0, 1.0) as f32
}

/// The time at `f` (0..=1) along the timeline.
pub fn time_at(f: f32, first: UtcMillis, last: UtcMillis) -> UtcMillis {
    let f = f64::from(f.clamp(0.0, 1.0));
    UtcMillis(first.0 + ((last.0 - first.0) as f64 * f).round() as i64)
}

/// The regular session's opens and closes inside the timeline (`true`: an open), at most a
/// dozen: the marks under the track.
pub fn session_marks(
    cal: &SessionCalendar,
    first: UtcMillis,
    last: UtcMillis,
) -> Vec<(UtcMillis, bool)> {
    let kind = if cal.has_rth() {
        SessionKind::Rth
    } else {
        SessionKind::Eth
    };
    let mut v = Vec::new();
    for s in cal.sessions(first, last, kind).take(6) {
        for (t, open) in [(s.open, true), (s.close, false)] {
            if (first.0..=last.0).contains(&t.0) {
                v.push((t, open));
            }
        }
    }
    v
}

/// What a jump back cancels on the paper accounts: their working orders and triggers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Canceled {
    pub orders: usize,
    pub triggers: usize,
}

impl Canceled {
    /// The bar's note, or `None` when nothing was working.
    pub fn note(self) -> Option<String> {
        let words =
            |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
        match (self.orders, self.triggers) {
            (0, 0) => None,
            (o, 0) => Some(format!(
                "Jumped back: {} canceled",
                words(o, "replay order", "replay orders")
            )),
            (0, t) => Some(format!(
                "Jumped back: {} canceled",
                words(t, "trigger", "triggers")
            )),
            (o, t) => Some(format!(
                "Jumped back: {} and {} canceled",
                words(o, "replay order", "replay orders"),
                words(t, "trigger", "triggers")
            )),
        }
    }
}

/// Jump to `t` (paused there). A jump back first cancels the paper accounts' working orders
/// and triggers (they belong to later in the replay); the note says how many.
pub fn seek(t: UtcMillis, cx: &mut App) -> Option<String> {
    let r = cx.try_global::<Replay>()?;
    let control = r.control.clone();
    // Paused first: the clock holds, so "is this back?" here and in the control agree.
    control.set_paused(true);
    let s = control.snapshot();
    let t = match s.first {
        Some(f) if t.0 < f.0 => f,
        _ => t,
    };
    let back = s.clock.is_some_and(|c| t.0 < c.0);
    let canceled = if back {
        cancel_working(cx)
    } else {
        Canceled::default()
    };
    control.seek(t);
    let note = canceled.note();
    if note.is_some() {
        cx.global_mut::<Replay>().note = note.clone();
    }
    note
}

/// Cancel every paper account's working orders and live triggers (a replay's paper accounts
/// trade only the replay: its state isn't saved).
fn cancel_working(cx: &App) -> Canceled {
    use lines_trading::trigger::TriggerState;
    use lines_trading::{Command as TradeCommand, Source};
    let Some(trading) = crate::trading::Trading::try_global(cx) else {
        return Canceled::default();
    };
    let mut out = Canceled::default();
    let paper: Vec<_> = trading
        .accounts()
        .into_iter()
        .filter(|a| a.paper)
        .map(|a| a.id)
        .collect();
    let (orders, triggers) = {
        let st = trading.cache.read();
        let orders: Vec<_> = paper
            .iter()
            .map(|a| (a.clone(), st.open_orders_of(a).count()))
            .collect();
        let triggers: Vec<_> = st
            .triggers()
            .filter(|t| paper.contains(&t.account) && t.state != TriggerState::Ended)
            .map(|t| (t.account.clone(), t.contract, t.id))
            .collect();
        (orders, triggers)
    };
    let now = Instant::now();
    for (account, contract, id) in triggers {
        trading.submit(
            Source::Hotkey,
            now,
            TradeCommand::CancelTrigger {
                account,
                contract,
                id,
            },
        );
        out.triggers += 1;
    }
    // The paper broker cancels its working orders itself when the core sees the jump
    // (`Broker::rewound`): at once, before any replayed data, whatever order the messages
    // arrive in. Here they are counted for the note.
    out.orders = orders.iter().map(|(_, n)| n).sum();
    out
}

/// Run a replay command. `step`: the focused chart's bar length ([`step_ms`]). `Err`: why it
/// can't (said in the footer); `Ok(Some)`: a note for the bar.
pub fn run(cmd: ReplayCmd, step: i64, cx: &mut App) -> Result<Option<String>, &'static str> {
    let Some(r) = cx.try_global::<Replay>() else {
        return Err("no replay is playing");
    };
    let (control, calendar) = (r.control.clone(), r.calendar);
    let s = control.snapshot();
    let clock = s.clock;
    match cmd {
        ReplayCmd::PlayPause => {
            control.toggle();
            Ok(None)
        }
        ReplayCmd::Slower | ReplayCmd::Faster => {
            control.step_speed(cmd == ReplayCmd::Faster);
            Ok(None)
        }
        ReplayCmd::StepBack | ReplayCmd::StepForward => {
            let clock = clock.ok_or("the replay hasn't started")?;
            let t =
                step_target(clock, step, cmd == ReplayCmd::StepForward).ok_or("no bar there")?;
            Ok(seek(t, cx))
        }
        ReplayCmd::PrevSession | ReplayCmd::NextSession => {
            let clock = clock.ok_or("the replay hasn't started")?;
            let cal = calendar.ok_or("this product has no session hours")?;
            let t = session_target(cal, clock, cmd == ReplayCmd::NextSession)
                .ok_or("no session open there")?;
            Ok(seek(t, cx))
        }
        // The workspace opens its Go to prompt.
        ReplayCmd::GoTo => Ok(None),
    }
}

/// The bar itself (main window only).
pub struct ReplayBar {
    pub theme: Theme,
    control: Arc<ReplayControl>,
    calendar: Option<&'static SessionCalendar>,
    /// Dragging the playhead: where along the track (0..=1).
    drag: Option<f32>,
    /// The track's left edge and width (window px), from the last paint.
    track: Rc<Cell<(f32, f32)>>,
    /// What the last jump back canceled; cleared when the replay plays again.
    note: Option<String>,
    seen: (u64, Option<i64>),
    /// The bar's width from the last paint (logical px; 0: not painted yet).
    width: Rc<Cell<f32>>,
    /// Whether the last frame drew the speeds collapsed (a change redraws).
    collapsed: bool,
    /// The collapsed speed list is open.
    speeds_open: bool,
}

impl ReplayBar {
    pub fn new(theme: Theme, cx: &mut Context<Self>) -> Option<ReplayBar> {
        let r = cx.try_global::<Replay>()?;
        let (control, calendar) = (r.control.clone(), r.calendar);
        // Redraw when the control changes or the shown second moves (5 Hz: the clock reads
        // whole seconds; nothing paints between).
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(200))
                    .await;
                let alive = this.update(cx, |v, cx| {
                    // A jump back from another window (a popped-out chart's key): its note.
                    if let Some(n) = cx.try_global::<Replay>().and_then(|r| r.note.clone())
                        && v.note.as_ref() != Some(&n)
                    {
                        v.note = Some(n);
                        cx.notify();
                    }
                    // The window's width crossed the collapse point.
                    if speeds_collapse(v.width.get()) != v.collapsed {
                        cx.notify();
                    }
                    let s = v.control.snapshot();
                    let now = (v.control.revision(), s.clock.map(|c| c.0 / 1000));
                    if now != v.seen {
                        v.seen = now;
                        if !s.paused && v.note.is_some() && s.seeking.is_none() {
                            v.note = None;
                            if cx.has_global::<Replay>() {
                                cx.global_mut::<Replay>().note = None;
                            }
                        }
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();
        Some(ReplayBar {
            theme,
            control,
            calendar,
            drag: None,
            track: Rc::new(Cell::new((0.0, 0.0))),
            note: None,
            seen: (0, None),
            width: Rc::new(Cell::new(0.0)),
            collapsed: false,
            speeds_open: false,
        })
    }

    /// A note from a jump the workspace ran (keys, Go to).
    pub fn set_note(&mut self, note: Option<String>, cx: &mut Context<Self>) {
        if note.is_some() {
            self.note = note;
            cx.notify();
        }
    }

    fn at_x(&self, x: f32) -> f32 {
        let (x0, w) = self.track.get();
        if w <= 0.0 {
            return 0.0;
        }
        ((x - x0) / w).clamp(0.0, 1.0)
    }

    fn command(&mut self, cmd: ReplayCmd, cx: &mut Context<Self>) {
        if let Ok(note) = run(cmd, UtcMillis::MINUTE, cx) {
            self.set_note(note, cx);
        }
        cx.notify();
    }

    fn icon_button(
        &self,
        id: &'static str,
        icon: Icon,
        name: &'static str,
        cmd: ReplayCmd,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        icons::button(
            id,
            icon,
            name,
            Some(Command::Replay(cmd)),
            false,
            &self.theme,
        )
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                cx.stop_propagation();
                this.command(cmd, cx);
            }),
        )
    }
}

impl Render for ReplayBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let s: Snapshot = self.control.snapshot();
        let zone = display_tz::current();
        let ui = crate::zoom::ui;

        let tag = div()
            .flex_none()
            .px_1()
            .rounded_sm()
            .bg(t.warn)
            .text_color(t.on_warn)
            .font_weight(FontWeight::BOLD)
            .text_size(ui(10.0))
            .child("REPLAY");

        let play = {
            let (icon, name) = if s.paused {
                (Icon::Play, "Play")
            } else {
                (Icon::Pause, "Pause")
            };
            let fg = t.bg;
            crate::tooltip::with_tip(
                div()
                    .id("replay-play")
                    .flex_none()
                    .w(px(icons::HIT))
                    .h(px(icons::HIT))
                    .rounded_sm()
                    .cursor_pointer()
                    .bg(t.accent)
                    .child(
                        canvas(
                            |_, _, _| (),
                            move |b, (), w, _| icons::paint(icon, b, fg, w),
                        )
                        .size_full(),
                    ),
                name,
                Some(Command::Replay(ReplayCmd::PlayPause)),
                &t,
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.command(ReplayCmd::PlayPause, cx);
                }),
            )
        };

        self.collapsed = speeds_collapse(self.width.get());
        let mut speeds = div()
            .flex()
            .flex_row()
            .flex_none()
            .border_1()
            .border_color(t.control_edge)
            .rounded_sm()
            .overflow_hidden();
        for (i, sp) in SPEEDS.iter().copied().enumerate() {
            let on = sp == s.speed;
            let control = self.control.clone();
            speeds = speeds.child(
                div()
                    .id(("replay-speed", i))
                    .px_2()
                    .cursor_pointer()
                    .when(on, |d| d.bg(t.axis_line).text_color(t.text_main))
                    .when(!on, |d| d.text_color(t.text_dim))
                    .child(speed_label(sp))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |_, _: &MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            control.set_speed(sp);
                            cx.notify();
                        }),
                    ),
            );
        }

        // The timeline: from the first recorded frame to the last (once the scan finds it;
        // until then, to the clock).
        let (first, last) = match (s.first, s.last.or(s.clock)) {
            (Some(f), Some(l)) => (f, l.max(f)),
            _ => (UtcMillis(0), UtcMillis(0)),
        };
        let shown_at = match self.drag {
            Some(f) => Some(time_at(f, first, last)),
            None => s.seeking.or(s.clock),
        };
        let done = shown_at.map_or(0.0, |c| fraction(c, first, last));
        let marks = match self.calendar {
            Some(cal) if last.0 > first.0 => session_marks(cal, first, last),
            _ => Vec::new(),
        };
        let slot = self.track.clone();
        let mut track = div()
            .id("replay-track")
            .relative()
            .flex_1()
            .min_w(ui(120.0))
            .h(ui(26.0))
            .cursor_pointer()
            .child(
                canvas(
                    move |b, _, _| slot.set((f32::from(b.origin.x), f32::from(b.size.width))),
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .child(
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .top(ui(9.0))
                    .h(ui(4.0))
                    .rounded_sm()
                    .bg(t.axis_line),
            )
            .child(
                div()
                    .absolute()
                    .left_0()
                    .top(ui(9.0))
                    .h(ui(4.0))
                    .rounded_sm()
                    .bg(t.accent)
                    .w(gpui::relative(done)),
            );
        for (at, open) in marks {
            let f = fraction(at, first, last);
            let mut label = String::new();
            let _ = zone.write_hhmm(at, &mut label);
            track = track
                .child(
                    div()
                        .absolute()
                        .left(gpui::relative(f))
                        .top(ui(6.0))
                        .w(px(1.0))
                        .h(ui(10.0))
                        .bg(t.text_dim),
                )
                .child(
                    div()
                        .absolute()
                        .left(gpui::relative(f))
                        .top(ui(15.0))
                        .text_size(ui(9.0))
                        .text_color(t.text_dim)
                        // Opens label to their right, closes to their left.
                        .when(!open, |d| d.ml(ui(-26.0)))
                        .child(label),
                );
        }
        track = track.child(
            div()
                .absolute()
                .left(gpui::relative(done))
                .ml(ui(-1.5))
                .top(ui(3.0))
                .w(ui(3.0))
                .h(ui(16.0))
                .rounded_sm()
                .bg(t.text_main),
        );
        let track = track
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.control.set_paused(true);
                    this.drag = Some(this.at_x(f32::from(ev.position.x)));
                    cx.notify();
                }),
            )
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
                if this.drag.is_some() {
                    if ev.pressed_button != Some(MouseButton::Left) {
                        this.drag = None;
                    } else {
                        this.drag = Some(this.at_x(f32::from(ev.position.x)));
                    }
                    cx.notify();
                }
            }));

        let mut time = String::new();
        let mut day = String::new();
        if let Some(c) = shown_at {
            let _ = zone.write_clock(c, &mut time);
            let _ = zone.write_name(c, &mut day);
            let (y, m, d, ..) = zone.civil(c);
            let wd = zone.local(c).weekday() as usize;
            const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
            const MONTHS: [&str; 12] = [
                "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
            ];
            let _ = write!(
                day,
                " · {} {} {d}, {y}",
                DAYS[wd % 7],
                MONTHS[(m as usize).clamp(1, 12) - 1]
            );
        }

        let go_to = div()
            .id("replay-go-to")
            .flex_none()
            .px_2()
            .border_1()
            .border_color(t.control_edge)
            .rounded_sm()
            .cursor_pointer()
            .text_color(t.text_main)
            .child("Go to…")
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    window.dispatch_action(Box::new(GoTo), cx);
                }),
            );

        div()
            .id("replay-bar")
            .h(px(bar_h()))
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_3()
            .overflow_hidden()
            .bg(t.panel)
            .border_t_1()
            .border_color(t.axis_line)
            .text_size(ui(12.0))
            .text_color(t.text_main)
            .relative()
            .child({
                let w = self.width.clone();
                canvas(
                    move |b, _, _| w.set(f32::from(b.size.width)),
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full()
            })
            .child(tag)
            .child(self.icon_button(
                "replay-prev",
                Icon::SkipBack,
                "Previous session open",
                ReplayCmd::PrevSession,
                cx,
            ))
            .child(play)
            .child(self.icon_button(
                "replay-next",
                Icon::SkipForward,
                "Next session open",
                ReplayCmd::NextSession,
                cx,
            ))
            .child(if self.collapsed {
                self.speed_dropdown(s.speed, cx).into_any_element()
            } else {
                speeds.text_size(ui(11.0)).into_any_element()
            })
            .child(track)
            .child(
                div()
                    .flex_none()
                    .font_weight(FontWeight::BOLD)
                    .child(SharedString::from(time)),
            )
            .child(div().flex_none().text_color(t.text_dim).child(day))
            .children(self.note.clone().map(|n| {
                div()
                    .flex_none()
                    .px_1()
                    .rounded_sm()
                    .bg(t.warn)
                    .text_color(t.on_warn)
                    .child(n)
            }))
            .child(go_to)
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| this.end_drag(cx)),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| this.end_drag(cx)),
            )
    }
}

impl ReplayBar {
    /// A narrow bar's speeds: one chip with the speed in force ("10× ▾"), its list below.
    fn speed_dropdown(&mut self, speed: f64, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let ui = crate::zoom::ui;
        let trigger = div()
            .id("replay-speed-chip")
            .flex_none()
            .px_2()
            .border_1()
            .border_color(t.control_edge)
            .rounded_sm()
            .cursor_pointer()
            .text_size(ui(11.0))
            .child(format!("{} ▾", speed_label(speed)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.speeds_open = !this.speeds_open;
                    cx.notify();
                }),
            );
        let list = self.speeds_open.then(|| {
            let mut l = div()
                .flex()
                .flex_col()
                .py_1()
                .bg(t.bg)
                .border_1()
                .border_color(t.control_edge)
                .rounded_sm()
                .text_size(ui(11.0));
            for (i, sp) in SPEEDS.iter().copied().enumerate() {
                let on = sp == speed;
                let control = self.control.clone();
                l = l.child(
                    div()
                        .id(("replay-speed-row", i))
                        .px_3()
                        .cursor_pointer()
                        .hover(|d| d.bg(t.axis_line))
                        .when(on, |d| d.text_color(t.text_main))
                        .when(!on, |d| d.text_color(t.text_dim))
                        .child(speed_label(sp))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                                cx.stop_propagation();
                                control.set_speed(sp);
                                this.speeds_open = false;
                                cx.notify();
                            }),
                        ),
                );
            }
            l.into_any_element()
        });
        let dismiss: crate::popover::OnDismiss =
            Box::new(cx.listener(|this, _: &MouseDownEvent, _, cx| {
                this.speeds_open = false;
                cx.notify();
            }));
        crate::dropdown::dropdown(trigger, list).on_dismiss(dismiss)
    }

    fn end_drag(&mut self, cx: &mut Context<Self>) {
        let Some(f) = self.drag.take() else {
            return;
        };
        let s = self.control.snapshot();
        if let (Some(first), Some(last)) = (s.first, s.last.or(s.clock)) {
            let note = seek(time_at(f, first, last.max(first)), cx);
            self.set_note(note, cx);
        }
        cx.notify();
    }
}

gpui::actions!(replay_bar, [GoTo]);

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> UtcMillis {
        UtcMillis::parse_iso(s).unwrap()
    }

    /// Forward completes the current bar; back shows the bar before it complete, and from a
    /// boundary goes a whole bar back.
    #[test]
    fn a_step_is_one_bar() {
        let m = UtcMillis::MINUTE;
        let c = at("2026-11-05T15:42:17Z");
        assert_eq!(step_target(c, m, true), Some(at("2026-11-05T15:43:00Z")));
        assert_eq!(step_target(c, m, false), Some(at("2026-11-05T15:42:00Z")));
        let b = at("2026-11-05T15:42:00Z");
        assert_eq!(step_target(b, m, false), Some(at("2026-11-05T15:41:00Z")));
        assert_eq!(
            step_target(b, 5 * m, true),
            Some(at("2026-11-05T15:45:00Z"))
        );
        // At the edges of the range: refused, never wrapped.
        assert_eq!(step_target(UtcMillis(i64::MAX), m, true), None);
        assert_eq!(step_target(UtcMillis(i64::MIN), m, false), None);
        let (lo, hi) = (UtcMillis(i64::MIN), UtcMillis(i64::MAX));
        assert_eq!(
            fraction(UtcMillis(0), lo, hi),
            0.5,
            "no overflow over the whole range"
        );
        assert_eq!(
            step_ms(Some(Timeframe::Ticks(500))),
            m,
            "tick bars step a minute"
        );
        assert_eq!(step_ms(Some(Timeframe::Seconds(30))), 30_000);
    }

    /// ES: the regular session's opens (09:30 ET), back and forward, across a weekend.
    #[test]
    fn session_jumps_go_to_the_regular_open() {
        let cal = lines_feed::catalog::sessions(lines_feed::catalog::ES).unwrap();
        // Thu 2026-11-05 10:42 ET.
        let c = at("2026-11-05T15:42:00Z");
        assert_eq!(
            session_target(cal, c, false),
            Some(at("2026-11-05T14:30:00Z"))
        );
        assert_eq!(
            session_target(cal, c, true),
            Some(at("2026-11-06T14:30:00Z"))
        );
        // From an open, back goes to the one before.
        let open = at("2026-11-05T14:30:00Z");
        assert_eq!(
            session_target(cal, open, false),
            Some(at("2026-11-04T14:30:00Z"))
        );
        // Friday afternoon: next is Monday's.
        let fri = at("2026-11-06T20:00:00Z");
        assert_eq!(
            session_target(cal, fri, true),
            Some(at("2026-11-09T14:30:00Z"))
        );
    }

    #[test]
    fn go_to_reads_the_display_zone() {
        let now = at("2026-11-05T15:42:17Z");
        let et = DisplayTz::exchange();
        assert_eq!(
            parse_go_to("09:30", now, &et),
            Ok(at("2026-11-05T14:30:00Z"))
        );
        assert_eq!(
            parse_go_to("9:30:15", now, &et),
            Ok(at("2026-11-05T14:30:15Z"))
        );
        assert_eq!(
            parse_go_to("2026-11-04 16:00", now, &et),
            Ok(at("2026-11-04T21:00:00Z"))
        );
        // Summer time: EDT is UTC-4.
        assert_eq!(
            parse_go_to("2026-07-01 09:30", now, &et),
            Ok(at("2026-07-01T13:30:00Z"))
        );
        // The 2026 DST changes, ET: 2026-03-08 (EST to EDT at 02:00), 2026-11-01 (back at 02:00).
        assert_eq!(
            parse_go_to("2026-03-08 09:30", now, &et),
            Ok(at("2026-03-08T13:30:00Z"))
        );
        assert_eq!(
            parse_go_to("2026-03-08 01:30", now, &et),
            Ok(at("2026-03-08T06:30:00Z"))
        );
        assert_eq!(
            parse_go_to("2026-11-01 09:30", now, &et),
            Ok(at("2026-11-01T14:30:00Z"))
        );
        assert_eq!(
            parse_go_to("09:30", now, &DisplayTz::utc()),
            Ok(at("2026-11-05T09:30:00Z"))
        );
        for bad in ["", "25:00", "9", "2026-13-01 09:30", "x y z", "09:60"] {
            assert!(parse_go_to(bad, now, &et).is_err(), "{bad}");
        }
    }

    /// Plain replay keys go to the pane when nothing replays, while typing, and the chain's
    /// Left/Right stay its own.
    #[test]
    fn replay_keys_leave_the_panes_their_keys() {
        use ReplayCmd::*;
        assert!(key_goes_to_pane(PlayPause, false, false, false));
        assert!(!key_goes_to_pane(PlayPause, true, false, false));
        assert!(key_goes_to_pane(PlayPause, true, true, false));
        assert!(key_goes_to_pane(StepBack, true, false, true));
        assert!(!key_goes_to_pane(PlayPause, true, false, true));
        assert!(!key_goes_to_pane(PrevSession, true, false, true));
    }

    #[test]
    fn the_jump_back_note_counts_what_it_canceled() {
        let c = |orders, triggers| Canceled { orders, triggers }.note();
        assert_eq!(c(0, 0), None);
        assert_eq!(c(1, 0).unwrap(), "Jumped back: 1 replay order canceled");
        assert_eq!(
            c(2, 1).unwrap(),
            "Jumped back: 2 replay orders and 1 trigger canceled"
        );
    }

    /// Narrow bars collapse the speeds into a dropdown; an unpainted bar (0 wide) doesn't.
    #[test]
    fn narrow_bars_collapse_the_speeds() {
        assert!(!speeds_collapse(0.0));
        assert!(speeds_collapse(crate::zoom::f(NARROW_100) - 1.0));
        assert!(!speeds_collapse(crate::zoom::f(NARROW_100)));
    }

    #[test]
    fn the_timeline_maps_both_ways() {
        let (a, b) = (UtcMillis(1_000), UtcMillis(11_000));
        assert_eq!(fraction(UtcMillis(6_000), a, b), 0.5);
        assert_eq!(time_at(0.5, a, b), UtcMillis(6_000));
        assert_eq!(fraction(UtcMillis(0), a, b), 0.0);
        assert_eq!(fraction(a, a, a), 1.0);
    }
}
