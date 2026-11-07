//! Backtests (decision 0025): one strategy instance over a recording (or a stored contract or
//! day: Sierra and Databento imports are stored sessions) plus recorded levels, with the paper
//! broker, prop rules, risk, guardrails and the strategy gate, on the recording's own clock.
//! Single-threaded, no wall clock: the same inputs always give the same [`Report`], byte for
//! byte ([`Report::to_json`]).
//!
//! Order of events at one instant: level batches, then the market batch, then the timer.
//! The instance is armed once market data has started and every required level source has a
//! current map; after that, nothing re-arms it (as live: a pause lasts until a person arms).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};
use lines_core::bars::BarSeries;
use lines_core::{ContractKey, Price, Product, UtcMillis};
use lines_feed::levels::runtime::{LevelState, LevelStatus, read_recording, recording_path};
use lines_feed::levels::{ChartPrices, LevelContext, LevelFrame, LevelSource};
use lines_feed::provider::MarketEvent;
use lines_feed::recorded::RecordedEvents;
use lines_trading::types::time_serde;
use lines_trading::{
    AccountConfig, Command, Event, Journal, Money, OrderId, PaperBroker, PaperConfig, Reject,
    Source, StrategyConfig, StrategyId, StrategyLimits, StrategyState, Trade, TradingCore,
};
use serde::{Deserialize, Serialize};

use crate::{Ctx, Decision, Input, LevelView, Strategy, StrategyExtension};

/// The instance id a backtest runs as.
pub const INSTANCE: StrategyId = StrategyId(1);
/// Decisions one input may cause (a strategy answering its own events in a loop pauses).
const MAX_DECISIONS_PER_INPUT: usize = 64;
/// Report format version.
pub const FORMAT: u32 = 1;

/// What to run.
pub struct Backtest {
    pub strategy: Arc<dyn StrategyExtension>,
    pub params: serde_json::Value,
    /// A `.lfr` recording, a stored day (`.lseg`) or a stored contract directory.
    pub market: PathBuf,
    /// Where the recorded levels are (`<source>-<PRODUCT>.lfr`, decision 0022).
    pub levels_dir: Option<PathBuf>,
    pub paper: PaperConfig,
    pub account: AccountConfig,
    pub limits: StrategyLimits,
    /// Timer inputs this often (market time).
    pub timer_ms: i64,
}

/// A backtest's settings file (`lines --backtest <name> --backtest-config <file>`): the
/// strategy's parameters and the instance's limits are required; the account balance, paper
/// broker and timer have defaults.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub params: serde_json::Value,
    pub limits: StrategyLimits,
    /// Whole currency units (default 50,000).
    #[serde(default = "default_balance")]
    pub starting_balance: i64,
    /// Fill model, latency, fees, session checks (default: the paper broker's defaults:
    /// trade-through fills, 25 ms, no fees).
    #[serde(default)]
    pub paper: PaperConfig,
    /// Timer inputs, in ms of market time (default 1000).
    #[serde(default = "default_timer")]
    pub timer_ms: i64,
}

fn default_balance() -> i64 {
    50_000
}

fn default_timer() -> i64 {
    1000
}

impl Backtest {
    /// A backtest from a settings file's contents, on a paper account.
    pub fn from_settings(
        strategy: Arc<dyn StrategyExtension>,
        settings: Settings,
        market: PathBuf,
        levels_dir: Option<PathBuf>,
    ) -> Backtest {
        Backtest {
            strategy,
            params: settings.params,
            market,
            levels_dir,
            paper: settings.paper,
            account: AccountConfig::paper("paper-backtest", settings.starting_balance),
            limits: settings.limits,
            timer_ms: settings.timer_ms,
        }
    }
}

impl Summary {
    /// One line for the console.
    pub fn line(&self) -> String {
        format!(
            "{} trades ({} won, {} lost), net {}, fees {}, max drawdown {}",
            self.trades, self.wins, self.losses, self.net, self.fees, self.max_drawdown
        )
    }
}

/// One input file, identified by content.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct InputFile {
    /// The file name only (reports don't carry local paths).
    pub name: String,
    pub bytes: u64,
    /// FNV-1a 64 of the content, hex (identity, not security).
    pub fnv1a64: String,
}

/// A decision with what came of its command.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DecisionRecord {
    #[serde(with = "time_serde")]
    pub at: UtcMillis,
    #[serde(flatten)]
    pub decision: Decision,
    /// The command's result: the orders it created or touched, or why it was denied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Result<Vec<OrderId>, Reject>>,
}

