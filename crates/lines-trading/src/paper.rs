//! The paper broker (decisions 0004, 0005): a deterministic exchange simulation behind the same
//! [`Broker`] trait as live brokers.
//!
//! - **Acks** arrive after a configurable latency (requests are queued and released by
//!   [`Broker::poll`]); nothing works before its ack.
//! - **Validation at the broker**: tick grid, positive quantity, the market open per the
//!   product's sessions calendar (closed, halted or outside every session → rejected).
//! - **Fill models** ([`FillModel`]) for resting limit orders, driven by trades:
//!   - *trade-through* (default, pessimistic): a buy limit at `L` fills only when a trade prints
//!     **below** `L` (a sell, above), up to the traded quantity, at `L`;
//!   - *touch* (optimistic, opt-in): a trade **at** `L` is enough;
//!   - *queue estimate* (needs depth): joining a price puts the order behind the book's size
//!     there; trades at the price eat the queue ahead first, a shrinking level shortens it
//!     (cancels assumed ahead only as far as the level shrinks), trades through fill it.
//!
//!   Several paper orders share a trade's quantity (oldest first): the simulation never fills
//!   more than printed.
//! - **Market orders** (and marketable limits, triggered stops) **walk the book** level by
//!   level from the touch; liquidity they took is not reused until the book updates that level.
//!   With no book on that side they fill at the last trade price; with neither they reject.
//! - **Stops trigger on trades** at or through the trigger.
//! - **Fees** per contract (per quantity unit), per side, from the account's balance.
//! - **Options** (decision 0028) trade on their own quote, never the underlying's price:
//!   - each `MarketInput::OptionQuote` replaces the series' book with its bid and ask, one level
//!     each, of unlimited size (quotes carry no size: a known gap);
//!   - an order that crosses the quote takes it at once, at the quote's price (a buy limit
//!     above the ask fills at the ask);
//!   - a resting limit fills only when a later quote goes **through** it, at its limit
//!     (trade-through; *touch* also fills at the limit, and queue estimate, which needs depth,
//!     behaves as trade-through);
//!   - prints, on the option or its underlying, never fill an option order;
//!   - **a missing side never fills**: no bid, a zero bid or a crossed quote leaves that side
//!     empty, so nothing can fill against it, and a market order (Flatten) is cancelled rather
//!     than filling at a last price;
//!   - **a stale quote never fills**: older than `option_quote_stale_ms`, an order that would
//!     cross it rests instead and a market order is cancelled, until a fresh quote comes;
//!   - orders and fills only in the regular session (options don't trade extended hours);
//!   - expiry: [`Broker::settle`], asked by the core at the close.
//! - **Brackets / OCO are server-side**: the broker holds the exits (acked, inactive) until the
//!   entry fills, sizes them to the filled quantity, and cancels or shrinks the sibling when
//!   one exit fills. They are part of the broker's saved state, so they survive the app
//!   restarting.

use std::collections::{BTreeMap, HashMap, VecDeque};

use lines_core::{ContractKey, OrderBook, Price, Qty, SessionKind, Side as BookSide, UtcMillis};
use serde::{Deserialize, Serialize};

