//! The strategy panel's model (decision 0025, phase c): pure, no GPUI.
//!
//! - **Where it reads from:** the instances come from a [`StrategySource`] (the strategy host;
//!   a fake in tests and screenshots, [`DemoStrategies`]). Each instance's state comes from
//!   the trading core's `Event::Strategy` when the UI has seen one, because the core enforces
//!   it. Today's trades and P&L come from the journal (fills whose source is the instance).
//!   The position is the account's position on the instance's contract.
//! - **Commands** go straight to the core (`Trading::submit`, a manual source), never
//!   through the host, so they work even if the host hangs:
//!   - Start arms the instance.
//!   - Stop and Stop all stop it and flatten (the default in 0025).
//!   - "keep positions" (palette only) stops without flattening.

use std::sync::Arc;
#[cfg(any(test, feature = "screenshot"))]
use std::sync::atomic::{AtomicU64, Ordering};

use gpui::Global;
#[cfg(any(test, feature = "screenshot"))]
use lines_core::UtcMillis;
use lines_core::{CivilDate, ContractKey};
use lines_strategy::backtest::DecisionRecord;
use lines_trading::journal::Trade;
use lines_trading::strategy::{StrategyId, StrategyLimits, StrategyState};
use lines_trading::{AccountId, Command, Money, PositionView, Source};

use crate::pnl::{self, PnlDisplay};

/// What the panel lists. The strategy host implements it (its `snapshot` and `revision`).
pub trait StrategySource: Send + Sync + 'static {
    /// Bumped whenever [`StrategySource::snapshot`] would return something new.
    fn revision(&self) -> u64;
    /// Every instance. A short lock at most; never waits on a running strategy.
    fn snapshot(&self) -> Vec<StrategyInfo>;
    /// Why an instance couldn't be added (shown in the panel), if one couldn't.
    fn problem(&self) -> Option<String> {
        None
    }
}

/// One instance as the host knows it.
#[derive(Clone, Debug)]
pub struct StrategyInfo {
    pub id: StrategyId,
    /// e.g. `level-touch #2`.
    pub label: String,
    /// The strategy's stable name and version.
    pub strategy: String,
    pub version: String,
    pub account: AccountId,
    pub contract: ContractKey,
    pub limits: StrategyLimits,
    /// The host's view of the state; the core's events win when the UI has one.
    pub state: StrategyState,
    pub reason: Option<String>,
    /// The strategy failed and is never called again.
    pub fault: Option<String>,
    /// Its feed moved to another contract: kept paused, never fed again.
    pub rolled: Option<String>,
    /// Newest last.
    pub decisions: Vec<DecisionRecord>,
}

/// The app's strategy source (absent: no strategies registered).
pub struct Strategies(pub Arc<dyn StrategySource>);

impl Global for Strategies {}

/// The state as the panel shows it, as a word (never color alone).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Not started (Disarmed) or stopped by the trader.
    Stopped,
    /// Armed, flat, no working orders: waiting for a signal.
    Armed,
    /// Armed with a position or working orders.
    Running,
    /// Paused by a rule, the host or the strategy itself (the reason says which).
    Paused,
    /// Paused at its daily loss limit: no entries until the next trade date.
    Halted,
    /// The strategy failed, or its contract rolled: it never runs again.
    Faulted,
}

impl Phase {
    pub fn word(self) -> &'static str {
        match self {
            Phase::Stopped => "STOPPED",
            Phase::Armed => "ARMED",
            Phase::Running => "RUNNING",
            Phase::Paused => "PAUSED",
            Phase::Halted => "HALTED",
            Phase::Faulted => "FAULTED",
        }
    }
}

/// The panel's phase for a state.
pub fn phase(state: StrategyState, reason: Option<&str>, failed: bool, active: bool) -> Phase {
    if failed {
        return Phase::Faulted;
    }
    match state {
        StrategyState::Disarmed | StrategyState::Stopped => Phase::Stopped,
        StrategyState::Armed if active => Phase::Running,
        StrategyState::Armed => Phase::Armed,
        // The core's exact reason (decision 0027): its trade was closed at the daily loss.
        StrategyState::Paused if reason == Some(lines_trading::strategy::DAILY_LOSS) => {
            Phase::Halted
        }
        StrategyState::Paused => Phase::Paused,
    }
}

