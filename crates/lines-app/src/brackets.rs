//! Bracket presets (trade panel v2): editable stop/target presets per account and product,
//! shown and typed in ticks or points. Pure: no GPUI.
//!
//! - **Storage is ticks**, exact integers, whatever the display unit ([`NamedBracket`]).
//!   Points convert through the product's tick size (ES 0.25: 20 t = 5 pt), exactly: a price
//!   unit is a rational of the product's `PriceScale`, so a tick in points is `tick × num /
//!   den`, printed with every decimal it needs (ES 5.25, ZN 0.015625) and never rounded.
//! - **Points input must land on the tick grid.** Off-grid input is refused with the nearest
//!   valid value shown ([`AmountError::OffGrid`]); nothing is rounded silently.
//! - **Presets differ per product** (an NQ stop in ticks isn't an ES stop): keyed by account,
//!   then product symbol, with [`DEFAULT_PRESETS`] for a product that has none.
//! - **The display unit** is per account ([`BracketUnits`], default ticks).
//!
//! Saved in `settings.json` as the optional `bracket_presets` field (no version bump).

use std::collections::BTreeMap;

use lines_core::{Price, Product};
use serde::{Deserialize, Serialize};

use crate::dom_trade::BracketPreset;

/// How bracket distances show and are typed.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BracketUnits {
    #[default]
    Ticks,
    Points,
}

impl BracketUnits {
    pub fn toggled(self) -> BracketUnits {
        match self {
            BracketUnits::Ticks => BracketUnits::Points,
            BracketUnits::Points => BracketUnits::Ticks,
        }
    }

    /// `t` / `pt`.
    pub fn suffix(self) -> &'static str {
        match self {
            BracketUnits::Ticks => "t",
            BracketUnits::Points => "pt",
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            BracketUnits::Ticks => "ticks",
            BracketUnits::Points => "points",
        }
    }

    pub fn is_default(&self) -> bool {
        *self == BracketUnits::Ticks
    }
}

/// One saved preset: stop and target in ticks from the entry, and an optional name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamedBracket {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    pub stop_ticks: u32,
    pub target_ticks: u32,
}

impl NamedBracket {
    pub fn preset(&self) -> BracketPreset {
        BracketPreset {
            stop_ticks: self.stop_ticks,
            target_ticks: self.target_ticks,
        }
    }
}

/// One account's bracket settings.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountBrackets {
    #[serde(default, skip_serializing_if = "BracketUnits::is_default")]
    pub units: BracketUnits,
    /// Presets per product symbol (`ES`, `NQ`, …), in display order.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub products: BTreeMap<String, Vec<NamedBracket>>,
}

/// Every account's bracket settings, keyed by account id (`settings.json`).
pub type BracketSettings = BTreeMap<String, AccountBrackets>;

/// Presets for a product that has none of its own.
pub const DEFAULT_PRESETS: [(u32, u32); 3] = [(8, 16), (12, 24), (20, 40)];

/// Most presets the segmented control shows (more go behind "…").
pub const MAX_SHOWN: usize = 4;
/// Largest distance accepted, in ticks.
pub const MAX_TICKS: u32 = 100_000;

pub fn units_for(s: &BracketSettings, account: &str) -> BracketUnits {
    s.get(account).map(|a| a.units).unwrap_or_default()
}

/// The presets for `account` on `product` (its own, else the defaults), in order.
pub fn presets_for(s: &BracketSettings, account: &str, product: &str) -> Vec<NamedBracket> {
    match s.get(account).and_then(|a| a.products.get(product)) {
        // Saved, even empty (the trader deleted them all): theirs.
        Some(v) => v.clone(),
        _ => DEFAULT_PRESETS
            .iter()
            .map(|&(s, t)| NamedBracket {
                name: String::new(),
                stop_ticks: s,
                target_ticks: t,
            })
            .collect(),
    }
}

