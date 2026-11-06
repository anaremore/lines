//! Pre-trade risk (decision 0006), in the core before any broker, for paper and live alike:
//! maximum order size, maximum position, a fat-finger price band around the market, and a
//! daily loss limit. Flatten, Cancel All and cancels never pass through here.

use lines_core::{Price, Qty};
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
