//! Drawings per product (roadmap 11): one versioned file per product,
//! `<config dir>/drawings/<PRODUCT>.json`, never in the workspace file, so every chart of a
//! product shows the same drawings in every workspace.
//!
//! The [`Drawings`] global hands each chart an `Arc<ProductDrawings>`: the product's
//! [`DrawingSet`] (with undo) behind a mutex the UI thread takes briefly, and a revision the
//! charts poll (`Pane::changed`), so a change on one chart repaints every chart of that
//! product. Every change is saved through the background `persist::Writer`. Screenshots and
//! the benchmark keep drawings in memory only (`--drawings FILE` seeds them).
//!
//! The file holds anchors as ISO times and prices as integer units of the product's price
//! scale (exact). A drawing that can't be used (unknown tool, bad time, a price off the tick
//! grid: never rounded) is skipped and reported, and kept: every save writes it back exactly
//! as it was read, so a newer Lines (or a hand fix) can still use it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use gpui::{App, Global};
use lines_core::drawings::{Anchor, Drawing, DrawingSet, Style, Tool};
use lines_core::{Price, Product, ProductId, UtcMillis};
use serde::{Deserialize, Serialize};

use crate::persist::Writer;
use crate::versioned::{self, Format};

/// Longest note kept (characters).
pub const MAX_NOTE: usize = 200;

fn drawings_v0_to_v1(v: serde_json::Value) -> Result<serde_json::Value, String> {
    Ok(v)
}

/// `drawings/<PRODUCT>.json`'s format. Version 0 = written before versioning (same shape).
pub const FORMAT: Format = Format {
    what: "drawings",
    current: 1,
    migrations: &[drawings_v0_to_v1],
};

