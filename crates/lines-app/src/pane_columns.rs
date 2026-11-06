//! The DOM's and time & sales' columns: which there are, their sizes, what is saved, and the
//! per-pane state the views use (layout math: `lines_core::columns`, pure and allocation-free).
//!
//! Each pane paints a slim header row with the column names. Its dividers can be dragged (the
//! pointer shows a resize cursor over them) and double-clicked to fit the column to its widest
//! current content; right-clicking the header row shows and hides columns and resets them.
//! Every one of those is a [`ColumnCmd`] (decision 0018), which the palette also issues on the
//! focused pane. A column is never narrower than what it currently shows: numbers are never
//! truncated (docs/design.md §4).
//!
//! Saved per pane as an optional `columns` map (only what differs from the defaults), so
//! workspace files without it load with defaults and need no version bump.

use std::collections::BTreeMap;

use gpui::{
    App, Bounds, CursorStyle, Hitbox, HitboxBehavior, Pixels, Point, Window, fill, point, px, size,
};
use lines_core::columns::{Cell, ColumnSet, ColumnSpec, MAX_COLS};
use serde::{Deserialize, Serialize};

use crate::text::Glyphs;
use crate::theme::Theme;

/// Height of the column header row.
pub const HEADER_ROW_H: f32 = 18.0;
/// How close (px) to a divider the pointer must be to grab it.
pub const GRAB: f32 = 4.0;

const fn spec(min_w: f32, default_w: f32, hideable: bool, shown: bool) -> ColumnSpec {
    ColumnSpec {
        min_w,
        default_w,
        hideable,
        shown_by_default: shown,
    }
}

/// DOM columns, left to right.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DomCol {
    /// The trader's working orders (milestone 7): hidden until trading exists.
    Orders,
    Bid,
    Price,
    Ask,
    /// Traded volume at price.
    Volume,
}

impl DomCol {
    pub const ALL: [DomCol; 5] = [
        DomCol::Orders,
        DomCol::Bid,
        DomCol::Price,
        DomCol::Ask,
        DomCol::Volume,
    ];

    /// The header row's text (the orders column is still empty: no title).
    pub fn header(self) -> &'static str {
        match self {
            DomCol::Orders => "",
            DomCol::Volume => "Vol",
            c => c.label(),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            DomCol::Orders => "Orders",
            DomCol::Bid => "Bid",
            DomCol::Price => "Price",
            DomCol::Ask => "Ask",
            DomCol::Volume => "Volume",
        }
    }

    pub fn index(self) -> usize {
        self as usize
    }
}

pub const DOM_SPECS: [ColumnSpec; 5] = [
    spec(16.0, 22.0, true, false),
    spec(36.0, 64.0, true, true),
    spec(48.0, 76.0, false, true),
    spec(36.0, 64.0, true, true),
    spec(36.0, 64.0, true, true),
];
/// The volume column takes the DOM's leftover width.
pub const DOM_FILL: usize = 4;

/// Time & sales columns, left to right.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TapeCol {
    Time,
    Price,
    Size,
    /// The aggressor marker (▲ ▼ ·).
    Side,
}

impl TapeCol {
    pub const ALL: [TapeCol; 4] = [TapeCol::Time, TapeCol::Price, TapeCol::Size, TapeCol::Side];

    /// The header row's text (the side marker needs none).
    pub fn header(self) -> &'static str {
        match self {
            TapeCol::Side => "",
            c => c.label(),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            TapeCol::Time => "Time",
            TapeCol::Price => "Price",
            TapeCol::Size => "Size",
            TapeCol::Side => "Side",
        }
    }

    pub fn index(self) -> usize {
        self as usize
    }
}

pub const TAPE_SPECS: [ColumnSpec; 4] = [
    spec(48.0, 96.0, true, true),
    spec(48.0, 76.0, false, true),
    spec(28.0, 52.0, true, true),
    spec(16.0, 20.0, true, true),
];
/// The time column takes the tape's leftover width (prices and sizes stay together).
pub const TAPE_FILL: usize = 0;

/// A column of either table, for commands.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Column {
    Dom(DomCol),
    Tape(TapeCol),
}

impl Column {
    pub fn label(self) -> String {
        match self {
            Column::Dom(c) => format!("DOM column: {}", c.label()),
            Column::Tape(c) => format!("Time & sales column: {}", c.label()),
        }
    }
}