/// The next choice after `current` in `[Off, presets…]` (wrapping; a custom bracket rejoins
/// at the start).
pub fn next_choice(
    presets: &[NamedBracket],
    current: Option<BracketPreset>,
) -> Option<BracketPreset> {
    let all: Vec<Option<BracketPreset>> = std::iter::once(None)
        .chain(presets.iter().map(|p| Some(p.preset())))
        .collect();
    let i = all.iter().position(|p| *p == current);
    all[i.map_or(0, |i| (i + 1) % all.len())]
}

// ---- exact conversion ------------------------------------------------------------------------

/// A tick of `product` in points, as an exact fraction (numerator, denominator), reduced.
fn tick_points(product: &Product) -> (i128, i128) {
    let s = product.price.scale;
    reduce(
        i128::from(product.price.tick) * i128::from(s.num()),
        i128::from(s.den()),
    )
}

fn gcd(a: i128, b: i128) -> i128 {
    if b == 0 { a.abs() } else { gcd(b, a % b) }
}

fn reduce(n: i128, d: i128) -> (i128, i128) {
    let g = gcd(n, d).max(1);
    (n / g, d / g)
}

/// An exact non-negative fraction as a decimal, every digit it needs (the catalog's scales
/// have denominators of 2s and 5s, so it terminates; capped at 12 decimals for safety).
fn decimal_text(n: i128, d: i128) -> String {
    let (int, mut rem) = (n / d, n % d);
    let mut s = int.to_string();
    if rem != 0 {
        s.push('.');
        let mut digits = 0;
        while rem != 0 && digits < 12 {
            rem *= 10;
            s.push(char::from(b'0' + (rem / d) as u8));
            rem %= d;
            digits += 1;
        }
    }
    s
}

/// `ticks` as text in `units` (no suffix): `20`, or points `5`, `5.25`, `0.015625`.
pub fn amount_text(ticks: u32, product: &Product, units: BracketUnits) -> String {
    match units {
        BracketUnits::Ticks => ticks.to_string(),
        BracketUnits::Points => {
            let (n, d) = tick_points(product);
            let (n, d) = reduce(n * i128::from(ticks), d);
            decimal_text(n, d)
        }
    }
}

/// A preset's label: `20/40 t` or `5/10 pt`; a name, if given, first (`Scalp 20/40 t`).
pub fn label(p: BracketPreset, product: &Product, units: BracketUnits) -> String {
    format!(
        "{}/{} {}",
        amount_text(p.stop_ticks, product, units),
        amount_text(p.target_ticks, product, units),
        units.suffix()
    )
}

/// Why typed text isn't a distance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AmountError {
    /// Not a number (or empty).
    NotANumber,
    /// Zero, negative or above [`MAX_TICKS`].
    OutOfRange,
    /// Between ticks: the nearest valid value, in the same units (never applied silently).
    OffGrid { nearest: String },
}

impl AmountError {
    pub fn message(&self, units: BracketUnits) -> String {
        match self {
            AmountError::NotANumber => format!("type a number of {}", units.word()),
            AmountError::OutOfRange => format!("1 to {MAX_TICKS} ticks"),
            AmountError::OffGrid { nearest } => match units {
                BracketUnits::Ticks => format!("whole ticks only: nearest {nearest}"),
                BracketUnits::Points => format!("not on the tick grid: nearest {nearest}"),
            },
        }
    }
}

/// Parse a decimal (`5`, `5.25`, `.5`) to an exact fraction (n, d), non-negative.
fn parse_decimal(s: &str) -> Option<(i128, i128)> {
    let s = s.trim();
    if s.is_empty() || s.len() > 24 {
        return None;
    }
    let (int, frac) = s.split_once('.').unwrap_or((s, ""));
    if int.is_empty() && frac.is_empty() {
        return None;
    }
    if !int.bytes().all(|b| b.is_ascii_digit()) || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let digits = format!("{int}{frac}");
    let n: i128 = digits.trim_start_matches('0').parse().unwrap_or(0);
    Some((n, 10i128.pow(frac.len() as u32)))
}