/// One instance as the panel draws it (formatted only when something it shows changed).
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub id: StrategyId,
    pub label: String,
    /// `level-touch 0.1.0`.
    pub strategy: String,
    /// `ESZ26`.
    pub contract: String,
    /// `Paper 1`.
    pub account: String,
    pub phase: Phase,
    /// Why it is paused, halted, faulted or stopped.
    pub why: Option<String>,
    /// `Flat`, `Long 2 @ 6012.25`.
    pub position: String,
    /// `3 trades today`.
    pub trades: String,
    /// Its own limits: `max 2 contracts · daily loss $500 · 6 entries/min · 09:30-16:00 ET`
    /// (no money while the P&L display hides it).
    pub limits: String,
    /// Today's net P&L as the P&L display allows (`None`: off, or nothing to show).
    pub pnl: Option<String>,
    /// The value's sign, for its color (its text is signed too).
    pub pnl_sign: i8,
    /// The newest decision: `14:31:07  skipped: target crosses the zone`.
    pub last: Option<String>,
    pub can_start: bool,
    pub can_stop: bool,
}

/// What the row needs besides the instance.
pub struct RowCtx<'a> {
    /// The core's state and reason, if the UI saw an `Event::Strategy` for it.
    pub core: Option<(StrategyState, Option<&'a str>)>,
    /// The account's position on the instance's contract (`None`: flat).
    pub position: Option<&'a PositionView>,
    /// The instance has working orders.
    pub working: bool,
    /// The journal's trades (all of them; filtered here).
    pub trades: &'a [Trade],
    pub today: Option<CivilDate>,
    pub account_name: &'a str,
    pub mode: PnlDisplay,
}

/// A quantity (quantity units) as the product prints it: `2`, `0.5`.
pub fn qty_text(product: &lines_core::Product, units: i64) -> String {
    let mut s = String::new();
    let _ = product.qty.write(lines_core::Qty(units), &mut s);
    s
}

/// First letter upper case (the core's reasons start lower case).
fn sentence(s: String) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => s,
    }
}

/// Whether the trade is the instance's: on its account and contract, with a fill from it.
pub fn is_its_trade(t: &Trade, info: &StrategyInfo) -> bool {
    t.account == info.account
        && t.contract == info.contract
        && t.fills
            .iter()
            .any(|f| f.source == Some(Source::Strategy(info.id)))
}

