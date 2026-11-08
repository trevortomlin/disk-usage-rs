mod app;
mod assets;
mod bench;
mod format;
mod mounts;
mod scan;
#[cfg(feature = "snapshot")]
mod snapshot;
mod theme;
mod treemap;

use std::path::PathBuf;

use gpui::{
    App, Bounds, KeyBinding, TitlebarOptions, WindowBounds, WindowOptions, prelude::*, px, size,
};

use app::*;

pub fn bind_keys(cx: &mut App) {
    let ctx = Some("DiskUsage");
    cx.bind_keys([
        KeyBinding::new("down", SelectNext, ctx),
        KeyBinding::new("j", SelectNext, ctx),
        KeyBinding::new("up", SelectPrev, ctx),
        KeyBinding::new("k", SelectPrev, ctx),
        KeyBinding::new("home", SelectFirst, ctx),
        KeyBinding::new("end", SelectLast, ctx),
        KeyBinding::new("enter", Confirm, ctx),
        KeyBinding::new("right", OpenSelected, ctx),
        KeyBinding::new("l", OpenSelected, ctx),
        KeyBinding::new("backspace", GoUp, ctx),
        KeyBinding::new("left", GoUp, ctx),
        KeyBinding::new("h", GoUp, ctx),
        KeyBinding::new("alt-up", GoUp, ctx),
        KeyBinding::new("alt-left", GoBack, ctx),
        KeyBinding::new("alt-right", GoForward, ctx),
        KeyBinding::new("ctrl-r", Rescan, ctx),
        KeyBinding::new("f5", Rescan, ctx),
        KeyBinding::new("ctrl-o", OpenFolder, ctx),
        KeyBinding::new("ctrl-m", ToggleSizeMode, ctx),
        KeyBinding::new("escape", Dismiss, ctx),
        KeyBinding::new("ctrl-c", CopyPath, ctx),
        KeyBinding::new("ctrl-e", RevealSelected, ctx),
        KeyBinding::new("ctrl-n", StartOver, ctx),
        KeyBinding::new("delete", TrashSelected, ctx),
        KeyBinding::new("shift-delete", DeleteSelected, ctx),
        KeyBinding::new("ctrl-q", Quit, None),
        KeyBinding::new("cmd-q", Quit, None),
    ]);
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("--bench") {
        bench::run();
        return;
    }
    #[cfg(feature = "snapshot")]
    if std::env::args().nth(1).as_deref() == Some("--snapshot") {
        snapshot::run();
        return;
    }

    let initial = std::env::args_os().nth(1).map(PathBuf::from);

    gpui_platform::application()
        .with_assets(assets::Assets)
        .run(move |cx: &mut App| {
            bind_keys(cx);
            cx.on_action(|_: &Quit, cx| cx.quit());

            let bounds = Bounds::centered(None, size(px(1320.0), px(840.0)), cx);
            let initial = initial.clone();
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some("Disk Usage".into()),
                        ..Default::default()
                    }),
                    app_id: Some("disk-usage".into()),
                    window_min_size: Some(size(px(760.0), px(480.0))),
                    ..Default::default()
                },
                |window, cx| cx.new(|cx| DiskUsage::new(initial, window, cx)),
            )
            .expect("failed to open window");
            cx.on_window_closed(|cx, _| cx.quit()).detach();
            cx.activate(true);
        });
}
