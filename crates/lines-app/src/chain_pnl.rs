//! "P&L to close" and the Risk / Reward summary on the option chart (decision 0029), without GPUI:
//! how the pane divides, and the words. Display only: every value here is an estimate from
//! `option_estimate`, always marked ≈, and never reaches an order, a check or P&L.

use std::fmt::{self, Write};

use lines_core::ContractKey;
use lines_core::Right;

use crate::option_estimate::PnlEstimate;

/// The pane with P&L to close on: the panel's width and the chain's, in px. The chart has the
/// rest, and with the panel it has more than the chain's half left it. The chain shows one side,
/// so it needs less, but never less than a strike, a bid, an ask and their margin.
pub fn split(pane_w: f32) -> (f32, f32) {
    let chain = (pane_w * 0.26).max(250.0);
    let panel = (pane_w * 0.30).max(200.0);
    (panel, chain)
}

/// The side P&L to close hides: the one that isn't being traded.
pub fn other_side(right: Right) -> Right {
    match right {
        Right::Call => Right::Put,
        Right::Put => Right::Call,
    }
}

/// `Risk ≈ -$72  ·  Reward ≈ +$308  ·  1 : 4.3`, for whichever exits are set.
pub fn write_summary(
    out: &mut impl Write,
    risk: Option<PnlEstimate>,
    reward: Option<PnlEstimate>,
) -> fmt::Result {
    if let Some(r) = risk {
        out.write_str("Risk ≈ ")?;
        r.write(out)?;
    }
    if let Some(r) = reward {
        if risk.is_some() {
            out.write_str("  ·  ")?;
        }
        out.write_str("Reward ≈ ")?;
        r.write(out)?;
    }
    if let (Some(risk), Some(reward)) = (risk, reward)
        && let Some(ratio) = PnlEstimate::reward_to_risk(reward, risk)
    {
        write!(out, "  ·  1 : {ratio:.1}")?;
    }
    Ok(())
}

/// The panel's title: what the P&L is of. A position is said as held; without one, what it
/// assumes, so nobody reads it as theirs.
pub fn write_title<W: Write>(
    out: &mut W,
    contract: ContractKey,
    position: i64,
    basis: impl FnOnce(&mut W) -> fmt::Result,
    qty: i64,
) -> fmt::Result {
    crate::chain_lines::write_series(out, contract)?;
    if position != 0 {
        write!(out, " {position:+} @ ")?;
    } else if qty < 0 {
        write!(out, " if {} sold @ ", -qty)?;
    } else {
        write!(out, " if {qty} bought @ ")?;
    }
    basis(out)?;
    out.write_str("  ≈ P&L to close")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::option_estimate::Estimate;
    use lines_core::{CivilDate, Price};

    fn pnl(value: f64) -> PnlEstimate {
        // From a basis of 100 at a dollar a unit.
        test_estimate(100.0 + value).pnl(1, 100.0, 1.0)
    }

    fn test_estimate(value: f64) -> Estimate {
        let q = lines_core::OptionQuote {
            bid: Some(Price(value as i64)),
            ask: Some(Price(value as i64)),
            last: None,
            volume: lines_core::Qty(0),
            open_interest: 0,
            iv_bp: Some(1_500),
            delta_e4: None,
            time: lines_core::UtcMillis(0),
        };
        let c = series(Right::Call);
        crate::option_estimate::estimate(
            c,
            Some(&q),
            Some(Price(68_400)),
            Price(68_400),
            lines_core::UtcMillis::parse_iso("2026-11-09T15:00:00Z"),
        )
        .unwrap()
    }

    fn series(right: Right) -> ContractKey {
        ContractKey::option(
            lines_feed::catalog::SPY,
            CivilDate::new(2026, 11, 13).unwrap(),
            Price(68_400),
            right,
        )
    }

    fn say(risk: Option<PnlEstimate>, reward: Option<PnlEstimate>) -> String {
        let mut s = String::new();
        write_summary(&mut s, risk, reward).unwrap();
        s
    }

    /// Every figure is marked ≈, and it says only what's set.
    #[test]
    fn the_summary_says_whats_set() {
        let (risk, reward) = (pnl(-72.0), pnl(308.0));
        assert_eq!(
            say(Some(risk), Some(reward)),
            "Risk ≈ -$72  ·  Reward ≈ +$308  ·  1 : 4.3"
        );
        assert_eq!(say(Some(risk), None), "Risk ≈ -$72");
        assert_eq!(say(None, Some(reward)), "Reward ≈ +$308");
        assert_eq!(say(None, None), "");
    }

    /// A Stop in profit isn't a risk to divide by, and nor is one that reads as nothing.
    #[test]
    fn no_ratio_without_a_loss_and_a_gain() {
        assert_eq!(
            say(Some(pnl(20.0)), Some(pnl(308.0))),
            "Risk ≈ +$20  ·  Reward ≈ +$308"
        );
        assert_eq!(
            say(
                Some(test_estimate(100.0).pnl(1, 100.4, 1.0)),
                Some(pnl(308.0))
            ),
            "Risk ≈ +$0  ·  Reward ≈ +$308"
        );
    }

    /// Nobody reads a what-if as their own position.
    #[test]
    fn the_title_says_whose_it_is() {
        let c = series(Right::Call);
        let title = |position, qty| {
            let mut s = String::new();
            write_title(&mut s, c, position, |o| o.write_str("0.65"), qty).unwrap();
            s
        };
        assert_eq!(title(2, 2), "684C +2 @ 0.65  ≈ P&L to close");
        assert_eq!(title(-1, 1), "684C -1 @ 0.65  ≈ P&L to close");
        assert_eq!(title(0, 3), "684C if 3 bought @ 0.65  ≈ P&L to close");
        assert_eq!(title(0, -2), "684C if 2 sold @ 0.65  ≈ P&L to close");
    }

    /// The chart keeps room at the narrowest width the option chart has, and has more than it
    /// did at any width: with the panel it's past half.
    #[test]
    fn the_chart_widens_and_keeps_room() {
        for w in [760.0f32, 1000.0, 1600.0, 2400.0] {
            let (panel, chain) = split(w);
            assert!(chain >= 250.0, "{w}");
            assert!(w - panel - chain >= 280.0, "{w}: {}", w - panel - chain);
            assert!(w - chain > 0.55 * w, "{w}");
        }
    }

    #[test]
    fn the_hidden_side_is_the_other_one() {
        assert_eq!(other_side(Right::Call), Right::Put);
        assert_eq!(other_side(Right::Put), Right::Call);
    }
}
