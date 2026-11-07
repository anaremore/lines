//! The economic calendar in the UI (roadmap 8b, phase 2): what panes show about scheduled
//! releases. Pure: the schedule is the core's (`lines_feed::econ::bundled`), the rule is the
//! account's in force as the core reports it (`AccountView.limits`), and time is the core's
//! clock for the pane (wall live, market time in replays).

use lines_core::UtcMillis;
use lines_core::econ::{EconSchedule, Importance};
use lines_trading::news::{self, NewsAction, NewsBlackout, NewsStatus};

/// A schedule standing in for the bundled one (screenshots: `--econ-calendar`), set once at
/// start before anything reads it.
static OVERRIDE: std::sync::OnceLock<std::sync::Arc<EconSchedule>> = std::sync::OnceLock::new();

/// The economic calendar the app and its trading cores read: the bundled one, unless a
/// screenshot run gave another.
pub fn schedule() -> std::sync::Arc<EconSchedule> {
    OVERRIDE
        .get()
        .cloned()
        .unwrap_or_else(lines_feed::econ::bundled)
}

/// Read a schedule file (the bundled format) to stand in for the bundled one.
pub fn set_override_from(path: &std::path::Path) -> anyhow::Result<()> {
    use anyhow::Context as _;
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let s = EconSchedule::parse(&text).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
    let _ = OVERRIDE.set(std::sync::Arc::new(s));
    Ok(())
}

/// A release this close affecting the pane's product shows its countdown.
pub const SOON_MS: i64 = 30 * 60_000;

/// What a pane shows about the calendar now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Chip {
    /// A release affecting the product within [`SOON_MS`]: its short name and time.
    Soon { name: String, at: UtcMillis },
    /// Inside the account's blackout window. `refusing`: Block or Flatten (Warn refuses
    /// nothing).
    Blackout {
        title: String,
        until: UtcMillis,
        refusing: bool,
    },
    /// The calendar can't answer under a refusing rule: entries are refused.
    Unknown { reason: String },
}

/// The news blackout in force for an account: its guardrail's and its prop preset's, the
/// tighter of each part (as the core decides).
pub fn rule_of(limits: &lines_trading::event::AccountLimits) -> Option<NewsBlackout> {
    NewsBlackout::tighter(
        limits.guardrails_in_force.news,
        limits.rules_in_force.as_ref().and_then(|r| r.news),
    )
}

/// The pane's chip for `symbol` at `now` under the account's `rule`, if any.
pub fn chip(
    schedule: Option<&EconSchedule>,
    rule: Option<&NewsBlackout>,
    symbol: &str,
    now: UtcMillis,
) -> Option<Chip> {
    if let Some(r) = rule {
        let refusing = r.action != NewsAction::Warn;
        match news::status(schedule, r, symbol, now) {
            NewsStatus::Blackout { title, until, .. } => {
                return Some(Chip::Blackout {
                    title,
                    until,
                    refusing,
                });
            }
            NewsStatus::Unknown { reason } if refusing => return Some(Chip::Unknown { reason }),
            _ => {}
        }
    }
    let s = schedule?;
    let min = rule.map_or(Importance::High, NewsBlackout::min_importance);
    let e = s.next_event(Some(symbol), now, min)?;
    (e.at.0 - now.0 <= SOON_MS).then(|| Chip::Soon {
        name: s.release(e).name.clone(),
        at: e.at,
    })
}

/// `m:ss` until `until` (rounded up to the second; 0:00 when past).
pub fn countdown(now: UtcMillis, until: UtcMillis) -> String {
    let secs = ((until.0 - now.0).max(0) + 999) / 1000;
    if secs >= 3600 {
        format!("{}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
    } else {
        format!("{}:{:02}", secs / 60, secs % 60)
    }
}

/// The chip's words (never color alone).
pub fn chip_text(c: &Chip, now: UtcMillis) -> String {
    match c {
        Chip::Soon { name, at } => format!("{name} in {}", countdown(now, *at)),
        Chip::Blackout {
            title,
            until,
            refusing,
        } => format!(
            "News blackout{} · {title} · ends in {}",
            if *refusing { "" } else { " (warn)" },
            countdown(now, *until)
        ),
        Chip::Unknown { reason } => {
            let mut r = reason.clone();
            if let Some(first) = r.get_mut(..1) {
                first.make_ascii_uppercase();
            }
            format!("{r}: entries refused")
        }
    }
}

/// The switch's short reason when the chip refuses entries.
pub fn blocker(c: &Chip) -> Option<&'static str> {
    match c {
        Chip::Blackout { refusing: true, .. } => Some("news blackout"),
        Chip::Unknown { reason } if reason.contains("stale") => Some("calendar stale"),
        Chip::Unknown { .. } => Some("no calendar"),
        _ => None,
    }
}