/// A column action on the focused pane's table (decision 0018).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum ColumnCmd {
    /// Show or hide a column (the price column always stays).
    Toggle(Column),
    /// Fit a column to its widest current content.
    AutoFit(Column),
    /// Fit every shown column.
    AutoFitAll,
    /// Back to the defaults.
    Reset,
    /// Set a column's width in logical px (what dragging a divider issues).
    Width(Column, u16),
}

/// A pane asks for its column menu at this window position (right-click on its header
/// row). The workspace shows it, window-wide: a narrow pane would clip a menu of its own.
#[derive(Clone, Copy, Debug)]
pub struct ColumnMenu(pub Point<Pixels>);

/// What is saved for one column (only what differs from the default).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ColumnDoc {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shown: Option<bool>,
}

/// Which table a pane has.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Table {
    Dom,
    Tape,
}

/// A pane's columns: the trader's choices, this frame's layout, and what the content needs.
#[derive(Clone, Debug)]
pub struct Columns {
    table: Table,
    pub set: ColumnSet,
    /// Where each column was placed in the last frame (x relative to the table's left).
    pub cells: [Cell; MAX_COLS],
    /// Each column's widest current content plus padding, measured in the last frame.
    pub needed: [f32; MAX_COLS],
    /// A divider being dragged.
    pub drag: Option<Drag>,
}

/// A divider drag, kept absolute (start width + pointer offset) so moves that arrive between
/// frames never compound against a stale layout.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Drag {
    col: usize,
    grows: bool,
    start_x: f32,
    start_w: f32,
}

/// What a press on the column header row means.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum HeaderHit {
    /// Not on the header row (the pane handles it).
    Outside,
    /// On the row, not on a divider.
    Row,
    /// Grab divider `d`.
    Divider(usize),
    /// Double-click on a divider: fit this column.
    AutoFit(Column),
}

impl Columns {
    pub fn new(table: Table) -> Columns {
        Columns {
            table,
            set: match table {
                Table::Dom => ColumnSet::new(&DOM_SPECS, DOM_FILL),
                Table::Tape => ColumnSet::new(&TAPE_SPECS, TAPE_FILL),
            },
            cells: [Cell::default(); MAX_COLS],
            needed: [0.0; MAX_COLS],
            drag: None,
        }
    }

