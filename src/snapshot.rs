//! Developer tool: render the UI offscreen to PNG without opening a window.
//!
//!   cargo run --release --features snapshot -- --snapshot OUT.png [PATH] [select=N] [hover=X,Y] [zoom=N]

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{AppContext as _, HeadlessAppContext, point, px, size};

use crate::app::DiskUsage;

pub fn run() {
    let args: Vec<String> = std::env::args().skip(2).collect();
    let out = PathBuf::from(&args[0]);
    let mut path = None;
    let mut select = None;
    let mut hover = None;
    let mut zoom = None;
    let mut dims = (1320.0, 840.0);
    let mut confirm = None;
    let mut perform = None;
    for a in &args[1..] {
        if let Some(v) = a.strip_prefix("select=") {
            select = v.parse::<usize>().ok();
        } else if let Some(v) = a.strip_prefix("zoom=") {
            zoom = v.parse::<usize>().ok();
        } else if let Some(v) = a.strip_prefix("hover=") {
            let (x, y) = v.split_once(',').unwrap();
            hover = Some((x.parse::<f32>().unwrap(), y.parse::<f32>().unwrap()));
        } else if let Some(v) = a.strip_prefix("do=") {
            perform = Some(v == "delete");
        } else if let Some(v) = a.strip_prefix("confirm=") {
            confirm = Some(v == "delete");
        } else if let Some(v) = a.strip_prefix("size=") {
            let (w, h) = v.split_once('x').unwrap();
            dims = (w.parse().unwrap(), h.parse().unwrap());
        } else {
            path = Some(PathBuf::from(a));
        }
    }

    let text = Arc::new(gpui_wgpu::CosmicTextSystem::new("Noto Sans"));
    let mut cx = HeadlessAppContext::with_platform(text, Arc::new(crate::assets::Assets), || {
        gpui_platform::current_headless_renderer()
    });
    cx.update(crate::bind_keys);
    let window = cx
        .open_window(size(px(dims.0), px(dims.1)), |window, cx| {
            cx.new(|cx| DiskUsage::new(path, window, cx))
        })
        .unwrap();
    cx.run_until_parked();
    // Let the scan thread pool finish if it is still running.
    for _ in 0..200 {
        let busy = cx
            .update_window(window.into(), |root, _, cx| {
                root.downcast::<DiskUsage>().unwrap().read(cx).is_scanning()
            })
            .unwrap();
        if !busy {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
        cx.run_until_parked();
    }
    // Draw once so the treemap layout cache exists before simulating input.
    let _ = cx.capture_screenshot(window.into());
    cx.update_window(window.into(), |root, window, cx| {
        let view = root.downcast::<DiskUsage>().unwrap();
        view.update(cx, |this, cx| {
            if let Some(z) = zoom {
                this.debug_zoom(z, cx);
            }
            if let Some(s) = select {
                this.debug_select(s, cx);
            }
            if let Some(permanent) = confirm.or(perform) {
                this.debug_confirm(permanent, cx);
            }
            if perform.is_some() {
                this.debug_accept(cx);
            }
            if let Some((x, y)) = hover {
                this.debug_hover(point(px(x), px(y)), window, cx);
            }
        });
    })
    .unwrap();
    cx.run_until_parked();
    // Removal runs on a background thread; give it a moment.
    for _ in 0..20 {
        std::thread::sleep(std::time::Duration::from_millis(50));
        cx.run_until_parked();
    }
    let _ = cx.capture_screenshot(window.into());
    cx.update_window(window.into(), |root, window, cx| {
        let view = root.downcast::<DiskUsage>().unwrap();
        view.update(cx, |this, cx| {
            if let Some((x, y)) = hover {
                this.debug_hover(point(px(x), px(y)), window, cx);
            }
        });
    })
    .unwrap();
    cx.run_until_parked();
    if let Some(n) = std::env::var("DU_FRAMES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
    {
        frame_bench(&mut cx, window.into(), n, dims);
    }
    let img = cx.capture_screenshot(window.into()).expect("screenshot");
    img.save(&out).expect("save png");
    eprintln!("wrote {}", out.display());
}

fn thread_cpu() -> f64 {
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    ts.tv_sec as f64 + ts.tv_nsec as f64 / 1e9
}

/// One frame's change to the view.
type FrameStep<'a> =
    dyn Fn(usize, &mut DiskUsage, &mut gpui::Window, &mut gpui::Context<DiskUsage>) + 'a;

/// Main-thread CPU per frame: (a) the pointer moving over the treemap,
/// (b) moving over list rows, (c) redrawing with nothing changed.
fn frame_bench(
    cx: &mut HeadlessAppContext,
    window: gpui::AnyWindowHandle,
    n: usize,
    dims: (f32, f32),
) {
    let run = |label: &str, cx: &mut HeadlessAppContext, f: &FrameStep<'_>| {
        let mut total = 0.0;
        let mut worst: f64 = 0.0;
        for i in 0..n {
            let t = thread_cpu();
            cx.update_window(window, |root, window, cx| {
                let view = root.downcast::<DiskUsage>().unwrap();
                view.update(cx, |this, cx| f(i, this, window, cx));
            })
            .unwrap();
            cx.run_until_parked();
            let _ = cx.capture_screenshot(window);
            let dt = thread_cpu() - t;
            total += dt;
            worst = worst.max(dt);
        }
        eprintln!(
            "frames[{label}] avg_cpu_ms={:.3} worst_ms={:.3}",
            total / n as f64 * 1e3,
            worst * 1e3
        );
    };
    let (w, h) = dims;
    run("treemap-hover", cx, &|i, this, window, cx| {
        // A diagonal sweep at 4 px per frame, like a real pointer.
        let x = 480.0 + (i as f32 * 4.0) % (w - 500.0);
        let y = 90.0 + (i as f32 * 3.0) % (h - 180.0);
        this.debug_hover(point(px(x), px(y)), window, cx);
    });
    run("list-select", cx, &|i, this, _, cx| {
        this.debug_select(i % 8, cx)
    });
    run("idle", cx, &|_, _, _, _| {});
}