pub fn row(info: &StrategyInfo, c: &RowCtx<'_>) -> Row {
    let (state, reason) = c.core.unwrap_or((info.state, info.reason.as_deref()));
    let failed = info.fault.is_some() || info.rolled.is_some();
    // The account's position on the contract is the instance's only while it trades it (armed
    // or paused); once stopped or before it starts, a position there is the trader's (0025).
    let owned = matches!(state, StrategyState::Armed | StrategyState::Paused);
    let held = c.position.filter(|_| owned);
    let active = held.is_some() || c.working;
    let phase = phase(state, reason, failed, active);
    let why = match phase {
        Phase::Faulted => info.fault.clone().or_else(|| info.rolled.clone()),
        Phase::Halted => Some("Daily loss limit reached; its trade was closed".to_owned()),
        Phase::Paused => reason.map(str::to_owned),
        Phase::Stopped if state == StrategyState::Disarmed => Some("Not started".to_owned()),
        Phase::Stopped => Some(reason.unwrap_or("Stopped by the trader").to_owned()),
        Phase::Armed | Phase::Running => None,
    }
    .map(sentence);
    let product = lines_feed::catalog::get(info.contract.product);
    let contract = product.as_ref().map_or_else(
        || format!("{:?}", info.contract.product),
        |p| info.contract.label(p).as_str().to_owned(),
    );
    let position = match held {
        Some(p) => {
            // Direction by shape too, as the DOM shows it.
            let side = if p.qty > 0 { "▲ Long" } else { "▼ Short" };
            let avg = match (p.avg_price, &product) {
                (Some(a), Some(pr)) => {
                    // `avg_price` is in price units: shown in the product's notation.
                    let dec = (pr.price.scale.den().max(1) as f64).log10().ceil() as usize;
                    let v = lines_trading::journal::price_value(pr, a);
                    format!(" @ {v:.dec$}")
                }
                _ => String::new(),
            };
            let qty = match &product {
                Some(pr) => qty_text(pr, p.qty.abs()),
                None => p.qty.abs().to_string(),
            };
            format!("{side} {qty}{avg}")
        }
        None => "Flat".to_owned(),
    };
    let mine: Vec<&Trade> = c
        .trades
        .iter()
        .filter(|t| Some(t.trade_date) == c.today && is_its_trade(t, info))
        .collect();
    let n = mine.len();
    let trades = format!("{n} closed today");
    // Today's net: closed trades (fees included) plus the open position's P&L. Money only:
    // ticks of trades with different sizes don't add up (as the journal's stats).
    let net = mine.iter().fold(Money::ZERO, |m, t| Money(m.0 + t.net().0))
        + held.map_or(Money::ZERO, |p| p.unrealised);
    // Always shown in money modes ($0.00 with nothing traded), never left out.
    let (pnl, pnl_sign) = if c.mode.money() {
        let mut s = String::new();
        let _ = pnl::write_money(&mut s, net);
        (Some(s), net.0.signum() as i8)
    } else {
        (None, 0)
    };
    let last = info.decisions.last().map(|d| {
        // The zone named: the line above gives the strategy's window in its own zone.
        format!(
            "{} {}  {}",
            crate::journal_view::clock_text(d.at)
                .get(6..)
                .unwrap_or_default(),
            crate::journal_view::zone_text(d.at),
            d.decision.reason
        )
    });
    let l = &info.limits;
    // Quantities in the product's own units (a crypto spot's aren't whole contracts).
    let mut limits = match &product {
        Some(pr) if pr.qty.decimals > 0 => format!("max {}", qty_text(pr, l.max_position)),
        Some(pr) => format!(
            "max {} contract{}",
            qty_text(pr, l.max_position),
            if l.max_position == 1 { "" } else { "s" }
        ),
        None => format!("max {}", l.max_position),
    };
    // P&L display off shows no money: the limit is still named.
    if c.mode.money() {
        limits.push_str(&format!(" · daily loss ${}", l.max_daily_loss));
    } else {
        limits.push_str(" · daily loss limit set");
    }
    limits.push_str(&format!(" · {} entries/min", l.max_orders_per_min));
    let hours: Vec<String> = l
        .hours
        .windows
        .iter()
        .map(|w| format!("{}-{}", w.start, w.end))
        .collect();
    if !hours.is_empty() {
        // New York's hours as "ET", as the display zone names it; any other zone by its name.
        let tz = match l.hours.tz.as_str() {
            lines_core::display_tz::EXCHANGE => "ET",
            z => z,
        };
        limits.push_str(&format!(" · {} {tz}", hours.join(", ")));
    }
    Row {
        id: info.id,
        label: info.label.clone(),
        strategy: format!("{} {}", info.strategy, info.version),
        contract,
        account: c.account_name.to_owned(),
        phase,
        why,
        position,
        trades,
        limits,
        pnl,
        pnl_sign,
        last,
        can_start: matches!(phase, Phase::Stopped | Phase::Paused),
        // Stop flattens: never offered for a position that is the trader's.
        can_stop: owned,
    }
}

/// Start: arm the instance (a person's command; the core refuses it with its reason if it
/// can't trade now).
pub fn start(id: StrategyId) -> Command {
    Command::ArmStrategy { id }
}

/// Stop one instance: its entries cancelled, its contract flattened.
pub fn stop(id: StrategyId) -> Command {
    Command::StopStrategies {
        id: Some(id),
        flatten: true,
    }
}

/// Stop every instance; `flatten` (the default) closes each account and contract they trade.
pub fn stop_all(flatten: bool) -> Command {
    Command::StopStrategies { id: None, flatten }
}

/// Stop all as commands: stop every instance (entries cancelled) at once, then, with
/// `flatten`, flatten only the instances in `owning` (Armed or Paused before the stop): a
/// never-started or already stopped instance's contract holds the trader's position, which
/// Stop all must not close.
pub fn stop_all_commands(flatten: bool, owning: &[StrategyId]) -> Vec<Command> {
    let mut v = vec![stop_all(false)];
    if flatten {
        v.extend(owning.iter().map(|&id| stop(id)));
    }
    v
}

/// Send Stop all from the UI (panel button, key, palette, pane windows). Which instances own
/// their positions comes from the UI's copy of the core's states (a short lock on the UI's
/// cache, never one the core takes).
pub fn submit_stop_all(trading: &crate::trading::Trading, flatten: bool, source: Source) {
    let owning = trading.cache.read().owning_strategies();
    let issued = std::time::Instant::now();
    for c in stop_all_commands(flatten, &owning) {
        trading.submit(source, issued, c);
    }
}

/// A fixed set of instances (tests and `--strategies-demo` screenshots).
#[cfg(any(test, feature = "screenshot"))]
pub struct DemoStrategies {
    infos: std::sync::Mutex<Vec<StrategyInfo>>,
    revision: AtomicU64,
}

