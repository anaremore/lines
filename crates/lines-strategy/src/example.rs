//! A trivial, synthetic example strategy (decision 0025: the public build ships at most
//! this). **It has no edge and is not advice:** it exists to exercise the framework in tests
//! and to show the shape of a strategy.
//!
//! `level-touch`: while flat, with no entry working, it places one bracket buy limit at the
//! nearest unused line of one kind below the last trade, if that line is within a few ticks.
//! Each line is used once.

use std::collections::BTreeSet;

use lines_core::{Price, Product, Qty};
use lines_feed::levels::runtime::LevelStatus;
use lines_feed::levels::{LevelShape, SettingSpec};
use lines_feed::provider::MarketEvent;
use lines_trading::{
    BracketSpec, Command, Event, OrderId, OrderKind, OrderSpec, Side, StrategyState,
};
use serde::Deserialize;

use crate::{Ctx, Decision, Input, LevelRef, Rounding, Strategy, StrategyExtension, to_tick};

/// The example's parameters.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Params {
    /// The level kind to buy at, e.g. `support`.
    pub kind: String,
    pub qty: i64,
    pub stop_ticks: i64,
    pub target_ticks: i64,
    /// Only lines this close below the last trade.
    pub max_distance_ticks: i64,
}

impl Params {
    fn check(&self) -> Result<(), String> {
        if self.kind.is_empty() {
            return Err("kind is empty".into());
        }
        for (name, v) in [
            ("qty", self.qty),
            ("stop_ticks", self.stop_ticks),
            ("target_ticks", self.target_ticks),
            ("max_distance_ticks", self.max_distance_ticks),
        ] {
            if v <= 0 {
                return Err(format!("{name} must be above 0"));
            }
        }
        Ok(())
    }
}

/// The example as an extension, reading lines from one level source.
pub struct LevelTouchExt {
    sources: [&'static str; 1],
}

impl LevelTouchExt {
    pub fn new(source: &'static str) -> LevelTouchExt {
        LevelTouchExt { sources: [source] }
    }
}

static SETTINGS: &[SettingSpec] = &[];

impl StrategyExtension for LevelTouchExt {
    fn name(&self) -> &str {
        "level-touch"
    }
    fn label(&self) -> &str {
        "Level touch (example)"
    }
    fn version(&self) -> &str {
        "1"
    }
    fn serves(&self, product: &Product) -> bool {
        product.price.tick > 0
    }
    fn level_sources(&self) -> &[&'static str] {
        &self.sources
    }
    fn settings(&self) -> &'static [SettingSpec] {
        SETTINGS
    }
    fn create(&self, params: &serde_json::Value) -> Result<Box<dyn Strategy>, String> {
        let p: Params = serde_json::from_value(params.clone()).map_err(|e| e.to_string())?;
        p.check()?;
        Ok(Box::new(LevelTouch {
            source: self.sources[0],
            params: p,
            lines: Vec::new(),
            used: BTreeSet::new(),
            last: None,
            position: 0,
            entry: Entry::None,
        }))
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Entry {
    None,
    /// Sent, no answer yet.
    Sent,
    Working(OrderId),
}

struct LevelTouch {
    source: &'static str,
    params: Params,
    /// Lines of the kind: price and label, ascending.
    lines: Vec<(Price, Option<String>)>,
    used: BTreeSet<Price>,
    last: Option<Price>,
    position: i64,
    entry: Entry,
}

impl Strategy for LevelTouch {
    fn on_input(&mut self, ctx: &Ctx<'_>, input: Input<'_>, out: &mut Vec<Decision>) {
        match input {
            Input::Levels(view) if view.source == self.source => {
                self.lines.clear();
                if view.status == LevelStatus::Missing {
                    return;
                }
                if let Some(map) = &view.map {
                    let kind = map.kind_index(&self.params.kind);
                    for l in map.levels.iter().filter(|l| Some(l.kind) == kind) {
                        if let LevelShape::Line(v) = &l.shape
                            && let Some(p) = to_tick(v, ctx.product, Rounding::Down)
                        {
                            self.lines.push((p, l.label.clone()));
                        }
                    }
                }
                self.lines.sort();
            }
            Input::Trading(e) => self.on_event(e),
            Input::Market(events) => {
                for e in events {
                    if let MarketEvent::Trade { price, .. } | MarketEvent::LastPrice(price) = e {
                        self.last = Some(*price);
                    }
                }
                self.consider(ctx, out);
            }
            Input::Levels(_) | Input::Timer => {}
        }
    }

    fn status(&self) -> Vec<String> {
        vec![
            format!(
                "{} {} lines, {} used",
                self.lines.len(),
                self.params.kind,
                self.used.len()
            ),
            format!("position {}", self.position),
        ]
    }
}

impl LevelTouch {
    fn on_event(&mut self, e: &Event) {
        match e {
            Event::Command { result, .. } if self.entry == Entry::Sent => {
                self.entry = match result {
                    Ok(ids) => ids.first().map_or(Entry::None, |id| Entry::Working(*id)),
                    Err(_) => Entry::None,
                };
            }
            Event::Order(o) if self.entry == Entry::Working(o.id) && !o.is_open() => {
                self.entry = Entry::None;
            }
            Event::Position(p) => self.position = p.qty,
            _ => {}
        }
    }

    fn consider(&mut self, ctx: &Ctx<'_>, out: &mut Vec<Decision>) {
        if ctx.state != StrategyState::Armed || self.position != 0 || self.entry != Entry::None {
            return;
        }
        let Some(last) = self.last else {
            return;
        };
        let tick = ctx.product.price.tick;
        let reach = self.params.max_distance_ticks * tick;
        let Some((price, label)) = self
            .lines
            .iter()
            .rev()
            .find(|(p, _)| *p < last && !self.used.contains(p))
            .cloned()
        else {
            return;
        };
        if last.0 - price.0 > reach {
            return;
        }
        self.used.insert(price);
        self.entry = Entry::Sent;
        let p = &self.params;
        let name = label.clone().unwrap_or_else(|| p.kind.clone());
        out.push(Decision {
            rule: "touch-long".into(),
            reason: format!(
                "Buy limit at {name} {}: price is within {} ticks above it",
                ctx.product.price.format::<32>(price),
                p.max_distance_ticks
            ),
            levels: vec![LevelRef {
                source: self.source.into(),
                kind: p.kind.clone(),
                label,
                price,
            }],
            command: Some(Command::PlaceBracket(BracketSpec {
                entry: OrderSpec {
                    account: ctx.account.clone(),
                    contract: ctx.contract,
                    side: Side::Buy,
                    qty: Qty(p.qty),
                    kind: OrderKind::Limit { price },
                },
                take_profit: Some(Price(price.0 + p.target_ticks * tick)),
                stop_loss: Some(Price(price.0 - p.stop_ticks * tick)),
            })),
        });
    }
}
