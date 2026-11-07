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
}

/// Findings that are known and documented (`docs/known-gaps.md`): (scene, finding line). They
/// are reported as known instead of failing; a known finding that no longer occurs fails, so
/// this list can't go stale.
pub const KNOWN: &[(&str, &str)] = &[];

/// Before the first trade: an empty chart and book.
const EMPTY_AT: &str = "2026-11-03T22:30:00Z";

fn args(s: &str) -> Vec<String> {
    s.split_whitespace().map(str::to_owned).collect()
}

/// Paper orders around the replay's last price (6019.50): working, held bracket exits,
/// pending (after the last `ack`).
const ORDERS: &str =
    "buy 1 mkt; ack; buy 1 @ 6015.00; buy 1 @ 6016.00 bracket 8/16; ack; buy 1 @ 6014.00";

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
    v.push(paper("orders-disarmed", false, ORDERS, ""));
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
    if let Some(p) = v.iter_mut().find(|s| s.name == "popout") {
        p.windows = 2;
    }
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
    let journal = format!(
        r#"{{ "split": {{ "axis": "column", "ratio": 0.5, "first": {}, "second": {} }} }}"#,
        pane("dom", ""),
        pane("journal", "")
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
    for (f, layout) in [
        ("popout.json", popout),
        ("footprint.json", footprint),
        ("journal.json", journal),
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
            .arg(&replay)
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