    pub fn specs(&self) -> &'static [ColumnSpec] {
        match self.table {
            Table::Dom => &DOM_SPECS,
            Table::Tape => &TAPE_SPECS,
        }
    }

    /// The column's index here, if it belongs to this table.
    fn index(&self, c: Column) -> Option<usize> {
        match (self.table, c) {
            (Table::Dom, Column::Dom(d)) => Some(d.index()),
            (Table::Tape, Column::Tape(t)) => Some(t.index()),
            _ => None,
        }
    }

    /// The column at index `i` of this table.
    pub fn column(&self, i: usize) -> Option<Column> {
        match self.table {
            Table::Dom => DomCol::ALL.get(i).map(|c| Column::Dom(*c)),
            Table::Tape => TapeCol::ALL.get(i).map(|c| Column::Tape(*c)),
        }
    }

    pub fn label(&self, i: usize) -> &'static str {
        match self.column(i) {
            Some(Column::Dom(c)) => c.label(),
            Some(Column::Tape(c)) => c.label(),
            None => "",
        }
    }

    /// Lay the columns out in a table `total` px wide (plain arithmetic, no allocation).
    pub fn layout(&mut self, total: f32) {
        let specs = self.specs();
        self.set.layout(specs, &self.needed, total, &mut self.cells);
    }

    /// Run a column command. `Err` says why not (inline).
    pub fn apply(&mut self, cmd: ColumnCmd) -> Result<(), &'static str> {
        let specs = self.specs();
        let col = |c: Column| {
            self.index(c).ok_or(match self.table {
                Table::Dom => "that column belongs to time & sales",
                Table::Tape => "that column belongs to the DOM",
            })
        };
        match cmd {
            ColumnCmd::Toggle(c) => {
                let i = col(c)?;
                let shown = !self.set.is_shown(i);
                if !self.set.set_shown(specs, i, shown) {
                    return Err(if specs[i].hideable {
                        "the last column stays"
                    } else {
                        "the price column is always shown"
                    });
                }
            }
            ColumnCmd::AutoFit(c) => {
                let i = col(c)?;
                self.set.auto_fit(specs, i, &self.needed);
            }
            ColumnCmd::AutoFitAll => {
                let fill = self.set.fill_index();
                for i in 0..self.set.len() {
                    if self.set.is_shown(i) && Some(i) != fill {
                        self.set.auto_fit(specs, i, &self.needed);
                    }
                }
            }
            ColumnCmd::Reset => self.set.reset(specs),
            ColumnCmd::Width(c, w) => {
                let i = col(c)?;
                self.set.set_width(specs, i, f32::from(w), &self.needed);
            }
        }
        Ok(())
    }

    /// The divider under table x `x`, if any.
    pub fn divider_at(&self, x: f32) -> Option<usize> {
        self.set.divider_at(&self.cells, x, GRAB)
    }

    /// The column a divider resizes (for double-click auto-fit).
    pub fn divider_column(&self, d: usize) -> Option<Column> {
        self.set.divider_column(d).and_then(|(i, _)| self.column(i))
    }

    /// What a press at table coordinates `x, y` (y from the header row's top) means.
    pub fn hit(&self, x: f32, y: f32, clicks: usize) -> HeaderHit {
        if !(0.0..HEADER_ROW_H).contains(&y) {
            return HeaderHit::Outside;
        }
        match self.divider_at(x) {
            Some(d) if clicks >= 2 => self
                .divider_column(d)
                .map_or(HeaderHit::Row, HeaderHit::AutoFit),
            Some(d) => HeaderHit::Divider(d),
            None => HeaderHit::Row,
        }
    }

    /// Grab divider `d` with the pointer at table x `x`.
    pub fn start_drag(&mut self, d: usize, x: f32) {
        self.drag = self.set.divider_column(d).map(|(col, grows)| Drag {
            col,
            grows,
            start_x: x,
            start_w: self.cells[col].w,
        });
    }

    /// The pointer moved to table x `x` during a drag: the width command to issue.
    pub fn drag_to(&self, x: f32) -> Option<ColumnCmd> {
        let d = self.drag?;
        let dx = x - d.start_x;
        let w = d.start_w + if d.grows { dx } else { -dx };
        Some(ColumnCmd::Width(
            self.column(d.col)?,
            w.round().clamp(0.0, f32::from(u16::MAX)) as u16,
        ))
    }

    /// Measure what each column needs this frame: its header text and widest content (from
    /// `content`, already measured, in px), plus padding on both sides.
    pub fn measure_headers(
        &mut self,
        glyphs: &mut Glyphs,
        t: &Theme,
        pad: f32,
        window: &mut Window,
    ) {
        for i in 0..self.set.len() {
            let label = self.header(i);
            self.needed[i] = glyphs.width(label, t.text_dim, false, window) + 2.0 * pad;
        }
    }

    /// Widen column `i`'s need to hold text `w` px wide (plus padding).
    pub fn need(&mut self, i: usize, w: f32, pad: f32) {
        let n = w + 2.0 * pad;
        if n > self.needed[i] {
            self.needed[i] = n;
        }
    }

    fn header(&self, i: usize) -> &'static str {
        match self.column(i) {
            Some(Column::Dom(c)) => c.header(),
            Some(Column::Tape(c)) => c.header(),
            None => "",
        }
    }

    /// A shown column's cell in window coordinates, given the table's left edge.
    pub fn cell(&self, i: usize, ox: f32) -> Option<(f32, f32)> {
        let c = self.cells[i];
        c.shown.then_some((ox + c.x, c.w))
    }

    /// Paint the header row: column titles in the dim text colour (right-aligned where the
    /// column's numbers are) and a rule under it. No allocation.
    #[allow(clippy::too_many_arguments)]
    pub fn paint_header(
        &self,
        glyphs: &mut Glyphs,
        t: &Theme,
        bounds: Bounds<Pixels>,
        pad: f32,
        left_aligned: &[bool],
        window: &mut Window,
        cx: &mut App,
    ) {
        let (ox, oy) = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
        let w = f32::from(bounds.size.width);
        let hair = 1.0 / window.scale_factor();
        window.paint_quad(fill(
            Bounds::new(point(px(ox), px(oy)), size(px(w), px(HEADER_ROW_H))),
            t.panel,
        ));
        for i in 0..self.set.len() {
            let Some((x, cw)) = self.cell(i, ox) else {
                continue;
            };
            let label = self.header(i);
            if left_aligned.get(i).copied().unwrap_or(false) {
                glyphs.paint(
                    label,
                    x + pad,
                    oy,
                    HEADER_ROW_H,
                    t.text_dim,
                    false,
                    window,
                    cx,
                );
            } else {
                glyphs.paint_right(
                    label,
                    x + cw - pad,
                    oy,
                    HEADER_ROW_H,
                    t.text_dim,
                    false,
                    window,
                    cx,
                );
            }
        }
        window.paint_quad(fill(
            Bounds::new(
                point(px(ox), px(oy + HEADER_ROW_H - hair)),
                size(px(w), px(hair)),
            ),
            t.axis_line,
        ));
    }

    /// Vertical rules between shown columns, over the whole table.
    pub fn paint_rules(&self, t: &Theme, bounds: Bounds<Pixels>, window: &mut Window) {
        let (ox, oy) = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
        let h = f32::from(bounds.size.height);
        let hair = 1.0 / window.scale_factor();
        let last = (0..self.set.len()).rev().find(|&i| self.set.is_shown(i));
        for i in 0..self.set.len() {
            if Some(i) == last {
                continue;
            }
            if let Some((x, cw)) = self.cell(i, ox) {
                window.paint_quad(fill(
                    Bounds::new(point(px(x + cw), px(oy)), size(px(hair), px(h))),
                    t.grid,
                ));
            }
        }
    }

    /// Hitboxes over the dividers (canvas prepaint), so the pointer shows a resize cursor;
    /// while a drag is on, the whole table shows it.
    pub fn divider_hitboxes(
        &self,
        bounds: Bounds<Pixels>,
        window: &mut Window,
    ) -> [Option<Hitbox>; MAX_COLS] {
        let mut out: [Option<Hitbox>; MAX_COLS] = Default::default();
        let (ox, oy) = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
        if self.drag.is_some() {
            out[0] = Some(window.insert_hitbox(bounds, HitboxBehavior::Normal));
            return out;
        }
        let last = (0..self.set.len()).rev().find(|&i| self.set.is_shown(i));
        for (i, slot) in out.iter_mut().enumerate().take(self.set.len()) {
            if Some(i) == last {
                continue;
            }
            if let Some((x, cw)) = self.cell(i, ox) {
                let b = Bounds::new(
                    point(px(x + cw - GRAB), px(oy)),
                    size(px(2.0 * GRAB), px(HEADER_ROW_H)),
                );
                *slot = Some(window.insert_hitbox(b, HitboxBehavior::Normal));
            }
        }
        out
    }
}

