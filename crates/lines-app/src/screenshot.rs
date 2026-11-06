//! Reproducible screenshot mode (decision 0009).
//!
//! Inputs fully determine the output on a given machine: replay file + `--at` cut-off
//! (replayed at full speed, so wall-clock pacing doesn't matter), window size, theme, and a
//! frozen clock (the header shows the replay's market time instead of live rates). The frame
//! is produced with `Window::draw` and read back from the GPU with GPUI's
//! `Window::render_to_image` (DirectX staging-texture copy on Windows), so the window is
//! never shown.
//!
//! The window holds the same workspace as the app (`--layout`): chart, DOM and time & sales
//! by default. `--dom-grouping` and `--large-print` pin the DOM and tape options.
//!
//! `--cursor X,Y` places a fixed pointer (logical px from the plot's top-left corner) so the
//! crosshair, its axis tags and the OHLCV readout appear in the image. No OS pointer is
//! involved: the position is handed straight to the view.
//!
//! DPI scale cannot be forced: GPUI on Windows takes it from the monitor
//! (`GetDpiForWindow`). `--scale` therefore *asserts* the scale and fails on mismatch.

use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use gpui::{
    App, AppContext, Bounds, TitlebarOptions, WindowBounds, WindowOptions, point, px, size,
};
use lines_feed::registry::FeedRegistry;
use lines_feed::store::FeedStatus;

use crate::feeds::Feeds;
use crate::layout::Layout;
use crate::workspace::{PaneSetup, WorkspaceView};

/// Everything besides the data that determines the image.
pub struct Shot {
    pub out: PathBuf,
    /// Window size in logical px.
    pub size: (f32, f32),
    /// Required DPI scale (asserted, not forced).
    pub scale: Option<f32>,
    pub layout: Layout,
    /// The replay feed, theme, fixed pointer and pane options.
    pub setup: PaneSetup,
}

pub fn run(registry: FeedRegistry, shot: Shot) -> Result<i32> {
    let store = shot.setup.feed.sub.store().clone();

    // 1. Replay to the cut-off before any UI exists: the frame depends only on the data.
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        match store.read().status.clone() {
            FeedStatus::ReplayFinished => break,
            FeedStatus::Failed(e) => bail!("replay failed: {e}"),
            _ if Instant::now() > deadline => bail!("replay did not finish within 120 s"),
            _ => {}
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    // 2. Render one frame off-screen and read it back. Every outcome ends the process from
    // inside the app with its own exit code: quitting the app is not enough, because on
    // macOS it terminates the process with status 0 before `run` returns.
    gpui_platform::application().run(move |cx: &mut App| {
        cx.set_global(Feeds(registry));
        if let Err(e) = capture(cx, shot) {
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

fn capture(cx: &mut App, shot: Shot) -> Result<()> {
    let Shot {
        out,
        size: (w, h),
        scale,
        layout,
        setup,
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
        move |_, cx| cx.new(|cx| WorkspaceView::new(layout, setup, cx)),
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
                exit(0)
            }
            Err(e) => fail(&e),
        }
    })
    .detach();
    Ok(())
}
