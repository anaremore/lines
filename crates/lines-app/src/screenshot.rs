//! Reproducible screenshot mode (decision 0009).
//!
//! Inputs fully determine the output on a given machine: replay file + `--at` cut-off
//! (replayed at full speed, so wall-clock pacing doesn't matter), window size, theme, and a
//! frozen clock (the header shows the replay's market time instead of live rates). The frame
//! is produced with `Window::draw` and read back from the GPU with GPUI's
//! `Window::render_to_image` (DirectX staging-texture copy on Windows), so the window is
//! never shown.
//!
//! `--cursor X,Y` places a fixed pointer (logical px from the plot's top-left corner) so the
//! crosshair, its axis tags and the OHLCV readout appear in the image. No OS pointer is
//! involved: the position is handed straight to the view.
//!
//! DPI scale cannot be forced: GPUI on Windows takes it from the monitor
//! (`GetDpiForWindow`). `--scale` therefore *asserts* the scale and fails on mismatch.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use gpui::{
    App, AppContext, Bounds, TitlebarOptions, WindowBounds, WindowOptions, point, px, size,
};
use lines_feed::FeedConfig;
use lines_feed::store::FeedStatus;

use crate::chart_view::{ChartView, Fixture};
use crate::theme::Theme;

/// Everything besides the data that determines the image.
pub struct Shot {
    pub out: PathBuf,
    /// Window size in logical px.
    pub size: (f32, f32),
    /// Required DPI scale (asserted, not forced).
    pub scale: Option<f32>,
    pub theme: Theme,
    /// Fixed pointer position and other stand-ins for input.
    pub fixture: Fixture,
}

pub fn run(config: FeedConfig, shot: Shot) -> Result<i32> {
    let feed = lines_feed::spawn(config)?;

    // 1. Replay to the cut-off before any UI exists: the frame depends only on the data.
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        match feed.store.read().status.clone() {
            FeedStatus::ReplayFinished => break,
            FeedStatus::Failed(e) => bail!("replay failed: {e}"),
            _ if Instant::now() > deadline => bail!("replay did not finish within 120 s"),
            _ => {}
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    // 2. Render one frame off-screen and read it back.
    let code = Arc::new(AtomicI32::new(1));
    let exit = code.clone();
    gpui_platform::application().run(move |cx: &mut App| {
        let result = capture(cx, feed, shot, exit.clone());
        if let Err(e) = result {
            eprintln!("screenshot failed: {e:#}");
            cx.quit();
        }
    });
    Ok(code.load(Ordering::SeqCst))
}

fn capture(
    cx: &mut App,
    feed: lines_feed::FeedHandle,
    shot: Shot,
    exit: Arc<AtomicI32>,
) -> Result<()> {
    let Shot {
        out,
        size: (w, h),
        scale,
        theme,
        fixture,
    } = shot;
    let bounds = Bounds {
        origin: point(px(0.0), px(0.0)),
        size: size(px(w), px(h)),
    };
    let handle = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some("Lines (screenshot)".into()),
                ..Default::default()
            }),
            show: false,
            focus: false,
            ..Default::default()
        },
        move |_, cx| cx.new(|cx| ChartView::new(feed, theme, true, fixture, cx)),
    )?;
    cx.spawn(async move |cx| {
        // A hidden window's DirectX render target is only sized on WM_SIZE, which a window
        // created at its final size never receives. Nudge the size and set it back.
        for (dw, dh) in [(w + 1.0, h + 1.0), (w, h)] {
            let _ = cx.update_window(handle.into(), |_, window, _| window.resize(size(px(dw), px(dh))));
            cx.background_executor().timer(Duration::from_millis(100)).await;
        }
        let result = cx.update_window(handle.into(), |_, window, cx| -> Result<(u32, u32)> {
            let actual = window.scale_factor();
            if let Some(want) = scale
                && (actual - want).abs() > 1e-3
            {
                bail!(
                    "display scale is {actual}, --scale requested {want}; GPUI cannot override DPI on this \
                     platform, so run on a display at that scale (or omit --scale)"
                );
            }
            // Two frames: the first warms the glyph atlas and label cache; the second is
            // what we read back. Both draw the identical, already-final data.
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
            img.save(&out).map_err(|e| anyhow!("write {}: {e}", out.display()))?;
            Ok((img.width(), img.height()))
        });
        match result.and_then(|r| r) {
            Ok((pw, ph)) => {
                println!("wrote {} ({pw}x{ph} px)", out.display());
                exit.store(0, Ordering::SeqCst);
            }
            Err(e) => eprintln!("screenshot failed: {e:#}"),
        }
        cx.update(|cx| cx.quit());
    })
    .detach();
    Ok(())
}