#[cfg(any(test, feature = "screenshot"))]
impl DemoStrategies {
    pub fn new(infos: Vec<StrategyInfo>) -> DemoStrategies {
        DemoStrategies {
            infos: std::sync::Mutex::new(infos),
            revision: AtomicU64::new(1),
        }
    }

    /// Give instance `id` decisions behind its fills (screenshots: after a `strategy N`
    /// script, so its markers have reasons): the first fill's order is an entry at a support
    /// level, the others its stop; plus a signal it skipped at `skip_at`.
    pub fn attach(
        &self,
        id: StrategyId,
        fills: &[lines_trading::FillEvent],
        skip_at: Option<(UtcMillis, lines_core::Price)>,
    ) {
        use lines_strategy::{Decision, LevelRef};
        let level = |p: lines_core::Price| LevelRef {
            source: "demo".into(),
            kind: "support".into(),
            label: Some("Support".into()),
            price: p,
        };
        let mut out: Vec<DecisionRecord> = Vec::new();
        let mut seen: Vec<lines_trading::OrderId> = Vec::new();
        for f in fills {
            if seen.contains(&f.order) {
                continue;
            }
            let first = seen.is_empty();
            seen.push(f.order);
            out.push(DecisionRecord {
                at: f.at,
                decision: Decision {
                    rule: if first { "touch-long" } else { "stop" }.into(),
                    reason: if first {
                        "Buy at Support: price touched it from above".into()
                    } else {
                        "Stop: Support broke".into()
                    },
                    levels: vec![level(f.price)],
                    command: Some(Command::PlaceOrder(lines_trading::OrderSpec {
                        account: f.account.clone(),
                        contract: f.contract,
                        side: f.side,
                        qty: f.qty,
                        kind: lines_trading::OrderKind::Market,
                    })),
                },
                result: Some(Ok(vec![f.order])),
                not_sent: None,
            });
        }
        if let Some((at, price)) = skip_at {
            let mut d = Decision::note("skip-wide", "The target crosses the zone boundary");
            d.levels.push(level(price));
            out.push(DecisionRecord {
                at,
                decision: d,
                result: None,
                not_sent: None,
            });
        }
        out.sort_by_key(|d| d.at.0);
        let mut infos = self.infos.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(i) = infos.iter_mut().find(|i| i.id == id) {
            i.decisions = out;
        }
        self.revision.fetch_add(1, Ordering::AcqRel);
    }

    /// Four instances on `account`, one per interesting state, with decisions up to `now`:
    /// #3 (halted) on `contract`, the others on MES, NQ and MNQ of the same delivery (one
    /// instance per account and contract, as the core requires). A screenshot script can make
    /// #3's trade real (`strategy 3; …; halt 3`). Synthetic: a level-touch example, no real
    /// strategy.
    pub fn sample(account: &AccountId, contract: ContractKey, now: UtcMillis) -> DemoStrategies {
        use lines_strategy::Decision;
        let limits = StrategyLimits {
            max_position: 2,
            max_daily_loss: 500,
            max_orders_per_min: 6,
            hours: lines_trading::strategy::TradingHours {
                tz: "America/New_York".into(),
                windows: vec![lines_trading::strategy::HoursWindow::new("09:30", "16:00")],
            },
        };
        let at = |s: i64| UtcMillis(now.0 - s * 1000);
        let rec = |s: i64, rule: &str, reason: &str| DecisionRecord {
            at: at(s),
            decision: Decision::note(rule, reason),
            result: None,
            not_sent: None,
        };
        let products = [
            lines_feed::catalog::MES,
            lines_feed::catalog::NQ,
            contract.product,
            lines_feed::catalog::MNQ,
        ];
        // #3's limits match the screenshot script's (`strategy N`: 10 contracts, 100).
        let limit = |id: u32| StrategyLimits {
            max_position: if id == 3 { 10 } else { 2 },
            max_daily_loss: if id == 3 { 100 } else { 500 },
            ..limits.clone()
        };
        let info = |id: u32, state, reason: Option<&str>, decisions| StrategyInfo {
            id: StrategyId(id),
            label: format!("level-touch #{id}"),
            strategy: "level-touch".into(),
            version: "0.1.0".into(),
            account: account.clone(),
            contract: ContractKey {
                product: products[(id as usize - 1) % products.len()],
                ..contract
            },
            limits: limit(id),
            state,
            reason: reason.map(str::to_owned),
            fault: None,
            rolled: None,
            decisions,
        };
        DemoStrategies::new(vec![
            info(
                1,
                StrategyState::Armed,
                None,
                vec![rec(
                    95,
                    "skip-wide",
                    "Skipped: the target crosses the zone boundary",
                )],
            ),
            info(
                2,
                StrategyState::Paused,
                Some("manual order activity on NQZ26"),
                vec![rec(
                    240,
                    "touch-long",
                    "Buy limit at Support: price touched it from above",
                )],
            ),
            info(
                3,
                StrategyState::Paused,
                Some(lines_trading::strategy::DAILY_LOSS),
                Vec::new(),
            ),
            info(4, StrategyState::Disarmed, None, Vec::new()),
        ])
    }
}

