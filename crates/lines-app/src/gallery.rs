//! The UI gallery (`--gallery DIR`, UI checks layers 1 and 2): a fixed set of scenes rendered
//! into DIR (one PNG per scene, named by scene) from synthetic data, each linted by
//! `lines_core::layout_lint` (`<scene>.lint.txt`, empty when clean). `DIR/gallery.txt` sums it
//! up; the run exits 1 if any scene failed or has a finding.
//!
//! Each scene runs this binary in screenshot mode as its own process, so every scene starts
//! from scratch and nothing leaks between them (paper state, settings, caches).

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

/// One scene: its file name, whether the quick set (`tests/gallery.rs`) renders it, the
/// market time it is taken at, and the screenshot arguments.
pub struct Scene {
    pub name: &'static str,
    pub quick: bool,
    pub at: &'static str,
    pub args: Vec<String>,
    /// Windows the scene draws (a popped-out pane adds one): each must report.
    pub windows: usize,
    /// Replays the sessions recorded only from mid-session on ([`LATE_FROM`]): a feed opened
    /// late.
    pub late: bool,
    /// Replays the synthetic SPY option chain instead.
    pub chain: bool,
}

/// A half hour into the synthetic SPY chain's session.
const CHAIN_AT: &str = "2026-11-09T15:00:00Z";

/// Where the late replay starts: mid-session on 2026-11-04.
pub const LATE_FROM: &str = "2026-11-04T09:30:00Z";

/// Twenty minutes into the 2026-11-04 evening session: the session break in view.
const BREAK_AT: &str = "2026-11-04T23:20:00Z";

/// Findings that are known and documented (`docs/known-gaps.md`): (scene, finding line). They
/// are reported as known instead of failing; a known finding that no longer occurs fails, so
/// this list can't go stale.
pub const KNOWN: &[(&str, &str)] = &[];

/// Before the first trade: an empty chart and book.
const EMPTY_AT: &str = "2026-11-03T22:30:00Z";

fn args(s: &str) -> Vec<String> {
    s.split_whitespace().map(str::to_owned).collect()
}

/// The replay's last price at [`lines_feed::synth::END`] (6019.50), in hundredths.
const LAST_TICKS: i64 = 601_950;

/// Paper orders around the replay's last price (6019.50): working, held bracket exits,
/// pending (after the last `ack`).
const ORDERS: &str =
    "buy 1 mkt; ack; buy 1 @ 6015.00; buy 1 @ 6016.00 bracket 8/16; ack; buy 1 @ 6014.00";

/// A buy staged under [`ORDERS`]' working orders, with both legs (`--stage-order`).
const STAGED: &str = "buy limit 6011.00 stop 6009.00 target 6023.00";

