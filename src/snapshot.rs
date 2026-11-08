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
    for a in &args[1..] {
        if let Some(v) = a.strip_prefix("select=") {
            select = v.parse::<usize>().ok();
        } else if let Some(v) = a.strip_prefix("zoom=") {
            zoom = v.parse::<usize>().ok();
        } else if let Some(v) = a.strip_prefix("hover=") {
            let (x, y) = v.split_once(',').unwrap();
            hover = Some((x.parse::<f32>().unwrap(), y.parse::<f32>().unwrap()));
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
            if let Some((x, y)) = hover {
                this.debug_hover(point(px(x), px(y)), window, cx);
            }
        });
    })
    .unwrap();
    cx.run_until_parked();
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
    let img = cx.capture_screenshot(window.into()).expect("screenshot");
    img.save(&out).expect("save png");
    eprintln!("wrote {}", out.display());
}
