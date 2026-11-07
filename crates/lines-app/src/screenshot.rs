//! Reproducible screenshot mode (decision 0009).
//!
//! Inputs fully determine the output on a given machine: replay file + `--at` cut-off
//! (replayed at full speed, so wall-clock pacing doesn't matter), window size, theme, the
//! workspace (`--layout`, default `trading`, or `--workspace`: a built-in such as `pair`, or a
//! workspace file), and a frozen clock (headers show the replay's market time instead of live
//! rates). The frame is produced with `Window::draw` and read back from the GPU with GPUI's
//! `Window::render_to_image` (DirectX staging-texture copy on Windows), so the window is never
//! shown. No user settings or workspaces are read or written.
//!
//! The first window is rendered (its active tab; other tabs show in the tab bar and, being
//! hidden, paint nothing). Popped-out panes of that window's tabs are rendered too, each into
//! `<out stem>-pane-<n>.png` at its saved window size. Panes whose target the recording doesn't hold show greyed with the
//! reason (e.g. `--workspace pair` on an ES recording greys NQ).
//!
//! `--cursor X,Y` places a fixed pointer (logical px from the plot's top-left corner) on the
//! first chart with data, so the crosshair, its axis tags and the OHLCV readout appear in the
//! image. No OS pointer is involved: the position is handed straight to the view.
//!
//! DPI scale cannot be forced: GPUI on Windows takes it from the monitor
//! (`GetDpiForWindow`). `--scale` therefore *asserts* the scale and fails on mismatch.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use gpui::{AnyWindowHandle, App, AppContext, AsyncApp, px, size};
use lines_feed::registry::FeedRegistry;
use lines_feed::store::FeedStatus;

use crate::feeds::{Feeds, PaneFeed};
use crate::pane_window;
use crate::picker_view::AppSettings;
use crate::trading::{Trading, TradingSetup};
use crate::workspace::{self, Workspace};
use crate::wsdoc::{WindowDoc, WorkspaceDoc};
use lines_trading::PaperConfig;

/// Everything besides the data that determines the image.
pub struct Shot {
    pub out: PathBuf,
    /// Window size in logical px.
    pub size: (f32, f32),
    /// Required DPI scale (asserted, not forced).
    pub scale: Option<f32>,
    pub doc: WorkspaceDoc,
    /// A scripted paper session run before the frame (`--paper`).
    pub paper: Option<String>,
    /// DOMs start armed (`--arm`).
    pub arm: bool,
    /// Charts and DOMs start with the account list open (`--account-menu`).
    pub account_menu: bool,
    /// The P&L display (`--pnl`; default: the app's default).
    pub pnl: Option<crate::pnl::PnlDisplay>,
    /// Drawings to show (`--drawings FILE`), in memory only.
    pub drawings: Option<(lines_core::Product, Vec<lines_core::drawings::Drawing>)>,
}

