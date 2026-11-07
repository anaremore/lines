//! Chart screenshots of journal trades (roadmap 8c, phase 2).
//!
//! The journal names two files per trade and asks for them as the trade opens and closes
//! (`lines_trading::journal::ShotRequest`). A capturer thread (`lines-shots`) takes each
//! request, copies the contract's 1-minute bars from a running feed's store (the one an open
//! chart or DOM shows: the same data the panes draw, briefly locked, never by the UI), derives
//! the timeframe with the chart's own bucketing (`lines_core::timeframe::Derived`) and writes
//! an SVG: candles, a price and time axis, the trade's span, every fill marked by a triangle
//! (up for a buy, down for a sell: never color alone) and its average entry and exit.
//!
//! - **Entry**: the chart up to the bar holding the first fill, as it stood when the request
//!   was taken (normally the same instant: that bar may hold a few prints after the fill).
//! - **Exit**: from some bars before the entry to the exit, every fill marked, the net P&L in
//!   the title as the P&L display shows it (off: no P&L at all).
//! - **Timeframe**: the shortest of 1, 2, 5, 15, 30 minutes, 1 or 4 hours that fits the trade
//!   in [`MAX_TRADE_BARS`] bars.
//! - **No data** (no pane of the contract open): the file is still written, saying so.
//!
//! SVG because nothing can render the app's own chart off the UI thread (GPUI paints in a
//! window), and SVG needs no new dependency, opens in any browser and is small. Times are
//! UTC, like the chart's axis and the journal's table. The look (theme, palette, P&L display)
//! is the app's at the time of the screenshot.

use std::fmt::Write as _;
use std::path::Path;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, Weak};

use lines_core::bars::{Bar, BarSeries};
use lines_core::timeframe::Derived;
use lines_core::{CivilDate, Price, Product, Timeframe, UtcMillis};
use lines_feed::store::ChartStore;
use lines_trading::journal::{ShotKind, ShotRequest, Trade, price_value};
use lines_trading::Side;

use crate::pnl::{self, PnlDisplay};
use crate::theme::{Palette, Theme};

/// The stores whose bars screenshots may use: every running feed a pane trades from
/// (`Trading::follow`).
pub type Stores = Arc<Mutex<Vec<Weak<ChartStore>>>>;

/// At most this many bars span the trade itself (the timeframe grows until it fits).
pub const MAX_TRADE_BARS: i64 = 60;
/// Bars shown before the entry.
pub const CONTEXT_BARS: i64 = 40;
/// Empty slots right of the last bar.
const RIGHT_SLOTS: usize = 6;

const W: f32 = 1280.0;
const H: f32 = 720.0;
const PLOT_L: f32 = 16.0;
const PLOT_R: f32 = W - 96.0;
const PLOT_T: f32 = 72.0;
const PLOT_B: f32 = H - 36.0;
const FONT: &str = "Inter, 'Segoe UI', system-ui, -apple-system, sans-serif";

/// The app's look as screenshots use it (set by the UI as it changes).
#[derive(Copy, Clone, Debug)]
pub struct Look {
    pub theme: Theme,
    pub pnl: PnlDisplay,
}

static LOOK: Mutex<Option<Look>> = Mutex::new(None);

fn look() -> Look {
    LOOK.lock()
        .unwrap_or_else(|e| e.into_inner())
        .unwrap_or_else(|| Look {
            theme: Theme::dark(Palette::Standard),
            pnl: PnlDisplay::default(),
        })
}

/// Screenshots from now on use this theme.
pub fn set_theme(theme: Theme) {
    let mut l = LOOK.lock().unwrap_or_else(|e| e.into_inner());
    let pnl = l.map_or_else(PnlDisplay::default, |l| l.pnl);
    *l = Some(Look { theme, pnl });
}