/// Typed text in `units` to whole ticks, exactly. Off-grid input is refused with the nearest
/// valid value (in `units`), never rounded.
pub fn parse_amount(
    text: &str,
    product: &Product,
    units: BracketUnits,
) -> Result<u32, AmountError> {
    let (n, d) = parse_decimal(text).ok_or(AmountError::NotANumber)?;
    // ticks = value / tick_points = (n/d) / (tn/td) = n·td / (d·tn)
    let (tn, td) = match units {
        BracketUnits::Ticks => (1, 1),
        BracketUnits::Points => tick_points(product),
    };
    let (num, den) = (n * td, d * tn);
    let nearest = (num + den / 2) / den;
    let in_range = |t: i128| t >= 1 && t <= i128::from(MAX_TICKS);
    if num % den != 0 {
        if !in_range(nearest.max(1)) {
            return Err(AmountError::OutOfRange);
        }
        return Err(AmountError::OffGrid {
            nearest: amount_text(nearest.max(1) as u32, product, units),
        });
    }
    let t = num / den;
    if !in_range(t) {
        return Err(AmountError::OutOfRange);
    }
    Ok(t as u32)
}

// ---- the inline editor ----------------------------------------------------------------------

/// Which cell of a preset row has the cursor.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Field {
    Stop,
    Target,
    Name,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditRow {
    pub stop: String,
    pub target: String,
    pub name: String,
}

/// The panel's inline preset editor: typed text per cell, in the account's units, validated on
/// every key and on save. Pure: keys in, presets out.
#[derive(Clone, Debug, PartialEq)]
pub struct Editor {
    pub product: Product,
    pub units: BracketUnits,
    pub rows: Vec<EditRow>,
    pub row: usize,
    pub field: Field,
}

/// What a key did.
#[derive(Clone, Debug, PartialEq)]
pub enum EditorKey {
    Typing,
    /// Every cell valid: the presets to save (in ticks), in order.
    Save(Vec<NamedBracket>),
    /// Enter with an invalid cell: stays open, says why.
    Refused(String),
    Cancel,
    /// Not the editor's key.
    Ignored,
}

/// Most presets the editor holds.
pub const MAX_PRESETS: usize = 12;

impl Editor {
    pub fn new(product: &Product, units: BracketUnits, presets: &[NamedBracket]) -> Editor {
        Editor {
            product: product.clone(),
            units,
            rows: presets
                .iter()
                .map(|p| EditRow {
                    stop: amount_text(p.stop_ticks, product, units),
                    target: amount_text(p.target_ticks, product, units),
                    name: p.name.clone(),
                })
                .collect(),
            row: 0,
            field: Field::Stop,
        }
    }

    /// Show the rows in the other unit (valid cells convert exactly; invalid ones stay).
    /// Why the units can't switch now: a stop or target cell that doesn't read in the
    /// current units would be reinterpreted in the new ones (never silently).
    pub fn units_blocked(&self) -> Option<String> {
        self.rows.iter().enumerate().find_map(|(i, r)| {
            [(&r.stop, "stop"), (&r.target, "target")]
                .into_iter()
                .find(|(c, _)| parse_amount(c, &self.product, self.units).is_err())
                .map(|(_, f)| format!("fix row {}'s {f} before switching units", i + 1))
        })
    }

    /// Switch the units, converting every cell exactly. Callers check [`Self::units_blocked`]
    /// first; a cell that doesn't read is left as typed.
    pub fn set_units(&mut self, units: BracketUnits) {
        if units == self.units {
            return;
        }
        for r in &mut self.rows {
            for cell in [&mut r.stop, &mut r.target] {
                if let Ok(t) = parse_amount(cell, &self.product, self.units) {
                    *cell = amount_text(t, &self.product, units);
                }
            }
        }
        self.units = units;
    }