pub fn run(registry: FeedRegistry, ws: Workspace, shot: Shot) -> Result<i32> {
    // The first window, wherever it was saved: the screenshot fixes its size.
    let window = WindowDoc {
        placement: None,
        ..shot.doc.windows[0].clone()
    };

    // 1. Replay to the cut-off before any UI exists: the frame depends only on the data. The
    // feeds started here are the ones the panes join (the registry shares them).
    let mut feeds = Vec::new();
    for pane in window.tabs.iter().flat_map(|t| t.layout.leaves()) {
        let f = PaneFeed::open(
            &registry,
            &ws.env.base,
            &ws.env.availability,
            pane.target.as_ref(),
            &ws.env.fallback,
        );
        if f.sub.is_some() {
            feeds.push(f);
        }
    }
    let deadline = Instant::now() + Duration::from_secs(120);
    for f in &feeds {
        loop {
            match f.store().read().status.clone() {
                FeedStatus::ReplayFinished => break,
                FeedStatus::Failed(e) => bail!("replay failed: {e}"),
                _ if Instant::now() > deadline => bail!("replay did not finish within 120 s"),
                _ => {}
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    // 2. Render one frame off-screen and read it back. Every outcome ends the process from
    // inside the app with its own exit code: quitting the app is not enough, because on
    // macOS it terminates the process with status 0 before `run` returns.
    let Shot {
        out,
        size,
        scale,
        doc,
        paper,
        arm,
        account_menu,
        pnl,
        drawings: seed,
    } = shot;
    // A deterministic paper core: in memory, its clock moved only by the data and the script.
    let mut trading = Trading::start(
        TradingSetup::manual(),
        registry.clone(),
        ws.env.base.clone(),
    );
    trading.start_armed = arm;
    trading.start_account_menu = account_menu;
    let script = match &paper {
        Some(s) => {
            let f = feeds
                .iter()
                .find(|f| f.store().read().contract.is_some())
                .ok_or_else(|| anyhow!("--paper: the recording has no contract to trade"))?;
            let (product, contract, at) = {
                let d = f.store().read();
                (
                    d.product.clone(),
                    d.contract.expect("checked"),
                    d.market_time,
                )
            };
            let account = trading
                .default_account()
                .ok_or_else(|| anyhow!("--paper: no paper account"))?;
            let steps = crate::trading::parse_script(s, &product, contract, &account)
                .map_err(|e| anyhow!("--paper: {e:#}"))?;
            trading.start_account = steps.iter().find_map(|s| match s {
                crate::trading::Step::Account(a) => Some(a.clone()),
                _ => None,
            });
            Some((
                steps,
                at.ok_or_else(|| anyhow!("--paper: the replay has no market time"))?,
            ))
        }
        None => None,
    };
    gpui_platform::application().run(move |cx: &mut App| {
        let _keep = &feeds;
        cx.set_global(Feeds(registry));
        cx.set_global(trading);
        let mut settings = AppSettings::ephemeral();
        if let Some(m) = pnl {
            settings.settings.pnl_display = m;
        }
        cx.set_global(settings);
        cx.set_global(crate::drawings_store::Drawings::ephemeral(
            seed.into_iter().collect(),
        ));
        cx.set_global(ws);
        workspace::set_meta(&doc, cx);
        if let Err(e) = capture(cx, window, out, size, scale, script) {
            fail(&e);
        }
    });
    // Only reached if the platform returned without the capture finishing.
    eprintln!("screenshot failed: the app exited before a frame was captured");
    Ok(1)
}

fn fail(e: &anyhow::Error) -> ! {
    eprintln!("screenshot failed: {e:#}");
    exit(1)
}

fn exit(code: i32) -> ! {
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    std::process::exit(code)
}

fn capture(
    cx: &mut App,
    window: WindowDoc,
    out: PathBuf,
    (w, h): (f32, f32),
    scale: Option<f32>,
    script: Option<(Vec<crate::trading::Step>, lines_core::UtcMillis)>,
) -> Result<()> {
    let handle = workspace::open_doc_window(
        &window,
        Some(workspace::hidden_window("Lines (screenshot)", (w, h))),
        cx,
    )?;
    // The panes now follow their feeds (the core has the book): run the script and wait for
    // its events to reach the panes.
    let t = cx.global::<Trading>();
    if let Some((steps, at)) = &script {
        t.run_script(steps, *at, PaperConfig::default().latency_ms);
    } else {
        t.settle();
    }
    cx.spawn(async move |cx| {
        let mut wrote = vec![shoot(cx, handle.into(), (w, h), &out, scale).await];
        // Popped-out panes: their windows opened (hidden) with the main window's tabs.
        // At the size each was opened at (a hidden window reports no size of its own).
        let panes: Vec<(AnyWindowHandle, (f32, f32))> = cx.update(|cx| {
            pane_window::all(cx)
                .into_iter()
                .map(|(hw, size)| (hw.into(), size))
                .collect()
        });
        let stem = out
            .file_stem()
            .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
        for (n, (hw, size)) in panes.into_iter().enumerate() {
            let path = out.with_file_name(format!("{stem}-pane-{}.png", n + 1));
            wrote.push(shoot(cx, hw, size, &path, scale).await);
        }
        for r in wrote {
            match r {
                Ok((path, pw, ph)) => println!("wrote {} ({pw}x{ph} px)", path.display()),
                Err(e) => fail(&e),
            }
        }
        exit(0)
    })
    .detach();
    Ok(())
}

/// Size, warm up, draw and read back one hidden window, then write it to `out`.
async fn shoot(
    cx: &mut AsyncApp,
    handle: AnyWindowHandle,
    (w, h): (f32, f32),
    out: &Path,
    scale: Option<f32>,
) -> Result<(PathBuf, u32, u32)> {
    // A hidden window's DirectX render target is only sized on WM_SIZE, which a window
    // created at its final size never receives. Nudge the size and set it back.
    for (dw, dh) in [(w + 1.0, h + 1.0), (w, h)] {
        let _ = cx.update_window(handle, |_, window, _| window.resize(size(px(dw), px(dh))));
        cx.background_executor()
            .timer(Duration::from_millis(100))
            .await;
    }
    // Indicators are computed on the indicator thread: wait until it has processed the whole
    // replay and the panes' requests, then give the panes a poll tick to pick the values up,
    // so the frame is deterministic.
    lines_feed::indicators::engine().sync();
    // External levels: every source's first answer (live) or every batch due by the replay
    // clock (replay) is applied before the frame is read back.
    if !lines_feed::levels::runtime::wait_settled(Duration::from_secs(20)) {
        bail!("level sources did not settle within 20 s");
    }
    cx.background_executor()
        .timer(Duration::from_millis(100))
        .await;
    // Warm-up frame in its own update, so what it emits (crosshair sync between charts) is
    // delivered before the frame that is read back. It may also request indicator nodes that
    // only paint asks for (VWAP-relative levels follow the chart's ETH VWAP): sync the engine
    // again and give the panes a poll tick before the frames that are read back.
    let _ = cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx));
    lines_feed::indicators::engine().sync();
    cx.background_executor()
        .timer(Duration::from_millis(100))
        .await;
    cx.update_window(handle, |_, window, cx| -> Result<(PathBuf, u32, u32)> {
        let actual = window.scale_factor();
        if let Some(want) = scale
            && (actual - want).abs() > 1e-3
        {
            bail!(
                "display scale is {actual}, --scale requested {want}; GPUI cannot override DPI on this \
                 platform, so run on a display at that scale (or omit --scale)"
            );
        }
        // Two more frames: the first warms the glyph atlas and label cache; the second is
        // what we read back. All draw the identical, already-final data.
        window.draw(cx).clear(cx);
        window.draw(cx).clear(cx);
        let img = window.render_to_image()?;
        let want = ((w * actual).round() as u32, (h * actual).round() as u32);
        if (img.width(), img.height()) != want {
            bail!(
                "rendered {}x{} px, expected {}x{} (render target not sized); refusing to write a bad screenshot",
                img.width(),
                img.height(),
                want.0,
                want.1
            );
        }
        if let Some(dir) = out.parent()
            && !dir.as_os_str().is_empty()
        {
            std::fs::create_dir_all(dir)?;
        }
        img.save(out)
            .map_err(|e| anyhow!("write {}: {e}", out.display()))?;
        Ok((out.to_path_buf(), img.width(), img.height()))
    })?
}
