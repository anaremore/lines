//! Trade snapshots of journal trades (roadmap 8c, phase 2).
//!
//! As a trade of a saved journal closes, the journal names one file and asks for it
//! (`lines_trading::journal::ShotRequest`). A drawing thread (`lines-shots`) takes each request,
//! copies the contract's 1-minute bars from a running feed's store (the one an open chart or DOM
//! shows, briefly locked, never by the UI), derives the timeframe with the chart's own bucketing
//! (`lines_core::timeframe::Derived`) and writes an SVG to `shots/` beside the journal: candles,
//! a price and time axis, the trade's span, every fill marked by a triangle (up for a buy, down
//! for a sell: never color alone) and its average entry and exit.
//!
//! - **Timeframe**: the shortest of 1, 2, 5, 15, 30 minutes, 1 or 4 hours that fits the trade in
//!   [`MAX_TRADE_BARS`] bars, with [`CONTEXT_BARS`] before the entry, up to the exit.
//! - **No data** (no pane of the contract open): the file is still written, saying so.
//! - **Bars that stop early**: a trade is placed by its times, so it stands in a labeled gap
//!   past the last bar, never on the nearest bar.
//! - **No account, anywhere**: the request's trade has it blanked, and [`render`] writes the
//!   account's kind in words (PAPER, SIM, LIVE or UNLISTED), the contract, side, times, prices
//!   and (as the display allows) the P&L only.
//!
//! SVG because nothing can render the app's own chart off the UI thread (GPUI paints in a
//! window, its image readback is test-only), and SVG needs no new dependency, opens in any
//! browser and is small. Times are on the display zone's clock, like the chart's axis and the journal's table, and the header names it. The look
//! (theme, palette, P&L display) is the app's as the file is drawn.

use lines_core::display_tz::DisplayTz;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, RwLock, Weak};

use lines_core::bars::{Bar, BarSeries};
use lines_core::timeframe::Derived;
use lines_core::{CivilDate, Price, Product, Qty, Timeframe, UtcMillis};
use lines_feed::store::ChartStore;
use lines_trading::journal::{ShotMsg, Trade, is_shot_file_name, price_value};
use lines_trading::{AccountId, Side};

use crate::pnl::{self, PnlDisplay};
use crate::theme::{Palette, Theme};
use crate::trading::AccountInfo;

/// The stores whose bars trade snapshots may use: every running feed a pane trades from
/// (`Trading::follow`).
pub type Stores = Arc<Mutex<Vec<Weak<ChartStore>>>>;

/// The accounts now listed: only their kinds reach a trade snapshot.
pub type Kinds = Arc<RwLock<Vec<AccountInfo>>>;

/// At most this many bars span the trade itself (the timeframe grows until it fits).
pub const MAX_TRADE_BARS: i64 = 60;
/// Bars shown before the entry.
pub const CONTEXT_BARS: i64 = 40;
/// Width kept right of the last slot for the averages' labels.
const LABEL_ROOM: f32 = 120.0;
/// A time past the last bar stands at most this many slots beyond it.
const MAX_GAP_SLOTS: i64 = CONTEXT_BARS + MAX_TRADE_BARS + 2;
/// Triangles of a buy and a sell closer than this, and overlapping in height, stand apart.
const MARK_W: f32 = 15.0;

const W: f32 = 1280.0;
const H: f32 = 720.0;
const PLOT_L: f32 = 16.0;
const PLOT_R: f32 = W - 96.0;
const PLOT_T: f32 = 72.0;
const PLOT_B: f32 = H - 36.0;
const FONT: &str = "Inter, 'Segoe UI', system-ui, -apple-system, sans-serif";

/// The app's look as trade snapshots use it (set by the UI as it changes).
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

/// Trade snapshots from now on use this theme.
pub fn set_theme(theme: Theme) {
    let mut l = LOOK.lock().unwrap_or_else(|e| e.into_inner());
    let pnl = l.map_or_else(PnlDisplay::default, |l| l.pnl);
    *l = Some(Look { theme, pnl });
}

/// Trade snapshots from now on show P&L as `pnl` allows.
pub fn set_pnl(pnl: PnlDisplay) {
    let mut l = LOOK.lock().unwrap_or_else(|e| e.into_inner());
    let theme = l.map_or_else(|| Theme::dark(Palette::Standard), |l| l.theme);
    *l = Some(Look { theme, pnl });
}

/// Remember a feed a pane trades from (once per feed; gone ones are dropped).
pub fn remember(stores: &Stores, store: &Arc<ChartStore>) {
    let mut s = stores.lock().unwrap_or_else(|e| e.into_inner());
    s.retain(|w| w.strong_count() > 0);
    if !s
        .iter()
        .any(|w| std::ptr::eq(w.as_ptr(), Arc::as_ptr(store)))
    {
        s.push(Arc::downgrade(store));
    }
}