/// The scenes. `dir` holds the workspace files some of them use.
pub fn scenes(dir: &Path) -> Vec<Scene> {
    let ws = |f: &str| dir.join(f).display().to_string();
    let end = lines_feed::synth::END;
    let s = |name, quick, at, a: Vec<String>| Scene {
        name,
        quick,
        at,
        args: a,
        windows: 1,
        late: false,
        chain: false,
    };
    let mut v = vec![
        // Every pane kind, both themes and palettes.
        s(
            "chart-dark",
            true,
            end,
            args("--layout chart --size 1400x800 --theme dark"),
        ),
        s(
            "chart-zoom-to-150",
            true,
            end,
            args("--layout chart --size 1400x800 --theme dark --ui-zoom 100 --ui-zoom-to 150"),
        ),
        s(
            "chart-light",
            false,
            end,
            args("--layout chart --size 1400x800 --theme light"),
        ),
        s(
            "chart-dark-cb",
            false,
            end,
            args("--layout chart --size 1400x800 --theme dark --palette cb"),
        ),
        s(
            "chart-light-cb",
            false,
            end,
            args("--layout chart --size 1400x800 --theme light --palette cb"),
        ),
        // Theme: System, resolved by the pretended OS appearance.
        s(
            "chart-system-light",
            false,
            end,
            args("--layout chart --size 1400x800 --theme system --appearance light"),
        ),
        s(
            "trading-dark",
            true,
            end,
            args("--layout trading --size 1600x900 --theme dark"),
        ),
        s(
            "trading-light-cb",
            false,
            end,
            args("--layout trading --size 1600x900 --theme light --palette cb"),
        ),
        // Narrow and wide.
        s(
            "chart-narrow",
            true,
            end,
            args("--layout chart --size 320x480 --theme dark"),
        ),
        s(
            "trading-narrow",
            false,
            end,
            args("--layout trading --size 900x600 --theme dark"),
        ),
        s(
            "trading-wide",
            false,
            end,
            args("--layout trading --size 2560x900 --theme dark"),
        ),
        // States.
        s(
            "empty",
            false,
            EMPTY_AT,
            args("--layout trading --size 1600x900 --theme dark"),
        ),
        s(
            "timeframe-5m",
            false,
            end,
            args("--layout chart --size 1400x800 --theme dark --timeframe 5m"),
        ),
        s(
            "pnl-off",
            false,
            end,
            args("--layout trading --size 1600x900 --theme dark --pnl off"),
        ),
        // NQ on an ES replay: unavailable, with the one-click switch.
        s(
            "unavailable",
            false,
            end,
            args("--layout trading --size 1600x900 --theme dark --instrument NQ"),
        ),
        // Menus open.
        s(
            "timeframe-menu",
            true,
            end,
            args("--layout chart --size 1000x700 --theme dark --timeframe-menu"),
        ),
        s(
            "account-menu",
            false,
            end,
            args("--layout trading --size 1600x900 --theme dark --account-menu"),
        ),
        s(
            "studies-menu",
            false,
            end,
            args("--layout chart --size 1400x800 --theme dark --studies-menu open"),
        ),
        s(
            "picker",
            false,
            end,
            args("--layout chart --size 1400x800 --theme dark --picker nq"),
        ),
        // Floating panels: the palette where it opens (quick), Open workspace with the mouse,
        // both dragged (kept inside the window), and a narrow window.
        s(
            "panel-palette",
            true,
            end,
            args("--layout chart --size 1400x800 --theme dark --prompt palette:hide"),
        ),
        s(
            "panel-open-light",
            false,
            end,
            args("--layout chart --size 1400x800 --theme light --prompt open"),
        ),
        s(
            "panel-palette-moved",
            false,
            end,
            args(
                "--layout chart --size 1400x800 --theme dark --prompt palette --panel-at 1300,700",
            ),
        ),
        s(
            "panel-picker-moved-light",
            false,
            end,
            args("--layout chart --size 1400x800 --theme light --picker nq --panel-at 40,500"),
        ),
        s(
            "panel-palette-narrow",
            false,
            end,
            args("--layout chart --size 520x600 --theme dark --prompt palette"),
        ),
        s(
            "panel-studies-zoom-150",
            false,
            end,
            args("--layout chart --size 1400x800 --theme dark --studies-menu open --ui-zoom 150"),
        ),
        // Pop-out window, journal, footprint.
        s(
            "popout",
            false,
            end,
            vec![
                "--workspace".into(),
                ws("popout.json"),
                "--size".into(),
                "1400x800".into(),
            ],
        ),
        s(
            "footprint",
            false,
            end,
            vec![
                "--workspace".into(),
                ws("footprint.json"),
                "--size".into(),
                "1400x800".into(),
            ],
        ),
    ];
    // Trading states: armed with working / held / pending orders; a refusal; a lockout.
    let paper = |name, quick, script: &str, extra: &str| {
        let mut a = args(&format!(
            "--layout trading --size 1600x900 --theme dark {extra}"
        ));
        a.push("--paper".into());
        a.push(script.into());
        s(name, quick, end, a)
    };
    v.push(paper("orders-armed", true, ORDERS, "--arm"));
    // Level labels in each level's color, or theme text where that isn't AA (the light
    // themes' gray and olive fallbacks), zone labels at their edges (supply above its high
    // edge, demand under its low edge), the legend's title and signed stats. Only in a build
    // with the demo source (`--features demo-levels`).
    #[cfg(feature = "demo-levels")]
    for (name, quick, theme) in [
        ("levels-light", true, "--theme light --size 1400x800"),
        (
            "levels-light-cb",
            false,
            "--theme light --palette cb --size 1400x800",
        ),
        ("levels-dark", false, "--theme dark --size 1400x800"),
        // Two sources' labels at the same and adjacent prices (owner, 2026-11-13): one layout,
        // none over another; the crowd's Low ones give way; a short pane hides by priority.
        (
            "levels-crowd-dark",
            false,
            "--theme dark --demo-crowd --size 1400x800",
        ),
        (
            "levels-crowd-light-150",
            false,
            "--theme light --ui-zoom 150 --demo-crowd --size 1400x800",
        ),
        (
            "levels-crowd-short-dark",
            false,
            "--theme dark --demo-crowd --size 1400x260",
        ),
        // An event (owner-approved, 2026-11-13): its lines begin at their bars, never earlier;
        // A, E1 and V dots joined by a dashed path; a box from the anchor to the YL.
        (
            "levels-event-dark",
            false,
            "--theme dark --demo-event --size 1400x800",
        ),
        (
            "levels-event-light-150",
            false,
            "--theme light --ui-zoom 150 --demo-event --size 1400x800",
        ),
    ] {
        v.push(s(
            name,
            quick,
            end,
            args(&format!("--layout chart {theme} --demo-levels")),
        ));
    }
    // A plugin's two-point drawing tool (decision 0020; the synthetic one): its button in the
    // toolbar, its levels in the levels layer's style, the drawing selected (its two handles).
    // The demo tool exists only with `demo-levels` (as the `levels-*` scenes above).
    #[cfg(feature = "demo-levels")]
    for (name, theme) in [
        ("draw-tool-dark", "--theme dark"),
        ("draw-tool-light-150", "--theme light --ui-zoom 150"),
    ] {
        let mut a = args(&format!(
            "--layout chart {theme} --size 1400x800 --price-zoom 0.45 --drawing-tools --demo-draw-tool"
        ));
        a.push("--plugin-drawing".into());
        a.push("demo-span 2026-11-05T19:40:00Z@6002.00 2026-11-05T20:30:00Z@6016.00".into());
        v.push(s(name, false, end, a));
    }
    // Settings › Charts at the narrowest window: Bar period is a dropdown (one field), Hollow
    // candles' chips keep to their line. Dark at 100 %, light at 150 % (tall enough that the page
    // needs no scrolling, so nothing is cut at its edge).
    let mut set = s(
        "settings-charts-narrow",
        false,
        end,
        args(
            "--layout chart --size 1000x700 --theme dark --settings charts --settings-size 640x420",
        ),
    );
    set.windows = 2;
    v.push(set);
    let mut set = s(
        "settings-charts-narrow-light-150",
        true,
        end,
        args(
            "--layout chart --size 1000x700 --theme light --ui-zoom 150 --settings charts --settings-size 640x900",
        ),
    );
    set.windows = 2;
    v.push(set);
    // The pages at the narrowest window and 150 %: General (the Updates dropdown), Appearance
    // (the Light palette and UI zoom dropdowns), Order entry (P&L display's chips, the − / +
    // number fields) and Accounts (the news blackout's chips, the prop preset's dropdown). Each
    // page whole in its window, every row with its description; Accounts at 150 % is taller
    // than a window can be, so its two parts are searched for.
    for (page, name, h, search) in [
        ("general", "settings-general-narrow-light-150", 700, ""),
        (
            "appearance",
            "settings-appearance-narrow-light-150",
            900,
            "",
        ),
        (
            "order-entry",
            "settings-order-entry-narrow-light-150",
            1400,
            "",
        ),
        (
            "accounts",
            "settings-accounts-narrow-light-150",
            900,
            " --settings-search news",
        ),
        (
            "accounts",
            "settings-accounts-preset-narrow-light-150",
            1200,
            " --settings-search preset",
        ),
    ] {
        let mut set = s(
            name,
            false,
            end,
            args(&format!(
                "--layout chart --size 1000x700 --theme light --ui-zoom 150 --settings {page} --settings-size 640x{h}{search}"
            )),
        );
        set.windows = 2;
        v.push(set);
    }
    // Near the threshold: Appearance at 760 px and 150 %, where UI zoom's chips just fit (or just
    // don't fit) a line of their own; either way the row holds them.
    let mut set = s(
        "settings-appearance-760-150",
        false,
        end,
        args(
            "--layout chart --size 1000x700 --theme dark --ui-zoom 150 --settings appearance --settings-size 760x900",
        ),
    );
    set.windows = 2;
    v.push(set);
    // Settings' dropdowns open (owner-approved, 2026-11-11): floating over the rows under them,
    // never moving them. Bar period's grouped list in the smallest window (capped, scrolling,
    // the value in force in view); the depth products' multi-select; the prop preset opening
    // upward near the window's bottom; the time zone's searchable list at 150 %.
    for (name, page, rest) in [
        (
            "settings-drop-timeframe",
            "charts",
            "--theme dark --settings-size 640x420 --settings-set charts.timeframe=open",
        ),
        (
            "settings-drop-custom-light-150",
            "charts",
            "--theme light --ui-zoom 150 --settings-size 640x900 --settings-set charts.timeframe=custom:750t",
        ),
        (
            "settings-drop-depth",
            "data-folder",
            "--theme dark --settings-size 640x420 --settings-set data.depth=open",
        ),
        (
            "settings-drop-preset-light",
            "accounts",
            "--theme light --settings-size 860x1400 --settings-set prop.preset=open",
        ),
        (
            "settings-drop-zone-150",
            "general",
            "--theme dark --ui-zoom 150 --settings-size 860x620 --settings-set appearance.time_zone=open",
        ),
    ] {
        let mut set = s(
            name,
            false,
            end,
            args(&format!(
                "--layout chart --size 1000x700 --settings {page} {rest}"
            )),
        );
        set.windows = 2;
        v.push(set);
    }
    // The trading panel's Qty and Bracket lists: anchored under their field, as wide as their
    // content (never the window's width).
    v.push(s(
        "panel-qty-list",
        true,
        end,
        args("--layout trading --size 1600x900 --theme dark --panel-menu qty"),
    ));
    v.push(s(
        "panel-bracket-list-light-150",
        false,
        end,
        args("--layout trading --size 1600x900 --theme light --panel-menu bracket --ui-zoom 150"),
    ));
    v.push(s(
        "panel-qty-list-narrow",
        false,
        end,
        args("--layout trading --size 700x700 --theme dark --panel-menu qty"),
    ));
    // The footer's zones: safety (PAPER, trading on, REC with Stop), links (Layout: …); in a
    // narrow window at 150 % the safety words shorten and the links fold into ⋯.
    v.push(s(
        "footer-full",
        true,
        end,
        args("--layout trading --size 1600x900 --theme dark --enable-trading --show-recording 724"),
    ));
    v.push(s(
        "footer-narrow-light-150",
        false,
        end,
        args(
            "--layout trading --size 480x600 --theme light --ui-zoom 150 --enable-trading --show-recording 724",
        ),
    ));
    // In-app updates (decision 0031) on the status line: ready (Restart to update), held by a
    // position, a discarded download, and the rollback offer after two failed starts.
    v.push(s(
        "update-ready",
        true,
        end,
        args("--layout chart --size 1400x800 --theme dark --update-shot ready"),
    ));
    let mut held = args("--layout trading --size 1600x900 --theme dark --update-shot ready");
    held.push("--paper".into());
    held.push("buy 1 mkt; ack".into());
    v.push(s("update-held", false, end, held));
    v.push(s(
        "update-rejected-light",
        false,
        end,
        args("--layout chart --size 1400x800 --theme light --update-shot rejected"),
    ));
    v.push(s(
        "update-rollback",
        false,
        end,
        args("--layout chart --size 1400x800 --theme dark --update-shot rollback"),
    ));
    // The grouped-order list in its floating panel.
    let mut g = args("--layout chart --size 1400x800 --theme dark --order-group-menu");
    g.push("--paper".into());
    g.push("buy 1 @ 6015.00; ack; buy 1 @ 6015.25; ack; buy 1 @ 6015.50; ack".into());
    v.push(s("panel-order-group", false, end, g));
    v.push(paper(
        "order-controls",
        true,
        ORDERS,
        "--arm --order-controls",
    ));
    v.push(paper("orders-disarmed", false, ORDERS, ""));
    // Chart trading's one style (docs/design.md "Chart trading"): the position, working, held
    // and pending orders, the order strip, a staged bracket and a refusal flash on one chart;
    // dark and light, at 100 % and 150 %. The staged order sits below the working ones, so the
    // tags stack without overlapping (laid out together, `chart_view::draw_orders`).
    for (name, quick, extra) in [
        ("chart-trading-style-dark", true, "--theme dark"),
        ("chart-trading-style-light", false, "--theme light"),
        (
            "chart-trading-style-dark-150",
            false,
            "--theme dark --ui-zoom 150",
        ),
        (
            "chart-trading-style-light-150",
            false,
            "--theme light --ui-zoom 150",
        ),
    ] {
        let mut a = args(&format!(
            "--layout trading --size 1600x900 --arm --order-strip {extra}"
        ));
        a.extend([
            "--paper".into(),
            ORDERS.into(),
            "--stage-order".into(),
            STAGED.into(),
            "--chart-flash".into(),
            // A refusal that can't contradict the scene's staged order.
            crate::chart_trade::MARKET_MOVED.into(),
        ]);
        v.push(s(name, quick, end, a));
    }
    // Staged with no bracket preset: no legs, the entry's "+T" / "+S" handles instead.
    let mut h = args("--layout trading --size 1600x900 --theme dark --arm");
    h.extend([
        "--paper".into(),
        "buy 1 mkt; ack".into(),
        "--stage-order".into(),
        "buy limit 6011.00".into(),
    ]);
    v.push(s("chart-staged-handles", false, end, h));
    // The chart's right-click order section with its "…with bracket" rows (long 1: the buy
    // adds, so only it can be staged).
    let mut m = args("--layout trading --size 1600x900 --theme dark --arm");
    m.extend([
        "--paper".into(),
        "buy 1 mkt; ack".into(),
        "--context-menu".into(),
        "pane@500,600".into(),
    ]);
    v.push(s("chart-order-menu-bracket", true, end, m));
    // A strategy armed on the account: the switch says so (decision 0025).
    v.push(paper(
        "switch-strategy-armed",
        false,
        "strategy 1; manual",
        "",
    ));
    // In the daily break the paper broker refuses orders (session closed): a rejection on
    // its row and in the header.
    let mut r = args("--layout trading --size 1600x900 --theme dark --paper");
    // The market clock moves 30 min from the 21:59 last trade into the daily break first.
    r.push("wait 1800; buy 1 @ 6015.00; ack; buy 1 @ 6016.00; ack".into());
    v.push(s("rejected", false, end, r));
    v.push(paper(
        "lockout",
        false,
        "guard trades 1; buy 1 mkt; ack; sell 1 mkt; ack; buy 1 mkt; ack",
        "",
    ));
    v.push(paper(
        "position-pnl-off",
        false,
        "buy 2 mkt; ack",
        "--pnl off",
    ));
    let mut j = vec![
        "--workspace".into(),
        ws("journal.json"),
        "--size".into(),
        "1400x800".into(),
    ];
    j.push("--paper".into());
    j.push("buy 2 mkt; ack; print 6011.50; wait 20; sell 2 mkt; ack".into());
    v.push(s("journal", false, end, j));
    // The strategy panel (decision 0025) on the synthetic instances, beside a DOM.
    let mut st = vec![
        "--workspace".into(),
        ws("strategies.json"),
        "--size".into(),
        "1400x800".into(),
        "--strategies-demo".into(),
    ];
    st.push("--paper".into());
    // #3 trades a losing round trip as itself (its stop is hit), and the core halts it at
    // its daily loss.
    st.push(
        "strategy 3; buy 10 mkt; ack; sell 10 stop 6018.75; ack; print 6018.75; ack; manual".into(),
    );
    let mut zoomed = st.clone();
    v.push(s("strategies", true, end, st));
    // At 150 % UI zoom: the panel's text and buttons zoom too.
    zoomed.extend(["--ui-zoom".into(), "150".into()]);
    v.push(s("strategies-zoom-150", false, end, zoomed));
    // The chart's strategy markers (decision 0025) beside the panel: #3's entry, its stop and a
    // skipped signal, from the same script.
    let mut mk = vec![
        "--workspace".into(),
        ws("strategy-marks.json"),
        "--size".into(),
        "1400x800".into(),
        "--strategies-demo".into(),
    ];
    mk.push("--paper".into());
    mk.push(
        "strategy 3; buy 10 mkt; ack; sell 10 stop 6018.75; ack; print 6018.75; ack; manual".into(),
    );
    v.push(s("strategy-marks", false, end, mk));
    // Unlinked market panes: each header's hollow swatch and "–", wide and narrow (the DOM
    // and T&S headers at their narrowest).
    for (name, size) in [("unlinked", "1400x800"), ("unlinked-narrow", "560x600")] {
        v.push(s(
            name,
            false,
            end,
            vec![
                "--workspace".into(),
                ws("unlinked.json"),
                "--size".into(),
                size.into(),
            ],
        ));
    }
    if let Some(p) = v.iter_mut().find(|s| s.name == "popout") {
        p.windows = 2;
    }
    // The option chain's ticket (decision 0028), on the synthetic SPY chain: positions and
    // orders on their rows, and each way it says no.
    let held = "option 2026-11-09 682 call; buy 2 @ 1.89; ack; option 2026-11-09 686 put; sell 1 @ 3.00; ack; option 2026-11-09 683 call; buy 3 @ 1.18; ack; sell 2 @ 1.40; ack";
    for (name, quick, size, theme, paper, ticket) in [
        (
            "chain-ticket",
            true,
            "1600x700",
            "dark",
            held,
            "sell 683 call",
        ),
        (
            "chain-ticket-narrow",
            true,
            "340x640",
            "dark",
            held,
            "sell 683 call close",
        ),
        (
            "chain-ticket-light",
            false,
            "1400x600",
            "light",
            held,
            "buy 684 call",
        ),
        (
            "chain-far-from-mid",
            false,
            "1000x520",
            "dark",
            "",
            "buy 684 call at 3.00",
        ),
        (
            "chain-one-sided",
            false,
            "1000x520",
            "light",
            "",
            "buy 696 call",
        ),
        (
            "chain-stale-quote",
            false,
            "1000x520",
            "dark",
            "wait 1200; ack",
            "buy 684 call",
        ),
        (
            "chain-prop-account",
            false,
            "1000x520",
            "dark",
            "account paper-eval; ack",
            "buy 684 call",
        ),
        (
            "chain-one-click",
            false,
            "1000x520",
            "dark",
            held,
            "one-click",
        ),
    ] {
        let mut a = vec![
            "--workspace".to_owned(),
            ws("chain.json"),
            "--size".to_owned(),
            size.to_owned(),
            "--theme".to_owned(),
            theme.to_owned(),
            "--chain-ticket".to_owned(),
            ticket.to_owned(),
        ];
        if !paper.is_empty() {
            a.extend(["--paper".to_owned(), paper.to_owned()]);
        }
        let mut sc = s(name, quick, CHAIN_AT, a);
        sc.chain = true;
        v.push(sc);
    }
    // The option chart (decision 0029): rows on the chart's price scale; a pane too narrow for
    // both is the plain chain, and says so.
    for (name, quick, size, extra) in [
        ("chain-chart", true, "1600x700", "--theme dark"),
        ("chain-chart-light", false, "1600x700", "--theme light"),
        (
            "chain-chart-cb",
            false,
            "1600x700",
            "--theme dark --palette cb",
        ),
        ("chain-chart-narrow", false, "700x520", "--theme dark"),
        ("chain-chart-mid", false, "860x560", "--theme dark"),
    ] {
        let mut a = vec![
            "--workspace".to_owned(),
            ws("chain-chart.json"),
            "--size".to_owned(),
            size.to_owned(),
            "--paper".to_owned(),
            held.to_owned(),
        ];
        a.extend(args(extra));
        let mut sc = s(name, quick, CHAIN_AT, a);
        sc.chain = true;
        v.push(sc);
    }
    // Held and worked strikes past the rows: a marker at each edge, and at a pane barely wide
    // enough with the ticket covering the foot, both in one.
    let far = "option 2026-11-09 697 put; buy 2 @ 13.48; ack; option 2026-11-09 670 call; buy 1 @ 13.71; ack; option 2026-11-09 672 call; sell 2 @ 12.00; ack";
    for (name, quick, size, ticket) in [
        ("chain-chart-markers", true, "1600x700", None),
        (
            "chain-chart-markers-narrow",
            false,
            "780x560",
            Some("buy 684 put"),
        ),
    ] {
        let mut a = vec![
            "--workspace".to_owned(),
            ws("chain-chart.json"),
            "--size".to_owned(),
            size.to_owned(),
            "--paper".to_owned(),
            far.to_owned(),
        ];
        if let Some(open) = ticket {
            a.extend(["--chain-ticket".to_owned(), open.to_owned()]);
        }
        let mut sc = s(name, quick, CHAIN_AT, a);
        sc.chain = true;
        v.push(sc);
    }
    // Entry, Stop and Target lines (decision 0029): armed with their rows; a ticket's "When SPY";
    // a Stop not armed yet; a drag that refuses; and rows after a restart.
    let armed = "under 683.59; option 2026-11-09 684 call; buy 2 @ 0.65; ack; trigger stop 682.50; trigger target 686.00; option 2026-11-09 680 put; trigger entry 681.50 max 0.25 1";
    let long = "under 683.59; option 2026-11-09 684 call; buy 2 @ 0.65; ack";
    let stale = format!("{armed}; wait 11");
    let lines = [
        (
            "chain-lines-stale",
            false,
            stale.as_str(),
            None,
            "1600x800",
            "dark",
        ),
        ("chain-lines", true, armed, None, "1600x800", "dark"),
        ("chain-lines-light", false, armed, None, "1600x800", "light"),
        ("chain-lines-narrow", false, armed, None, "800x640", "dark"),
        (
            "chain-lines-when",
            true,
            long,
            Some("buy 685 call when 685.00"),
            "1600x800",
            "dark",
        ),
        (
            "chain-lines-when-narrow",
            false,
            long,
            Some("buy 685 call when 685.00"),
            "800x640",
            "dark",
        ),
        (
            "chain-lines-draft",
            false,
            long,
            Some("buy 684 call draft stop 682.50"),
            "1600x800",
            "dark",
        ),
        (
            "chain-lines-refused",
            false,
            armed,
            Some("drag stop 685.00"),
            "1600x800",
            "dark",
        ),
        (
            "chain-lines-restored",
            false,
            armed,
            Some("restored"),
            "1600x800",
            "dark",
        ),
    ];
    for (name, quick, script, shot, size, theme) in lines {
        let mut a = vec![
            "--workspace".to_owned(),
            ws("chain-chart.json"),
            "--size".to_owned(),
            size.to_owned(),
            "--theme".to_owned(),
            theme.to_owned(),
            "--paper".to_owned(),
            script.to_owned(),
        ];
        if let Some(shot) = shot {
            a.extend(["--chain-ticket".to_owned(), shot.to_owned()]);
        }
        let mut sc = s(name, quick, CHAIN_AT, a);
        sc.chain = true;
        v.push(sc);
    }
    // Every open position: a chart beside the positions pane with three of them (a long future,
    // a short one, an option whose exits are its triggers); the status line counts them from
    // any tab. Narrow, light with the colorblind palette, and flat too.
    let three = format!(
        "{armed}; future ES; print 6000.00; buy 2 mkt; ack; future NQ; print 21000.00; sell 1 mkt; ack"
    );
    for (name, quick, size, look, script) in [
        (
            "positions",
            true,
            "1400x600",
            "--theme dark",
            three.as_str(),
        ),
        (
            "positions-narrow",
            false,
            "760x520",
            "--theme dark",
            three.as_str(),
        ),
        (
            "positions-light-cb",
            false,
            "1400x600",
            "--theme light --palette cb",
            three.as_str(),
        ),
        ("positions-flat", false, "1000x400", "--theme dark", ""),
        // At 150 % UI zoom: the pane's header, titles, rows and buttons zoom with the rest.
        (
            "positions-zoom-150",
            false,
            "1400x600",
            "--theme dark --ui-zoom 150",
            three.as_str(),
        ),
        // 1000 px at 150 %: columns drop by the zoomed width, never run past the edge.
        (
            "positions-zoom-150-narrow",
            false,
            "1000x450",
            "--theme dark --ui-zoom 150",
            three.as_str(),
        ),
    ] {
        let mut a = vec![
            "--workspace".to_owned(),
            ws("positions.json"),
            "--size".to_owned(),
            size.to_owned(),
        ];
        if !script.is_empty() {
            a.extend(["--paper".to_owned(), script.to_owned()]);
        }
        a.extend(args(look));
        let mut sc = s(name, quick, CHAIN_AT, a);
        sc.chain = true;
        v.push(sc);
    }
    // A trigger no pane on screen shows: the workspace says so, in the plain chain's status too.
    {
        let mut a = vec![
            "--workspace".to_owned(),
            ws("chart-only.json"),
            "--size".to_owned(),
            "1200x500".to_owned(),
            "--paper".to_owned(),
            armed.to_owned(),
        ];
        a.extend(args("--theme dark"));
        let mut sc = s("trigger-off-screen", true, CHAIN_AT, a);
        sc.chain = true;
        v.push(sc);
    }
    // P&L to close (decision 0029): the chart continues to the close and the chain keeps one
    // side. The summary comes with the position's exits.
    for (name, quick, size, extra, shot) in [
        ("chain-pnl", true, "1600x800", "--theme dark", "pnl"),
        ("chain-pnl-light", false, "1600x800", "--theme light", "pnl"),
        (
            "chain-pnl-cb",
            false,
            "1600x800",
            "--theme dark --palette cb",
            "pnl",
        ),
        ("chain-pnl-narrow", false, "800x640", "--theme dark", "pnl"),
        ("chain-pnl-mid", false, "940x640", "--theme dark", "pnl"),
        (
            "chain-pnl-ticket-narrow",
            false,
            "800x640",
            "--theme dark",
            "buy 685 call pnl",
        ),
        (
            "chain-pnl-when-narrow",
            false,
            "800x640",
            "--theme dark",
            "buy 685 call when 685.00 pnl",
        ),
        (
            "chain-pnl-ticket",
            false,
            "1600x800",
            "--theme dark",
            "buy 685 call pnl",
        ),
        (
            "chain-pnl-put",
            false,
            "1600x800",
            "--theme dark",
            "buy 683 put pnl",
        ),
    ] {
        let mut a = vec![
            "--workspace".to_owned(),
            ws("chain-chart.json"),
            "--size".to_owned(),
            size.to_owned(),
            "--paper".to_owned(),
            armed.to_owned(),
            "--chain-ticket".to_owned(),
            shot.to_owned(),
        ];
        a.extend(args(extra));
        let mut sc = s(name, quick, CHAIN_AT, a);
        sc.chain = true;
        v.push(sc);
    }
    // The ticket covers the chain's foot and moves no row.
    let mut foot = s(
        "chain-chart-ticket",
        true,
        CHAIN_AT,
        vec![
            "--workspace".to_owned(),
            ws("chain-chart.json"),
            "--size".to_owned(),
            "1600x700".to_owned(),
            "--paper".to_owned(),
            held.to_owned(),
            "--chain-ticket".to_owned(),
            "buy 684 put".to_owned(),
        ],
    );
    foot.chain = true;
    v.push(foot);
    let mut cb = s(
        "chain-ticket-dark-cb",
        false,
        CHAIN_AT,
        vec![
            "--workspace".to_owned(),
            ws("chain.json"),
            "--size".to_owned(),
            "1400x600".to_owned(),
            "--palette".to_owned(),
            "cb".to_owned(),
            "--chain-ticket".to_owned(),
            "buy 684 put".to_owned(),
        ],
    );
    cb.chain = true;
    v.push(cb);
    let mut pop = s(
        "chain-popout",
        false,
        CHAIN_AT,
        vec![
            "--workspace".to_owned(),
            ws("chain-popout.json"),
            "--size".to_owned(),
            "900x600".to_owned(),
            "--chain-ticket".to_owned(),
            "buy 684 call".to_owned(),
        ],
    );
    pop.chain = true;
    pop.windows = 2;
    v.push(pop);
    // Every drawing tool, with a pinned measure and a note.
    let mut d = args("--layout chart --size 1400x800 --theme dark --timeframe 5m --drawings");
    d.push(ws("drawings.json"));
    v.push(s("drawings", false, end, d));
    // User styles (decision 0026): styled drawings in three theme/palette combinations, and
    // the style editor open on a trend line (dark) and a note (light).
    let styled = |name, theme: &str, extra: &str| {
        let mut a = args(&format!(
            "--layout chart --size 1400x800 --theme {theme} --timeframe 5m{extra} --drawings"
        ));
        a.push(ws("styled-drawings.json"));
        s(name, false, end, a)
    };
    v.push(styled("styled-drawings-dark", "dark", ""));
    v.push(styled("styled-drawings-light-cb", "light", " --palette cb"));
    let mut e = styled("style-editor-trend-dark", "dark", "");
    e.args.extend(["--style-editor".into(), "3".into()]);
    v.push(e);
    let mut e = styled("style-editor-note-light", "light", "");
    e.args.extend(["--style-editor".into(), "7".into()]);
    v.push(e);
    // Keyboard focus on a swatch that isn't the selected one: both are named.
    let mut e = styled("style-editor-focus-light", "light", "");
    e.args.extend(["--style-editor".into(), "3:0:1".into()]);
    v.push(e);
    // The color picker: a navy that fails on dark (its warnings live), with recent colors; on
    // light colorblind at 150 % UI zoom; and the eyedropper over a candle.
    let picker = |name, theme: &str, extra: &str, more: &[&str]| {
        let mut e = styled(name, theme, extra);
        e.args.extend(
            [
                "--style-editor",
                "3",
                "--color-picker",
                "1d2a6b",
                "--recent-colors",
                "f2e05a,ff9f1a,5ac8fa",
            ]
            .iter()
            .chain(more)
            .map(|a| (*a).to_owned()),
        );
        e
    };
    v.push(picker("color-picker-dark", "dark", "", &[]));
    v.push(picker(
        "color-picker-light-cb-zoom",
        "light",
        " --palette cb",
        &["--ui-zoom", "150"],
    ));
    v.push(picker(
        "color-picker-dropper-dark",
        "dark",
        "",
        &["--dropper", "706,600"],
    ));
    // The editor kept inside the plot: a line near the bottom edge (warnings and all), a
    // narrow short pane, and a chart tiled above a DOM.
    let edge = |name, size: &str, theme: &str, id: &str| {
        let mut a = args(&format!(
            "--layout chart --size {size} --theme {theme} --timeframe 5m --drawings"
        ));
        a.push(ws("editor-edge.json"));
        a.extend(["--style-editor".into(), id.into()]);
        s(name, false, end, a)
    };
    v.push(edge("style-editor-bottom-dark", "1400x800", "dark", "1"));
    v.push(edge(
        "style-editor-warnings-light-cb",
        "1400x800",
        "light --palette cb",
        "1",
    ));
    v.push(edge("style-editor-short", "420x300", "dark", "1"));
    // Keyboard focus on a row below the fold: the body scrolled to show it.
    v.push(edge(
        "style-editor-short-focus",
        "420x300",
        "dark",
        "1:0:0:4",
    ));
    v.push(edge("style-editor-narrow", "300x700", "dark", "1"));
    let mut t = vec![
        "--workspace".into(),
        ws("chart-over-dom.json"),
        "--size".into(),
        "1400x900".into(),
        "--drawings".into(),
        ws("editor-edge.json"),
    ];
    t.extend(["--style-editor".into(), "1".into()]);
    v.push(s("style-editor-tiled", false, end, t));
    // Study styles (decision 0026, PR 2): the style editor on a VWAP (dark), the volume
    // profile's POC (light) and the footprint (dark, colorblind: its cells, and its POC in a
    // color that warns); restyled studies (light and dark: a heavy band fill under the
    // candles); white footprint cells on dark, held back so their counts stay AA.
    let studies = |name, theme: &str, extra: &str| {
        let mut a = args(&format!(
            "--layout chart --size 1400x800 --theme {theme}{extra} --study-set"
        ));
        a.push(ws("studies-styled.json"));
        s(name, false, end, a)
    };
    let mut e = studies("study-style-vwap-dark", "dark", "");
    e.args
        .extend(["--studies-menu".into(), "style:vwap:0".into()]);
    v.push(e);
    let mut e = studies("study-style-profile-light", "light", "");
    e.args
        .extend(["--studies-menu".into(), "style:profile:2".into()]);
    v.push(e);
    v.push(studies("restyled-studies-light", "light", ""));
    v.push(studies("restyled-studies-dark", "dark", ""));
    // The VWAP's Band fill tab (light): a heavy fill held back under the candles, warned.
    let mut e = studies("study-style-vwap-band-fill-light", "light", "");
    e.args
        .extend(["--studies-menu".into(), "style:vwap:0:2".into()]);
    v.push(e);
    // The studies menu's lists (owner-approved controls, 2026-11-11), floating over the menu:
    // the VWAP's bands (several at once, dark), the profile's grouping with Custom… (light,
    // 150 %), and the footprint's − / + numbers beside its grouping list (dark).
    for (name, theme, menu) in [
        ("studies-drop-vwap-bands", "dark", "drop:vwap:0:Bands"),
        (
            "studies-drop-profile-grouping-light-150",
            "light --ui-zoom 150",
            "drop:profile:Grouping",
        ),
    ] {
        let mut e = studies(name, theme, "");
        e.args.extend(["--studies-menu".into(), menu.into()]);
        v.push(e);
    }
    // Volume (owner-approved 2026-11-09): plain bars in a lower pane under cumulative delta's
    // (two stacked lower panes, quick set); colored by candle, split and averaged (light,
    // colorblind); as an overlay; its editor open.
    let volume = |name, quick, opts: &str, theme: &str, extra: &str| {
        let mut a = args(&format!(
            "--layout chart --size 1400x800 --theme {theme} --volume {opts}{extra}"
        ));
        if name == "volume-with-delta-dark" {
            // A study set replaces the command line's studies: both are in it.
            a.push("--study-set".into());
            a.push(ws("volume-and-delta.json"));
        }
        s(name, quick, end, a)
    };
    v.push(volume("volume-with-delta-dark", true, "pane", "dark", ""));
    // UI zoom changed to 150 % after the first frame, as Ctrl+= does (owner's bug: the time
    // labels kept their 100 % spacing and ran together). With two lower panes: their time
    // axis too.
    let mut zoomed = args(
        "--layout chart --size 1400x800 --theme light --volume pane --ui-zoom 100 --ui-zoom-to 150",
    );
    zoomed.push("--study-set".into());
    zoomed.push(ws("volume-and-delta.json"));
    v.push(s("zoom-to-150-lower-panes", false, end, zoomed));
    v.push(volume(
        "volume-split-average-light-cb",
        false,
        "color,split,avg",
        "light",
        " --palette cb",
    ));
    v.push(volume(
        "volume-overlay-dark",
        false,
        "overlay,avg",
        "dark",
        "",
    ));
    v.push(volume(
        "volume-editor-dark",
        false,
        "color,avg",
        "dark",
        " --studies-menu edit:volume:0",
    ));
    // An unstyled VWAP with ±1σ ±2σ ±3σ bands: its fills now sit under the candles.
    for theme in ["light", "dark"] {
        let mut a = args(&format!(
            "--layout chart --size 1400x800 --theme {theme} --study-set"
        ));
        a.push(ws("vwap-bands-unstyled.json"));
        v.push(s(
            if theme == "light" {
                "unstyled-vwap-bands-light"
            } else {
                "unstyled-vwap-bands-dark"
            },
            false,
            end,
            a,
        ));
    }
    let footprint = |name, palette: &str, set: &str, menu: &str| {
        s(
            name,
            false,
            end,
            vec![
                "--workspace".into(),
                ws("footprint.json"),
                "--size".into(),
                "1400x800".into(),
                "--palette".into(),
                palette.into(),
                "--study-set".into(),
                ws(set),
                "--studies-menu".into(),
                menu.into(),
            ],
        )
    };
    v.push(footprint(
        "studies-drop-footprint-grouping",
        "standard",
        "footprint-styled.json",
        "drop:footprint:Grouping",
    ));
    v.push(footprint(
        "study-style-footprint-dark-cb",
        "cb",
        "footprint-styled.json",
        "style:footprint",
    ));
    v.push(footprint(
        "study-style-footprint-poc-dark-cb",
        "cb",
        "footprint-styled.json",
        "style:footprint:2",
    ));
    // The Imbalances tab: one color for both sides' marks, warned (place tells them apart).
    v.push(footprint(
        "study-style-footprint-imbalances-dark-cb",
        "cb",
        "footprint-styled.json",
        "style:footprint:1",
    ));
    v.push(footprint(
        "study-style-footprint-white-cells-dark",
        "standard",
        "footprint-white-cells.json",
        "style:footprint",
    ));
    // Trading lines and a styled horizontal line a few ticks apart: a pending, a working and
    // a held order, a position, and the drawing with its axis tag.
    let mut o = args("--layout trading --size 1600x900 --theme dark --arm --drawings");
    o.push(ws("orders-drawings.json"));
    o.push("--paper".into());
    o.push(ORDERS.into());
    v.push(s("styled-line-near-orders", false, end, o));
    // Axis tags crowding the last price: stepped off it, and past each other.
    let mut c = args("--layout chart --size 1200x700 --theme dark --timeframe 5m --drawings");
    c.push(ws("tags-near-last.json"));
    v.push(s("drawing-tags-near-last", false, end, c));
    // Tick bars (roadmap 9b): both themes and palettes, the hovered forming bar's readout,
    // continuous bars, footprints, the list with the tick presets (also on a short pane), and a
    // feed opened mid-session (the "tick bars from" label).
    let chart = |a: &str| args(&format!("--layout chart --size 1400x800 {a}"));
    // Shift+drag: the measure it leaves (both themes; a narrow chart too).
    v.push(s(
        "chart-shift-measure",
        true,
        end,
        chart("--theme dark --shift-drag 700,520:1150,180"),
    ));
    v.push(s(
        "chart-shift-measure-light",
        false,
        end,
        chart("--theme light --shift-drag 700,520:1150,180"),
    ));
    // A placed measure moved by a drag inside it; a readout with only ticks and bars (light);
    // the measure's right-click menu; at 150 %.
    v.push(s(
        "measure-moved",
        true,
        end,
        chart("--theme dark --shift-drag 700,520:1150,180 --move-measure 900,350:600,420"),
    ));
    v.push(s(
        "measure-ticks-bars-light",
        false,
        end,
        chart("--theme light --shift-drag 700,520:1150,180 --measure-parts ticks,bars"),
    ));
    v.push(s(
        "measure-menu",
        false,
        end,
        chart("--theme dark --shift-drag 700,520:1150,180 --context-menu measure@900,350"),
    ));
    v.push(s(
        "measure-menu-pnl-off-150",
        false,
        end,
        chart("--theme dark --shift-drag 500,520:1000,180 --pnl off --ui-zoom 150 --context-menu measure@700,300"),
    ));
    v.push(s(
        "measure-zoom-150",
        false,
        end,
        chart("--theme dark --shift-drag 500,520:1000,180 --ui-zoom 150"),
    ));
    v.push(s(
        "chart-shift-measure-narrow",
        false,
        end,
        args("--layout chart --size 520x600 --theme dark --shift-drag 120,420:400,160"),
    ));
    v.push(s(
        "chart-233t",
        true,
        end,
        chart("--theme dark --timeframe 233t --cursor 1290,150"),
    ));
    v.push(s(
        "chart-1000t-light",
        false,
        end,
        chart("--theme light --timeframe 1000t --cursor 1290,150"),
    ));
    v.push(s(
        "chart-233t-cb",
        false,
        end,
        chart("--theme dark --palette cb --timeframe 233t --cursor 1290,150"),
    ));
    v.push(s(
        "chart-500t-light-cb",
        false,
        end,
        chart("--theme light --palette cb --timeframe 500t --cursor 1290,150"),
    ));
    // Just after a session open: the crosshair on the previous session's short last bar
    // ("partial (n of 233 trades)"), then on the new session's forming bar; the dashed divider
    // between them.
    v.push(s(
        "chart-233t-break-partial",
        false,
        BREAK_AT,
        chart("--theme dark --timeframe 233t --cursor 1274,150"),
    ));
    v.push(s(
        "chart-233t-break-forming",
        false,
        BREAK_AT,
        chart("--theme light --timeframe 233t --cursor 1298,400"),
    ));
    v.push(s(
        "chart-1000t-continuous",
        false,
        end,
        chart("--theme dark --timeframe 1000t --tick-continuous"),
    ));
    for (name, theme) in [
        ("footprint-500t", "--theme dark"),
        ("footprint-500t-light", "--theme light"),
        ("footprint-500t-cb", "--theme dark --palette cb"),
    ] {
        let mut a = vec!["--workspace".into(), ws("footprint-500t.json")];
        a.extend(args(&format!("--size 1400x800 {theme}")));
        v.push(s(name, false, end, a));
    }
    v.push(s(
        "timeframe-menu-ticks",
        false,
        end,
        args("--layout chart --size 1000x700 --theme dark --timeframe 233t --timeframe-menu"),
    ));
    v.push(s(
        "timeframe-menu-short",
        false,
        end,
        args("--layout chart --size 900x420 --theme light --timeframe 500t --timeframe-menu"),
    ));
    for (name, theme) in [
        ("chart-1000t-late", "--theme dark"),
        ("chart-1000t-late-light", "--theme light"),
    ] {
        let mut sc = s(
            name,
            false,
            end,
            chart(&format!("{theme} --timeframe 1000t")),
        );
        sc.late = true;
        v.push(sc);
    }
    v
}