    fn cell(&mut self) -> Option<&mut String> {
        let f = self.field;
        self.rows.get_mut(self.row).map(|r| match f {
            Field::Stop => &mut r.stop,
            Field::Target => &mut r.target,
            Field::Name => &mut r.name,
        })
    }

    /// The first invalid cell's problem, if any: (row, field, message).
    pub fn problem(&self) -> Option<(usize, Field, String)> {
        for (i, r) in self.rows.iter().enumerate() {
            for (f, text) in [(Field::Stop, &r.stop), (Field::Target, &r.target)] {
                if let Err(e) = parse_amount(text, &self.product, self.units) {
                    return Some((i, f, e.message(self.units)));
                }
            }
        }
        None
    }

    /// The cell's problem, if it has one (shown beside it while typing).
    pub fn cell_problem(&self, row: usize, f: Field) -> Option<String> {
        let r = self.rows.get(row)?;
        let text = match f {
            Field::Stop => &r.stop,
            Field::Target => &r.target,
            Field::Name => return None,
        };
        parse_amount(text, &self.product, self.units)
            .err()
            .map(|e| e.message(self.units))
    }

    pub fn add(&mut self) {
        if self.rows.len() >= MAX_PRESETS {
            return;
        }
        let (s, t) = DEFAULT_PRESETS[0];
        let at = (self.row + 1).min(self.rows.len());
        self.rows.insert(
            at,
            EditRow {
                stop: amount_text(s, &self.product, self.units),
                target: amount_text(t, &self.product, self.units),
                name: String::new(),
            },
        );
        self.row = at;
        self.field = Field::Stop;
    }

    pub fn delete(&mut self) {
        if self.row < self.rows.len() {
            self.rows.remove(self.row);
            self.row = self.row.min(self.rows.len().saturating_sub(1));
        }
    }

    /// Move the selected preset up (`-1`) or down (`1`).
    pub fn reorder(&mut self, by: i32) {
        let to = self.row as i32 + by;
        if to >= 0 && (to as usize) < self.rows.len() {
            self.rows.swap(self.row, to as usize);
            self.row = to as usize;
        }
    }

    /// Validate and return the presets in ticks.
    pub fn save(&self) -> Result<Vec<NamedBracket>, String> {
        if let Some((i, f, m)) = self.problem() {
            let which = if f == Field::Stop { "stop" } else { "target" };
            return Err(format!("preset {}: {which}: {m}", i + 1));
        }
        Ok(self
            .rows
            .iter()
            .map(|r| NamedBracket {
                name: r.name.trim().to_owned(),
                stop_ticks: parse_amount(&r.stop, &self.product, self.units).unwrap_or(1),
                target_ticks: parse_amount(&r.target, &self.product, self.units).unwrap_or(1),
            })
            .collect())
    }

    /// A key: `key` as GPUI names it, `typed` the character it types, `ctrl` / `shift` held.
    /// Up/Down move between presets (Ctrl: reorder), Tab / Shift+Tab between cells, Insert
    /// adds, Delete removes, Enter saves, Esc cancels.
    pub fn key(&mut self, key: &str, typed: Option<&str>, ctrl: bool, shift: bool) -> EditorKey {
        match key {
            "enter" => match self.save() {
                Ok(v) => EditorKey::Save(v),
                Err(m) => EditorKey::Refused(m),
            },
            "escape" => EditorKey::Cancel,
            "up" if ctrl => {
                self.reorder(-1);
                EditorKey::Typing
            }
            "down" if ctrl => {
                self.reorder(1);
                EditorKey::Typing
            }
            "up" => {
                self.row = self.row.saturating_sub(1);
                EditorKey::Typing
            }
            "down" => {
                self.row = (self.row + 1).min(self.rows.len().saturating_sub(1));
                EditorKey::Typing
            }
            "tab" => {
                self.field = match (self.field, shift) {
                    (Field::Stop, false) => Field::Target,
                    (Field::Target, false) => Field::Name,
                    (Field::Name, false) => Field::Stop,
                    (Field::Stop, true) => Field::Name,
                    (Field::Target, true) => Field::Stop,
                    (Field::Name, true) => Field::Target,
                };
                EditorKey::Typing
            }
            "insert" => {
                self.add();
                EditorKey::Typing
            }
            "delete" => {
                self.delete();
                EditorKey::Typing
            }
            "backspace" => {
                if let Some(c) = self.cell() {
                    c.pop();
                }
                EditorKey::Typing
            }
            _ => {
                let Some(t) = typed.filter(|t| !t.chars().any(char::is_control)) else {
                    return EditorKey::Ignored;
                };
                let numeric = self.field != Field::Name;
                if numeric && !t.chars().all(|c| c.is_ascii_digit() || c == '.') {
                    return EditorKey::Ignored;
                }
                let limit = if numeric { 12 } else { 24 };
                if let Some(c) = self.cell()
                    && c.chars().count() < limit
                {
                    c.push_str(t);
                }
                EditorKey::Typing
            }
        }
    }