/// Screenshots from now on show P&L as `pnl` allows.
pub fn set_pnl(pnl: PnlDisplay) {
    let mut l = LOOK.lock().unwrap_or_else(|e| e.into_inner());
    let theme = l.map_or_else(|| Theme::dark(Palette::Standard), |l| l.theme);
    *l = Some(Look { theme, pnl });
}

/// Start the capturer thread. `names`: account names by id, for the title (read per shot).
pub fn spawn(stores: Stores, names: Arc<dyn Fn(&str) -> Option<String> + Send + Sync>) -> Option<Sender<ShotRequest>> {
    let (tx, rx) = mpsc::channel::<ShotRequest>();
    let spawned = std::thread::Builder::new()
        .name("lines-shots".into())
        .spawn(move || {
            for req in rx {
                let account = names(&req.trade.account.0).unwrap_or_else(|| req.trade.account.0.clone());
                let data = chart_data(&stores, &req);
                let svg = render(&req, &account, data.as_ref(), look());
                if let Err(e) = write_atomic(&req.path, &svg) {
                    tracing::warn!("journal screenshot {} not written: {e}", req.path.display());
                }
            }
        });
    match spawned {
        Ok(_) => Some(tx),
        Err(e) => {
            tracing::error!("can't start the journal screenshot thread: {e}");
            None
        }
    }
}

/// Write to a temporary file beside `path`, then rename: a reader never sees half a file.
fn write_atomic(path: &Path, content: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("svg.tmp");
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path)
}

/// The bars a screenshot draws, at its timeframe.
#[derive(Clone, Debug)]
pub struct ChartBars {
    pub product: Product,
    pub tf: Timeframe,
    pub bars: Vec<Bar>,
}

/// The shortest timeframe that holds `span_ms` in [`MAX_TRADE_BARS`] bars.
pub fn pick_timeframe(span_ms: i64) -> Timeframe {
    const CHOICES: [Timeframe; 7] = [
        Timeframe::Minutes(1),
        Timeframe::Minutes(2),
        Timeframe::Minutes(5),
        Timeframe::Minutes(15),
        Timeframe::Minutes(30),
        Timeframe::Hours(1),
        Timeframe::Hours(4),
    ];
    CHOICES
        .into_iter()
        .find(|tf| span_ms / tf.nominal_ms() < MAX_TRADE_BARS)
        .unwrap_or(Timeframe::Hours(4))
}

/// The time a screenshot runs to.
fn until(req: &ShotRequest) -> UtcMillis {
    match req.kind {
        ShotKind::Entry => req.trade.entry_time,
        ShotKind::Exit => req.trade.exit_time,
    }
}

/// Copy the contract's bars from a store that holds it (`None`: no pane of it is open).
fn chart_data(stores: &Stores, req: &ShotRequest) -> Option<ChartBars> {
    let live: Vec<Arc<ChartStore>> = {
        let mut v = stores.lock().unwrap_or_else(|e| e.into_inner());
        v.retain(|w| w.strong_count() > 0);
        v.iter().filter_map(Weak::upgrade).collect()
    };
    let t = &req.trade;
    let tf = pick_timeframe(until(req).0 - t.entry_time.0);
    let from = UtcMillis(t.entry_time.0 - (CONTEXT_BARS + 2) * tf.nominal_ms());
    let to = until(req);
    let mut best: Option<(Product, Vec<Bar>)> = None;
    for s in live {
        let d = s.read();
        if d.contract != Some(t.contract) {
            continue;
        }
        let bars: Vec<Bar> = d
            .series
            .bars()
            .iter()
            .filter(|b| b.time >= from && b.time <= to)
            .copied()
            .collect();
        if best.as_ref().is_none_or(|(_, b)| bars.len() > b.len()) {
            best = Some((d.product.clone(), bars));
        }
    }
    let (product, minute) = best?;
    Some(ChartBars {
        bars: derive(&minute, tf, lines_feed::catalog::sessions(t.contract.product)),
        product,
        tf,
    })
}