/// The workspace files the scenes use, written into `dir`.
fn write_workspaces(dir: &Path) -> Result<()> {
    let es = r#"{ "product": 1, "symbol": "ES" }"#;
    let doc = |layout: String| {
        format!(
            r#"{{ "version": 3, "name": "gallery", "crosshair_sync": false,
  "windows": [ {{ "active_tab": 0, "tabs": [ {{ "focused": 0, "layout": {layout} }} ] }} ] }}"#
        )
    };
    let pane = |kind: &str, extra: &str| {
        format!(
            r#"{{ "pane": {{ "kind": "{kind}", "link": 0, "state": "shown", "target": {es}{extra} }} }}"#
        )
    };
    let popout = format!(
        r#"{{ "split": {{ "axis": "row", "ratio": 0.6, "first": {}, "second": {{ "pane": {{ "kind": "tape", "link": 0, "state": "detached", "target": {es}, "window": {{ "x": 100, "y": 100, "w": 420, "h": 600 }} }} }} }} }}"#,
        pane("dom", "")
    );
    let footprint = pane(
        "chart",
        r#", "chart": { "bar_px": 90.0, "timeframe": "30m", "footprint": { "mode": "bid_ask" } }"#,
    );
    let footprint_500t = pane(
        "chart",
        r#", "chart": { "bar_px": 90.0, "timeframe": "500t", "footprint": { "mode": "bid_ask" } }"#,
    );
    let journal = format!(
        r#"{{ "split": {{ "axis": "column", "ratio": 0.5, "first": {}, "second": {} }} }}"#,
        pane("dom", ""),
        pane("journal", "")
    );
    let strategies = format!(
        r#"{{ "split": {{ "axis": "row", "ratio": 0.35, "first": {}, "second": {} }} }}"#,
        pane("dom", ""),
        pane("strategies", "")
    );
    // Unlinked panes (a new tab with every link group in use): their headers say so.
    let unlinked = format!(
        r#"{{ "split": {{ "axis": "row", "ratio": 0.6, "first": {}, "second": {{ "split": {{ "axis": "column", "ratio": 0.6, "first": {}, "second": {} }} }} }} }}"#,
        pane("chart", ""),
        pane("dom", ""),
        pane("tape", "")
    )
    .replace(r#""link": 0"#, r#""link": null"#);
    let strategy_marks = format!(
        r#"{{ "split": {{ "axis": "row", "ratio": 0.6, "first": {}, "second": {} }} }}"#,
        pane("chart", r#", "chart": { "bar_px": 14.0 }"#),
        pane("strategies", "")
    );
    let anchor =
        |t: &str, p: i64| format!(r#"{{ "time": "2026-11-05T{t}:00.000Z", "price": {p} }}"#);
    let drawings = format!(
        r#"{{ "version": 1, "product": "ES", "drawings": [
  {{ "id": 1, "tool": "hline", "a": {}, "color": 2, "dashed": true }},
  {{ "id": 2, "tool": "hray", "a": {}, "width": 2 }},
  {{ "id": 3, "tool": "trend", "a": {}, "b": {}, "color": 3 }},
  {{ "id": 4, "tool": "ray", "a": {}, "b": {} }},
  {{ "id": 5, "tool": "rect", "a": {}, "b": {}, "color": 1 }},
  {{ "id": 6, "tool": "vline", "a": {}, "color": 2, "dashed": true }},
  {{ "id": 7, "tool": "note", "a": {}, "text": "Low of day: sweep and reclaim" }},
  {{ "id": 8, "tool": "measure", "a": {}, "b": {} }} ] }}"#,
        anchor("14:00", 601_600),
        anchor("15:45", 599_000),
        anchor("14:30", 603_400),
        anchor("15:45", 599_000),
        anchor("15:45", 599_000),
        anchor("17:30", 600_400),
        anchor("18:00", 600_700),
        anchor("20:40", 600_300),
        anchor("14:30", 603_500),
        anchor("15:50", 598_900),
        anchor("15:45", 599_000),
        anchor("21:00", 602_000),
    );
    std::fs::write(dir.join("drawings.json"), drawings).context("drawings.json")?;
    // Styled: custom colors, line types and widths, extensions, axis tags, a zone's own fill,
    // a large note without its box (in a yellow that needs one on light).
    let styled = format!(
        r##"{{ "version": 1, "product": "ES", "drawings": [
  {{ "id": 1, "tool": "hline", "a": {}, "color": 1, "axis_tag": true }},
  {{ "id": 2, "tool": "hray", "a": {}, "line_color": "#f719cb", "width": 2, "axis_tag": true }},
  {{ "id": 3, "tool": "trend", "a": {}, "b": {}, "line_color": "#9c8756", "width": 3,
     "dashed": true, "line": "dash-dot", "extend_right": true }},
  {{ "id": 4, "tool": "ray", "a": {}, "b": {}, "color": 3, "dashed": true, "line": "dotted" }},
  {{ "id": 5, "tool": "rect", "a": {}, "b": {}, "line_color": "#f719cb",
     "fill_color": "#9c8756", "fill_opacity": 25, "axis_tag": true }},
  {{ "id": 6, "tool": "vline", "a": {}, "color": 2, "width": 2 }},
  {{ "id": 7, "tool": "note", "a": {}, "text": "Yellow note, no box",
     "text_color": "#e0b000", "text_bg": false, "text_size": "l" }} ] }}"##,
        anchor("14:00", 601_600),
        anchor("15:45", 599_000),
        anchor("14:30", 603_400),
        anchor("15:45", 599_000),
        anchor("15:45", 599_000),
        anchor("17:30", 600_400),
        anchor("18:00", 600_700),
        anchor("20:40", 600_300),
        anchor("14:30", 603_500),
        anchor("15:50", 598_900),
    );
    std::fs::write(dir.join("styled-drawings.json"), styled).context("styled-drawings.json")?;
    // A horizontal line low on the chart in a color that looks like up (every warning the
    // editor has for it), with its price tag; a zone under it.
    let edge = format!(
        r##"{{ "version": 1, "product": "ES", "drawings": [
  {{ "id": 1, "tool": "hline", "a": {}, "line_color": "#3fcf9a", "axis_tag": true }},
  {{ "id": 2, "tool": "rect", "a": {}, "b": {}, "color": 2 }} ] }}"##,
        anchor("19:00", 599_000),
        anchor("15:00", 599_250),
        anchor("17:00", 599_500),
    );
    std::fs::write(dir.join("editor-edge.json"), edge).context("editor-edge.json")?;
    // Restyled studies: a VWAP in a custom dashed line with dotted ±1σ edges, cumulative
    // delta in study colors, a split profile with every part styled.
    let studies = r##"{
  "indicators": [
    { "kind": "vwap", "reset": "eth", "bands": 3, "color": 1,
      "style": { "line": { "color": "#f719cb", "line": "dashed" },
                 "bands": { "shown": true, "line": "dotted" },
                 "fill": { "opacity": 100 } } },
    { "kind": "cum_delta", "reset": "eth",
      "style": { "up": { "color": "p1" }, "down": { "color": "#9c8756" } } } ],
  "profile": { "mode": "session", "split": true },
  "styles": { "profile": { "buy": { "color": "p1" }, "sell": { "color": "#9c8756" },
                           "poc": { "color": "#f719cb", "width": 2, "line": "dashed" },
                           "va": { "color": "p3", "opacity": 75 } } } }"##;
    std::fs::write(dir.join("studies-styled.json"), studies).context("studies-styled.json")?;
    let both =
        r##"{ "indicators": [ { "kind": "volume" }, { "kind": "cum_delta", "reset": "eth" } ] }"##;
    std::fs::write(dir.join("volume-and-delta.json"), both).context("volume-and-delta.json")?;
    let bands = r##"{ "indicators": [ { "kind": "vwap", "reset": "eth", "bands": 7 } ] }"##;
    std::fs::write(dir.join("vwap-bands-unstyled.json"), bands)
        .context("vwap-bands-unstyled.json")?;
    let fp = r##"{ "footprint": { "mode": "bid_ask" },
  "styles": { "footprint": { "cells": { "color": "p1", "opacity": 75 },
                             "imbalance": { "color": "#f719cb" },
                             "poc": { "color": "#4fa8ff" } } } }"##;
    std::fs::write(dir.join("footprint-styled.json"), fp).context("footprint-styled.json")?;
    let white = r##"{ "footprint": { "mode": "bid_ask" },
  "styles": { "footprint": { "cells": { "color": "#ffffff" } } } }"##;
    std::fs::write(dir.join("footprint-white-cells.json"), white)
        .context("footprint-white-cells.json")?;
    let near = format!(
        r##"{{ "version": 1, "product": "ES", "drawings": [
  {{ "id": 1, "tool": "hline", "a": {}, "line_color": "#f719cb", "width": 2, "dashed": true,
     "axis_tag": true }},
  {{ "id": 2, "tool": "hline", "a": {}, "color": 1, "line": "dotted", "dashed": true,
     "axis_tag": true }} ] }}"##,
        anchor("19:00", 601_525),
        anchor("19:00", 601_925),
    );
    std::fs::write(dir.join("orders-drawings.json"), near).context("orders-drawings.json")?;
    let crowd = format!(
        r##"{{ "version": 1, "product": "ES", "drawings": [
  {{ "id": 1, "tool": "hline", "a": {}, "line_color": "#f719cb", "axis_tag": true }},
  {{ "id": 2, "tool": "hline", "a": {}, "color": 1, "axis_tag": true }},
  {{ "id": 3, "tool": "hline", "a": {}, "color": 3, "axis_tag": true }},
  {{ "id": 4, "tool": "hline", "a": {}, "line_color": "#9c8756", "axis_tag": true }} ] }}"##,
        anchor("19:00", LAST_TICKS - 25),
        anchor("19:00", LAST_TICKS),
        anchor("19:00", LAST_TICKS + 25),
        anchor("19:00", LAST_TICKS + 50),
    );
    std::fs::write(dir.join("tags-near-last.json"), crowd).context("tags-near-last.json")?;
    let chart_over_dom = format!(
        r#"{{ "split": {{ "axis": "column", "ratio": 0.5, "first": {}, "second": {} }} }}"#,
        pane(
            "chart",
            r#", "chart": { "bar_px": 8.0, "timeframe": "5m" }"#
        ),
        pane("dom", "")
    );
    let chain = r#"{ "pane": { "kind": "chain", "link": 0, "state": "shown", "target": { "product": 2001, "symbol": "SPY" } } }"#;
    std::fs::write(
        dir.join("chain.json"),
        format!(
            r#"{{ "version": 6, "name": "gallery", "crosshair_sync": false,
  "windows": [ {{ "active_tab": 0, "tabs": [ {{ "focused": 0, "layout": {chain} }} ] }} ] }}"#
        ),
    )
    .context("chain.json")?;
    // A chart over the positions pane, in one link group.
    let positions = format!(
        r#"{{ "split": {{ "axis": "column", "ratio": 0.55, "first": {}, "second": {} }} }}"#,
        chain.replacen(r#""kind": "chain""#, r#""kind": "chart""#, 1),
        chain.replacen(r#""kind": "chain""#, r#""kind": "positions""#, 1),
    );
    std::fs::write(
        dir.join("positions.json"),
        format!(
            r#"{{ "version": 8, "name": "gallery", "crosshair_sync": false,
  "windows": [ {{ "active_tab": 0, "tabs": [ {{ "focused": 1, "layout": {positions} }} ] }} ] }}"#
        ),
    )
    .context("positions.json")?;
    // A chart of the same underlying, with no chain to show its triggers.
    let chart_only = chain.replacen(r#""kind": "chain""#, r#""kind": "chart""#, 1);
    std::fs::write(
        dir.join("chart-only.json"),
        format!(
            r#"{{ "version": 6, "name": "gallery", "crosshair_sync": false,
  "windows": [ {{ "active_tab": 0, "tabs": [ {{ "focused": 0, "layout": {chart_only} }} ] }} ] }}"#
        ),
    )
    .context("chart-only.json")?;
    // The option chart (decision 0029): the same pane with its chart on.
    let chain_chart = chain.replacen(
        r#""kind": "chain""#,
        r#""kind": "chain", "chain_chart": true"#,
        1,
    );
    assert_ne!(
        chain_chart, chain,
        "the chain workspace names its pane kind"
    );
    std::fs::write(
        dir.join("chain-chart.json"),
        format!(
            r#"{{ "version": 7, "name": "gallery", "crosshair_sync": false,
  "windows": [ {{ "active_tab": 0, "tabs": [ {{ "focused": 0, "layout": {chain_chart} }} ] }} ] }}"#
        ),
    )
    .context("chain-chart.json")?;
    let spy = r#"{ "product": 2001, "symbol": "SPY" }"#;
    std::fs::write(
        dir.join("chain-popout.json"),
        format!(
            r#"{{ "version": 6, "name": "gallery", "crosshair_sync": false,
  "windows": [ {{ "active_tab": 0, "tabs": [ {{ "focused": 0, "layout": {{ "split": {{ "axis": "row", "ratio": 0.6, "first": {{ "pane": {{ "kind": "tape", "link": 0, "state": "shown", "target": {spy} }} }}, "second": {{ "pane": {{ "kind": "chain", "link": 0, "state": "detached", "target": {spy}, "window": {{ "x": 100, "y": 100, "w": 760, "h": 560 }} }} }} }} }} }} ] }} ] }}"#
        ),
    )
    .context("chain-popout.json")?;
    for (f, layout) in [
        ("chart-over-dom.json", chart_over_dom),
        ("popout.json", popout),
        ("footprint.json", footprint),
        ("footprint-500t.json", footprint_500t),
        ("journal.json", journal),
        ("strategies.json", strategies),
        ("strategy-marks.json", strategy_marks),
        ("unlinked.json", unlinked),
    ] {
        std::fs::write(dir.join(f), doc(layout)).with_context(|| f.to_owned())?;
    }
    Ok(())
}