/// The instance's state changes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StateChange {
    #[serde(with = "time_serde")]
    pub at: UtcMillis,
    pub state: StrategyState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Totals over closed trades (fees included in `net`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Summary {
    pub trades: usize,
    pub wins: usize,
    pub losses: usize,
    pub gross: Money,
    pub fees: Money,
    pub net: Money,
    /// Largest fall of cumulative net P&L from a high, over closed trades.
    pub max_drawdown: Money,
    pub worst_mae: Money,
    pub best_mfe: Money,
}

/// The position left open at the end: marked to the last price, never closed silently.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct OpenAtEnd {
    pub qty: i64,
    pub avg_price: Option<f64>,
    pub unrealised: Money,
}

/// A backtest's result.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Report {
    pub format: u32,
    pub strategy: String,
    pub version: String,
    pub params: serde_json::Value,
    pub market: Vec<InputFile>,
    pub levels: Vec<InputFile>,
    pub product: String,
    pub contract: String,
    #[serde(with = "time_serde")]
    pub from: UtcMillis,
    #[serde(with = "time_serde")]
    pub to: UtcMillis,
    pub paper: PaperConfig,
    pub starting_balance: i64,
    pub limits: StrategyLimits,
    pub summary: Summary,
    pub open_at_end: Option<OpenAtEnd>,
    pub trades: Vec<Trade>,
    pub states: Vec<StateChange>,
    pub decisions: Vec<DecisionRecord>,
    /// Problems with the inputs (a level batch refused, a frame that didn't decode).
    pub warnings: Vec<String>,
    /// The strategy's own status lines at the end.
    pub final_status: Vec<String>,
}

impl Report {
    /// The report as JSON: identical inputs give identical bytes.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("a report always serializes")
    }
}

/// A required level source and its recorded batches.
struct Levels {
    source: Arc<dyn LevelSource>,
    batches: Vec<(UtcMillis, Vec<LevelFrame>)>,
    next: usize,
    state: LevelState,
    /// The status last given to the strategy.
    told: Option<LevelStatus>,
}

/// The bars so far, for level decoding (decision 0022's price-at-time).
struct Prices<'a> {
    bars: &'a BarSeries,
    now: UtcMillis,
}

impl ChartPrices for Prices<'_> {
    fn price_at(&self, t: UtcMillis, tolerance_ms: i64) -> Option<Price> {
        lines_core::levels::price_at(self.bars.bars(), t, tolerance_ms)
    }
    fn now(&self) -> Option<UtcMillis> {
        Some(self.now)
    }
}

struct Run {
    core: TradingCore,
    journal: Journal,
    strategy: Box<dyn Strategy>,
    cfg: StrategyConfig,
    product: Product,
    state: StrategyState,
    decisions: Vec<DecisionRecord>,
    states: Vec<StateChange>,
    /// Command id → index of its decision.
    pending: BTreeMap<u64, usize>,
    /// The harness's own commands (add, arm) and what came of them.
    harness: BTreeMap<u64, &'static str>,
    refused: Vec<String>,
    last_position: Option<(i64, Option<f64>, Money)>,
}

/// The context for one call (a free function: it borrows the run's fields, not the run).
fn ctx<'a>(
    cfg: &'a StrategyConfig,
    product: &'a Product,
    state: StrategyState,
    now: UtcMillis,
) -> Ctx<'a> {
    Ctx {
        now,
        id: INSTANCE,
        account: &cfg.account,
        contract: cfg.contract,
        product,
        state,
    }
}

impl Run {
    /// Whether a core event concerns the instance.
    fn concerns(&self, e: &Event) -> bool {
        let (a, k) = (&self.cfg.account, self.cfg.contract);
        match e {
            Event::Command { source, .. } => *source == Source::Strategy(INSTANCE),
            Event::Order(o) => &o.account == a && o.contract == k,
            Event::Fill(f) => &f.account == a && f.contract == k,
            Event::Position(p) => &p.account == a && p.contract == k,
            Event::Account(v) => &v.account == a,
            Event::Breach { account, .. } | Event::Guardrail { account, .. } => account == a,
            Event::Overfill { account, .. } => account == a,
            Event::Strategy { id, .. } => *id == INSTANCE,
            Event::Rebuilt { .. } => true,
        }
    }