#[cfg(any(test, feature = "screenshot"))]
impl StrategySource for DemoStrategies {
    fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    fn snapshot(&self) -> Vec<StrategyInfo> {
        self.infos.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

#[cfg(test)]
mod tests {
    use lines_core::{Month, Price, Qty};
    use lines_feed::catalog;
    use lines_trading::Side;
    use lines_trading::journal::TradeFill;

    use super::*;

    fn es() -> ContractKey {
        ContractKey::monthly(catalog::ES, 2026, Month::new(12).unwrap())
    }

    fn now() -> UtcMillis {
        UtcMillis::parse_iso("2026-11-10T15:00:00Z").unwrap()
    }

    fn infos() -> Vec<StrategyInfo> {
        DemoStrategies::sample(&AccountId::new("paper-1"), es(), now()).snapshot()
    }

    fn ctx<'a>(trades: &'a [Trade], pos: Option<&'a PositionView>, mode: PnlDisplay) -> RowCtx<'a> {
        RowCtx {
            core: None,
            position: pos,
            working: false,
            trades,
            today: Some(CivilDate::from_utc(now())),
            account_name: "Paper 1",
            mode,
        }
    }

    fn trade(source: Option<Source>, realized: i64) -> Trade {
        let fill = |side| TradeFill {
            trade_id: "f".into(),
            at: now(),
            side,
            price: Price(600_000),
            qty: Qty(1),
            fee: Money::ZERO,
            source,
            reconciled: false,
        };
        Trade {
            id: "t".into(),
            account: AccountId::new("paper-1"),
            contract: es(),
            side: Side::Buy,
            entry_time: now(),
            exit_time: now(),
            trade_date: CivilDate::from_utc(now()),
            max_qty: 1,
            entry_qty: 1,
            entry_value: 600_000,
            exit_qty: 1,
            exit_value: 600_000,
            realized: Money::whole(realized),
            fees: Money::ZERO,
            mae: Money::ZERO,
            mfe: Money::ZERO,
            low: Price(600_000),
            high: Price(600_000),
            fills: vec![fill(Side::Buy), fill(Side::Sell)],
            tags: Vec::new(),
            note: String::new(),
            screenshots: Vec::new(),
            incomplete: false,
        }
    }

    #[test]
    fn phases_are_words_and_daily_loss_reads_halted() {
        let i = infos();
        let rows: Vec<Row> = i
            .iter()
            .map(|i| row(i, &ctx(&[], None, PnlDisplay::Both)))
            .collect();
        let words: Vec<&str> = rows.iter().map(|r| r.phase.word()).collect();
        assert_eq!(words, ["ARMED", "PAUSED", "HALTED", "STOPPED"]);
        assert_eq!(
            rows[1].why.as_deref(),
            Some("Manual order activity on NQZ26")
        );
        assert_eq!(rows[3].why.as_deref(), Some("Not started"));
        // Starting is explicit and only from a stopped or paused instance; a halted one waits
        // for the next trade date.
        let starts: Vec<bool> = rows.iter().map(|r| r.can_start).collect();
        assert_eq!(starts, [false, true, false, true]);
        let stops: Vec<bool> = rows.iter().map(|r| r.can_stop).collect();
        assert_eq!(stops, [true, true, true, false]);
        assert_eq!(
            (rows[0].contract.as_str(), rows[0].account.as_str()),
            ("MESZ26", "Paper 1")
        );
        assert_eq!(
            rows[0].limits,
            "max 2 contracts · daily loss $500 · 6 entries/min · 09:30-16:00 ET"
        );
        let mut one = i[0].clone();
        one.limits.max_position = 1;
        assert!(
            row(&one, &ctx(&[], None, PnlDisplay::Both))
                .limits
                .starts_with("max 1 contract ·"),
        );
        let off = row(&i[0], &ctx(&[], None, PnlDisplay::Off));
        assert!(!off.limits.contains('$'), "{}", off.limits);
        assert!(
            off.limits.contains("daily loss limit set"),
            "{}",
            off.limits
        );
        assert_eq!(
            rows[2].why.as_deref(),
            Some("Daily loss limit reached; its trade was closed")
        );
        // Money modes always show today's P&L, $0.00 with nothing traded.
        assert_eq!(rows[3].pnl.as_deref(), Some("$0.00"));
        assert!(
            rows[0]
                .last
                .as_deref()
                .unwrap()
                .ends_with("the zone boundary")
        );
    }

    #[test]
    fn core_state_wins_and_failures_are_faulted() {
        let mut i = infos().remove(0);
        let c = RowCtx {
            core: Some((StrategyState::Stopped, None)),
            ..ctx(&[], None, PnlDisplay::Both)
        };
        let r = row(&i, &c);
        assert_eq!(r.phase, Phase::Stopped);
        assert_eq!(r.why.as_deref(), Some("Stopped by the trader"));
        i.fault = Some("the strategy panicked".into());
        let r = row(&i, &ctx(&[], None, PnlDisplay::Both));
        assert_eq!((r.phase, r.can_start), (Phase::Faulted, false));
        assert_eq!(r.why.as_deref(), Some("The strategy panicked"));
    }

    #[test]
    fn armed_with_a_position_is_running_and_pnl_counts_only_its_trades() {
        let mut i = infos().remove(0);
        i.contract = es();
        let pos = PositionView {
            account: i.account.clone(),
            contract: es(),
            qty: -2,
            avg_price: Some(601_225.0),
            unrealised: Money::whole(-25),
        };
        let trades = [
            trade(Some(Source::Strategy(i.id)), 100),
            trade(Some(Source::Dom), 1000),
            trade(Some(Source::Strategy(StrategyId(9))), 1000),
        ];
        let r = row(&i, &ctx(&trades, Some(&pos), PnlDisplay::Both));
        assert_eq!(r.phase, Phase::Running);
        assert_eq!(r.position, "▼ Short 2 @ 6012.25");
        assert_eq!(r.trades, "1 closed today");
        assert_eq!((r.pnl.as_deref(), r.pnl_sign), (Some("+$75.00"), 1));
        // P&L display off: no money anywhere; ticks only: no money either.
        for m in [PnlDisplay::Off, PnlDisplay::Ticks] {
            assert_eq!(row(&i, &ctx(&trades, Some(&pos), m)).pnl, None);
        }
    }

    /// A stopped or never-started instance doesn't own the account's position on its
    /// contract: no position, open P&L or Stop (which flattens) on its row.
    #[test]
    fn a_traders_position_is_never_the_stopped_strategys() {
        let i = infos().remove(3);
        assert_eq!(i.state, StrategyState::Disarmed);
        let pos = PositionView {
            account: i.account.clone(),
            contract: i.contract,
            qty: 2,
            avg_price: Some(601_225.0),
            unrealised: Money::whole(-25),
        };
        let r = row(&i, &ctx(&[], Some(&pos), PnlDisplay::Both));
        assert_eq!((r.phase, r.position.as_str()), (Phase::Stopped, "Flat"));
        assert_eq!(r.pnl.as_deref(), Some("$0.00"));
        assert!(!r.can_stop);
        let c = RowCtx {
            core: Some((StrategyState::Stopped, None)),
            ..ctx(&[], Some(&pos), PnlDisplay::Both)
        };
        assert!(!row(&i, &c).can_stop);
    }

    /// Stop all stops everything first, then flattens only what strategies owned.
    #[test]
    fn stop_all_flattens_only_owned_positions() {
        let (a, b) = (StrategyId(1), StrategyId(4));
        assert_eq!(
            stop_all_commands(true, &[a, b]),
            vec![stop_all(false), stop(a), stop(b)]
        );
        assert_eq!(stop_all_commands(true, &[]), vec![stop_all(false)]);
        assert_eq!(stop_all_commands(false, &[a]), vec![stop_all(false)]);
    }

    #[test]
    fn commands_are_manual_controls_that_flatten_by_default() {
        let id = StrategyId(3);
        assert_eq!(start(id), Command::ArmStrategy { id });
        assert_eq!(
            stop(id),
            Command::StopStrategies {
                id: Some(id),
                flatten: true
            }
        );
        assert_eq!(
            stop_all(true),
            Command::StopStrategies {
                id: None,
                flatten: true
            }
        );
    }
}