use crate::broker::{
    Broker, BrokerAccount, BrokerCaps, BrokerEvent, BrokerPosition, BrokerRequest, BrokerSnapshot,
};
use crate::market::MarketInput;
use crate::order::{Order, OrderState, Role};
use crate::position::Position;
use crate::types::{AccountId, ContractSpec, Money, OrderId, OrderKind, Side, contract_serde};

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FillModel {
    #[default]
    TradeThrough,
    Touch,
    QueueEstimate,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaperConfig {
    /// Request → ack (and → cancel/modify confirmation) delay.
    pub latency_ms: i64,
    pub fill_model: FillModel,
    /// Fee per contract (quantity unit) per side, by product id; default for others.
    pub fees: BTreeMap<u32, Money>,
    pub default_fee: Money,
    /// Reject orders while the product's market is closed (sessions calendar).
    pub check_sessions: bool,
    /// An option quote older than this fills nothing.
    #[serde(default = "default_quote_stale_ms")]
    pub option_quote_stale_ms: i64,
}

fn default_quote_stale_ms() -> i64 {
    crate::risk::DEFAULT_QUOTE_STALE_MS
}

/// A quote side's size in the one-level book: quotes carry none, so it never runs out.
const QUOTE_SIZE: i64 = 1 << 40;

impl Default for PaperConfig {
    fn default() -> Self {
        PaperConfig {
            latency_ms: 25,
            fill_model: FillModel::TradeThrough,
            fees: BTreeMap::new(),
            default_fee: Money::ZERO,
            check_sessions: true,
            option_quote_stale_ms: default_quote_stale_ms(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaperAccount {
    pub balance: Money,
    pub positions: Vec<PaperPosition>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaperPosition {
    #[serde(with = "contract_serde")]
    pub contract: ContractKey,
    pub position: Position,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaperOrder {
    pub order: Order,
    /// Queue estimate: quantity ahead of us at our price.
    #[serde(default)]
    pub queue_ahead: i64,
    /// A bracket exit waiting for its entry to fill.
    #[serde(default)]
    pub held: bool,
}

/// What the paper broker saves (its accounts and open orders).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaperState {
    pub accounts: BTreeMap<String, PaperAccount>,
    pub orders: Vec<PaperOrder>,
}

#[derive(Default)]
struct Market {
    book: OrderBook,
    last: Option<Price>,
    /// Liquidity taken by our market orders since the book last updated each level.
    taken: HashMap<(bool, i64), i64>,
    /// When an option's book was last quoted.
    quoted_at: Option<UtcMillis>,
}

pub struct PaperBroker {
    cfg: PaperConfig,
    accounts: BTreeMap<AccountId, PaperAccount>,
    orders: BTreeMap<OrderId, PaperOrder>,
    pending: VecDeque<(UtcMillis, BrokerRequest)>,
    markets: HashMap<ContractKey, Market>,
    specs: HashMap<ContractKey, ContractSpec>,
    out: Vec<BrokerEvent>,
}

fn book_side(side: Side) -> BookSide {
    match side {
        Side::Buy => BookSide::Bid,
        Side::Sell => BookSide::Ask,
    }
}

impl PaperBroker {
    pub fn new(cfg: PaperConfig) -> PaperBroker {
        PaperBroker {
            cfg,
            accounts: BTreeMap::new(),
            orders: BTreeMap::new(),
            pending: VecDeque::new(),
            markets: HashMap::new(),
            specs: HashMap::new(),
            out: Vec::new(),
        }
    }

    pub fn config(&self) -> &PaperConfig {
        &self.cfg
    }

    pub fn set_fill_model(&mut self, m: FillModel) {
        self.cfg.fill_model = m;
    }

    /// Open a paper account (or keep an existing one).
    pub fn open_account(&mut self, id: AccountId, balance: Money) {
        self.accounts.entry(id).or_insert(PaperAccount {
            balance,
            positions: Vec::new(),
        });
    }

    /// Restore saved state.
    pub fn from_state(cfg: PaperConfig, state: PaperState) -> PaperBroker {
        let mut b = PaperBroker::new(cfg);
        b.accounts = state
            .accounts
            .into_iter()
            .map(|(k, v)| (AccountId(k), v))
            .collect();
        b.orders = state.orders.into_iter().map(|o| (o.order.id, o)).collect();
        b
    }

    pub fn state(&self) -> PaperState {
        PaperState {
            accounts: self
                .accounts
                .iter()
                .map(|(k, v)| (k.0.clone(), v.clone()))
                .collect(),
            orders: self.orders.values().cloned().collect(),
        }
    }

    fn spec(&mut self, contract: ContractKey) -> Option<ContractSpec> {
        if let Some(s) = self.specs.get(&contract) {
            return Some(s.clone());
        }
        let product = lines_feed::catalog::get(contract.product)?;
        let fee = self
            .cfg
            .fees
            .get(&contract.product.0)
            .copied()
            .unwrap_or(self.cfg.default_fee);
        let s = ContractSpec::new(product, contract, fee)?;
        self.specs.insert(contract, s.clone());
        Some(s)
    }

    /// The contract's market is open: an option's only in the regular session.
    fn market_open(&self, contract: ContractKey, now: UtcMillis) -> bool {
        if !self.cfg.check_sessions {
            return true;
        }
        let Some(cal) = lines_feed::catalog::sessions(contract.product) else {
            return true;
        };
        match contract.option_key() {
            Some(_) => cal.session_at(now, SessionKind::Rth).is_some(),
            None => cal.is_open(now),
        }
    }

    /// An option whose quote is missing or too old: nothing fills on it.
    fn quote_stale(&self, contract: ContractKey, now: UtcMillis) -> bool {
        contract.option_key().is_some()
            && self
                .markets
                .get(&contract)
                .and_then(|m| m.quoted_at)
                .is_none_or(|t| now.0 - t.0 > self.cfg.option_quote_stale_ms)
    }

    fn reject(&mut self, id: OrderId, reason: impl Into<String>, at: UtcMillis) {
        self.out.push(BrokerEvent::Rejected {
            id,
            reason: reason.into(),
            at,
        });
    }

    /// Validate an order at the broker (tick grid, quantity, account, session).
    fn validate(&mut self, o: &Order, now: UtcMillis) -> Result<(), String> {
        let spec = self
            .spec(o.contract)
            .ok_or_else(|| "unknown contract".to_owned())?;
        if !self.accounts.contains_key(&o.account) {
            return Err(format!("no paper account {}", o.account));
        }
        if o.qty.0 <= 0 {
            return Err("quantity must be positive".into());
        }
        if let Some(p) = o.kind.prices().find(|p| !spec.on_tick_grid(*p)) {
            return Err(format!(
                "price {} is not on the {} tick grid",
                spec.product.price.format::<32>(p),
                spec.product.symbol
            ));
        }
        if !self.market_open(o.contract, now) {
            return Err(format!("{} market is closed", spec.product.symbol));
        }
        Ok(())
    }

    fn handle(&mut self, now: UtcMillis, req: BrokerRequest) {
        match req {
            BrokerRequest::Place(o) => self.place(now, o),
            BrokerRequest::Bracket { entry, exits } => {
                let entry_id = entry.id;
                if let Err(e) = self.validate(&entry, now) {
                    self.reject(entry_id, e.clone(), now);
                    for x in exits {
                        self.reject(x.id, format!("bracket entry not placed: {e}"), now);
                    }
                    return;
                }
                for x in exits {
                    // Held server-side until the entry fills.
                    if let Err(e) = self.validate(&x, now) {
                        self.reject(x.id, e, now);
                        continue;
                    }
                    self.out.push(BrokerEvent::Acked { id: x.id, at: now });
                    let mut x = x;
                    x.state = OrderState::Acked;
                    self.orders.insert(
                        x.id,
                        PaperOrder {
                            order: x,
                            queue_ahead: 0,
                            held: true,
                        },
                    );
                }
                self.place(now, entry);
            }
            BrokerRequest::Modify { id, qty, price } => self.modify(now, id, qty, price),
            BrokerRequest::Cancel { id } => self.cancel(now, id, "canceled"),
        }
    }

    fn place(&mut self, now: UtcMillis, mut o: Order) {
        if let Err(e) = self.validate(&o, now) {
            self.reject(o.id, e, now);
            return;
        }
        o.state = OrderState::Acked;
        self.out.push(BrokerEvent::Acked { id: o.id, at: now });
        let id = o.id;
        let queue = self.queue_at(&o);
        let reduce = o.reduce_only.then(|| (o.account.clone(), o.contract));
        self.orders.insert(
            id,
            PaperOrder {
                order: o,
                queue_ahead: queue,
                held: false,
            },
        );
        // A reduce-only order meets the position as it is *now* (it may have closed while the
        // order was on its way): shrink it to what is left to close, or cancel it.
        if let Some((account, contract)) = reduce {
            let position = self.position_of(&account, contract);
            self.shrink_reduce_only(now, &account, contract, position);
            if !self.orders.contains_key(&id) {
                return;
            }
        }
        self.activate(now, id);
    }

    /// Book size at the order's limit price on its own side (queue estimate).
    fn queue_at(&self, o: &Order) -> i64 {
        let OrderKind::Limit { price } = o.kind else {
            return 0;
        };
        self.markets
            .get(&o.contract)
            .map(|m| {
                m.book
                    .range(book_side(o.side), price, Price(price.0 + 1))
                    .map(|(_, l)| l.qty.0)
                    .sum()
            })
            .unwrap_or(0)
    }

    /// An acked order goes live: market orders execute now, marketable limits take what
    /// they can, the rest rests (working).
    fn activate(&mut self, now: UtcMillis, id: OrderId) {
        let Some(po) = self.orders.get(&id) else {
            return;
        };
        let o = po.order.clone();
        match o.kind {
            OrderKind::Market => {
                self.execute(now, id, None);
                // A market order never rests: what the book couldn't fill is cancelled.
                if self.orders.get(&id).is_some_and(|p| p.order.is_open()) {
                    self.cancel(now, id, "no more liquidity for the market order");
                }
            }
            OrderKind::Limit { price } => {
                self.set_working(now, id);
                if self.marketable(&o, price) {
                    self.execute(now, id, Some(price));
                }
            }
            OrderKind::Stop { .. } | OrderKind::StopLimit { .. } => self.set_working(now, id),
        }
    }

    fn set_working(&mut self, now: UtcMillis, id: OrderId) {
        if let Some(po) = self.orders.get_mut(&id)
            && po.order.state == OrderState::Acked
        {
            po.order.state = OrderState::Working;
            self.out.push(BrokerEvent::Working { id, at: now });
        }
    }

    /// A limit order that crosses the opposite best.
    fn marketable(&self, o: &Order, limit: Price) -> bool {
        let Some(m) = self.markets.get(&o.contract) else {
            return false;
        };
        match o.side {
            Side::Buy => m.book.best_ask().is_some_and(|(a, _)| a <= limit),
            Side::Sell => m.book.best_bid().is_some_and(|(b, _)| b >= limit),
        }
    }

    /// Take liquidity from the book (opposite side) up to `limit` (`None`: any price).
    fn execute(&mut self, now: UtcMillis, id: OrderId, limit: Option<Price>) {
        let Some(po) = self.orders.get(&id) else {
            return;
        };
        let (contract, side) = (po.order.contract, po.order.side);
        if self.quote_stale(contract, now) {
            return; // it rests (a market order is cancelled) until a fresh quote
        }
        let mut left = po.order.remaining().0;
        let mut fills: Vec<(Price, i64)> = Vec::new();
        {
            let Some(m) = self.markets.get_mut(&contract) else {
                return;
            };
            let opp = book_side(side.opposite());
            let is_bid = opp == BookSide::Bid;
            let levels: Vec<(Price, i64)> = m.book.levels(opp).map(|(p, l)| (p, l.qty.0)).collect();
            for (p, size) in levels {
                if left == 0 {
                    break;
                }
                if let Some(l) = limit
                    && match side {
                        Side::Buy => p > l,
                        Side::Sell => p < l,
                    }
                {
                    break;
                }
                let taken = m.taken.entry((is_bid, p.0)).or_insert(0);
                let avail = size - *taken;
                if avail <= 0 {
                    continue;
                }
                let q = avail.min(left);
                *taken += q;
                left -= q;
                fills.push((p, q));
            }
            // No book on that side: fill at the last trade (market orders only; an option
            // never fills on a missing side).
            if fills.is_empty()
                && limit.is_none()
                && contract.option_key().is_none()
                && m.book.levels(opp).next().is_none()
                && let Some(last) = m.last
            {
                fills.push((last, left));
            }
        }
        for (p, q) in fills {
            self.fill(now, id, p, Qty(q));
        }
    }

    fn fill(&mut self, now: UtcMillis, id: OrderId, price: Price, qty: Qty) {
        self.fill_with_fee(now, id, price, qty, true);
    }

    fn fill_with_fee(&mut self, now: UtcMillis, id: OrderId, price: Price, qty: Qty, charge: bool) {
        let Some(po) = self.orders.get_mut(&id) else {
            return;
        };
        let qty = Qty(qty.0.min(po.order.remaining().0));
        if qty.0 <= 0 {
            return;
        }
        // A stable execution id: the order and how much it had filled before (unique per
        // fill, the same if the broker's history is replayed).
        let trade_id = format!("paper-{}-{}", id.0, po.order.filled.0);
        po.order.apply_fill(price, qty, now);
        let o = po.order.clone();
        if !o.is_open() {
            self.orders.remove(&id);
        }
        let Some(spec) = self.spec(o.contract) else {
            return;
        };
        let fee = if charge { spec.fee(qty) } else { Money::ZERO };
        let acct = self.accounts.entry(o.account.clone()).or_default();
        let pos = match acct.positions.iter_mut().find(|p| p.contract == o.contract) {
            Some(p) => &mut p.position,
            None => {
                acct.positions.push(PaperPosition {
                    contract: o.contract,
                    position: Position::default(),
                });
                &mut acct.positions.last_mut().expect("just pushed").position
            }
        };
        let realised = pos.apply_fill(o.side, price, qty);
        let position = pos.qty();
        acct.balance += spec.money(realised, 1) - fee;
        acct.positions.retain(|p| !p.position.is_flat());
        self.out.push(BrokerEvent::Fill {
            id,
            trade_id,
            price,
            qty,
            fee,
            at: now,
        });
        self.after_fill(now, &o, qty);
        self.shrink_reduce_only(now, &o.account, o.contract, position);
    }

    /// Reduce-only orders never close more than the position: shrink them as it shrinks,
    /// cancel them when it is flat (or on their side).
    fn shrink_reduce_only(
        &mut self,
        now: UtcMillis,
        account: &AccountId,
        contract: ContractKey,
        position: i64,
    ) {
        let ids: Vec<OrderId> = self
            .orders
            .values()
            .filter(|p| {
                // A held bracket exit has nothing live to budget (its entry fills it again).
                p.order.reduce_only
                    && !p.held
                    && &p.order.account == account
                    && p.order.contract == contract
            })
            .map(|p| p.order.id)
            .collect();
        // What the position lets reduce-only orders close, oldest first. An OCO pair (a stop
        // and a target for the same position: only one of them can fill) is budgeted once:
        // both may cover what is left, and the second of the pair consumes nothing.
        let mut closable = position.abs();
        let mut budgeted: Vec<(OrderId, i64)> = Vec::with_capacity(ids.len());
        for id in ids {
            let Some(po) = self.orders.get(&id) else {
                continue;
            };
            let sibling = po
                .order
                .oco
                .filter(|s| self.orders.contains_key(s))
                .or_else(|| {
                    self.orders
                        .values()
                        .find(|q| q.order.oco == Some(id))
                        .map(|q| q.order.id)
                });
            // The pair's share: what its first order was allowed (closable before it).
            let paired = sibling.and_then(|s| budgeted.iter().find(|(b, _)| *b == s).map(|b| b.1));
            // A bracket exit whose entry still works protects what the entry fills next.
            let entry_working = po
                .order
                .parent
                .and_then(|e| self.orders.get(&e))
                .is_some_and(|e| e.order.is_open());
            let Some(po) = self.orders.get_mut(&id) else {
                continue;
            };
            let opposes = position != 0 && po.order.side.sign() == -position.signum();
            let pool = paired.unwrap_or(closable);
            let allowed = if opposes { pool } else { 0 };
            let left = po.order.remaining().0;
            if allowed == 0 && entry_working {
                // Nothing to close now (a manual close), but the entry still works: hold the
                // exit, as after its sibling's fill; the entry's next fill grows it again.
                po.order.qty = Qty(po.order.filled.0);
                po.held = true;
                let order = po.order.clone();
                self.out.push(BrokerEvent::Modified { order });
                continue;
            }
            if allowed == 0 {
                self.cancel(now, id, "reduce-only: nothing left to close");
                continue;
            }
            if left > allowed {
                po.order.qty = Qty(po.order.filled.0 + allowed);
                let order = po.order.clone();
                self.out.push(BrokerEvent::Modified { order });
            }
            if paired.is_none() {
                budgeted.push((id, allowed));
                closable -= left.min(allowed);
            }
        }
    }

    /// Bracket bookkeeping after a fill (server-side OCO, one-updates-other).
    ///
    /// Every live exit's remaining quantity is the bracket's **open** quantity: what the entry
    /// has filled less what the exits have closed. An entry fill grows each exit by the fill;
    /// an exit fill shrinks its sibling by the fill. When nothing is open: with the entry
    /// finished, the sibling is cancelled (OCO done); with the entry still working, the
    /// sibling is held (inactive, nothing to close) until the entry fills again.
    fn after_fill(&mut self, now: UtcMillis, o: &Order, qty: Qty) {
        match o.role {
            Role::Entry => {
                // Live exits only ever close what the entry opened: reduce-only from its first
                // fill, so they shrink and cancel with the position (a manual close, a Flatten
                // of part, the rest of the entry cancelled) like any exit, never outliving it.
                let exits: Vec<OrderId> = self
                    .orders
                    .values()
                    .filter(|p| p.order.parent == Some(o.id))
                    .map(|p| p.order.id)
                    .collect();
                for x in exits {
                    let Some(po) = self.orders.get_mut(&x) else {
                        continue;
                    };
                    // Never activated yet: its quantity is still the bracket's request.
                    let fresh = po.held && po.order.state == OrderState::Acked;
                    po.order.qty = if fresh {
                        qty
                    } else {
                        Qty(po.order.qty.0 + qty.0)
                    };
                    po.order.reduce_only = true;
                    let order = po.order.clone();
                    self.out.push(BrokerEvent::Modified { order });
                    if po.held {
                        po.held = false;
                        self.activate(now, x);
                    }
                }
            }
            Role::TakeProfit | Role::StopLoss => {
                let Some(sib) = o.oco else { return };
                let entry_open = o
                    .parent
                    .and_then(|p| self.orders.get(&p))
                    .is_some_and(|e| e.order.is_open());
                let Some(sp) = self.orders.get_mut(&sib) else {
                    return;
                };
                let left = (sp.order.remaining().0 - qty.0).max(0);
                if left == 0 && !entry_open {
                    self.cancel(now, sib, "OCO: the other exit filled");
                    return;
                }
                sp.order.qty = Qty(sp.order.filled.0 + left);
                if left == 0 {
                    sp.held = true; // nothing to close until the entry fills again
                }
                let order = sp.order.clone();
                self.out.push(BrokerEvent::Modified { order });
            }
            _ => {}
        }
    }

    fn modify(&mut self, now: UtcMillis, id: OrderId, qty: Option<Qty>, price: Option<Price>) {
        let Some(po) = self.orders.get(&id) else {
            self.reject(id, "no such open order", now);
            return;
        };
        // Build the modified order, validate it, then apply it.
        let mut o = po.order.clone();
        if let Some(q) = qty {
            if q.0 <= o.filled.0 {
                self.reject(id, "new quantity is not above the filled quantity", now);
                return;
            }
            o.qty = q;
        }
        if let Some(p) = price {
            o.kind = match o.kind {
                OrderKind::Limit { .. } => OrderKind::Limit { price: p },
                OrderKind::Stop { .. } => OrderKind::Stop { trigger: p },
                OrderKind::StopLimit { trigger, .. } if o.triggered => {
                    OrderKind::StopLimit { trigger, limit: p }
                }
                OrderKind::StopLimit { limit, .. } => OrderKind::StopLimit { trigger: p, limit },
                OrderKind::Market => {
                    self.reject(id, "a market order has no price", now);
                    return;
                }
            };
        }
        if let Some(spec) = self.spec(o.contract)
            && o.kind.prices().any(|p| !spec.on_tick_grid(p))
        {
            self.reject(id, "price is not on the tick grid", now);
            return;
        }
        o.updated = now;
        let queue = self.queue_at(&o);
        let held = self.orders.get(&id).is_some_and(|p| p.held);
        if let Some(po) = self.orders.get_mut(&id) {
            po.order = o.clone();
            po.queue_ahead = queue; // a moved order goes to the back of its new queue
        }
        self.out.push(BrokerEvent::Modified { order: o.clone() });
        // A reduce-only order never closes more than the position, whatever it was changed to.
        if o.reduce_only {
            let position = self.position_of(&o.account, o.contract);
            self.shrink_reduce_only(now, &o.account, o.contract, position);
            if !self.orders.contains_key(&id) {
                return;
            }
        }
        // Moved across the market: it takes what it now crosses (an option only in its session).
        if let OrderKind::Limit { price } = o.kind
            && !held
            && self.marketable(&o, price)
            && (o.contract.option_key().is_none() || self.market_open(o.contract, now))
        {
            self.execute(now, id, Some(price));
        }
    }

    /// The account's position in `contract` here (signed quantity units).
    fn position_of(&self, account: &AccountId, contract: ContractKey) -> i64 {
        self.accounts
            .get(account)
            .and_then(|a| a.positions.iter().find(|p| p.contract == contract))
            .map_or(0, |p| p.position.qty())
    }

    fn cancel(&mut self, now: UtcMillis, id: OrderId, reason: &str) {
        let Some(po) = self.orders.remove(&id) else {
            return;
        };
        self.out.push(BrokerEvent::Cancelled {
            id,
            reason: reason.into(),
            at: now,
        });
        // An entry cancelled takes with it the exits that have nothing to close: all of them
        // before any fill, else those held with nothing open.
        if po.order.role == Role::Entry {
            let exits: Vec<OrderId> = self
                .orders
                .values()
                .filter(|p| p.order.parent == Some(id))
                .filter(|p| po.order.filled.0 == 0 || p.order.remaining().0 == 0)
                .map(|p| p.order.id)
                .collect();
            for x in exits {
                self.cancel(now, x, "bracket entry canceled");
            }
        }
    }

    fn on_trade(&mut self, now: UtcMillis, contract: ContractKey, price: Price, qty: Qty) {
        // Stops first: a trade at or through the trigger arms them as market/limit orders.
        let triggered: Vec<OrderId> = self
            .orders
            .values()
            .filter(|p| p.order.contract == contract && !p.held && !p.order.triggered)
            .filter(|p| match p.order.kind {
                OrderKind::Stop { trigger } | OrderKind::StopLimit { trigger, .. } => {
                    match p.order.side {
                        Side::Buy => price >= trigger,
                        Side::Sell => price <= trigger,
                    }
                }
                _ => false,
            })
            .map(|p| p.order.id)
            .collect();
        for id in triggered {
            let Some(po) = self.orders.get_mut(&id) else {
                continue;
            };
            po.order.triggered = true;
            match po.order.kind {
                OrderKind::Stop { .. } => {
                    self.execute(now, id, None);
                    if self.orders.get(&id).is_some_and(|p| p.order.is_open()) {
                        // No book: the triggering trade's price.
                        self.fill(now, id, price, Qty(i64::MAX));
                    }
                }
                OrderKind::StopLimit { limit, .. } => {
                    let o = po.order.clone();
                    po.queue_ahead = 0;
                    if self.marketable(&o, limit) {
                        self.execute(now, id, Some(limit));
                    }
                }
                _ => {}
            }
        }
        // Resting limits (and triggered stop-limits) fill from the trade per the model.
        let mut left = qty.0;
        let resting: Vec<OrderId> = self
            .orders
            .values()
            .filter(|p| p.order.contract == contract && !p.held && p.order.is_open())
            .map(|p| p.order.id)
            .collect();
        for id in resting {
            if left <= 0 {
                break;
            }
            let Some(po) = self.orders.get_mut(&id) else {
                continue;
            };
            let limit = match po.order.kind {
                OrderKind::Limit { price } => price,
                OrderKind::StopLimit { limit, .. } if po.order.triggered => limit,
                _ => continue,
            };
            let through = match po.order.side {
                Side::Buy => price < limit,
                Side::Sell => price > limit,
            };
            let at = price == limit;
            let can = match self.cfg.fill_model {
                FillModel::TradeThrough => {
                    if through {
                        left
                    } else {
                        0
                    }
                }
                FillModel::Touch => {
                    if through || at {
                        left
                    } else {
                        0
                    }
                }
                FillModel::QueueEstimate => {
                    if through {
                        left
                    } else if at {
                        // The queue ahead trades first.
                        let eat = po.queue_ahead.min(left);
                        po.queue_ahead -= eat;
                        left -= eat;
                        left
                    } else {
                        0
                    }
                }
            };
            let q = can.min(po.order.remaining().0);
            if q > 0 {
                left -= q;
                self.fill(now, id, limit, Qty(q));
            }
        }
    }

    /// An option's quote: its book becomes the quote's usable sides, then resting limits it
    /// went through fill at their limits. Outside the regular session, or stamped too old to
    /// count, it fills nothing (but stays the book).
    fn on_option_quote(
        &mut self,
        now: UtcMillis,
        contract: ContractKey,
        bid: Option<Price>,
        ask: Option<Price>,
        last: Option<Price>,
        time: UtcMillis,
    ) {
        let (bid, ask) = crate::market::quote_sides(bid, ask);
        let Some(m) = self.markets.get_mut(&contract) else {
            return;
        };
        // A stamp from the future counts as now, so it still goes stale and blocks no real quote.
        let time = time.min(now);
        // A late or re-seeded quote never replaces a newer one.
        if m.quoted_at.is_some_and(|t| time < t) {
            return;
        }
        m.book.clear();
        m.taken.clear();
        m.quoted_at = Some(time);
        m.last = last.or(m.last);
        if let Some(p) = bid {
            m.book.set(BookSide::Bid, p, Qty(QUOTE_SIZE), 0);
        }
        if let Some(p) = ask {
            m.book.set(BookSide::Ask, p, Qty(QUOTE_SIZE), 0);
        }
        if !self.market_open(contract, now) || self.quote_stale(contract, now) {
            return;
        }
        let touch = self.cfg.fill_model == FillModel::Touch;
        let resting: Vec<(OrderId, Price)> = self
            .orders
            .values()
            .filter(|p| p.order.contract == contract && !p.held && p.order.is_open())
            .filter_map(|p| match p.order.kind {
                OrderKind::Limit { price } => {
                    let through = match p.order.side {
                        Side::Buy => ask.is_some_and(|a| a < price || (touch && a == price)),
                        Side::Sell => bid.is_some_and(|b| b > price || (touch && b == price)),
                    };
                    through.then_some((p.order.id, price))
                }
                _ => None,
            })
            .collect();
        for (id, limit) in resting {
            self.fill(now, id, limit, Qty(i64::MAX));
        }
    }

    fn on_book(&mut self, contract: ContractKey, side: BookSide, price: Price, qty: Qty) {
        let Some(m) = self.markets.get_mut(&contract) else {
            return;
        };
        m.book.set(side, price, qty, 0);
        m.taken.remove(&(side == BookSide::Bid, price.0));
        // Queue estimate: a level that shrank below our queue means cancels ahead of us.
        for po in self.orders.values_mut() {
            if po.order.contract == contract
                && book_side(po.order.side) == side
                && matches!(po.order.kind, OrderKind::Limit { price: p } if p == price)
            {
                po.queue_ahead = po.queue_ahead.min(qty.0);
            }
        }
    }
}

impl Broker for PaperBroker {
    fn name(&self) -> &str {
        "Paper"
    }

    fn simulated(&self) -> bool {
        true
    }

    fn capabilities(&self) -> BrokerCaps {
        BrokerCaps {
            market_orders: true,
            stop_orders: true,
            modify: true,
            server_brackets: true,
            oco: true,
            fractional_qty: true,
            external_fills: false,
        }
    }

    fn request(&mut self, now: UtcMillis, req: BrokerRequest) {
        let due = UtcMillis(now.0 + self.cfg.latency_ms.max(0));
        self.pending.push_back((due, req));
    }

    fn market(&mut self, now: UtcMillis, contract: ContractKey, input: &MarketInput) {
        self.markets.entry(contract).or_default();
        match *input {
            MarketInput::Trade { price, qty, .. } => {
                if let Some(m) = self.markets.get_mut(&contract) {
                    m.last = Some(price);
                }
                // An option fills on its own quote, never on a print.
                if contract.option_key().is_none() {
                    self.on_trade(now, contract, price, qty);
                }
            }
            MarketInput::OptionQuote {
                bid,
                ask,
                last,
                time,
            } => self.on_option_quote(now, contract, bid, ask, last, time),
            MarketInput::Book { side, price, qty } => self.on_book(contract, side, price, qty),
            MarketInput::BookClear => {
                if let Some(m) = self.markets.get_mut(&contract) {
                    m.book.clear();
                    m.taken.clear();
                }
            }
            MarketInput::Last(price) => {
                if let Some(m) = self.markets.get_mut(&contract) {
                    m.last = Some(price);
                }
            }
            MarketInput::Bid(_) | MarketInput::Ask(_) => {}
        }
    }

    fn poll(&mut self, now: UtcMillis, out: &mut Vec<BrokerEvent>) {
        while self.pending.front().is_some_and(|(due, _)| *due <= now) {
            let (due, req) = self.pending.pop_front().expect("checked");
            self.handle(due, req);
        }
        out.append(&mut self.out);
    }

    fn next_wake(&self) -> Option<UtcMillis> {
        self.pending.front().map(|(due, _)| *due)
    }

    fn settle(&mut self, now: UtcMillis, mut order: Order, price: Price) {
        // Nothing else works on an expired series.
        let working: Vec<OrderId> = self
            .orders
            .values()
            .filter(|p| p.order.account == order.account && p.order.contract == order.contract)
            .map(|p| p.order.id)
            .collect();
        for id in working {
            self.cancel(now, id, "the option expired");
        }
        let position = self.position_of(&order.account, order.contract);
        if position == 0 || order.side.sign() == position.signum() {
            self.reject(order.id, "no position to settle", now);
            return;
        }
        order.qty = Qty(order.qty.0.min(position.abs()));
        order.state = OrderState::Acked;
        let (id, qty) = (order.id, order.qty);
        self.out.push(BrokerEvent::Acked { id, at: now });
        self.orders.insert(
            id,
            PaperOrder {
                order,
                queue_ahead: 0,
                held: false,
            },
        );
        self.fill_with_fee(now, id, price, qty, false);
    }

    fn saved_state(&self) -> Option<serde_json::Value> {
        serde_json::to_value(self.state()).ok()
    }

    fn snapshot(&self) -> BrokerSnapshot {
        BrokerSnapshot {
            accounts: self
                .accounts
                .iter()
                .map(|(id, a)| BrokerAccount {
                    id: id.clone(),
                    balance: a.balance,
                    positions: a
                        .positions
                        .iter()
                        .map(|p| BrokerPosition {
                            contract: p.contract,
                            position: p.position.clone(),
                        })
                        .collect(),
                    orders: self
                        .orders
                        .values()
                        .filter(|p| &p.order.account == id)
                        .map(|p| p.order.clone())
                        .collect(),
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod reduce_only_tests {
    use super::*;
    use crate::broker::{Broker, BrokerEvent, BrokerRequest};

    fn es() -> ContractKey {
        crate::types::ContractDoc {
            product: 1,
            year: Some(2026),
            month: Some(12),
            equity: false,
            option: None,
        }
        .to_key()
        .unwrap()
    }

    fn acct() -> AccountId {
        AccountId::new("paper-1")
    }

    // Tuesday 2026-11-10 15:00 UTC: CME open.
    fn t0() -> UtcMillis {
        UtcMillis::parse_iso("2026-11-10T15:00:00Z").unwrap()
    }

    fn broker(position: i64) -> PaperBroker {
        let mut b = PaperBroker::new(PaperConfig::default());
        b.open_account(acct(), Money::whole(50_000));
        if position != 0 {
            let mut p = Position::default();
            let side = if position > 0 { Side::Buy } else { Side::Sell };
            p.apply_fill(side, Price(600_000), Qty(position.abs()));
            b.accounts
                .get_mut(&acct())
                .unwrap()
                .positions
                .push(PaperPosition {
                    contract: es(),
                    position: p,
                });
        }
        b
    }

    fn stop_sell(id: u64, qty: i64) -> Order {
        Order {
            id: OrderId(id),
            account: acct(),
            contract: es(),
            side: Side::Sell,
            qty: Qty(qty),
            kind: OrderKind::Stop {
                trigger: Price(599_000),
            },
            state: OrderState::Submitted,
            filled: Qty(0),
            fill_value: 0,
            role: Role::Single,
            parent: None,
            oco: None,
            triggered: false,
            created: t0(),
            updated: t0(),
            reduce_only: true,
            overfill: Qty(0),
        }
    }

    fn run(b: &mut PaperBroker, req: BrokerRequest) -> Vec<BrokerEvent> {
        b.request(t0(), req);
        let mut out = Vec::new();
        b.poll(UtcMillis(t0().0 + 1_000), &mut out);
        out
    }

    /// Focused review: a reduce-only order placed while flat (the position closed while it was
    /// on its way) is cancelled on arrival, never left working to open one.
    #[test]
    fn reduce_only_placed_while_flat_is_cancelled() {
        let mut b = broker(0);
        let out = run(&mut b, BrokerRequest::Place(stop_sell(7, 2)));
        assert!(
            out.iter()
                .any(|e| matches!(e, BrokerEvent::Cancelled { id, reason, .. }
                if id.0 == 7 && reason.contains("nothing left to close"))),
            "{out:?}"
        );
        assert!(!b.orders.contains_key(&OrderId(7)));
    }

    /// Focused review: a reduce-only order larger than the position when it arrives shrinks
    /// to the position.
    #[test]
    fn reduce_only_placed_larger_than_the_position_shrinks() {
        let mut b = broker(1);
        run(&mut b, BrokerRequest::Place(stop_sell(7, 2)));
        assert_eq!(b.orders[&OrderId(7)].order.qty, Qty(1));
    }

    /// Focused review: a modify that grows a reduce-only order past the position is brought
    /// back to the position by the broker too (the core refuses it first).
    #[test]
    fn reduce_only_modify_never_grows_past_the_position() {
        let mut b = broker(2);
        run(&mut b, BrokerRequest::Place(stop_sell(7, 2)));
        run(
            &mut b,
            BrokerRequest::Modify {
                id: OrderId(7),
                qty: Some(Qty(5)),
                price: None,
            },
        );
        assert_eq!(b.orders[&OrderId(7)].order.qty, Qty(2));
    }
}