/// Start the drawing thread, writing into `dir`. It ends when the journal's thread does.
pub fn spawn(stores: Stores, kinds: Kinds, dir: PathBuf) -> Option<Sender<ShotMsg>> {
    let (tx, rx) = mpsc::channel::<ShotMsg>();
    let spawned = std::thread::Builder::new()
        .name("lines-shots".into())
        .spawn(move || {
            for msg in rx {
                match msg {
                    ShotMsg::Take(req) => {
                        let data = chart_data(&stores, &req.trade);
                        let kind = kind_word(&kinds, &req.account);
                        let svg = render(&req.trade, kind, data.as_ref(), look());
                        // Logged by name only: the folder would hold the OS user name. The
                        // viewer's missing-file notice covers it.
                        if let Err(e) = write_atomic(&dir.join(&req.file), &svg) {
                            tracing::warn!("journal chart {} not written: {e}", req.file);
                        }
                    }
                    ShotMsg::Flush(done) => {
                        let _ = done.send(());
                    }
                }
            }
        });
    match spawned {
        Ok(_) => Some(tx),
        Err(e) => {
            tracing::error!("can't start the journal chart thread: {e}");
            None
        }
    }
}

/// The account's kind in words, as the Journal pane says it (an account no list has: UNLISTED,
/// never assumed to be paper).
fn kind_word(kinds: &Kinds, account: &AccountId) -> &'static str {
    let list = kinds.read().unwrap_or_else(|e| e.into_inner());
    crate::journal_view::account_badges(&list)
        .get(&account.0)
        .copied()
        .unwrap_or(crate::journal_view::UNLISTED)
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

/// The trade snapshot a row names, if it is there. `Err` says why not, for a notice.
pub fn locate(dir: Option<&Path>, file: Option<&str>) -> Result<PathBuf, &'static str> {
    let file = file.ok_or("this trade has no snapshot")?;
    let dir = dir.ok_or("this journal has no snapshot folder")?;
    // A saved name is data: only a plain name inside the folder is ever opened.
    if !is_shot_file_name(file) {
        return Err("this trade's snapshot name isn't valid");
    }
    let path = dir.join(file);
    if path.is_file() {
        Ok(path)
    } else {
        Err("this trade's snapshot is missing: still being written, not written, or deleted")
    }
}

/// `path` as a `file:` URL: every byte but the plain ones percent-encoded (a home folder can
/// hold spaces and non-ASCII letters).
pub fn file_url(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let mut url = String::from(match () {
        _ if text.starts_with("//") => "file:",
        _ if text.starts_with('/') => "file://",
        _ => "file:///",
    });
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'/' | b':' | b'-' | b'_' | b'.' | b'~') {
            url.push(char::from(b));
        } else {
            let _ = write!(url, "%{b:02X}");
        }
    }
    url
}

/// The bars a trade snapshot draws, at its timeframe.
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

/// Copy the contract's bars from a store that holds it (`None`: no pane of it is open). Each
/// store's lock is held for the copy only; the stores' list isn't held meanwhile.
fn chart_data(stores: &Stores, t: &Trade) -> Option<ChartBars> {
    let live: Vec<Arc<ChartStore>> = {
        let mut v = stores.lock().unwrap_or_else(|e| e.into_inner());
        v.retain(|w| w.strong_count() > 0);
        v.iter().filter_map(Weak::upgrade).collect()
    };
    let tf = pick_timeframe(t.duration_ms());
    let from = UtcMillis(t.entry_time.0 - (CONTEXT_BARS + 2) * tf.nominal_ms());
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
            .filter(|b| b.time >= from && b.time <= t.exit_time)
            .copied()
            .collect();
        if best.as_ref().is_none_or(|(_, b)| bars.len() > b.len()) {
            best = Some((d.product.clone(), bars));
        }
    }
    let (product, minute) = best?;
    Some(ChartBars {
        bars: derive(
            &minute,
            tf,
            lines_feed::catalog::sessions(t.contract.product),
        ),
        product,
        tf,
    })
}

/// 1-minute bars at `tf`, bucketed as the chart buckets them; the first bucket (possibly cut by
/// the copy) dropped unless it is the only one.
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

fn price_text(p: &Product, v: Price) -> String {
    let mut s = String::new();
    let _ = p.price.write(v, &mut s);
    s
}

