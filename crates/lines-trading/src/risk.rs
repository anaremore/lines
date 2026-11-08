//! Pre-trade risk (decision 0006), in the core before any broker, for paper and live alike:
//! maximum order size, maximum position, a fat-finger price band around the market, and a
//! daily loss limit. Flatten, Cancel All and cancels never pass through here.

use lines_core::{Price, Qty, UtcMillis};
use serde::{Deserialize, Serialize};

use crate::command::{Reject, RejectCode};
use crate::types::{ContractSpec, Money, OrderKind, Side};

/// Per-account limits; `None` = no limit. Quantities in the product's quantity units.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RiskLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_order_qty: Option<i64>,
    /// Worst case: current position plus every open order on the same side.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_position: Option<i64>,
    /// Limit and stop prices must be within this many ticks of the market (last trade, else
    /// the mid).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_band_ticks: Option<i64>,
    /// Whole currency units of loss today (realised + unrealised, fees included).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daily_loss: Option<i64>,
    /// Largest notional per order (price × quantity × point value), whole currency units.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_notional: Option<i64>,
    /// New orders per second (a runaway script or a stuck key can't flood the broker).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_submits_per_sec: Option<u32>,
    /// Modifies per second, counted separately (dragging an order sends many).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_modifies_per_sec: Option<u32>,
    /// The guard on option orders (decision 0028). `None`: [`OptionGuard::default`]; it is
    /// never off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub option_guard: Option<OptionGuard>,
}

/// One more guard on option orders (decision 0028): a limit far from the mid is refused (a
/// fat finger on an option is often the whole premium), and so are market orders by default.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OptionGuard {
    /// A limit may be this far from the mid, as a percentage of the mid ...
    pub max_from_mid_pct: u32,
    /// ... or this many ticks, whichever is wider (a percentage of a cheap option is nothing).
    pub min_band_ticks: i64,
    /// Allow market orders (Flatten always may).
    #[serde(default)]
    pub allow_market: bool,
    /// A quote older than this has no mid to check against.
    pub stale_after_ms: i64,
}

impl Default for OptionGuard {
    fn default() -> Self {
        OptionGuard {
            max_from_mid_pct: 25,
            min_band_ticks: 10,
            allow_market: false,
            stale_after_ms: DEFAULT_QUOTE_STALE_MS,
        }
    }
}

/// How old an option quote may be before it counts as stale, by default: 15 minutes. A quiet
/// series quotes rarely, and its quote stays good until it changes.
pub const DEFAULT_QUOTE_STALE_MS: i64 = 15 * 60_000;

/// An option's quote as the guard sees it.
#[derive(Copy, Clone, Debug, Default)]
pub struct OptionQuoteView {
    pub bid: Option<Price>,
    pub ask: Option<Price>,
    /// When it was quoted (`None`: never).
    pub at: Option<UtcMillis>,
}

/// The guard on one option order. `closing`: it only closes a position (reduce-only). With no
/// two-sided fresh quote there is no mid: an opening order is refused, a closing limit may
/// still rest (it never fills on a missing side, and a trader must be able to offer out of a
/// position whose bid is gone).
pub fn check_option(
    guard: &OptionGuard,
    spec: &ContractSpec,
    kind: &OrderKind,
    closing: bool,
    quote: OptionQuoteView,
    now: UtcMillis,
) -> Result<(), Reject> {
    let price = match *kind {
        OrderKind::Market if guard.allow_market => return Ok(()),
        OrderKind::Market => {
            return Err(risk(
                "market orders on options are off: send a limit (Close prices one for you)".into(),
            ));
        }
        OrderKind::Stop { .. } | OrderKind::StopLimit { .. } => {
            return Err(Reject::new(
                RejectCode::Invalid,
                "no stop orders on option prices",
            ));
        }
        OrderKind::Limit { price } => price,
    };
    if price.0 <= 0 {
        return Err(Reject::new(
            RejectCode::Invalid,
            "an option limit must be above zero",
        ));
    }
    if let Some(rules) = spec.product.options() {
        let tick = rules.tick.at(price);
        if price.0.rem_euclid(tick.max(1)) != 0 {
            return Err(Reject::new(
                RejectCode::Invalid,
                format!(
                    "price {} is not on the option's tick grid",
                    spec.product.price.format::<32>(price)
                ),
            ));
        }
    }
    let fresh = quote
        .at
        .is_some_and(|t| now.0 - t.0 <= guard.stale_after_ms);
    let (Some(bid), Some(ask), true) = (quote.bid, quote.ask, fresh) else {
        if closing {
            return Ok(());
        }
        return Err(risk(if quote.at.is_some() && !fresh {
            "this option's quote is stale: no mid to check the limit against".into()
        } else {
            "this option has no two-sided quote: no mid to check the limit against".into()
        }));
    };
    // In doubled price units, so a half-tick mid stays exact.
    let sum = i128::from(bid.0) + i128::from(ask.0);
    let off = (2 * i128::from(price.0) - sum).abs();
    let by_pct = off * 100 <= i128::from(guard.max_from_mid_pct) * sum;
    let by_ticks = off <= 2 * i128::from(guard.min_band_ticks) * i128::from(spec.tick());
    if by_pct || by_ticks {
        return Ok(());
    }
    Err(risk(format!(
        "limit {} is too far from the mid (bid {}, ask {}; within {}% or {} ticks)",
        spec.product.price.format::<32>(price),
        spec.product.price.format::<32>(bid),
        spec.product.price.format::<32>(ask),
        guard.max_from_mid_pct,
        guard.min_band_ticks,
    )))
}