    /// Hand the core's events to the journal and the strategy; run the strategy's answers.
    fn flush(&mut self, now: UtcMillis, budget: &mut usize) {
        loop {
            let events = self.core.drain();
            if events.is_empty() {
                return;
            }
            let mut out = Vec::new();
            for e in &events {
                self.journal.on_event(e);
                match e {
                    Event::Command { id, result, .. } => {
                        if let Some(i) = self.pending.remove(&id.0) {
                            self.decisions[i].result = Some(result.clone());
                        }
                        if let (Some(what), Err(r)) = (self.harness.remove(&id.0), result) {
                            self.refused.push(format!("{what} refused: {}", r.message));
                        }
                    }
                    Event::Strategy {
                        id: INSTANCE,
                        state,
                        reason,
                        ..
                    } => {
                        self.state = *state;
                        self.states.push(StateChange {
                            at: now,
                            state: *state,
                            reason: reason.clone(),
                        });
                    }
                    Event::Position(p)
                        if p.account == self.cfg.account && p.contract == self.cfg.contract =>
                    {
                        self.last_position = Some((p.qty, p.avg_price, p.unrealised));
                    }
                    _ => {}
                }
                if self.concerns(e) {
                    let c = ctx(&self.cfg, &self.product, self.state, now);
                    self.strategy.on_input(&c, Input::Trading(e), &mut out);
                }
            }
            self.decide(now, out, budget);
        }
    }

    /// Log decisions and send their commands, in order.
    fn decide(&mut self, now: UtcMillis, out: Vec<Decision>, budget: &mut usize) {
        for d in out {
            if *budget == 0 {
                if self.state == StrategyState::Armed {
                    self.core.submit(
                        now,
                        Source::Strategy(INSTANCE),
                        Instant::now(),
                        Command::PauseStrategy {
                            id: INSTANCE,
                            reason: format!(
                                "more than {MAX_DECISIONS_PER_INPUT} decisions for one input"
                            ),
                        },
                    );
                }
                return;
            }
            *budget -= 1;
            let i = self.decisions.len();
            let command = d.command.clone();
            self.decisions.push(DecisionRecord {
                at: now,
                decision: d,
                result: None,
            });
            if let Some(c) = command {
                let id = self
                    .core
                    .submit(now, Source::Strategy(INSTANCE), Instant::now(), c);
                self.pending.insert(id.0, i);
            }
        }
    }

    fn input(&mut self, now: UtcMillis, input: Input<'_>) {
        let mut out = Vec::new();
        let c = ctx(&self.cfg, &self.product, self.state, now);
        self.strategy.on_input(&c, input, &mut out);
        let mut budget = MAX_DECISIONS_PER_INPUT;
        self.decide(now, out, &mut budget);
        self.flush(now, &mut budget);
    }
}

