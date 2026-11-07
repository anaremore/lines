//! Layout lint (UI checks layer 1, `docs/design.md` "Checking a UI change"): pure rules over the
//! rectangles a frame drew, recorded by the app in screenshot builds (`lines-app/src/layout_rec.rs`).
//!
//! Rules (each a [`Rule`]):
//! - **Overlap**: no two text runs, chips or buttons in the same layer intersect (a control
//!   drawn over text or another control).
//! - **Clipped**: a text run lies inside its clip rectangle, or ends in "…".
//! - **HitTarget**: interactive elements (chips, buttons) are at least 20×20 logical px.
//! - **Spacing**: the gap between siblings (same group, same row) is on the spacing scale
//!   (2, 4, 8, 12, 16, 24 px).
//! - **Reserved**: nothing outside a reserved region (the DOM's price column, the chart's
//!   last-price tag) draws over it.
//! - **Popup**: a popup touches its trigger (opens directly under or over it).
//!
//! Rectangles are logical px, window coordinates. Allocation is fine here: this runs in tests
//! and the gallery, never in paint.

/// A rectangle, logical px: left, top, width, height.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect { x, y, w, h }
    }

    pub fn right(&self) -> f32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }

    /// The area both cover (0 when they only touch).
    pub fn overlap_area(&self, o: &Rect) -> f32 {
        let w = self.right().min(o.right()) - self.x.max(o.x);
        let h = self.bottom().min(o.bottom()) - self.y.max(o.y);
        if w > 0.0 && h > 0.0 { w * h } else { 0.0 }
    }

    /// Inside `o`, within `tol` px.
    pub fn within(&self, o: &Rect, tol: f32) -> bool {
        self.x >= o.x - tol
            && self.y >= o.y - tol
            && self.right() <= o.right() + tol
            && self.bottom() <= o.bottom() + tol
    }

    /// The part inside `o` (`None` when nothing is).
    pub fn intersect(&self, o: &Rect) -> Option<Rect> {
        let (x, y) = (self.x.max(o.x), self.y.max(o.y));
        let (r, b) = (self.right().min(o.right()), self.bottom().min(o.bottom()));
        (r > x && b > y).then(|| Rect::new(x, y, r - x, b - y))
    }

    /// The largest distance between the two along either axis (0 when they touch or
    /// overlap).
    pub fn gap(&self, o: &Rect) -> f32 {
        let dx = (o.x - self.right()).max(self.x - o.right()).max(0.0);
        let dy = (o.y - self.bottom()).max(self.y - o.bottom()).max(0.0);
        dx.max(dy)
    }
}

/// What a recorded rectangle is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A run of text.
    Text,
    /// A clickable chip (a header chip, a menu trigger).
    Chip,
    /// A clickable button.
    Button,
    /// The space hover controls draw in (a pane header's gutter).
    HoverSlot,
    /// A popup (menu, list) opened from the item labelled `trigger`.
    Popup { trigger: String },
    /// A region nothing else may draw over (`name`: e.g. `dom-price`, `last-price-tag`).
    Reserved { name: String },
}

impl Kind {
    pub fn interactive(&self) -> bool {
        matches!(self, Kind::Chip | Kind::Button)
    }
}

/// One recorded rectangle.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub kind: Kind,
    /// What it shows or does ("5m ▾", "6019.50", "pop out").
    pub label: String,
    pub rect: Rect,
    /// The clip in force when it was drawn (`None`: the window).
    pub clip: Option<Rect>,
    /// Drawing layer: items in different layers may overlap (a popup over the chart, a tag
    /// over its axis on purpose). 0 = the pane.
    pub layer: u8,
    /// Siblings whose gaps follow the spacing scale (e.g. `chart-header`); empty = none.
    pub group: String,
    /// The reserved region this item belongs to (its own text may sit in it).
    pub owner: String,
}

