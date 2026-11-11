//! How profit and loss is shown (owner request): off, ticks, currency or both, app-wide
//! (`settings.json`, [`PnlDisplay`]). Traders hide P&L for psychology, not to hide their
//! position: with P&L off, side, quantity and average price stay visible, and no money or
//! tick number appears anywhere in the trading UI (headers, chart tags, rule strip, refusal
//! messages: [`redact_amounts`]).
//!
//! Formatting is pure and allocation-free (it writes into a caller's buffer, so chart and DOM
//! paint can use it):
//! - **ticks**: per contract, from the average price, one decimal and always signed (`+2.5 t`,
//!   `−1.0 t`, `0.0 t`; the same true minus as currency);
//! - **currency**: exact from [`Money`] (integer arithmetic, rounded to cents only for display),
//!   always signed, with thousands separators and a true minus (U+2212, as wide as the plus and
//!   never read as a dash or a separator): `+$1,312.50`, `−$175.00`, `$0.00`. Display only:
//!   files and exports (the journal CSV) write their own plain numbers.
//!
//! Colour follows the sign through the up/down tokens ([`Tone`]), and the sign is always
//! written: never colour alone (docs/design.md §4).

use std::borrow::Cow;
use std::fmt::{self, Write};
use std::sync::atomic::{AtomicU64, Ordering};

use lines_trading::Money;
use serde::{Deserialize, Serialize};

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PnlDisplay {
    Off,
    Ticks,
    Currency,
    #[default]
    Both,
}

impl PnlDisplay {
    pub const ALL: [PnlDisplay; 4] = [
        PnlDisplay::Off,
        PnlDisplay::Ticks,
        PnlDisplay::Currency,
        PnlDisplay::Both,
    ];

    /// The next mode (the cycle command): off → ticks → currency → both → off.
    pub fn next(self) -> PnlDisplay {
        let i = PnlDisplay::ALL.iter().position(|m| *m == self).unwrap_or(0);
        PnlDisplay::ALL[(i + 1) % PnlDisplay::ALL.len()]
    }

    pub fn label(self) -> &'static str {
        match self {
            PnlDisplay::Off => "off",
            PnlDisplay::Ticks => "ticks",
            PnlDisplay::Currency => "currency",
            PnlDisplay::Both => "ticks and currency",
        }
    }

    pub fn is_default(&self) -> bool {
        *self == PnlDisplay::default()
    }

    pub fn ticks(self) -> bool {
        matches!(self, PnlDisplay::Ticks | PnlDisplay::Both)
    }

    pub fn money(self) -> bool {
        matches!(self, PnlDisplay::Currency | PnlDisplay::Both)
    }

    /// Whether any P&L number shows (off: none, anywhere).
    pub fn shown(self) -> bool {
        self != PnlDisplay::Off
    }
}

static EPOCH: AtomicU64 = AtomicU64::new(0);

/// A display setting changed (the P&L display, the time zone): panes repaint on their next
/// poll ([`display_epoch`]).
pub fn display_changed() {
    EPOCH.fetch_add(1, Ordering::Relaxed);
}

/// Bumped whenever the display setting changes (lock-free, read by every pane's poll).
pub fn display_epoch() -> u64 {
    EPOCH.load(Ordering::Relaxed)
}

/// Which way a P&L points, for its colour (up/down tokens; flat: the normal text colour).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Tone {
    Up,
    Down,
    Flat,
}

pub fn tone(m: Money) -> Tone {
    match m.0.signum() {
        1 => Tone::Up,
        -1 => Tone::Down,
        _ => Tone::Flat,
    }
}

/// `+2.5 t`, `−1.0 t`, `0.0 t` (never `−0.0`): the same true minus as money, so a cell holding
/// both reads one way.
pub fn write_ticks(out: &mut impl Write, ticks: f64) -> fmt::Result {
    let tenths = (ticks * 10.0).round() as i64;
    let sign = match tenths.signum() {
        1 => "+",
        -1 => "\u{2212}",
        _ => "",
    };
    let a = tenths.unsigned_abs();
    write!(out, "{sign}{}.{} t", a / 10, a % 10)
}