/// What the check needs to know about the account and market.
#[derive(Copy, Clone, Debug, Default)]
pub struct RiskContext {
    /// Signed position in this contract.
    pub position: i64,
    /// Open (unfilled) quantity of working orders per side in this contract.
    pub open_buy: i64,
    pub open_sell: i64,
    pub reference: Option<Price>,
    /// Today's P&L (realised + unrealised − fees).
    pub day_pnl: Money,
    /// New orders in the last second, this one excluded.
    pub recent_submits: u32,
}

fn risk(msg: String) -> Reject {
    Reject::new(RejectCode::Risk, msg)
}

pub fn check(
    limits: &RiskLimits,
    spec: &ContractSpec,
    side: Side,
    qty: Qty,
    kind: &OrderKind,
    ctx: &RiskContext,
) -> Result<(), Reject> {
    if let Some(loss) = limits.daily_loss
        && ctx.day_pnl <= -Money::whole(loss)
    {
        return Err(risk(format!(
            "daily loss limit reached ({} today, limit {loss}); only Flatten and Cancel All",
            ctx.day_pnl
        )));
    }
    if let Some(max) = limits.max_submits_per_sec
        && ctx.recent_submits >= max
    {
        return Err(risk(format!(
            "more than {max} new orders in a second; slow down (Flatten and Cancel All still work)"
        )));
    }
    if let Some(max) = limits.max_notional {
        // Market orders are valued at the market.
        let price = kind.prices().next().or(ctx.reference);
        if let Some(p) = price {
            let notional = spec.money(p.0.unsigned_abs() as i128, qty.0 as i128);
            if notional > Money::whole(max) {
                return Err(risk(format!(
                    "order value {notional} is over the maximum {max}.00"
                )));
            }
        }
    }
    if let Some(max) = limits.max_order_qty
        && qty.0 > max
    {
        return Err(risk(format!(
            "order quantity {} is over the maximum {max}",
            qty.0
        )));
    }
    if let Some(max) = limits.max_position {
        let worst = match side {
            Side::Buy => ctx.position + ctx.open_buy + qty.0,
            Side::Sell => ctx.position - ctx.open_sell - qty.0,
        };
        if worst.abs() > max {
            return Err(risk(format!(
                "position could reach {worst} (with open orders), over the maximum {max}"
            )));
        }
    }
    if let Some(ticks) = limits.price_band_ticks {
        let prices: Vec<Price> = kind.prices().collect();
        if !prices.is_empty() {
            let Some(r) = ctx.reference else {
                return Err(risk(
                    "no market price yet to check the order price against".into(),
                ));
            };
            let band = ticks * spec.tick();
            if let Some(p) = prices.iter().find(|p| (p.0 - r.0).abs() > band) {
                return Err(risk(format!(
                    "price {} is more than {ticks} ticks from the market",
                    spec.product.price.format::<32>(*p)
                )));
            }
        }
    }
    Ok(())
}