impl Item {
    /// What shows of it: its rect inside its clip (`None`: wholly clipped, not drawn).
    pub fn visible(&self) -> Option<Rect> {
        match self.clip {
            Some(c) => self.rect.intersect(&c),
            None => Some(self.rect),
        }
    }

    pub fn new(kind: Kind, label: &str, rect: Rect) -> Item {
        Item {
            kind,
            label: label.to_owned(),
            rect,
            clip: None,
            layer: 0,
            group: String::new(),
            owner: String::new(),
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Rule {
    Overlap,
    Clipped,
    HitTarget,
    Spacing,
    Reserved,
    Popup,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Violation {
    pub rule: Rule,
    /// The items involved, by label.
    pub what: String,
}

/// The spacing scale (logical px).
pub const SPACING: [f32; 6] = [2.0, 4.0, 8.0, 12.0, 16.0, 24.0];
/// Smallest interactive target (logical px each way).
pub const MIN_TARGET: f32 = 20.0;
/// Rounding slack (logical px): GPUI lays out on fractional pixels.
const TOL: f32 = 0.6;

fn show(r: &Rect) -> String {
    format!("{:.1},{:.1} {:.1}×{:.1}", r.x, r.y, r.w, r.h)
}

/// Every rule over one frame's items.
pub fn lint(items: &[Item]) -> Vec<Violation> {
    let mut out = Vec::new();
    let texts: Vec<&Item> = items.iter().filter(|i| i.kind == Kind::Text).collect();
    let solid: Vec<&Item> = items
        .iter()
        .filter(|i| i.kind == Kind::Text || i.kind.interactive())
        .collect();
    let v = |rule, what: String| Violation { rule, what };
    // Overlap: what shows of text and controls in the same layer.
    for (i, a) in solid.iter().enumerate() {
        for b in &solid[i + 1..] {
            let (Some(va), Some(vb)) = (a.visible(), b.visible()) else {
                continue;
            };
            if a.layer == b.layer && va.overlap_area(&vb) > TOL * TOL * 4.0 {
                out.push(v(Rule::Overlap, format!("{:?} × {:?}", a.label, b.label)));
            }
        }
    }
    // A control cut by its clip (partly shown: a half button).
    for i in items.iter().filter(|i| i.kind.interactive()) {
        if let Some(c) = i.clip
            && !i.rect.within(&c, TOL)
            && i.rect.overlap_area(&c) > TOL * TOL * 4.0
        {
            out.push(v(Rule::Clipped, format!("control {:?} is cut", i.label)));
        }
    }
    // Clipped text without an ellipsis.
    for t in &texts {
        if let Some(c) = t.clip
            && !t.rect.within(&c, TOL)
            && t.rect.overlap_area(&c) > 0.0
            && !t.label.trim_end().ends_with('…')
        {
            out.push(v(
                Rule::Clipped,
                format!("{:?} at {} in {}", t.label, show(&t.rect), show(&c)),
            ));
        }
    }
    // Hit targets.
    for i in items.iter().filter(|i| i.kind.interactive()) {
        if i.rect.w + TOL < MIN_TARGET || i.rect.h + TOL < MIN_TARGET {
            out.push(v(
                Rule::HitTarget,
                format!("{:?} is {:.0}×{:.0}", i.label, i.rect.w, i.rect.h),
            ));
        }
    }
    // Spacing between siblings: same group, same row, neighbours by x.
    let mut groups: Vec<&str> = items
        .iter()
        .filter(|i| !i.group.is_empty())
        .map(|i| i.group.as_str())
        .collect();
    groups.sort_unstable();
    groups.dedup();
    for g in groups {
        let mut row: Vec<&Item> = items.iter().filter(|i| i.group == g).collect();
        row.sort_by(|a, b| a.rect.x.total_cmp(&b.rect.x));
        for w in row.windows(2) {
            let (a, b) = (w[0], w[1]);
            let same_row = a.rect.y < b.rect.bottom() && b.rect.y < a.rect.bottom();
            if !same_row || b.rect.x < a.rect.right() - TOL {
                continue;
            }
            let gap = b.rect.x - a.rect.right();
            if !SPACING.iter().any(|s| (gap - s).abs() <= TOL) {
                out.push(v(
                    Rule::Spacing,
                    format!("{g}: {:?} → {:?} gap {gap:.1}", a.label, b.label),
                ));
            }
        }
    }
    // Reserved regions.
    for r in items.iter() {
        let Kind::Reserved { name } = &r.kind else {
            continue;
        };
        for i in items.iter() {
            if std::ptr::eq(i, r)
                || &i.owner == name
                || i.layer != r.layer
                || matches!(i.kind, Kind::Reserved { .. } | Kind::HoverSlot)
            {
                continue;
            }
            if i.visible()
                .is_some_and(|vis| vis.overlap_area(&r.rect) > TOL * TOL * 4.0)
            {
                out.push(v(Rule::Reserved, format!("{:?} over {name}", i.label)));
            }
        }
    }
    // Popups touch their triggers.
    for p in items.iter() {
        let Kind::Popup { trigger } = &p.kind else {
            continue;
        };
        // Several triggers may share a label (a chart's and a DOM's account chip): the nearest.
        match items
            .iter()
            .filter(|i| i.kind.interactive() && &i.label == trigger)
            .min_by(|a, b| p.rect.gap(&a.rect).total_cmp(&p.rect.gap(&b.rect)))
        {
            Some(t) if p.rect.gap(&t.rect) <= 2.0 + TOL => {}
            Some(t) => out.push(v(
                Rule::Popup,
                format!(
                    "{:?} is {:.0} px from its trigger {trigger:?}",
                    p.label,
                    p.rect.gap(&t.rect)
                ),
            )),
            None => out.push(v(
                Rule::Popup,
                format!("{:?}: trigger {trigger:?} not drawn", p.label),
            )),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(label: &str, r: Rect) -> Item {
        Item::new(Kind::Text, label, r)
    }

    fn rules(items: &[Item]) -> Vec<Rule> {
        lint(items).into_iter().map(|v| v.rule).collect()
    }

    #[test]
    fn overlapping_text_in_one_layer_is_flagged() {
        let a = text("6019.50", Rect::new(0.0, 0.0, 50.0, 14.0));
        let b = text("6020.00", Rect::new(40.0, 5.0, 50.0, 14.0));
        assert_eq!(rules(&[a.clone(), b.clone()]), [Rule::Overlap]);
        // Touching is fine; another layer (a popup) is fine.
        let c = text("x", Rect::new(50.0, 0.0, 10.0, 14.0));
        assert!(rules(&[a.clone(), c]).is_empty());
        let mut d = b;
        d.layer = 1;
        assert!(rules(&[a, d]).is_empty());
    }

    #[test]
    fn a_control_over_text_is_flagged() {
        let account = text("Paper 50K eval", Rect::new(300.0, 4.0, 90.0, 14.0));
        let pop = Item::new(Kind::Button, "pop out", Rect::new(370.0, 2.0, 48.0, 20.0));
        assert_eq!(rules(&[account, pop]), [Rule::Overlap]);
    }

    #[test]
    fn clipped_text_needs_an_ellipsis() {
        let mut t = text("E-mini S&P 500", Rect::new(0.0, 0.0, 120.0, 14.0));
        t.clip = Some(Rect::new(0.0, 0.0, 80.0, 20.0));
        assert_eq!(rules(&[t.clone()]), [Rule::Clipped]);
        t.label = "E-mini S…".into();
        assert!(rules(&[t.clone()]).is_empty());
        // Wholly outside its clip: not drawn, not clipped.
        t.label = "x".into();
        t.rect = Rect::new(200.0, 0.0, 10.0, 14.0);
        assert!(rules(&[t]).is_empty());
    }

    #[test]
    fn wholly_clipped_items_dont_overlap_but_cut_controls_count() {
        let clip = Rect::new(0.0, 0.0, 300.0, 24.0);
        let mut hidden = Item::new(Kind::Chip, "Paper ▾", Rect::new(310.0, 2.0, 60.0, 20.0));
        hidden.clip = Some(clip);
        let below = text("6024.00", Rect::new(310.0, 10.0, 50.0, 14.0));
        assert!(rules(&[hidden.clone(), below]).is_empty(), "not drawn");
        let mut cut = hidden;
        cut.rect.x = 270.0;
        assert_eq!(rules(&[cut]), [Rule::Clipped]);
    }

    #[test]
    fn hit_targets_are_at_least_20_px() {
        let small = Item::new(Kind::Chip, "5m ▾", Rect::new(0.0, 0.0, 34.0, 16.0));
        assert_eq!(rules(&[small]), [Rule::HitTarget]);
        let ok = Item::new(Kind::Button, "pop out", Rect::new(0.0, 0.0, 48.0, 20.0));
        assert!(rules(&[ok]).is_empty());
        // Text is not interactive.
        assert!(rules(&[text("a", Rect::new(0.0, 0.0, 5.0, 5.0))]).is_empty());
    }

    #[test]
    fn sibling_gaps_follow_the_spacing_scale() {
        let mut a = Item::new(Kind::Chip, "a", Rect::new(0.0, 0.0, 30.0, 20.0));
        let mut b = Item::new(Kind::Chip, "b", Rect::new(38.0, 0.0, 30.0, 20.0));
        a.group = "hdr".into();
        b.group = "hdr".into();
        assert!(rules(&[a.clone(), b.clone()]).is_empty(), "8 px");
        b.rect.x = 36.0;
        assert_eq!(rules(&[a.clone(), b.clone()]), [Rule::Spacing], "6 px");
        // Another row: not neighbours.
        b.rect.y = 30.0;
        assert!(rules(&[a, b]).is_empty());
    }

    #[test]
    fn nothing_draws_over_a_reserved_region() {
        let r = Item::new(
            Kind::Reserved {
                name: "dom-price".into(),
            },
            "price column",
            Rect::new(100.0, 0.0, 60.0, 400.0),
        );
        let mut own = text("6019.50", Rect::new(105.0, 10.0, 50.0, 14.0));
        own.owner = "dom-price".into();
        let intruder = text("DENIED: closed", Rect::new(80.0, 10.0, 60.0, 14.0));
        assert_eq!(rules(&[r.clone(), own]), Vec::<Rule>::new());
        assert_eq!(rules(&[r, intruder]), [Rule::Reserved]);
    }

    #[test]
    fn popups_touch_their_trigger() {
        let chip = Item::new(Kind::Chip, "5m ▾", Rect::new(200.0, 4.0, 40.0, 20.0));
        let under = Item::new(
            Kind::Popup {
                trigger: "5m ▾".into(),
            },
            "timeframe list",
            Rect::new(200.0, 26.0, 150.0, 300.0),
        );
        assert!(rules(&[chip.clone(), under.clone()]).is_empty());
        let far = Item {
            rect: Rect::new(8.0, 26.0, 150.0, 300.0),
            ..under.clone()
        };
        assert_eq!(rules(&[chip, far]), [Rule::Popup]);
        assert_eq!(
            rules(std::slice::from_ref(&under)),
            [Rule::Popup],
            "trigger not drawn"
        );
        // Two triggers with one label: the popup's own (nearest) counts.
        let other = Item::new(Kind::Chip, "5m ▾", Rect::new(900.0, 4.0, 40.0, 20.0));
        assert!(rules(&[other, chip_again(), under]).is_empty());
    }

    fn chip_again() -> Item {
        Item::new(Kind::Chip, "5m ▾", Rect::new(200.0, 4.0, 40.0, 20.0))
    }
}