/// The minus sign money is shown with (U+2212): as wide as `+` in the number font, and never
/// read as a hyphen or a dash beside a `·`.
pub const MINUS: char = '\u{2212}';

/// `1,234,567`: a whole amount with thousands separators, written without allocating.
pub fn write_grouped(out: &mut impl Write, n: u128) -> fmt::Result {
    if n < 1000 {
        return write!(out, "{n}");
    }
    write_grouped(out, n / 1000)?;
    write!(out, ",{:03}", n % 1000)
}

/// `+$1,312.50`, `−$4.00`, `$0.00`: exact from `Money` (1e-10 units), rounded half away from
/// zero to cents for display, in integers; thousands separated, a loss with a true minus.
pub fn write_money(out: &mut impl Write, m: Money) -> fmt::Result {
    const PER_CENT: i128 = 100_000_000; // Money is 1e-10 of the currency
    let neg = m.0 < 0;
    let a = m.0.unsigned_abs();
    let cents = (a + (PER_CENT as u128) / 2) / PER_CENT as u128;
    match (neg, cents) {
        (_, 0) => {}
        (true, _) => out.write_char(MINUS)?,
        (false, _) => out.write_char('+')?,
    }
    out.write_char('$')?;
    write_grouped(out, cents / 100)?;
    write!(out, ".{:02}", cents % 100)
}

/// An open position's P&L as `mode` shows it: ticks (when known), currency, or both
/// (`+2.5 t  +$125.00`). Writes nothing when `mode` is off; returns whether it wrote.
pub fn write_pnl(
    out: &mut impl Write,
    mode: PnlDisplay,
    ticks: Option<f64>,
    money: Money,
) -> Result<bool, fmt::Error> {
    let mut wrote = false;
    if mode.ticks()
        && let Some(t) = ticks
    {
        write_ticks(out, t)?;
        wrote = true;
    }
    if mode.money() {
        if wrote {
            out.write_str("  ")?;
        }
        write_money(out, money)?;
        wrote = true;
    }
    Ok(wrote)
}

/// An account's day P&L (realised + open, by session trade date) as `mode` shows it: a
/// currency amount (`Day +$125.00`, the trade panel's word). Day P&L spans contracts, so it has no tick form: shown
/// in currency and both modes only.
pub fn write_day(out: &mut impl Write, mode: PnlDisplay, day: Money) -> Result<bool, fmt::Error> {
    if !mode.money() {
        return Ok(false);
    }
    out.write_str("Day ")?;
    write_money(out, day)?;
    Ok(true)
}