/// Render the scenes (the quick set unless `full`) into `dir`; 0 when every scene rendered and
/// the lint found nothing, else 1.
pub fn run(dir: &Path, full: bool) -> Result<i32> {
    std::fs::create_dir_all(dir).with_context(|| dir.display().to_string())?;
    let replay = dir.join("synth-es.lfr");
    lines_feed::synth::write_es_sessions(&replay, true, false).context("synthetic replay")?;
    let late = dir.join("synth-es-late.lfr");
    let from = lines_core::UtcMillis::parse_iso(LATE_FROM).context("late start")?;
    lines_feed::synth::write_es_sessions_from(&late, true, false, Some(from))
        .context("synthetic late replay")?;
    let chain = dir.join("synth-spy-chain.lfr");
    lines_feed::synth_chain::write_spy_chain(&chain).context("synthetic chain")?;
    write_workspaces(dir)?;
    let exe = std::env::current_exe().context("current exe")?;
    let mut summary = String::new();
    let mut bad = 0;
    for scene in scenes(dir).into_iter().filter(|s| full || s.quick) {
        let png = dir.join(format!("{}.png", scene.name));
        // A reused DIR: no report from an earlier run may stand in for this one.
        for old in reports(dir, scene.name) {
            let _ = std::fs::remove_file(old);
        }
        let out = Command::new(&exe)
            .arg("--replay")
            .arg(if scene.chain {
                &chain
            } else if scene.late {
                &late
            } else {
                &replay
            })
            .args(["--at", scene.at, "--layout-report", "--screenshot"])
            .arg(&png)
            .args(&scene.args)
            .output()
            .with_context(|| format!("run scene {}", scene.name))?;
        if !out.status.success() {
            bad += 1;
            summary.push_str(&format!(
                "{}: FAILED ({})\n{}\n",
                scene.name,
                out.status,
                String::from_utf8_lossy(&out.stderr)
                    .lines()
                    .filter(|l| !l.contains("INFO"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
            continue;
        }
        let known: Vec<&str> = KNOWN
            .iter()
            .filter(|(s, _)| *s == scene.name)
            .map(|(_, f)| *f)
            .collect();
        let mut seen: Vec<String> = Vec::new();
        let found = reports(dir, scene.name);
        if found.len() < scene.windows {
            bad += 1;
            summary.push_str(&format!(
                "{}: {} of {} windows reported\n",
                scene.name,
                found.len(),
                scene.windows
            ));
        }
        for report in found {
            // A missing report, or a frame that recorded nothing, checked nothing: a failure.
            let Ok(text) = std::fs::read_to_string(&report) else {
                bad += 1;
                summary.push_str(&format!("{}: no lint report\n", report.display()));
                continue;
            };
            let items: usize = text
                .lines()
                .next()
                .and_then(|l| l.strip_prefix("# "))
                .and_then(|l| l.strip_suffix(" items"))
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
            if items == 0 {
                bad += 1;
                summary.push_str(&format!("{}: recorded nothing\n", report.display()));
                continue;
            }
            let text: String = text
                .lines()
                .filter(|l| !l.starts_with('#'))
                .map(|l| {
                    format!(
                        "{l}
"
                    )
                })
                .collect();
            let shot = report
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default();
            let (old, new): (Vec<&str>, Vec<&str>) = text.lines().partition(|l| known.contains(l));
            seen.extend(old.iter().map(|l| l.to_string()));
            if new.is_empty() {
                summary.push_str(&format!("{shot}: clean"));
                if !old.is_empty() {
                    summary.push_str(&format!(" ({} known, docs/known-gaps.md)", old.len()));
                }
                summary.push('\n');
            } else {
                bad += 1;
                summary.push_str(&format!("{shot}: {} findings\n", new.len()));
                for l in new {
                    summary.push_str(&format!("  {l}\n"));
                }
            }
        }
        for k in &known {
            if !seen.iter().any(|s| s == k) {
                bad += 1;
                summary.push_str(&format!(
                    "{}: known finding no longer occurs; remove it from gallery::KNOWN and docs/known-gaps.md: {k}\n",
                    scene.name
                ));
            }
        }
    }
    std::fs::write(dir.join("gallery.txt"), &summary)?;
    print!("{summary}");
    println!(
        "gallery: {} -> {}",
        if bad == 0 { "clean" } else { "FINDINGS" },
        dir.display()
    );
    Ok(if bad == 0 { 0 } else { 1 })
}

/// The lint reports a scene wrote: its main window and any popped-out panes.
fn reports(dir: &Path, name: &str) -> Vec<PathBuf> {
    let mut v = vec![dir.join(format!("{name}.lint.txt"))];
    // The Settings window (`--settings`) reports beside the main one.
    let settings = dir.join(format!("{name}-settings.lint.txt"));
    if settings.exists() {
        v.push(settings);
    }
    for n in 1.. {
        let p = dir.join(format!("{name}-pane-{n}.lint.txt"));
        if !p.exists() {
            break;
        }
        v.push(p);
    }
    v
}

/// Refuse to write a gallery over something that isn't one.
pub fn check_dir(dir: &Path) -> Result<()> {
    if dir.exists() && !dir.is_dir() {
        bail!("--gallery {}: not a directory", dir.display());
    }
    Ok(())
}