/// Paint: the resize cursor over the hitboxes from [`Columns::divider_hitboxes`].
pub fn set_divider_cursors(hitboxes: &[Option<Hitbox>; MAX_COLS], window: &mut Window) {
    for h in hitboxes.iter().flatten() {
        window.set_cursor_style(CursorStyle::ResizeLeftRight, h);
    }
}

impl Columns {
    /// Only what differs from the defaults, keyed by column.
    pub fn save<C: Copy + Ord>(&self, all: &[C]) -> BTreeMap<C, ColumnDoc> {
        let specs = self.specs();
        all.iter()
            .enumerate()
            .filter_map(|(i, c)| {
                let d = ColumnDoc {
                    width: self.set.width(i),
                    shown: (self.set.is_shown(i) != specs[i].shown_by_default)
                        .then_some(self.set.is_shown(i)),
                };
                (d != ColumnDoc::default()).then_some((*c, d))
            })
            .collect()
    }

    pub fn load<C: Copy + Ord>(&mut self, all: &[C], doc: &BTreeMap<C, ColumnDoc>) {
        let specs = self.specs();
        for (i, c) in all.iter().enumerate() {
            if let Some(d) = doc.get(c) {
                self.set.restore(specs, i, d.width, d.shown);
            }
        }
        // A file can't hide every column: keep the price column in that case.
        if (0..self.set.len()).all(|i| !self.set.is_shown(i)) {
            let price = specs.iter().position(|s| !s.hideable).unwrap_or(0);
            self.set.restore(specs, price, None, Some(true));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dom_defaults_hide_the_orders_column() {
        let c = Columns::new(Table::Dom);
        assert!(!c.set.is_shown(DomCol::Orders.index()));
        assert!(c.set.is_shown(DomCol::Price.index()));
        assert!(c.save(&DomCol::ALL).is_empty(), "defaults save nothing");
    }

    #[test]
    fn commands_hide_show_fit_and_reset() {
        let mut c = Columns::new(Table::Dom);
        assert_eq!(
            c.apply(ColumnCmd::Toggle(Column::Dom(DomCol::Price))),
            Err("the price column is always shown")
        );
        c.apply(ColumnCmd::Toggle(Column::Dom(DomCol::Volume)))
            .unwrap();
        c.apply(ColumnCmd::Toggle(Column::Dom(DomCol::Orders)))
            .unwrap();
        assert!(!c.set.is_shown(4) && c.set.is_shown(0));
        assert!(
            c.apply(ColumnCmd::Toggle(Column::Tape(TapeCol::Side)))
                .is_err()
        );
        c.needed[1] = 41.0;
        c.apply(ColumnCmd::AutoFit(Column::Dom(DomCol::Bid)))
            .unwrap();
        assert_eq!(c.set.width(1), Some(41.0));
        c.apply(ColumnCmd::Width(Column::Dom(DomCol::Ask), 5))
            .unwrap();
        assert_eq!(c.set.width(3), Some(36.0), "clamped to the minimum");
        c.apply(ColumnCmd::Reset).unwrap();
        assert!(c.save(&DomCol::ALL).is_empty());
    }

    #[test]
    fn dragging_issues_width_commands() {
        let mut c = Columns::new(Table::Tape);
        c.layout(300.0);
        // Time fills; its divider resizes the price column (dragging left widens it).
        let x = c.cells[0].right();
        let HeaderHit::Divider(d) = c.hit(x + 1.0, 5.0, 1) else {
            panic!("divider")
        };
        assert_eq!(c.divider_column(d), Some(Column::Tape(TapeCol::Price)));
        c.start_drag(d, x);
        // Several moves before the next frame: each is relative to where the drag began.
        for (to, want) in [(x - 10.0, 86), (x - 20.0, 96), (x - 20.0, 96)] {
            let cmd = c.drag_to(to).unwrap();
            assert_eq!(cmd, ColumnCmd::Width(Column::Tape(TapeCol::Price), want));
            c.apply(cmd).unwrap();
        }
        c.layout(300.0);
        assert_eq!(c.cells[1].w, 96.0);
        // Double-click the divider: fit the price column; off the row: not ours.
        assert_eq!(
            c.hit(x - 20.0, 5.0, 2),
            HeaderHit::AutoFit(Column::Tape(TapeCol::Price))
        );
        assert_eq!(c.hit(5.0, 5.0, 1), HeaderHit::Row);
        assert_eq!(c.hit(5.0, 40.0, 1), HeaderHit::Outside);
    }

    #[test]
    fn auto_fit_all_fits_every_column_but_the_filler() {
        let mut c = Columns::new(Table::Tape);
        c.needed = [70.0, 58.5, 30.0, 18.0, 0.0, 0.0, 0.0, 0.0];
        c.apply(ColumnCmd::AutoFitAll).unwrap();
        assert_eq!(c.set.width(0), None, "time fills: not fitted");
        assert_eq!(
            (c.set.width(1), c.set.width(2), c.set.width(3)),
            (Some(58.5), Some(30.0), Some(18.0))
        );
    }

    #[test]
    fn saved_form_round_trips_and_ignores_nonsense() {
        let mut c = Columns::new(Table::Dom);
        c.apply(ColumnCmd::Width(Column::Dom(DomCol::Bid), 90))
            .unwrap();
        c.apply(ColumnCmd::Toggle(Column::Dom(DomCol::Volume)))
            .unwrap();
        let saved = c.save(&DomCol::ALL);
        let json = serde_json::to_string(&saved).unwrap();
        assert_eq!(json, r#"{"bid":{"width":90.0},"volume":{"shown":false}}"#);
        let back: BTreeMap<DomCol, ColumnDoc> = serde_json::from_str(&json).unwrap();
        let mut d = Columns::new(Table::Dom);
        d.load(&DomCol::ALL, &back);
        assert_eq!(d.set, c.set);
        // Hand-edited: every column hidden, a negative width: the price column stays.
        let bad: BTreeMap<DomCol, ColumnDoc> = serde_json::from_str(
            r#"{"orders":{"shown":false},"bid":{"shown":false,"width":-4},"price":{"shown":false},"ask":{"shown":false},"volume":{"shown":false}}"#,
        )
        .unwrap();
        let mut e = Columns::new(Table::Dom);
        e.load(&DomCol::ALL, &bad);
        assert!(e.set.is_shown(DomCol::Price.index()));
        assert_eq!(e.set.width(1), None);
    }
}