/// FNV-1a 64 over bytes.
fn fnv1a64(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in data {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// The files behind an input path (a stored contract directory: its days, in order).
fn input_files(path: &Path) -> Result<Vec<InputFile>> {
    let mut paths = if path.is_dir() {
        let mut v: Vec<PathBuf> = std::fs::read_dir(path)
            .with_context(|| format!("read {}", path.display()))?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect();
        v.sort();
        v
    } else {
        vec![path.to_path_buf()]
    };
    paths.retain(|p| p.file_name().is_some());
    paths
        .iter()
        .map(|p| {
            let data = std::fs::read(p).with_context(|| format!("read {}", p.display()))?;
            Ok(InputFile {
                name: p
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                bytes: data.len() as u64,
                fnv1a64: format!("{:016x}", fnv1a64(&data)),
            })
        })
        .collect()
}

/// Run a backtest.
pub fn run(bt: &Backtest) -> Result<Report> {
    let mut md = RecordedEvents::open(&bt.market)?;
    let product = md.product().clone();
    let contract: ContractKey = md
        .contract()
        .ok_or_else(|| anyhow!("this recording doesn't say which contract it holds"))?;
    if !bt.strategy.serves(&product) {
        bail!("{} doesn't trade {}", bt.strategy.label(), product.symbol);
    }
    if bt.timer_ms <= 0 {
        bail!("the timer period must be above 0 ms");
    }
    bt.limits
        .validate()
        .map_err(|e| anyhow!("strategy limits: {e}"))?;
    let strategy = bt
        .strategy
        .create(&bt.params)
        .map_err(|e| anyhow!("{}: {e}", bt.strategy.label()))?;
    let mut warnings = Vec::new();

    // Required level sources and their recordings.
    let mut levels: Vec<Levels> = Vec::new();
    let mut level_files = Vec::new();
    for &id in bt.strategy.level_sources() {
        let source = lines_feed::levels::by_id(id)
            .ok_or_else(|| anyhow!("level source {id:?} is not registered in this build"))?;
        let mut state = LevelState {
            source: source.id(),
            label: source.label(),
            map: None,
            received: None,
            error: None,
            stale_after: source.stale_after(),
            replay: true,
        };
        let batches = match &bt.levels_dir {
            Some(dir) => {
                let path = recording_path(dir, id, product.symbol);
                if path.exists() {
                    level_files.extend(input_files(&path)?);
                    read_recording(&path, id, product.symbol).map_err(|e| anyhow!(e))?
                } else {
                    state.error = Some("not recorded for this replay".into());
                    Vec::new()
                }
            }
            None => {
                state.error = Some("not recorded for this replay".into());
                Vec::new()
            }
        };
        if batches.is_empty() {
            warnings.push(format!("levels {id}: not recorded for this replay"));
        }
        levels.push(Levels {
            source,
            batches,
            next: 0,
            state,
            told: None,
        });
    }

    let mut broker = PaperBroker::new(bt.paper.clone());
    broker.open_account(
        bt.account.id.clone(),
        Money::whole(bt.account.starting_balance),
    );
    let mut core = TradingCore::new(Box::new(broker));
    core.add_account(bt.account.clone(), None);
    let cfg = StrategyConfig {
        id: INSTANCE,
        label: format!("{} #{}", bt.strategy.label(), INSTANCE.0),
        account: bt.account.id.clone(),
        contract,
        limits: bt.limits.clone(),
    };
    let mut run = Run {
        core,
        journal: Journal::new(),
        strategy,
        cfg: cfg.clone(),
        product: product.clone(),
        state: StrategyState::Disarmed,
        decisions: Vec::new(),
        states: Vec::new(),
        pending: BTreeMap::new(),
        harness: BTreeMap::new(),
        refused: Vec::new(),
        last_position: None,
    };
    let ctx = LevelContext {
        product: product.clone(),
        contract: Some(contract),
        settings: Default::default(),
    };
    let mut bars = BarSeries::new(UtcMillis::MINUTE, 50_000);

    let mut next_md = md.next_batch()?;
    let Some((first, _)) = next_md.as_ref().map(|(t, e)| (*t, e.len())) else {
        bail!("the recording holds no market data");
    };
    let mut last = first;
    let mut next_timer = first.0 + bt.timer_ms;
    let mut added = false;
    let mut arm_tried = false;

    while let Some((t_md, _)) = next_md.as_ref() {
        let t_md = *t_md;
        let t_lv = levels
            .iter()
            .filter_map(|l| l.batches.get(l.next).map(|(t, _)| t.0))
            .min();
        let now;
        // Levels first at an instant, then market data, then the timer.
        if t_lv.is_some_and(|t| t <= t_md.0 && t <= next_timer) {
            now = UtcMillis(t_lv.expect("checked"));
            run.core.strategy_heartbeat(now);
            run.core.tick(now);
            run.flush(now, &mut MAX_DECISIONS_PER_INPUT.clone());
            for l in levels.iter_mut() {
                while let Some((t, frames)) = l.batches.get(l.next).filter(|(t, _)| *t == now) {
                    let _ = t;
                    let prices = Prices { bars: &bars, now };
                    match l
                        .source
                        .decode(&ctx, &prices, l.state.map.as_deref(), frames)
                    {
                        Ok(Some(map)) => {
                            l.state.map = Some(Arc::new(map));
                            l.state.received = Some(now);
                            l.state.error = None;
                        }
                        Ok(None) => {
                            if l.state.map.is_some() {
                                l.state.received = Some(now);
                            }
                            l.state.error = None;
                        }
                        Err(e) => {
                            warnings.push(format!("levels {}: batch refused: {e}", l.state.source));
                            l.state.error = Some(e);
                        }
                    }
                    l.next += 1;
                    l.told = None;
                }
            }
        } else if t_md.0 <= next_timer {
            now = t_md;
            let (_, events) = next_md.take().expect("checked");
            for e in &events {
                if let MarketEvent::Trade {
                    time, price, qty, ..
                } = e
                {
                    bars.apply_trade(*time, *price, *qty);
                }
            }
            if !added {
                added = true;
                let id = run.core.submit(
                    now,
                    Source::Test,
                    Instant::now(),
                    Command::AddStrategy(cfg.clone()),
                );
                run.harness.insert(id.0, "adding the strategy");
            }
            run.core.strategy_heartbeat(now);
            run.core.tick(now);
            let inputs = lines_trading::market::from_events(&events);
            run.core.market(now, contract, &inputs);
            run.journal.on_market(contract, &inputs);
            run.flush(now, &mut MAX_DECISIONS_PER_INPUT.clone());
            run.input(now, Input::Market(&events));
            next_md = md.next_batch()?;
        } else {
            now = UtcMillis(next_timer);
            next_timer += bt.timer_ms;
            run.core.strategy_heartbeat(now);
            run.core.tick(now);
            run.flush(now, &mut MAX_DECISIONS_PER_INPUT.clone());
            run.input(now, Input::Timer);
        }
        last = now;

        // Freshness, as the strategy sees it (and the host's auto-pause on missing levels).
        let mut stale: Option<String> = None;
        for l in levels.iter_mut() {
            let status = l.state.status(now);
            if !matches!(status, LevelStatus::Current) {
                stale.get_or_insert_with(|| {
                    format!(
                        "levels {} are {}",
                        l.state.label,
                        match status {
                            LevelStatus::Missing => "missing",
                            _ => "stale",
                        }
                    )
                });
            }
            if l.told != Some(status) {
                l.told = Some(status);
                let view = LevelView {
                    source: l.state.source,
                    status,
                    map: l.state.map.clone(),
                    received: l.state.received,
                };
                run.input(now, Input::Levels(&view));
            }
        }
        if added {
            match (&stale, run.state) {
                (Some(why), StrategyState::Armed) => {
                    run.core.submit(
                        now,
                        Source::Strategy(INSTANCE),
                        Instant::now(),
                        Command::PauseStrategy {
                            id: INSTANCE,
                            reason: why.clone(),
                        },
                    );
                    run.flush(now, &mut MAX_DECISIONS_PER_INPUT.clone());
                }
                (None, StrategyState::Disarmed) if !arm_tried => {
                    // Armed once, by "the trader", when everything it needs is there.
                    arm_tried = true;
                    let id = run.core.submit(
                        now,
                        Source::Test,
                        Instant::now(),
                        Command::ArmStrategy { id: INSTANCE },
                    );
                    run.harness.insert(id.0, "arming");
                    run.flush(now, &mut MAX_DECISIONS_PER_INPUT.clone());
                }
                _ => {}
            }
        }
    }
    // A run whose instance never existed is no result at all.
    if let Some(r) = run.refused.iter().find(|r| r.starts_with("adding")) {
        bail!("{r}");
    }
    warnings.extend(run.refused.iter().cloned());
    if md.decode_errors() > 0 {
        warnings.push(format!(
            "{} market frames didn't decode",
            md.decode_errors()
        ));
    }

    let trades: Vec<Trade> = run.journal.trades().to_vec();
    let mut summary = Summary {
        trades: trades.len(),
        ..Default::default()
    };
    let (mut cum, mut high) = (Money::ZERO, Money::ZERO);
    for t in &trades {
        let net = t.net();
        summary.gross += t.realized;
        summary.fees += t.fees;
        summary.net += net;
        if net > Money::ZERO {
            summary.wins += 1;
        } else if net < Money::ZERO {
            summary.losses += 1;
        }
        summary.worst_mae = summary.worst_mae.min(t.mae);
        summary.best_mfe = summary.best_mfe.max(t.mfe);
        cum += net;
        high = high.max(cum);
        summary.max_drawdown = summary.max_drawdown.max(high - cum);
    }
    let open_at_end =
        run.last_position
            .filter(|(q, _, _)| *q != 0)
            .map(|(qty, avg_price, unrealised)| OpenAtEnd {
                qty,
                avg_price,
                unrealised,
            });
    Ok(Report {
        format: FORMAT,
        strategy: bt.strategy.name().to_owned(),
        version: bt.strategy.version().to_owned(),
        params: bt.params.clone(),
        market: input_files(&bt.market)?,
        levels: level_files,
        product: product.symbol.to_owned(),
        contract: contract.label(&product).as_str().to_owned(),
        from: first,
        to: last,
        paper: bt.paper.clone(),
        starting_balance: bt.account.starting_balance,
        limits: bt.limits.clone(),
        summary,
        open_at_end,
        trades,
        states: run.states,
        decisions: run.decisions,
        warnings,
        final_status: run.strategy.status(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv_known_values() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
    }
}