    /// Live preview of the selected preset from `entry` (a long entry at the ask, or the
    /// position's side and average): `S 5807.75 · T 5822.75`. `None` when a cell is invalid
    /// or there is no price.
    pub fn preview(&self, entry: Option<(Price, bool)>) -> Option<String> {
        let (p, long) = entry?;
        let r = self.rows.get(self.row)?;
        let s = i64::from(parse_amount(&r.stop, &self.product, self.units).ok()?);
        let t = i64::from(parse_amount(&r.target, &self.product, self.units).ok()?);
        let tick = self.product.price.tick;
        let dir = if long { 1 } else { -1 };
        let fmt = self.product.price;
        Some(format!(
            "S {} · T {}",
            fmt.format::<32>(Price(p.0 - dir * s * tick)).as_str(),
            fmt.format::<32>(Price(p.0 + dir * t * tick)).as_str()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lines_feed::catalog;

    fn p(id: lines_core::ProductId) -> Product {
        catalog::get(id).unwrap()
    }

    /// Ticks ↔ points round-trip exactly for every catalog product, and points are printed
    /// with every decimal the tick needs.
    #[test]
    fn points_and_ticks_convert_exactly() {
        let cases: [(lines_core::ProductId, u32, &str); 10] = [
            (catalog::ES, 20, "5"),
            (catalog::ES, 21, "5.25"),
            (catalog::MES, 3, "0.75"),
            (catalog::NQ, 40, "10"),
            (catalog::MNQ, 1, "0.25"),
            (catalog::ZN, 1, "0.015625"),
            (catalog::ZN, 64, "1"),
            (catalog::BTC_USD, 150, "1.5"),
            (catalog::ETH_USD_KR, 7, "0.07"),
            // Kraken's BTC perpetual ticks in whole dollars.
            (catalog::BTC_PERP_KR, 12, "12"),
        ];
        for (id, ticks, points) in cases {
            let prod = p(id);
            let tick_pts = amount_text(1, &prod, BracketUnits::Points);
            assert_eq!(
                amount_text(ticks, &prod, BracketUnits::Points),
                points,
                "{} {ticks} t (tick = {tick_pts} pt)",
                prod.symbol
            );
            assert_eq!(
                parse_amount(points, &prod, BracketUnits::Points),
                Ok(ticks),
                "{} {points} pt",
                prod.symbol
            );
            assert_eq!(
                parse_amount(&ticks.to_string(), &prod, BracketUnits::Ticks),
                Ok(ticks)
            );
        }
        // Every product, a range of distances: points text parses back to the same ticks.
        for id in [
            catalog::ES,
            catalog::NQ,
            catalog::MES,
            catalog::MNQ,
            catalog::ZN,
            catalog::BTC_USD,
            catalog::ETH_USD,
            catalog::BTC_USD_KR,
            catalog::ETH_USD_KR,
            catalog::BTC_PERP_KR,
            catalog::ETH_PERP_KR,
        ] {
            let prod = p(id);
            for t in 1..=400u32 {
                let s = amount_text(t, &prod, BracketUnits::Points);
                assert_eq!(
                    parse_amount(&s, &prod, BracketUnits::Points),
                    Ok(t),
                    "{} {s}",
                    prod.symbol
                );
            }
        }
    }

    /// Off-grid input is refused with the nearest valid value, never rounded.
    #[test]
    fn off_grid_input_is_refused_with_the_nearest_value() {
        let es = p(catalog::ES);
        assert_eq!(
            parse_amount("5.1", &es, BracketUnits::Points),
            Err(AmountError::OffGrid {
                nearest: "5".into()
            })
        );
        assert_eq!(
            parse_amount("5.2", &es, BracketUnits::Points),
            Err(AmountError::OffGrid {
                nearest: "5.25".into()
            })
        );
        let zn = p(catalog::ZN);
        assert_eq!(
            parse_amount("0.01", &zn, BracketUnits::Points),
            Err(AmountError::OffGrid {
                nearest: "0.015625".into()
            })
        );
        assert_eq!(
            parse_amount("2.5", &es, BracketUnits::Ticks),
            Err(AmountError::OffGrid {
                nearest: "3".into()
            })
        );
        assert_eq!(
            parse_amount("0", &es, BracketUnits::Ticks),
            Err(AmountError::OutOfRange)
        );
        assert_eq!(
            parse_amount("", &es, BracketUnits::Ticks),
            Err(AmountError::NotANumber)
        );
        assert_eq!(
            parse_amount("-4", &es, BracketUnits::Ticks),
            Err(AmountError::NotANumber)
        );
        assert_eq!(
            parse_amount("4x", &es, BracketUnits::Points),
            Err(AmountError::NotANumber)
        );
        assert_eq!(
            AmountError::OffGrid {
                nearest: "5.25".into()
            }
            .message(BracketUnits::Points),
            "not on the tick grid: nearest 5.25"
        );
    }

    #[test]
    fn labels_presets_and_fallback() {
        let es = p(catalog::ES);
        let b = BracketPreset {
            stop_ticks: 20,
            target_ticks: 40,
        };
        assert_eq!(label(b, &es, BracketUnits::Ticks), "20/40 t");
        assert_eq!(label(b, &es, BracketUnits::Points), "5/10 pt");
        let mut s = BracketSettings::new();
        // No presets for the account or product: the defaults.
        assert_eq!(presets_for(&s, "paper", "ES").len(), DEFAULT_PRESETS.len());
        s.entry("paper".into()).or_default().products.insert(
            "NQ".into(),
            vec![NamedBracket {
                name: "scalp".into(),
                stop_ticks: 16,
                target_ticks: 32,
            }],
        );
        assert_eq!(presets_for(&s, "paper", "NQ")[0].stop_ticks, 16);
        assert_eq!(
            presets_for(&s, "paper", "ES")[0].stop_ticks,
            8,
            "per product"
        );
        assert_eq!(units_for(&s, "paper"), BracketUnits::Ticks);
        // Deleting them all sticks: no defaults come back.
        s.entry("paper".into())
            .or_default()
            .products
            .insert("MES".into(), Vec::new());
        assert!(presets_for(&s, "paper", "MES").is_empty());
        // Off first, then each preset, then back to Off.
        let nq = presets_for(&s, "paper", "NQ");
        let first = next_choice(&nq, None);
        assert_eq!(first, Some(nq[0].preset()));
        assert_eq!(next_choice(&nq, first), None);
    }

    #[test]
    fn saved_form_round_trips_and_omits_defaults() {
        let mut s = BracketSettings::new();
        assert_eq!(serde_json::to_string(&s).unwrap(), "{}");
        let a = s.entry("paper".into()).or_default();
        assert_eq!(
            serde_json::to_string(a).unwrap(),
            "{}",
            "defaults write nothing"
        );
        a.units = BracketUnits::Points;
        a.products.insert(
            "ES".into(),
            vec![
                NamedBracket {
                    name: String::new(),
                    stop_ticks: 21,
                    target_ticks: 42,
                },
                NamedBracket {
                    name: "runner".into(),
                    stop_ticks: 16,
                    target_ticks: 80,
                },
            ],
        );
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(
            json,
            r#"{"paper":{"units":"points","products":{"ES":[{"stop_ticks":21,"target_ticks":42},{"name":"runner","stop_ticks":16,"target_ticks":80}]}}}"#
        );
        let back: BracketSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn editor_edits_reorders_and_refuses_off_grid() {
        let es = p(catalog::ES);
        let presets = presets_for(&BracketSettings::new(), "paper", "ES");
        let mut e = Editor::new(&es, BracketUnits::Points, &presets);
        assert_eq!(
            (e.rows[0].stop.as_str(), e.rows[0].target.as_str()),
            ("2", "4")
        );
        // Type 5.1 into the stop: refused on Enter, nearest shown.
        e.key("backspace", None, false, false);
        for c in ["5", ".", "1"] {
            e.key(c, Some(c), false, false);
        }
        assert_eq!(
            e.cell_problem(0, Field::Stop).as_deref(),
            Some("not on the tick grid: nearest 5")
        );
        assert!(
            matches!(e.key("enter", None, false, false), EditorKey::Refused(m) if m.contains("nearest 5"))
        );
        // The units can't switch under an off-grid cell (it would read differently).
        assert_eq!(
            e.units_blocked().as_deref(),
            Some("fix row 1's stop before switching units")
        );
        e.key("backspace", None, false, false);
        e.key("2", Some("2"), false, false);
        e.key("5", Some("5"), false, false);
        assert_eq!(e.rows[0].stop, "5.25");
        assert_eq!(e.units_blocked(), None);
        // Letters never go into a number; the name takes them.
        assert_eq!(e.key("x", Some("x"), false, false), EditorKey::Ignored);
        e.key("tab", None, false, false);
        e.key("tab", None, false, false);
        e.key("A", Some("A"), false, false);
        // Add after it, reorder it down, delete one.
        e.key("insert", None, false, false);
        assert_eq!(e.rows.len(), 4);
        assert_eq!(e.row, 1);
        e.key("up", None, true, false);
        assert_eq!(e.row, 0);
        assert_eq!(e.rows[1].name, "A", "the edited preset moved down");
        e.key("down", None, false, false);
        e.key("delete", None, false, false);
        assert_eq!(e.rows.len(), 3);
        // Switch the units: valid cells convert exactly.
        e.set_units(BracketUnits::Ticks);
        assert_eq!(e.rows[0].stop, "8");
        let EditorKey::Save(saved) = e.key("enter", None, false, false) else {
            panic!("saves")
        };
        assert_eq!(saved.len(), 3);
        assert!(saved.iter().all(|s| s.stop_ticks > 0 && s.target_ticks > 0));
        assert_eq!(
            Editor::new(&es, BracketUnits::Ticks, &presets).key("escape", None, false, false),
            EditorKey::Cancel
        );
    }

    #[test]
    fn preview_from_the_entry_price() {
        let es = p(catalog::ES);
        let presets = vec![NamedBracket {
            name: String::new(),
            stop_ticks: 20,
            target_ticks: 40,
        }];
        let e = Editor::new(&es, BracketUnits::Points, &presets);
        assert_eq!(
            e.preview(Some((Price(581_275), true))).as_deref(),
            Some("S 5807.75 · T 5822.75")
        );
        assert_eq!(
            e.preview(Some((Price(581_250), false))).as_deref(),
            Some("S 5817.50 · T 5802.50")
        );
        assert_eq!(e.preview(None), None);
    }
}