/// A message with its amounts removed, for P&L off: parenthesised details with digits
/// ("daily loss limit reached (-600.00 today, limit 500)") go, and an order's value
/// ("order value 900000.00 is over the maximum 700000.00") loses its numbers. Other numbers
/// (prices, quantities, contract counts) stay: they aren't P&L. A loss limit's amount
/// ("daily loss limit of 1000 reached", a guardrail) goes too.
pub fn redact_amounts(message: &str) -> Cow<'_, str> {
    let has_group = message.split('(').skip(1).any(|g| {
        g.split(')')
            .next()
            .is_some_and(|g| g.bytes().any(|b| b.is_ascii_digit()))
    });
    let value = message.contains("order value");
    if !has_group && !value && !message.contains('$') && !message.contains("limit of ") {
        return Cow::Borrowed(message);
    }
    let mut out = String::with_capacity(message.len());
    let mut rest = message;
    while let Some(i) = rest.find('(') {
        let Some(j) = rest[i..].find(')') else { break };
        let group = &rest[i..i + j + 1];
        out.push_str(&rest[..i]);
        if !group.bytes().any(|b| b.is_ascii_digit()) {
            out.push_str(group);
        }
        rest = &rest[i + j + 1..];
    }
    out.push_str(rest);
    // Money amounts: `$…` tokens anywhere, and the numbers of an order's value.
    let words: Vec<&str> = out.split(' ').collect();
    let mut kept: Vec<&str> = Vec::with_capacity(words.len());
    let mut in_value = false;
    let number = |w: &str| w.trim_end_matches([';', ',', '.']).parse::<f64>().is_ok();
    let mut skip = 0;
    for (i, &w) in words.iter().enumerate() {
        if skip > 0 {
            skip -= 1;
            continue;
        }
        if w.starts_with('$') || w.starts_with("-$") || w.starts_with("+$") || w.starts_with("−$")
        {
            continue;
        }
        // "limit of 1000": the amount and its "of" go; a trailing comma stays.
        if w == "of"
            && i > 0
            && words[i - 1] == "limit"
            && words.get(i + 1).is_some_and(|n| number(n))
        {
            // The number's punctuation moves onto "limit" (the word kept just before).
            match words[i + 1].chars().last() {
                Some(',') => *kept.last_mut().expect("limit was kept") = "limit,",
                Some(';') => *kept.last_mut().expect("limit was kept") = "limit;",
                _ => {}
            }
            skip = 1;
            continue;
        }
        if w == "value" {
            in_value = true;
        }
        if in_value && number(w) {
            continue;
        }
        kept.push(w);
    }
    let s = kept.join(" ");
    // Tidy the spaces a removed group leaves behind (`reached ;` → `reached;`).
    Cow::Owned(
        s.replace(" ;", ";")
            .replace(" ,", ",")
            .replace("  ", " ")
            .trim()
            .to_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(f: impl FnOnce(&mut String) -> fmt::Result) -> String {
        let mut out = String::new();
        f(&mut out).unwrap();
        out
    }

    #[test]
    fn ticks_have_one_decimal_and_a_sign() {
        // Long 2 @ 5999.625 (an average between ticks) marked at 6000.25: +2.5 ticks.
        assert_eq!(s(|o| write_ticks(o, 2.5)), "+2.5 t");
        assert_eq!(s(|o| write_ticks(o, -1.0)), "−1.0 t");
        assert_eq!(s(|o| write_ticks(o, 0.04)), "0.0 t", "never −0.0 or +0.0");
        assert_eq!(s(|o| write_ticks(o, -0.04)), "0.0 t");
        assert_eq!(
            s(|o| write_ticks(o, 1.25)),
            "+1.3 t",
            "rounded half away from zero"
        );
        assert_eq!(s(|o| write_ticks(o, 12.0)), "+12.0 t");
    }

    #[test]
    fn currency_is_exact_from_money() {
        assert_eq!(s(|o| write_money(o, Money::whole(125))), "+$125.00");
        assert_eq!(s(|o| write_money(o, -Money::cents(400))), "\u{2212}$4.00");
        assert_eq!(s(|o| write_money(o, Money::ZERO)), "$0.00");
        // ES: one tick is exactly $12.50; 3 ticks short −$37.50.
        assert_eq!(s(|o| write_money(o, -Money::cents(3750))), "\u{2212}$37.50");
        // Thousands separated, at every boundary.
        assert_eq!(s(|o| write_money(o, Money::cents(131_250))), "+$1,312.50");
        assert_eq!(
            s(|o| write_money(o, -Money::whole(1_000))),
            "\u{2212}$1,000.00"
        );
        assert_eq!(s(|o| write_money(o, Money::whole(999))), "+$999.00");
        assert_eq!(s(|o| write_money(o, Money::whole(100_000))), "+$100,000.00");
        assert_eq!(
            s(|o| write_money(o, Money::whole(1_000_001))),
            "+$1,000,001.00"
        );
        // Sub-cent amounts (crypto): half a cent rounds away from zero; never via floats.
        assert_eq!(
            s(|o| write_money(o, Money(500_000_000 + 50_000_000))),
            "+$0.06"
        );
        assert_eq!(
            s(|o| write_money(o, Money(-49_999_999))),
            "$0.00",
            "rounds to zero: unsigned"
        );
        // Large values stay exact (i128 Money; no float rounding).
        assert_eq!(
            s(|o| write_money(o, Money::whole(123_456_789))),
            "+$123,456,789.00"
        );
        // The minus is one char, never an ASCII hyphen: nothing here reads "· −$".
        assert!(!s(|o| write_money(o, -Money::whole(175))).contains('-'));
    }

    #[test]
    fn each_mode_writes_its_part_and_off_writes_nothing() {
        let w = |mode, t| {
            let mut out = String::new();
            let wrote = write_pnl(&mut out, mode, t, Money::whole(125)).unwrap();
            (wrote, out)
        };
        assert_eq!(w(PnlDisplay::Off, Some(2.5)), (false, String::new()));
        assert_eq!(w(PnlDisplay::Ticks, Some(2.5)), (true, "+2.5 t".into()));
        assert_eq!(
            w(PnlDisplay::Currency, Some(2.5)),
            (true, "+$125.00".into())
        );
        assert_eq!(
            w(PnlDisplay::Both, Some(2.5)),
            (true, "+2.5 t  +$125.00".into())
        );
        // No mark yet: ticks unknown; currency still shows.
        assert_eq!(w(PnlDisplay::Both, None), (true, "+$125.00".into()));
        assert_eq!(w(PnlDisplay::Ticks, None), (false, String::new()));
        // Day P&L: currency and both only.
        let d = |mode| {
            let mut out = String::new();
            write_day(&mut out, mode, -Money::whole(25)).unwrap();
            out
        };
        assert_eq!(d(PnlDisplay::Both), "Day −$25.00");
        assert_eq!(d(PnlDisplay::Currency), "Day −$25.00");
        assert_eq!(d(PnlDisplay::Ticks), "");
        assert_eq!(d(PnlDisplay::Off), "");
    }

    #[test]
    fn modes_cycle_and_default_to_both() {
        assert_eq!(PnlDisplay::default(), PnlDisplay::Both);
        let mut m = PnlDisplay::Off;
        let mut seen = vec![m];
        for _ in 0..3 {
            m = m.next();
            seen.push(m);
        }
        assert_eq!(seen, PnlDisplay::ALL);
        assert_eq!(m.next(), PnlDisplay::Off);
        assert!(!PnlDisplay::Off.shown() && PnlDisplay::Ticks.shown());
    }

    #[test]
    fn tone_follows_the_sign() {
        assert_eq!(tone(Money::whole(1)), Tone::Up);
        assert_eq!(tone(-Money::cents(1)), Tone::Down);
        assert_eq!(tone(Money::ZERO), Tone::Flat);
    }

    #[test]
    fn amounts_are_redacted_but_the_reason_stays() {
        // As this formatter writes a loss (U+2212), too.
        assert_eq!(
            redact_amounts("at a loss of −$1,250.00 today"),
            "at a loss of today"
        );
        assert_eq!(
            redact_amounts(
                "daily loss limit reached (-600.00 today, limit 500); only Flatten and Cancel All"
            ),
            "daily loss limit reached; only Flatten and Cancel All"
        );
        assert_eq!(
            redact_amounts("order value 900000.00 is over the maximum 700000.00"),
            "order value is over the maximum"
        );
        // Any parenthesised detail with digits goes (a preset named "50K eval" included).
        assert_eq!(
            redact_amounts("reducing only: DailyLoss limit breached (50K eval); ..."),
            "reducing only: DailyLoss limit breached; ..."
        );
        assert_eq!(redact_amounts("loss of $25.00 today"), "loss of today");
        // Guardrail lockouts name their loss limit.
        assert_eq!(
            redact_amounts("Locked: daily loss limit of 1000 reached, until the next trade date"),
            "Locked: daily loss limit reached, until the next trade date"
        );
        assert_eq!(
            redact_amounts("daily loss limit of 500, today"),
            "daily loss limit, today"
        );
        assert_eq!(
            redact_amounts("max 5 trades reached"),
            "max 5 trades reached"
        );
        // Prices, quantities and contract counts aren't P&L: kept.
        let m = "would hold 60 micro-equivalent contracts, over the 50 allowed";
        assert_eq!(redact_amounts(m), m);
        assert!(matches!(redact_amounts(m), Cow::Borrowed(_)));
        assert_eq!(
            redact_amounts("6000.00: ES market is closed"),
            "6000.00: ES market is closed"
        );
    }
}