fn qty_text(p: Option<&Product>, q: Qty) -> String {
    match p {
        Some(p) => {
            let mut s = String::new();
            let _ = p.qty.write(q, &mut s);
            s
        }
        None => q.0.to_string(),
    }
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

/// The net P&L as the display allows (`None`: off, or ticks only for a product with no tick to
/// state).
fn pnl_text(t: &Trade, product: Option<&Product>, mode: PnlDisplay) -> Option<String> {
    let ticks = product
        .and_then(|p| t.ticks(p.price.tick))
        .map(crate::journal_view::per_contract_text);
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

/// The trade snapshot as SVG text. `kind`: the account's kind in words. `data`: the bars (`None`:
/// no chart data was open). Takes no account and writes none.
pub fn render(t: &Trade, kind: &str, data: Option<&ChartBars>, look: Look) -> String {
    let th = look.theme;
    let product = data
        .map(|d| d.product.clone())
        .or_else(|| lines_feed::catalog::get(t.contract.product));
    let contract = product.as_ref().map_or_else(
        || format!("{:?}", t.contract.product),
        |p| t.contract.label(p).as_str().to_owned(),
    );
    let mut s = String::with_capacity(32 * 1024);
    let _ = writeln!(
        s,
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{W}\" height=\"{H}\" viewBox=\"0 0 {W} {H}\" font-family=\"{FONT}\">"
    );
    let _ = writeln!(
        s,
        "<rect width=\"{W}\" height=\"{H}\" {}/>",
        paint("fill", th.bg)
    );
    // Title: what and which way; then when, how long, which bars.
    let side = if t.side == Side::Buy { "Long" } else { "Short" };
    let qty = qty_text(product.as_ref(), Qty(t.max_qty));
    let _ = writeln!(
        s,
        "<text x=\"{PLOT_L}\" y=\"28\" font-size=\"18\" font-weight=\"600\" {}>{}  ·  {}  ·  {side} {qty}</text>",
        paint("fill", th.text_main),
        esc(kind),
        esc(&contract),
    );
    // The image outlives the setting: it names its zone (one read: the header and the plot
    // agree even if the setting changes mid-render).
    let zone = lines_core::display_tz::current();
    let when = format!(
        "Entry {}-{}  →  exit {}-{} {}  ·  held {}",
        zone.civil(t.entry_time).0,
        clock_text(t.entry_time, &zone),
        zone.civil(t.exit_time).0,
        clock_text(t.exit_time, &zone),
        zone_text(t.exit_time, &zone),
        crate::journal_view::duration_text(t.duration_ms())
    );
    let _ = writeln!(
        s,
        "<text x=\"{PLOT_L}\" y=\"52\" font-size=\"13\" {}>{}</text>",
        paint("fill", th.text_dim),
        esc(&when)
    );
    // Right of the title: the result, signed.
    if let Some(p) = pnl_text(t, product.as_ref(), look.pnl) {
        let c = if look.pnl.money() {
            match pnl::tone(t.net()) {
                pnl::Tone::Up => th.up,
                pnl::Tone::Down => th.down,
                pnl::Tone::Flat => th.text_main,
            }
        } else {
            th.text_main
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
            "<text x=\"{}\" y=\"{}\" font-size=\"13\" text-anchor=\"middle\" {}>Trade snapshots use the bars of an open chart or DOM of the contract.</text>",
            W / 2.0,
            H / 2.0 + 14.0,
            paint("fill", th.text_dim)
        );
        s.push_str("</svg>\n");
        return s;
    };
    plot(&mut s, t, d, product, th, &zone);
    s.push_str("</svg>\n");
    s
}

/// The slot a time stands in: the last bar starting at or before it, or past the last bar by
/// how many bars' time it is beyond it (a trade the bars stop short of keeps its true place).
fn slot_index(bars: &[Bar], tf_ms: i64, at: UtcMillis) -> usize {
    let last = bars.len().saturating_sub(1);
    let i = bars.iter().rposition(|b| b.time <= at).unwrap_or(0);
    match bars.get(i) {
        Some(b) if i == last => {
            last + ((at.0 - b.time.0) / tf_ms.max(1)).clamp(0, MAX_GAP_SLOTS) as usize
        }
        _ => i,
    }
}

/// A slot's width, with `slots` slots and room for the averages' labels after them.
fn slot_width(slots: usize) -> f32 {
    (PLOT_R - PLOT_L - LABEL_ROOM) / slots.max(34) as f32
}

/// `HH:MM` on `zone`'s clock.
fn hhmm(t: UtcMillis, zone: &DisplayTz) -> String {
    let mut s = String::new();
    let _ = zone.write_hhmm(t, &mut s);
    s
}

/// `zone`'s label at `t`.
fn zone_text(t: UtcMillis, zone: &DisplayTz) -> String {
    let mut s = String::new();
    let _ = zone.write_label(t, &mut s);
    s
}

/// `11-05 14:31:07` on `zone`'s clock.
fn clock_text(t: UtcMillis, zone: &DisplayTz) -> String {
    let (_, mo, d, h, mi, s) = zone.civil(t);
    format!("{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}")
}

/// Candles, axes, the trade's span, fills and averages.
/// Drawn on `zone`'s clock, the one the header names.
fn plot(s: &mut String, t: &Trade, d: &ChartBars, product: &Product, th: Theme, zone: &DisplayTz) {
    let bars = &d.bars;
    let mut lo = bars.iter().map(|b| b.low.0).min().unwrap_or(0);
    let mut hi = bars.iter().map(|b| b.high.0).max().unwrap_or(0);
    for f in &t.fills {
        lo = lo.min(f.price.0);
        hi = hi.max(f.price.0);
    }
    let tick = product.price.tick.max(1);
    let pad = ((hi - lo) / 12).max(tick * 2);
    let (lo, hi) = (lo - pad, hi + pad);
    let span = (hi - lo).max(1) as f32;
    let y = |p: f64| PLOT_T + (PLOT_B - PLOT_T) * (1.0 - ((p - lo as f64) as f32 / span));
    let slot_of = |at: UtcMillis| slot_index(bars, d.tf.nominal_ms(), at);
    let slot = slot_width(slot_of(t.exit_time).max(bars.len() - 1) + 1);
    let x = |i: usize| PLOT_L + slot * (i as f32 + 0.5);
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
    // Time axis: about one label per 110 px, HH:MM (the display zone), the date where it
    // changes.
    let every = ((110.0 / slot).ceil() as usize).max(1);
    let mut last_day: Option<CivilDate> = None;
    for (i, b) in bars.iter().enumerate() {
        if i % every != 0 {
            continue;
        }
        // Days on the display zone's clock, as the chart's axis.
        let local = zone.local(b.time);
        let day = CivilDate::from_utc(local);
        let (_, _, _, h, mi, _) = local.to_civil();
        let label = if last_day != Some(day) {
            format!("{:02}-{:02} {h:02}:{mi:02}", day.month, day.day)
        } else {
            format!("{h:02}:{mi:02}")
        };
        last_day = Some(day);
        let _ = writeln!(
            s,
            "<text x=\"{:.1}\" y=\"{}\" font-size=\"12\" text-anchor=\"{}\" {}>{label}</text>",
            // The first label starts at the plot's edge: centered it would be cut off.
            if i == 0 { PLOT_L } else { x(i) },
            PLOT_B + 18.0,
            if i == 0 { "start" } else { "middle" },
            paint("fill", th.text_dim)
        );
    }
    // On through a gap past the last bar, so the trade keeps a time under it.
    let last = bars.len() - 1;
    for i in (last + 1)..=slot_of(t.exit_time) {
        if i % every == 0 {
            let at = UtcMillis(bars[last].time.0 + (i - last) as i64 * d.tf.nominal_ms());
            let _ = writeln!(
                s,
                "<text x=\"{:.1}\" y=\"{}\" font-size=\"12\" text-anchor=\"middle\" {}>{}</text>",
                x(i),
                PLOT_B + 18.0,
                paint("fill", th.text_dim),
                hhmm(at, zone)
            );
        }
    }
    // The trade's span, behind the candles.
    let (e0, e1) = (slot_of(t.entry_time), slot_of(t.exit_time));
    let _ = writeln!(
        s,
        "<rect x=\"{:.1}\" y=\"{PLOT_T}\" width=\"{:.1}\" height=\"{}\" {}/>",
        x(e0) - slot / 2.0,
        slot * (e1.saturating_sub(e0) + 1) as f32,
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
            "<text x=\"{:.1}\" y=\"{ty:.1}\" font-size=\"12\" {} paint-order=\"stroke\" stroke-width=\"3\" {}>{}</text>",
            // Right of the span, clear of the fills' triangles (`LABEL_ROOM` holds it).
            x(e1) + slot / 2.0 + 4.0,
            paint("fill", th.text_main),
            paint("stroke", th.bg),
            esc(&text)
        );
    };
    // Labels go on the far side of each other so they never overlap.
    let (entry_avg, exit_avg) = (t.avg_entry(), t.avg_exit());
    let entry_below = exit_avg.is_some_and(|x| entry_avg.is_some_and(|e| e < x));
    avg(entry_avg, "avg entry", entry_below);
    avg(exit_avg, "avg exit", !entry_below);
    // Fills: a triangle at the price, pointing up for a buy (below it) and down for a sell
    // (above it), outlined in the background color so it reads over candles. A buy and a sell
    // that would cover each other stand side by side, the earlier one in place.
    let mut placed: Vec<(Side, f32, f32, f32)> = Vec::with_capacity(t.fills.len());
    for f in &t.fills {
        let mut fx = x(slot_of(f.at));
        let fy = y(f.price.0 as f64);
        let (top, bot) = if f.side == Side::Buy {
            (fy, fy + 12.0)
        } else {
            (fy - 12.0, fy)
        };
        while placed
            .iter()
            .any(|&(s, px, a, b)| s != f.side && (fx - px).abs() < MARK_W && top < b && a < bot)
        {
            fx += MARK_W;
        }
        placed.push((f.side, fx, top, bot));
        let (dy, c, word) = if f.side == Side::Buy {
            (12.0, th.up, "Buy")
        } else {
            (-12.0, th.down, "Sell")
        };
        let _ = writeln!(
            s,
            "<polygon points=\"{fx:.1},{fy:.1} {:.1},{:.1} {:.1},{:.1}\" stroke-width=\"1.5\" {} {}><title>{word} {} @ {}</title></polygon>",
            fx - 7.0,
            fy + dy,
            fx + 7.0,
            fy + dy,
            paint("fill", c),
            paint("stroke", th.bg),
            esc(&qty_text(Some(product), f.qty)),
            esc(&price_text(product, f.price))
        );
    }
    // Legend, top left above the plot: the shapes' meaning in words, and what this isn't.
    let _ = writeln!(
        s,
        "<text x=\"{PLOT_L}\" y=\"{}\" font-size=\"12\" {}>▲ buy fill  ·  ▼ sell fill  ·  dashed: average price  ·  shaded: the trade  ·  Trade snapshot: {} picked to fit the trade, no studies or drawings</text>",
        PLOT_T - 6.0,
        paint("fill", th.text_dim),
        tf_text(d.tf)
    );
    // Missing data shows as missing (docs/design.md §7).
    let mut missing = String::new();
    if let Some(b) = bars.first().filter(|b| t.entry_time < b.time) {
        let _ = write!(
            missing,
            "No bars before {} {}. Earlier fills are placed at the first bar.  ",
            hhmm(b.time, zone),
            zone_text(b.time, zone)
        );
    }
    if let Some(b) = bars.last().filter(|_| e1 >= bars.len()) {
        let _ = write!(
            missing,
            "No bars after {} {}. Later fills are placed by their times.",
            hhmm(b.time, zone),
            zone_text(b.time, zone)
        );
    }
    if !missing.is_empty() {
        let _ = writeln!(
            s,
            "<text x=\"{}\" y=\"{}\" font-size=\"13\" {} paint-order=\"stroke\" stroke-width=\"3\" {}>{missing}</text>",
            PLOT_L,
            PLOT_T + 16.0,
            paint("fill", th.text_main),
            paint("stroke", th.bg)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lines_core::{ContractKey, Month};
    use lines_feed::catalog;
    use lines_trading::journal::{Journal, ShotRequest};
    use lines_trading::{AccountId, Event, FillEvent, Money, OrderId};

    const ACCOUNT: &str = "ACCT-DISTINCTIVE-42";

    fn es() -> ContractKey {
        ContractKey::monthly(catalog::ES, 2026, Month::new(12).unwrap())
    }

    fn t0() -> i64 {
        UtcMillis::parse_iso("2026-11-05T14:31:07Z").unwrap().0
    }

    /// Long 2 ES at 6000.00, out at 6002.00 four minutes later, on an account with a
    /// distinctive id (in its fill ids too): the request a journal with shots on makes.
    fn request() -> ShotRequest {
        request_of(&[(Side::Buy, 600_000, 0), (Side::Sell, 600_200, 240)])
    }

    /// A round trip of these fills (side, price, seconds after the entry), 2 lots each.
    fn request_of(fills: &[(Side, i64, i64)]) -> ShotRequest {
        let mut j = Journal::new();
        j.set_shots(true);
        for (i, &(side, p, s)) in fills.iter().enumerate() {
            j.on_event(&Event::Fill(FillEvent {
                account: AccountId::new(ACCOUNT),
                order: OrderId(i as u64),
                contract: es(),
                side,
                price: Price(p),
                qty: Qty(2),
                fee: Money::cents(124),
                realised: Money::ZERO,
                trade_id: format!("{ACCOUNT}-f{i}"),
                reconciled: false,
                at: UtcMillis(t0() + s * 1000),
            }));
        }
        j.take_shot_requests().remove(0)
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

    fn data(t: &Trade) -> ChartBars {
        ChartBars {
            product: catalog::get(catalog::ES).unwrap(),
            tf: Timeframe::Minutes(1),
            bars: minute_bars()
                .into_iter()
                .filter(|b| b.time <= t.exit_time)
                .collect(),
        }
    }

    fn look(pnl: PnlDisplay) -> Look {
        Look {
            theme: Theme::dark(Palette::Standard),
            pnl,
        }
    }

    #[test]
    fn marks_every_fill_with_averages_axes_and_the_result() {
        let r = request();
        let d = data(&r.trade);
        let svg = render(&r.trade, "PAPER", Some(&d), look(PnlDisplay::Both));
        assert!(svg.starts_with("<svg") && svg.trim_end().ends_with("</svg>"));
        assert!(svg.contains(">PAPER  ·  ESZ26  ·  Long 2<"), "{svg}");
        assert!(
            svg.contains(
                "Trade snapshot: 1-minute bars picked to fit the trade, no studies or drawings"
            ),
            "the legend says what this is: {svg}"
        );
        assert!(svg.contains(
            "Entry 2026-11-05 14:31:07  →  exit 2026-11-05 14:35:07 UTC  ·  held 4m 00s<"
        ));
        // +2 points × 2 × $50 = $200, less $2.48 of fees; and in ticks, per contract like the
        // journal's (the file stands alone: no header says so).
        assert!(svg.contains("net +$197.52  (+8.0 t/ct)"), "{svg}");
        assert!(svg.contains("avg entry 6000.00") && svg.contains("avg exit 6002.00"));
        // One candle per bar up to the exit (60 before the entry minute, the entry minute, 4).
        assert_eq!(d.bars.len(), 65);
        assert_eq!(
            svg.matches("<rect").count(),
            2 + 65,
            "background, span, candles"
        );
        assert!(svg.contains(">6000.00<"), "price axis: {svg}");
        assert!(svg.contains(">11-05 13:31<"), "time axis: {svg}");
    }

    /// One triangle per fill, up for a buy and down for a sell, whatever the trade's side.
    #[test]
    fn fills_are_marked_by_shape_for_both_sides() {
        let r = request();
        let d = data(&r.trade);
        let svg = render(&r.trade, "PAPER", Some(&d), look(PnlDisplay::Off));
        assert_eq!(svg.matches("<polygon").count(), 2);
        let triangle = |word: &str| {
            let line = svg
                .lines()
                .find(|l| l.contains(&format!("<title>{word} 2 @")))
                .unwrap_or_else(|| panic!("no {word} mark"));
            let pts = line.split('"').nth(1).unwrap();
            let ys: Vec<f32> = pts
                .split(' ')
                .map(|p| p.split(',').nth(1).unwrap().parse().unwrap())
                .collect();
            ys[1] - ys[0]
        };
        assert!(triangle("Buy") > 0.0, "tip on top: base below the price");
        assert!(
            triangle("Sell") < 0.0,
            "tip at the bottom: base above the price"
        );
    }

    /// The x of each fill's triangle, in order.
    fn mark_xs(svg: &str) -> Vec<f32> {
        svg.lines()
            .filter(|l| l.starts_with("<polygon"))
            .map(|l| {
                let pts = l.split('"').nth(1).unwrap();
                pts.split(',').next().unwrap().parse().unwrap()
            })
            .collect()
    }

    /// A buy just above a sell in one bar would cover each other: they stand side by side. At
    /// a distance they stay on their bar.
    #[test]
    fn a_buy_and_a_sell_never_cover_each_other() {
        // A wide price range, so one tick is about a pixel.
        let wide = |t: &Trade| {
            let mut d = data(t);
            d.bars[0].high = Price(610_000);
            d
        };
        let r = request_of(&[(Side::Buy, 600_025, 0), (Side::Sell, 600_000, 2)]);
        let d = wide(&r.trade);
        let xs = mark_xs(&render(&r.trade, "PAPER", Some(&d), look(PnlDisplay::Off)));
        assert!(xs[1] - xs[0] >= MARK_W, "{xs:?}");
        let r = request_of(&[(Side::Buy, 600_000, 0), (Side::Sell, 600_500, 2)]);
        let d = wide(&r.trade);
        let xs = mark_xs(&render(&r.trade, "PAPER", Some(&d), look(PnlDisplay::Off)));
        assert_eq!(xs[0], xs[1], "{xs:?}");
    }

    /// Bars that stop before the trade does: the fills keep their times and a label says so,
    /// never the nearest bar; bars that start after the entry say that too.
    #[test]
    fn missing_bars_show_as_missing() {
        let r = request();
        let mut d = data(&r.trade);
        assert!(
            !render(&r.trade, "PAPER", Some(&d), look(PnlDisplay::Off)).contains("No bars"),
            "bars through the trade"
        );
        let entry_bar = d.bars.iter().rposition(|b| b.time <= r.trade.entry_time);
        d.bars.truncate(entry_bar.unwrap() + 1);
        let svg = render(&r.trade, "PAPER", Some(&d), look(PnlDisplay::Off));
        assert!(svg.contains("No bars after 14:31 UTC"), "{svg}");
        assert!(
            svg.contains(">14:3"),
            "the axis runs on under the gap: {svg}"
        );
        let xs = mark_xs(&svg);
        let n = d.bars.len();
        assert_eq!(slot_index(&d.bars, 60_000, r.trade.exit_time), n - 1 + 4);
        assert!(xs[1] - xs[0] >= 3.0 * slot_width(n + 4), "{xs:?}");
        // A gap can't run the plot off the page.
        assert_eq!(
            slot_index(&d.bars, 60_000, UtcMillis(r.trade.exit_time.0 + 86_400_000)),
            n - 1 + MAX_GAP_SLOTS as usize
        );

        let mut late = data(&r.trade);
        late.bars.retain(|b| b.time > r.trade.entry_time);
        let svg = render(&r.trade, "PAPER", Some(&late), look(PnlDisplay::Off));
        assert!(svg.contains("No bars before 14:32 UTC"), "{svg}");
    }

    /// A broker's closing fill stamped before the entry: a one-slot span, not a panic (the
    /// thread would die and every later trade keep a name with no file).
    #[test]
    fn a_fill_stamped_before_the_entry_keeps_the_thread_alive() {
        let r = request_of(&[(Side::Buy, 600_000, 0), (Side::Sell, 600_200, -300)]);
        let d = data(&r.trade);
        let svg = render(&r.trade, "PAPER", Some(&d), look(PnlDisplay::Off));
        assert_eq!(svg.matches("<polygon").count(), 2, "{svg}");
    }

    /// The labels' room holds at every bar count the file can draw.
    #[test]
    fn the_averages_labels_stay_off_the_price_axis() {
        for slots in [1, 34, 40, 100, 102, 300] {
            let after_last = PLOT_L + slot_width(slots) * slots as f32;
            assert!(after_last + LABEL_ROOM <= PLOT_R + 0.5, "{slots}");
        }
    }

    /// A store of `contract` with 1-minute bars from `first` to `last` minutes after the entry.
    fn store_of(contract: ContractKey, first: i64, last: i64) -> Arc<ChartStore> {
        let store = Arc::new(ChartStore::new(
            catalog::get(catalog::ES).unwrap(),
            Some(contract),
            lines_core::Capabilities::default(),
            "test",
            60_000,
            300,
        ));
        for m in first..=last {
            store
                .read()
                .apply(&[lines_feed::provider::MarketEvent::Trade {
                    time: UtcMillis(t0() + m * 60_000),
                    price: Price(600_000 + m * 25),
                    qty: Qty(1),
                    aggressor: None,
                }]);
        }
        store
    }

    /// From a feed's store to the file, with real bars: the contract's store is used however
    /// many bars another contract's holds; the window starts before the entry and ends at the
    /// exit; a feed is remembered once and a gone one is dropped.
    #[test]
    fn a_followed_stores_bars_reach_the_file() {
        let r = request();
        let other_contract = ContractKey::monthly(catalog::ES, 2027, Month::new(3).unwrap());
        let (mine, other) = (store_of(es(), -50, 10), store_of(other_contract, -200, 10));
        let stores = Stores::default();
        remember(&stores, &other);
        assert!(chart_data(&stores, &r.trade).is_none(), "another contract");
        remember(&stores, &mine);
        remember(&stores, &mine);
        let gone = store_of(es(), 0, 1);
        remember(&stores, &gone);
        drop(gone);
        let d = chart_data(&stores, &r.trade).expect("the contract's bars");
        assert_eq!(stores.lock().unwrap().len(), 2, "once each, gone ones out");
        assert_eq!(d.tf, Timeframe::Minutes(1));
        assert!(d.bars.iter().all(|b| b.time <= r.trade.exit_time));
        // Bars starting within 42 minutes before the entry, its minute, 4 more to the exit.
        assert_eq!(d.bars.len(), 41 + 1 + 4, "{:?}", d.bars.first());
        assert!(d.bars[0].time < r.trade.entry_time);
        assert_eq!(d.bars.last().unwrap().close, Price(600_100), "mine, at +4");

        let dir = temp("real-bars");
        let kinds = Kinds::default();
        *kinds.write().unwrap() = vec![AccountInfo::paper(
            AccountId::new(ACCOUNT),
            "Name".into(),
            None,
        )];
        assert_eq!(kind_word(&kinds, &AccountId::new("elsewhere")), "UNLISTED");
        let tx = spawn(stores, kinds, dir.clone()).unwrap();
        let file = r.file.clone();
        tx.send(ShotMsg::Take(Box::new(r))).unwrap();
        let (done, wait) = mpsc::channel();
        tx.send(ShotMsg::Flush(done)).unwrap();
        wait.recv_timeout(std::time::Duration::from_secs(30))
            .unwrap();
        let svg = std::fs::read_to_string(dir.join(file)).unwrap();
        assert!(
            !svg.contains("No chart data") && !svg.contains("No bars"),
            "{svg}"
        );
        assert_eq!(
            svg.matches("<rect").count(),
            2 + 46,
            "background, span, candles"
        );
        assert!(svg.contains(">PAPER  ·  ESZ26"), "{svg}");
        assert!(
            !svg.contains("DISTINCTIVE") && !svg.contains("Name"),
            "{svg}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Writes evidence files to `LINES_EVIDENCE_DIR`: a scale-out with bars through the trade,
    /// in each theme (`cargo test -p lines-app --lib evidence_files -- --ignored`).
    #[test]
    #[ignore = "writes evidence files when asked"]
    fn evidence_files() {
        let dir = PathBuf::from(std::env::var("LINES_EVIDENCE_DIR").unwrap());
        std::fs::create_dir_all(&dir).unwrap();
        let r = request_of(&[
            (Side::Buy, 600_000, 0),
            (Side::Buy, 599_950, 95),
            (Side::Sell, 600_275, 480),
            (Side::Sell, 600_325, 720),
        ]);
        let d = ChartBars {
            product: catalog::get(catalog::ES).unwrap(),
            tf: Timeframe::Minutes(1),
            bars: minute_bars()
                .into_iter()
                .chain((0..16).map(|i| {
                    let o = 600_000 + i * 20;
                    Bar {
                        time: UtcMillis(UtcMillis(t0()).floor_to(60_000).0 + i * 60_000),
                        open: Price(o),
                        high: Price(o + 75),
                        low: Price(o - 75),
                        close: Price(o + if i % 3 == 0 { -25 } else { 25 }),
                        volume: Qty(100),
                    }
                }))
                .filter(|b| b.time <= r.trade.exit_time)
                .fold(Vec::<Bar>::new(), |mut v, b| {
                    v.retain(|o| o.time != b.time);
                    v.push(b);
                    v
                }),
        };
        for (name, theme, pnl) in [
            ("dark", Theme::dark(Palette::Standard), PnlDisplay::Both),
            ("light", Theme::light(Palette::Standard), PnlDisplay::Both),
            (
                "dark-cb",
                Theme::dark(Palette::Colorblind),
                PnlDisplay::Both,
            ),
            (
                "light-cb",
                Theme::light(Palette::Colorblind),
                PnlDisplay::Both,
            ),
            ("pnl-off", Theme::dark(Palette::Standard), PnlDisplay::Off),
        ] {
            let svg = render(&r.trade, "PAPER", Some(&d), Look { theme, pnl });
            std::fs::write(dir.join(format!("bars-{name}.svg")), svg).unwrap();
        }
    }

    /// The account appears in no form: not in the file name, not in the SVG.
    #[test]
    fn no_account_in_the_name_or_the_file() {
        let r = request();
        assert!(!r.file.contains(ACCOUNT) && !r.file.contains("DISTINCTIVE"));
        for d in [Some(data(&r.trade)), None] {
            for mode in PnlDisplay::ALL {
                let svg = render(&r.trade, "PAPER", d.as_ref(), look(mode));
                assert!(!svg.contains("DISTINCTIVE"), "{svg}");
                assert!(!svg.contains("ACCT"), "{svg}");
            }
        }
    }

    /// P&L display off: no money or ticks anywhere in the file; ticks: ticks only.
    #[test]
    fn follows_the_pnl_display() {
        let r = request();
        let d = data(&r.trade);
        let off = render(&r.trade, "PAPER", Some(&d), look(PnlDisplay::Off));
        assert!(
            !off.contains('$') && !off.contains(" t/ct<") && !off.contains("net "),
            "{off}"
        );
        assert!(off.contains("avg entry 6000.00"), "prices stay");
        let ticks = render(&r.trade, "PAPER", Some(&d), look(PnlDisplay::Ticks));
        assert!(
            ticks.contains(">+8.0 t/ct<") && !ticks.contains('$'),
            "{ticks}"
        );
        let none = render(&r.trade, "PAPER", None, look(PnlDisplay::Off));
        assert!(!none.contains('$') && !none.contains("net "), "{none}");
    }

    /// A product not in the catalog has no tick to state: its dollars, never raw price units
    /// called ticks.
    #[test]
    fn an_unlisted_product_states_no_ticks() {
        let r = request();
        assert_eq!(pnl_text(&r.trade, None, PnlDisplay::Ticks), None);
        let both = pnl_text(&r.trade, None, PnlDisplay::Both).unwrap();
        assert!(both.starts_with("net ") && !both.contains(" t"), "{both}");
    }

    /// The theme at the time colors the file (from its tokens).
    #[test]
    fn takes_the_theme_it_is_given() {
        let r = request();
        let d = data(&r.trade);
        let svg = |theme| {
            render(
                &r.trade,
                "PAPER",
                Some(&d),
                Look {
                    theme,
                    pnl: PnlDisplay::Off,
                },
            )
        };
        let dark = Theme::dark(Palette::Standard);
        let light = Theme::light(Palette::Standard);
        assert!(svg(dark).contains(&paint("fill", dark.bg)));
        assert!(svg(light).contains(&paint("fill", light.bg)));
        assert_ne!(svg(dark), svg(light));
    }

    #[test]
    fn no_chart_data_says_so() {
        let r = request();
        let svg = render(&r.trade, "PAPER", None, look(PnlDisplay::Both));
        assert!(svg.contains("No chart data for ESZ26 when this was taken."));
        assert!(!svg.contains("<polygon"));
        let empty = ChartBars {
            product: catalog::get(catalog::ES).unwrap(),
            tf: Timeframe::Minutes(1),
            bars: Vec::new(),
        };
        let svg = render(&r.trade, "PAPER", Some(&empty), look(PnlDisplay::Both));
        assert!(svg.contains("No chart data"));
        assert_eq!(esc("<A&B>"), "&lt;A&amp;B&gt;");
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
        let five = derive(
            &minute_bars(),
            Timeframe::Minutes(5),
            catalog::sessions(catalog::ES),
        );
        assert!(five.len() >= 14 && five.len() <= 16, "{}", five.len());
        assert!(five.iter().all(|b| b.time.0 % 300_000 == 0));
        let total: i64 = five.iter().map(|b| b.volume.0).sum();
        assert!((70 * 100..=75 * 100).contains(&total));
        assert_eq!(grid_step(1000, 25, 6), 250);
        assert_eq!(grid_step(30, 25, 6), 25);
    }

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("lines-shots-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// The thread writes the named file whole, with no data open it says so, and a flush
    /// answers only after what was sent before it.
    #[test]
    fn the_thread_writes_the_named_file_and_a_flush_waits_for_it() {
        let dir = temp("thread");
        let tx = spawn(Stores::default(), Kinds::default(), dir.clone()).unwrap();
        let r = request();
        let file = r.file.clone();
        tx.send(ShotMsg::Take(Box::new(r))).unwrap();
        let (done, wait) = mpsc::channel();
        tx.send(ShotMsg::Flush(done)).unwrap();
        wait.recv_timeout(std::time::Duration::from_secs(30))
            .unwrap();
        let svg = std::fs::read_to_string(dir.join(&file)).unwrap();
        assert!(svg.contains("No chart data for ESZ26"));
        assert!(!svg.contains("DISTINCTIVE"));
        assert!(!dir.join(&file).with_extension("svg.tmp").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A shot whose folder can't be written is logged and leaves the name: the thread goes on.
    #[test]
    fn a_failed_write_leaves_the_thread_running() {
        let dir = temp("blocked");
        std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
        std::fs::write(&dir, "a file where the folder should be").unwrap();
        let tx = spawn(Stores::default(), Kinds::default(), dir.clone()).unwrap();
        tx.send(ShotMsg::Take(Box::new(request()))).unwrap();
        let (done, wait) = mpsc::channel();
        tx.send(ShotMsg::Flush(done)).unwrap();
        wait.recv_timeout(std::time::Duration::from_secs(30))
            .unwrap();
        let _ = std::fs::remove_file(dir);
    }

    #[test]
    fn locating_a_chart_explains_each_way_it_fails() {
        let dir = temp("locate");
        std::fs::create_dir_all(&dir).unwrap();
        let name = "2026-11-05-0123456789abcdef.svg";
        assert_eq!(locate(Some(&dir), None), Err("this trade has no snapshot"));
        assert!(locate(None, Some(name)).is_err());
        let missing = locate(Some(&dir), Some(name)).unwrap_err();
        assert!(missing.contains("missing"), "{missing}");
        // A saved name that points elsewhere is never followed, even if the file exists.
        std::fs::write(dir.join("outside.svg"), "x").unwrap();
        let inner = dir.join("shots");
        std::fs::create_dir_all(&inner).unwrap();
        assert!(locate(Some(&inner), Some("../outside.svg")).is_err());
        assert!(locate(Some(&inner), Some("..\\outside.svg")).is_err());
        std::fs::write(dir.join(name), "<svg/>").unwrap();
        assert_eq!(locate(Some(&dir), Some(name)), Ok(dir.join(name)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn file_urls_encode_what_a_home_folder_can_hold() {
        assert_eq!(
            file_url(Path::new(r"C:\Users\A B\é\x.svg")),
            "file:///C:/Users/A%20B/%C3%A9/x.svg"
        );
        assert_eq!(
            file_url(Path::new("/home/a b/x.svg")),
            "file:///home/a%20b/x.svg"
        );
        assert_eq!(
            file_url(Path::new(r"\\server\share\x.svg")),
            "file://server/share/x.svg"
        );
    }
}