/// Whether the chip warns (a window, or a calendar that can't answer).
pub fn warns(c: &Chip) -> bool {
    !matches!(c, Chip::Soon { .. })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> UtcMillis {
        UtcMillis::parse_iso(s).unwrap()
    }

    /// A small schedule: CPI (high) on 2026-11-12 08:30 ET, PPI (medium) on 11-13.
    fn schedule() -> EconSchedule {
        EconSchedule::parse(
            "format 1
version 2026-11-07
zone America/New_York
release cpi
name CPI
agency BLS
importance high
affects ES MES NQ MNQ ZN
coverage 2026-11-01 2027-12-31 verified
source https://www.bls.gov/schedule/news_release/cpi.htm
2026-11-12 08:30 verified October 2026
release ppi
name PPI
agency BLS
importance medium
affects ES MES
coverage 2026-11-01 2027-12-31 verified
source https://www.bls.gov/schedule/news_release/ppi.htm
2026-11-13 08:30 verified October 2026
",
        )
        .expect("parses")
    }

    #[test]
    fn countdown_reads_minutes_and_seconds() {
        let t = UtcMillis(0);
        assert_eq!(countdown(t, UtcMillis(872_000)), "14:32");
        assert_eq!(countdown(t, UtcMillis(1)), "0:01", "rounded up");
        assert_eq!(countdown(UtcMillis(5), t), "0:00");
        assert_eq!(countdown(t, UtcMillis(3_725_000)), "1:02:05");
    }

    /// No rule: a countdown within 30 minutes of a high release on the product; nothing for
    /// a medium one, another product, or further away.
    #[test]
    fn soon_without_a_rule() {
        let s = schedule();
        let cpi = at("2026-11-12T13:30:00Z");
        let c = chip(Some(&s), None, "ES", UtcMillis(cpi.0 - 14 * 60_000)).unwrap();
        assert_eq!(
            chip_text(&c, UtcMillis(cpi.0 - 14 * 60_000)),
            "CPI in 14:00"
        );
        assert_eq!(
            chip(Some(&s), None, "ES", UtcMillis(cpi.0 - 31 * 60_000)),
            None
        );
        assert_eq!(chip(Some(&s), None, "BTC", UtcMillis(cpi.0 - 60_000)), None);
        let ppi = at("2026-11-13T13:30:00Z");
        assert_eq!(chip(Some(&s), None, "ES", UtcMillis(ppi.0 - 60_000)), None);
        // A rule with medium releases counts PPI.
        let r = NewsBlackout {
            include_medium: true,
            ..NewsBlackout::new(NewsAction::Warn)
        };
        assert!(matches!(
            chip(Some(&s), Some(&r), "ES", UtcMillis(ppi.0 - 10 * 60_000)),
            Some(Chip::Soon { .. })
        ));
    }

    /// Inside the window: the blackout with time left; Block refuses (the switch says so),
    /// Warn doesn't.
    #[test]
    fn the_window_refuses_under_block_and_not_under_warn() {
        let s = schedule();
        let now = at("2026-11-12T13:29:30Z");
        let block = NewsBlackout::new(NewsAction::Block);
        let c = chip(Some(&s), Some(&block), "ES", now).unwrap();
        assert_eq!(
            chip_text(&c, now),
            "News blackout · CPI, October 2026 · ends in 2:30"
        );
        assert_eq!(blocker(&c), Some("news blackout"));
        assert!(warns(&c));
        let warn = NewsBlackout::new(NewsAction::Warn);
        let c = chip(Some(&s), Some(&warn), "ES", now).unwrap();
        assert!(chip_text(&c, now).starts_with("News blackout (warn) · CPI"));
        assert_eq!(blocker(&c), None);
        // Another product: no window.
        assert_eq!(chip(Some(&s), Some(&block), "BTC", now), None);
    }

    /// A calendar that can't answer (none, or stale) under a refusing rule refuses entries;
    /// under Warn it says nothing.
    #[test]
    fn a_calendar_that_cant_answer_refuses_under_block() {
        let block = NewsBlackout::new(NewsAction::Block);
        let now = at("2026-11-10T15:00:00Z");
        let c = chip(None, Some(&block), "ES", now).unwrap();
        assert_eq!(
            chip_text(&c, now),
            "No economic calendar loaded: entries refused"
        );
        assert_eq!(blocker(&c), Some("no calendar"));
        let s = schedule();
        let later = at("2027-03-10T15:00:00Z");
        let c = chip(Some(&s), Some(&block), "ES", later).unwrap();
        assert!(matches!(c, Chip::Unknown { .. }), "stale: {c:?}");
        assert_eq!(blocker(&c), Some("calendar stale"));
        let warn = NewsBlackout::new(NewsAction::Warn);
        assert_eq!(chip(None, Some(&warn), "ES", now), None);
    }

    /// The rule in force is the tighter of the guardrail's and the prop preset's.
    #[test]
    fn the_rule_in_force_takes_both_sources() {
        let mut v = lines_trading::event::AccountLimits::default();
        assert_eq!(rule_of(&v), None);
        v.guardrails_in_force.news = Some(NewsBlackout::new(NewsAction::Warn));
        let mut preset = lines_trading::rules::presets().remove(0);
        preset.news = Some(NewsBlackout {
            before_min: 5,
            ..NewsBlackout::new(NewsAction::Block)
        });
        v.rules_in_force = Some(preset);
        let r = rule_of(&v).unwrap();
        assert_eq!((r.action, r.before_min), (NewsAction::Block, 5));
    }
}