/// 1-minute bars at `tf`, bucketed as the chart buckets them; the first bucket (possibly cut
/// by the copy) dropped unless it is the only one.
pub fn derive(
    minute: &[Bar],
    tf: Timeframe,
    cal: Option<&lines_core::sessions::SessionCalendar>,
) -> Vec<Bar> {
    if tf == Timeframe::Minutes(1) {
        return minute.to_vec();
    }
    let mut series = BarSeries::new(60_000, minute.len().max(1));
    for b in minute {
        series.upsert(*b);
    }
    let mut d = Derived::new(tf, minute.len().max(1));
    d.update(&series, cal);
    let bars = d.bars();
    if bars.len() > 1 {
        bars[1..].to_vec()
    } else {
        bars.to_vec()
    }
}

/// `#rrggbb` and the opacity.
fn color(c: gpui::Hsla) -> (String, f32) {
    let r = c.to_rgb();
    let b = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    (format!("#{:02x}{:02x}{:02x}", b(r.r), b(r.g), b(r.b)), r.a)
}

fn paint(attr: &str, c: gpui::Hsla) -> String {
    let (hex, a) = color(c);
    if a < 0.999 {
        format!("{attr}=\"{hex}\" {attr}-opacity=\"{a:.2}\"")
    } else {
        format!("{attr}=\"{hex}\"")
    }
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// `11-05 14:31:07` (UTC).
fn clock(t: UtcMillis) -> String {
    crate::journal_view::clock_text(t)
}

fn price_text(p: &Product, v: Price) -> String {
    let mut s = String::new();
    let _ = p.price.write(v, &mut s);
    s
}

/// An average (price units, possibly fractional) as the product's price.
fn avg_text(p: &Product, units: f64) -> String {
    crate::journal_view::decimal(price_value(p, units), p.price.scale.den())
}

fn tf_text(tf: Timeframe) -> String {
    match tf {
        Timeframe::Minutes(1) => "1-minute bars".into(),
        Timeframe::Minutes(n) => format!("{n}-minute bars"),
        Timeframe::Hours(1) => "1-hour bars".into(),
        Timeframe::Hours(n) => format!("{n}-hour bars"),
        other => format!("{other} bars"),
    }
}

/// The net P&L as the display allows (`None`: off).
fn pnl_text(t: &Trade, product: Option<&Product>, mode: PnlDisplay) -> Option<String> {
    let tick = product.map_or(1, |p| p.price.tick);
    let ticks = t.ticks(tick).map(|v| {
        let mut s = String::new();
        let _ = pnl::write_ticks(&mut s, v);
        s
    });
    let mut money = String::new();
    let _ = pnl::write_money(&mut money, t.net());
    match (mode.money(), mode.ticks(), ticks) {
        (true, true, Some(tk)) => Some(format!("net {money}  ({tk})")),
        (true, _, _) => Some(format!("net {money}")),
        (false, true, Some(tk)) => Some(tk),
        _ => None,
    }
}

/// A price step for about `lines` grid lines over `range` price units: a tick multiple of 1, 2
/// or 5 × 10ⁿ.
fn grid_step(range: i64, tick: i64, lines: i64) -> i64 {
    let tick = tick.max(1);
    let raw = (range / lines.max(1)).max(tick);
    let mut m = 1i64;
    loop {
        for k in [1, 2, 5] {
            let step = tick * k * m;
            if step >= raw {
                return step;
            }
        }
        m = m.saturating_mul(10);
        if m > 1_000_000_000_000 {
            return raw;
        }
    }
}

/// The screenshot as SVG text. `data`: the bars (`None`: no chart data was open).
pub fn render(req: &ShotRequest, account: &str, data: Option<&ChartBars>, look: Look) -> String {
    let th = look.theme;
    let t = &req.trade;
    let product = data
        .map(|d| d.product.clone())
        .or_else(|| lines_feed::catalog::get(t.contract.product));
    let contract = product.as_ref().map_or_else(
        || format!("{:?}", t.contract.product),
        |p| t.contract.label(p).as_str().to_owned(),
    );
    let mut s = String::with_capacity(32 * 1024);
    let _ = write!(
        s,
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{W}\" height=\"{H}\" viewBox=\"0 0 {W} {H}\" font-family=\"{FONT}\">\n"
    );
    let _ = writeln!(s, "<rect width=\"{W}\" height=\"{H}\" {}/>", paint("fill", th.bg));
    // Title: what, which way, whose; then when, how long, which bars.
    let side = if t.side == Side::Buy { "Long" } else { "Short" };
    let qty = match req.kind {
        ShotKind::Entry => t.entry_qty,
        ShotKind::Exit => t.max_qty,
    };
    let kind = match req.kind {
        ShotKind::Entry => "Entry",
        ShotKind::Exit => "Exit",
    };
    let _ = writeln!(
        s,
        "<text x=\"{PLOT_L}\" y=\"28\" font-size=\"18\" font-weight=\"600\" {}>{}  ·  {side} {qty}  ·  {}</text>",
        paint("fill", th.text_main),
        esc(&contract),
        esc(account)
    );
    let mut when = format!("Entry {}", clock(t.entry_time));
    if req.kind == ShotKind::Exit {
        let _ = write!(
            when,
            "  →  exit {}  ·  {}",
            clock(t.exit_time),
            crate::journal_view::duration_text(t.duration_ms())
        );
    }
    when.push_str(" UTC");
    if let Some(d) = data {
        let _ = write!(when, "  ·  {}", tf_text(d.tf));
    }
    let _ = writeln!(
        s,
        "<text x=\"{PLOT_L}\" y=\"52\" font-size=\"13\" {}>{}</text>",
        paint("fill", th.text_dim),
        esc(&when)
    );
    // Right of the title: which screenshot, and (at the exit) the result, signed.
    let _ = writeln!(
        s,
        "<text x=\"{}\" y=\"28\" font-size=\"13\" text-anchor=\"end\" {}>{kind} screenshot</text>",
        W - 16.0,
        paint("fill", th.text_dim)
    );
    if req.kind == ShotKind::Exit
        && let Some(p) = pnl_text(t, product.as_ref(), look.pnl)
    {
        let net = t.net();
        let c = if !look.pnl.money() {
            th.text_main
        } else {
            match pnl::tone(net) {
                pnl::Tone::Up => th.up,
                pnl::Tone::Down => th.down,
                pnl::Tone::Flat => th.text_main,
            }
        };
        let _ = writeln!(
            s,
            "<text x=\"{}\" y=\"52\" font-size=\"15\" font-weight=\"600\" text-anchor=\"end\" {}>{}</text>",
            W - 16.0,
            paint("fill", c),
            esc(&p)
        );
    }
    let (Some(d), Some(product)) = (data.filter(|d| !d.bars.is_empty()), product.as_ref()) else {
        let _ = writeln!(
            s,
            "<text x=\"{}\" y=\"{}\" font-size=\"14\" text-anchor=\"middle\" {}>No chart data for {} when this was taken.</text>",
            W / 2.0,
            H / 2.0 - 10.0,
            paint("fill", th.text_main),
            esc(&contract)
        );
        let _ = writeln!(
            s,
            "<text x=\"{}\" y=\"{}\" font-size=\"13\" text-anchor=\"middle\" {}>Screenshots use the bars of an open chart or DOM of the contract.</text>",
            W / 2.0,
            H / 2.0 + 14.0,
            paint("fill", th.text_dim)
        );
        s.push_str("</svg>\n");
        return s;
    };
    plot(&mut s, req, d, product, th);
    s.push_str("</svg>\n");
    s
}

/// Candles, axes, the trade's span, fills and averages.
fn plot(s: &mut String, req: &ShotRequest, d: &ChartBars, product: &Product, th: Theme) {
    let t = &req.trade;
    let bars = &d.bars;
    let fills: Vec<_> = t.fills.iter().filter(|f| f.at <= until(req)).collect();
    let mut lo = bars.iter().map(|b| b.low.0).min().unwrap_or(0);
    let mut hi = bars.iter().map(|b| b.high.0).max().unwrap_or(0);
    for f in &fills {
        lo = lo.min(f.price.0);
        hi = hi.max(f.price.0);
    }
    let tick = product.price.tick.max(1);
    let pad = ((hi - lo) / 12).max(tick * 2);
    let (lo, hi) = (lo - pad, hi + pad);
    let span = (hi - lo).max(1) as f32;
    let y = |p: f64| PLOT_T + (PLOT_B - PLOT_T) * (1.0 - ((p - lo as f64) as f32 / span));
    let slots = (bars.len() + RIGHT_SLOTS).max(40);
    let slot = (PLOT_R - PLOT_L) / slots as f32;
    let x = |i: usize| PLOT_L + slot * (i as f32 + 0.5);
    // The bar holding a time (the last one starting at or before it).
    let bar_of = |at: UtcMillis| bars.iter().rposition(|b| b.time <= at).unwrap_or(0);
    // Grid and price axis.
    let step = grid_step(hi - lo, tick, 6);
    let mut g = (lo + step - 1).div_euclid(step) * step;
    while g <= hi {
        let gy = y(g as f64);
        let _ = writeln!(
            s,
            "<line x1=\"{PLOT_L}\" x2=\"{PLOT_R}\" y1=\"{gy:.1}\" y2=\"{gy:.1}\" stroke-width=\"1\" {}/>",
            paint("stroke", th.grid)
        );
        let _ = writeln!(
            s,
            "<text x=\"{}\" y=\"{:.1}\" font-size=\"12\" {}>{}</text>",
            PLOT_R + 8.0,
            gy + 4.0,
            paint("fill", th.text_dim),
            esc(&price_text(product, Price(g)))
        );
        g += step;
    }
    let _ = writeln!(
        s,
        "<line x1=\"{PLOT_R}\" x2=\"{PLOT_R}\" y1=\"{PLOT_T}\" y2=\"{PLOT_B}\" stroke-width=\"1\" {}/>",
        paint("stroke", th.axis_line)
    );
    let _ = writeln!(
        s,
        "<line x1=\"{PLOT_L}\" x2=\"{PLOT_R}\" y1=\"{PLOT_B}\" y2=\"{PLOT_B}\" stroke-width=\"1\" {}/>",
        paint("stroke", th.axis_line)
    );
    // Time axis: about one label per 110 px, HH:MM (UTC), the date where it changes.
    let every = ((110.0 / slot).ceil() as usize).max(1);
    let mut last_day: Option<CivilDate> = None;
    for (i, b) in bars.iter().enumerate() {
        if i % every != 0 {
            continue;
        }
        let day = CivilDate::from_utc(b.time);
        let secs = b.time.0.rem_euclid(86_400_000) / 1000;
        let label = if last_day.is_some_and(|l| l != day) || last_day.is_none() && i == 0 {
            format!("{:02}-{:02} {:02}:{:02}", day.month, day.day, secs / 3600, secs / 60 % 60)
        } else {
            format!("{:02}:{:02}", secs / 3600, secs / 60 % 60)
        };
        last_day = Some(day);
        let _ = writeln!(
            s,
            "<text x=\"{:.1}\" y=\"{}\" font-size=\"12\" text-anchor=\"middle\" {}>{label}</text>",
            x(i),
            PLOT_B + 18.0,
            paint("fill", th.text_dim)
        );
    }
    // The trade's span, behind the candles.
    let (e0, e1) = (bar_of(t.entry_time), bar_of(until(req)));
    let _ = writeln!(
        s,
        "<rect x=\"{:.1}\" y=\"{PLOT_T}\" width=\"{:.1}\" height=\"{}\" {}/>",
        x(e0) - slot / 2.0,
        slot * (e1 - e0 + 1) as f32,
        PLOT_B - PLOT_T,
        paint("fill", th.accent.opacity(0.10))
    );
    // Candles: hollow or filled by direction (the theme's rule), never color alone.
    let body_w = (slot * 0.7).max(1.0);
    for (i, b) in bars.iter().enumerate() {
        let up = b.close >= b.open;
        let c = if up { th.up } else { th.down };
        let cx = x(i);
        let _ = writeln!(
            s,
            "<line x1=\"{cx:.1}\" x2=\"{cx:.1}\" y1=\"{:.1}\" y2=\"{:.1}\" stroke-width=\"1\" {}/>",
            y(b.high.0 as f64),
            y(b.low.0 as f64),
            paint("stroke", c)
        );
        let (top, bot) = (
            y(b.open.0.max(b.close.0) as f64),
            y(b.open.0.min(b.close.0) as f64),
        );
        let fill = if th.hollow.is_hollow(up) { th.bg } else { c };
        let _ = writeln!(
            s,
            "<rect x=\"{:.1}\" y=\"{top:.1}\" width=\"{body_w:.1}\" height=\"{:.1}\" stroke-width=\"1\" {} {}/>",
            cx - body_w / 2.0,
            (bot - top).max(1.0),
            paint("fill", fill),
            paint("stroke", c)
        );
    }
    // Average entry and exit: dashed lines over the span, labeled.
    let mut avg = |units: Option<f64>, label: &str, below: bool| {
        let Some(u) = units else { return };
        let ly = y(u);
        let (x0, x1) = (x(e0) - slot / 2.0, PLOT_R);
        let _ = writeln!(
            s,
            "<line x1=\"{x0:.1}\" x2=\"{x1:.1}\" y1=\"{ly:.1}\" y2=\"{ly:.1}\" stroke-width=\"1\" stroke-dasharray=\"5 4\" {}/>",
            paint("stroke", th.text_main)
        );
        let text = format!("{label} {}", avg_text(product, u));
        let ty = if below { ly + 15.0 } else { ly - 6.0 };
        let _ = writeln!(
            s,
            "<text x=\"{:.1}\" y=\"{ty:.1}\" font-size=\"12\" {} {}>{}</text>",
            x0 + 4.0,
            paint("fill", th.text_main),
            "paint-order=\"stroke\" stroke-width=\"3\" ".to_owned() + &paint("stroke", th.bg),
            esc(&text)
        );
    };
    let (entry_avg, exit_avg) = match req.kind {
        ShotKind::Entry => (t.avg_entry(), None),
        ShotKind::Exit => (t.avg_entry(), t.avg_exit()),
    };
    // Labels go on the far side of each other so they never overlap.
    let entry_below = exit_avg.is_some_and(|x| entry_avg.is_some_and(|e| e < x));
    avg(entry_avg, "avg entry", entry_below);
    avg(exit_avg, "avg exit", !entry_below);
    // Fills: a triangle at the price, pointing up for a buy (below it) and down for a sell
    // (above it), outlined in the background color so it reads over candles.
    for f in &fills {
        let fx = x(bar_of(f.at));
        let fy = y(f.price.0 as f64);
        let (pts, c) = if f.side == Side::Buy {
            (
                format!("{fx:.1},{fy:.1} {:.1},{:.1} {:.1},{:.1}", fx - 7.0, fy + 12.0, fx + 7.0, fy + 12.0),
                th.up,
            )
        } else {
            (
                format!("{fx:.1},{fy:.1} {:.1},{:.1} {:.1},{:.1}", fx - 7.0, fy - 12.0, fx + 7.0, fy - 12.0),
                th.down,
            )
        };
        let _ = writeln!(
            s,
            "<polygon points=\"{pts}\" stroke-width=\"1.5\" {} {}><title>{} {} @ {}</title></polygon>",
            paint("fill", c),
            paint("stroke", th.bg),
            if f.side == Side::Buy { "Buy" } else { "Sell" },
            f.qty.0,
            esc(&price_text(product, f.price))
        );
    }
    // Legend, bottom left above the axis: the shapes' meaning in words.
    let _ = writeln!(
        s,
        "<text x=\"{}\" y=\"{}\" font-size=\"12\" {}>▲ buy fill   ▼ sell fill   - - average price   shaded: the trade</text>",
        PLOT_L + 4.0,
        PLOT_T - 6.0,
        paint("fill", th.text_dim)
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use lines_core::{ContractKey, Month, Qty};
    use lines_feed::catalog;
    use lines_trading::journal::Journal;
    use lines_trading::Money;
    use lines_trading::{AccountId, Event, FillEvent, OrderId};

    fn es() -> ContractKey {
        ContractKey::monthly(catalog::ES, 2026, Month::new(12).unwrap())
    }

    fn t0() -> i64 {
        UtcMillis::parse_iso("2026-11-05T14:31:07Z").unwrap().0
    }

    /// Long 2 ES at 6000.00, out at 6002.00 four minutes later; the requests a journal with a
    /// screenshot folder asks for.
    fn requests() -> Vec<ShotRequest> {
        let mut j = Journal::new();
        j.set_shots_dir("shots".into());
        for (i, (side, p, s)) in [(Side::Buy, 600_000, 0), (Side::Sell, 600_200, 240)]
            .into_iter()
            .enumerate()
        {
            j.on_event(&Event::Fill(FillEvent {
                account: AccountId::new("paper"),
                order: OrderId(i as u64),
                contract: es(),
                side,
                price: Price(p),
                qty: Qty(2),
                fee: Money::cents(124),
                realised: Money::ZERO,
                trade_id: format!("f{i}"),
                reconciled: false,
                at: UtcMillis(t0() + s * 1000),
            }));
        }
        j.take_shot_requests()
    }

    /// 1-minute bars from an hour before the entry to ten minutes after the exit.
    fn minute_bars() -> Vec<Bar> {
        let start = UtcMillis(t0()).floor_to(60_000).0 - 60 * 60_000;
        (0..75)
            .map(|i| {
                let o = 599_800 + (i % 9) * 25;
                Bar {
                    time: UtcMillis(start + i * 60_000),
                    open: Price(o),
                    high: Price(o + 75),
                    low: Price(o - 50),
                    close: Price(o + if i % 2 == 0 { 25 } else { -25 }),
                    volume: Qty(100),
                }
            })
            .collect()
    }

    fn data(req: &ShotRequest) -> ChartBars {
        let to = until(req);
        ChartBars {
            product: catalog::get(catalog::ES).unwrap(),
            tf: Timeframe::Minutes(1),
            bars: minute_bars().into_iter().filter(|b| b.time <= to).collect(),
        }
    }

    fn look(pnl: PnlDisplay) -> Look {
        Look {
            theme: Theme::dark(Palette::Standard),
            pnl,
        }
    }

    #[test]
    fn exit_screenshot_marks_every_fill_and_the_result() {
        let r = requests();
        let exit = r.iter().find(|r| r.kind == ShotKind::Exit).unwrap();
        let svg = render(exit, "Paper 50K eval", Some(&data(exit)), look(PnlDisplay::Both));
        assert!(svg.starts_with("<svg") && svg.trim_end().ends_with("</svg>"));
        assert!(svg.contains("ESZ26  ·  Long 2  ·  Paper 50K eval"), "{svg}");
        assert!(svg.contains("Entry 11-05 14:31:07  →  exit 11-05 14:35:07  ·  4m 00s UTC  ·  1-minute bars"));
        assert!(svg.contains("Exit screenshot"));
        // +2 points × 2 × $50 = $200, less $2.48 of fees; and in ticks.
        assert!(svg.contains("net +$197.52  (+8.0 t)"), "{svg}");
        assert_eq!(svg.matches("<polygon").count(), 2, "one triangle per fill");
        assert!(svg.contains("<title>Buy 2 @ 6000.00</title>"));
        assert!(svg.contains("<title>Sell 2 @ 6002.00</title>"));
        assert!(svg.contains("avg entry 6000.00") && svg.contains("avg exit 6002.00"));
        // One candle per bar up to the exit (60 before the entry minute, the entry minute, 4).
        let bars = data(exit).bars.len();
        assert_eq!(bars, 65);
        assert_eq!(svg.matches("<rect").count(), 2 + bars, "background, span, candles");
    }

    #[test]
    fn entry_screenshot_stops_at_the_entry_and_shows_no_result() {
        let r = requests();
        let entry = r.iter().find(|r| r.kind == ShotKind::Entry).unwrap();
        let d = data(entry);
        assert!(d.bars.last().unwrap().time <= entry.trade.entry_time);
        let svg = render(entry, "paper", Some(&d), look(PnlDisplay::Both));
        assert!(svg.contains("Entry screenshot"));
        assert!(!svg.contains("net "), "no result before it exists");
        assert!(!svg.contains("avg exit"));
        assert_eq!(svg.matches("<polygon").count(), 1, "the first fill only");
    }

    /// P&L display off: no money or ticks anywhere in the file; ticks: ticks only.
    #[test]
    fn screenshots_follow_the_pnl_display() {
        let r = requests();
        let exit = r.iter().find(|r| r.kind == ShotKind::Exit).unwrap();
        let off = render(exit, "paper", Some(&data(exit)), look(PnlDisplay::Off));
        assert!(!off.contains('$') && !off.contains(" t)") && !off.contains("net"));
        let ticks = render(exit, "paper", Some(&data(exit)), look(PnlDisplay::Ticks));
        assert!(ticks.contains(">+8.0 t<") && !ticks.contains('$'), "{ticks}");
    }

    #[test]
    fn no_chart_data_says_so_and_text_is_escaped() {
        let r = requests();
        let exit = r.iter().find(|r| r.kind == ShotKind::Exit).unwrap();
        let svg = render(exit, "<A&B>", None, look(PnlDisplay::Both));
        assert!(svg.contains("No chart data for ESZ26 when this was taken."));
        assert!(svg.contains("&lt;A&amp;B&gt;") && !svg.contains("<A&B>"));
        assert!(!svg.contains("<polygon"));
    }

    #[test]
    fn timeframes_fit_the_trade_and_bucket_like_the_chart() {
        assert_eq!(pick_timeframe(0), Timeframe::Minutes(1));
        assert_eq!(pick_timeframe(59 * 60_000), Timeframe::Minutes(1));
        assert_eq!(pick_timeframe(60 * 60_000), Timeframe::Minutes(2));
        assert_eq!(pick_timeframe(3 * 3_600_000), Timeframe::Minutes(5));
        assert_eq!(pick_timeframe(40 * 3_600_000), Timeframe::Hours(1));
        assert_eq!(pick_timeframe(10_000 * 3_600_000), Timeframe::Hours(4));
        // 75 one-minute bars at 5 minutes: 15 buckets, the first (cut) one dropped.
        let five = derive(&minute_bars(), Timeframe::Minutes(5), catalog::sessions(catalog::ES));
        assert!(five.len() >= 14 && five.len() <= 16, "{}", five.len());
        assert!(five.iter().all(|b| b.time.0 % 300_000 == 0));
        let total: i64 = five.iter().map(|b| b.volume.0).sum();
        assert!(total <= 75 * 100 && total >= 70 * 100);
        assert_eq!(grid_step(1000, 25, 6), 250);
        assert_eq!(grid_step(30, 25, 6), 25);
    }

    #[test]
    fn written_whole_and_found_by_its_name() {
        let dir = std::env::temp_dir().join(format!("lines-shots-{}", std::process::id()));
        let path = dir.join("a").join("x-entry.svg");
        write_atomic(&path, "<svg/>").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "<svg/>");
        assert!(!path.with_extension("svg.tmp").exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