/// A drawings file.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DrawingsFile {
    /// The catalog product symbol (a check, and for people reading the file).
    #[serde(default)]
    pub product: String,
    #[serde(default)]
    pub drawings: Vec<DrawingDoc>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AnchorDoc {
    /// ISO-8601 UTC.
    pub time: String,
    /// Integer units of the product's price scale (e.g. ES 600025 = 6000.25).
    pub price: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DrawingDoc {
    pub id: u64,
    /// `hline`, `hray`, `trend`, `ray`, `rect`, `vline`, `note`, `measure`.
    pub tool: String,
    pub a: AnchorDoc,
    /// The second anchor (two-anchor tools only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub b: Option<AnchorDoc>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub color: u8,
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub width: u8,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dashed: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
}

fn is_zero(v: &u8) -> bool {
    *v == 0
}
fn one() -> u8 {
    1
}
fn is_one(v: &u8) -> bool {
    *v == 1
}

fn anchor_doc(a: Anchor) -> AnchorDoc {
    AnchorDoc {
        time: a.time.to_string(),
        price: a.price.0,
    }
}

fn anchor(d: &AnchorDoc, tick: i64) -> Result<Anchor, String> {
    let time = UtcMillis::parse_iso(&d.time).ok_or_else(|| format!("bad time {:?}", d.time))?;
    if d.price % tick.max(1) != 0 {
        return Err(format!("price {} is off the tick grid", d.price));
    }
    Ok(Anchor {
        time,
        price: Price(d.price),
    })
}

impl DrawingsFile {
    pub fn from_drawings(product: &Product, items: &[Drawing]) -> DrawingsFile {
        DrawingsFile {
            product: product.symbol.to_owned(),
            drawings: items
                .iter()
                .map(|d| DrawingDoc {
                    id: d.id,
                    tool: d.tool.key().to_owned(),
                    a: anchor_doc(d.a),
                    b: (d.tool.anchors() == 2).then(|| anchor_doc(d.b)),
                    color: d.style.color,
                    width: d.style.width,
                    dashed: d.style.dashed,
                    text: d.text.clone(),
                })
                .collect(),
        }
    }

    /// The drawings this file holds for a product with tick `tick`, and why any were skipped.
    pub fn to_drawings(&self, tick: i64) -> (Vec<Drawing>, Vec<String>) {
        let mut out: Vec<Drawing> = Vec::with_capacity(self.drawings.len());
        let mut skipped = Vec::new();
        for doc in &self.drawings {
            let d = (|| {
                let tool = Tool::from_key(&doc.tool)
                    .ok_or_else(|| format!("unknown tool {:?}", doc.tool))?;
                let a = anchor(&doc.a, tick)?;
                let b = match (&doc.b, tool.anchors()) {
                    (Some(b), 2) => anchor(b, tick)?,
                    (None, 2) => return Err("second anchor missing".to_owned()),
                    _ => a,
                };
                let style = Style {
                    color: doc.color,
                    width: doc.width,
                    dashed: doc.dashed,
                }
                .normalised();
                let mut d = Drawing::new(tool, a, b, style);
                d.id = doc.id;
                if tool == Tool::Note {
                    d.text = doc.text.chars().take(MAX_NOTE).collect();
                }
                Ok(d)
            })();
            match d {
                Ok(d) if d.id > 0 && !out.iter().any(|x| x.id == d.id) => out.push(d),
                Ok(d) => skipped.push(format!("drawing {}: duplicate or zero id", d.id)),
                Err(e) => skipped.push(format!("drawing {}: {e}", doc.id)),
            }
        }
        out.sort_by_key(|d| d.id);
        (out, skipped)
    }

    /// The entries `to_drawings` didn't turn into `usable` (each usable id once).
    pub fn unusable(&self, usable: &[Drawing]) -> Vec<DrawingDoc> {
        let mut taken: Vec<u64> = Vec::new();
        self.drawings
            .iter()
            .filter(|d| {
                let use_it = usable.iter().any(|u| u.id == d.id) && !taken.contains(&d.id);
                if use_it {
                    taken.push(d.id);
                }
                !use_it
            })
            .cloned()
            .collect()
    }

    pub fn encode(&self) -> Vec<u8> {
        versioned::encode(&FORMAT, self).unwrap_or_default()
    }
}

/// `<config dir>/drawings/<PRODUCT>.json` (`/` in a symbol becomes `-`).
pub fn path(dir: &Path, product: &Product) -> PathBuf {
    let name: String = product
        .symbol
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    dir.join("drawings").join(format!("{name}.json"))
}

/// One product's drawings, shared by every chart of it.
pub struct ProductDrawings {
    pub product: Product,
    set: Mutex<DrawingSet>,
    revision: AtomicU64,
    /// Where changes are saved (`None`: in memory only, or a file we couldn't back up).
    path: Option<PathBuf>,
    writer: Option<Writer>,
    /// Entries of the file this version couldn't use: written back unchanged on every save.
    kept: Vec<DrawingDoc>,
}

impl ProductDrawings {
    fn new(
        product: Product,
        set: DrawingSet,
        path: Option<PathBuf>,
        writer: Option<Writer>,
    ) -> Self {
        ProductDrawings {
            product,
            set: Mutex::new(set),
            revision: AtomicU64::new(1),
            path,
            writer,
            kept: Vec::new(),
        }
    }

    /// In memory only (tests, screenshots).
    pub fn ephemeral(product: Product, items: Vec<Drawing>) -> Arc<ProductDrawings> {
        Arc::new(Self::new(
            product,
            DrawingSet::from_drawings(items),
            None,
            None,
        ))
    }

    /// Bumped by every change: charts poll it.
    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    /// The set, for reading (paint) or a change through [`Self::change`].
    pub fn lock(&self) -> MutexGuard<'_, DrawingSet> {
        self.set.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Change the set; if `f` says it changed, bump the revision and save.
    pub fn change<R>(&self, f: impl FnOnce(&mut DrawingSet) -> (bool, R)) -> R {
        let mut set = self.lock();
        let (changed, r) = f(&mut set);
        if changed {
            self.revision.fetch_add(1, Ordering::AcqRel);
            if let (Some(p), Some(w)) = (&self.path, &self.writer) {
                let mut file = DrawingsFile::from_drawings(&self.product, set.items());
                file.drawings.extend(self.kept.iter().cloned());
                w.write(p.clone(), file.encode());
            }
        }
        r
    }
}

/// Every product's drawings this session (loaded on first use).
pub struct Drawings {
    dir: Option<PathBuf>,
    writer: Option<Writer>,
    products: HashMap<ProductId, Arc<ProductDrawings>>,
}

impl Global for Drawings {}

impl Drawings {
    /// Saved under `dir` through `writer`.
    pub fn new(dir: Option<PathBuf>, writer: Option<Writer>) -> Drawings {
        Drawings {
            dir,
            writer,
            products: HashMap::new(),
        }
    }

    /// In memory only (screenshots, the benchmark), seeded from `file` if given.
    pub fn ephemeral(seed: Vec<(Product, Vec<Drawing>)>) -> Drawings {
        let mut d = Drawings::new(None, None);
        for (p, items) in seed {
            d.products
                .insert(p.id, ProductDrawings::ephemeral(p, items));
        }
        d
    }

    /// The drawings of `product`, loading its file the first time. Problems reading it are
    /// returned for the footer.
    pub fn product(&mut self, product: &Product) -> (Arc<ProductDrawings>, Option<String>) {
        if let Some(p) = self.products.get(&product.id) {
            return (p.clone(), None);
        }
        let (set, path, report, kept) = match &self.dir {
            None => (DrawingSet::default(), None, None, Vec::new()),
            Some(dir) => {
                let file = path(dir, product);
                let l = versioned::load::<DrawingsFile>(&FORMAT, &file);
                let doc = l.value.unwrap_or_default();
                let (items, skipped) = doc.to_drawings(product.price.tick);
                let kept = doc.unusable(&items);
                let mut report = l.report;
                if !skipped.is_empty() {
                    let s = format!(
                        "{} drawings: skipped {} that couldn't be used, kept in the file ({})",
                        product.symbol,
                        skipped.len(),
                        skipped.join("; ")
                    );
                    report = Some(match report {
                        Some(r) => format!("{r}; {s}"),
                        None => s,
                    });
                }
                let mut set = DrawingSet::from_drawings(items);
                if let Some(max) = kept.iter().map(|d| d.id).max() {
                    set.reserve_ids_through(max);
                }
                (set, l.writable.then_some(file), report, kept)
            }
        };
        let mut p = ProductDrawings::new(product.clone(), set, path, self.writer.clone());
        p.kept = kept;
        let p = Arc::new(p);
        self.products.insert(product.id, p.clone());
        (p, report)
    }
}

/// The drawings of `product` for a chart (an in-memory set when there is no global: tests).
pub fn for_product(product: &Product, cx: &mut App) -> Arc<ProductDrawings> {
    if !cx.has_global::<Drawings>() {
        return ProductDrawings::ephemeral(product.clone(), Vec::new());
    }
    let (p, report) = cx.global_mut::<Drawings>().product(product);
    if let Some(r) = report
        && cx.has_global::<crate::workspace::Workspace>()
    {
        cx.global_mut::<crate::workspace::Workspace>().add_report(r);
    }
    p
}

/// Read a `--drawings FILE` (screenshots): the product it names and its drawings.
#[cfg(any(test, feature = "screenshot"))]
pub fn read_seed(file: &Path) -> Result<(Product, Vec<Drawing>), String> {
    let bytes = std::fs::read(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let doc: DrawingsFile = match versioned::decode(&FORMAT, &bytes) {
        versioned::Decoded::Ok { value, .. } => value,
        versioned::Decoded::Newer { found } => {
            return Err(format!("written by a newer Lines (version {found})"));
        }
        versioned::Decoded::Corrupt { reason } => return Err(reason),
    };
    let product = lines_feed::catalog::all()
        .into_iter()
        .find(|p| p.symbol == doc.product)
        .ok_or_else(|| format!("unknown product {:?}", doc.product))?;
    let (items, skipped) = doc.to_drawings(product.price.tick);
    if let Some(e) = skipped.first() {
        return Err(e.clone());
    }
    Ok((product, items))
}

#[cfg(test)]
mod tests;
