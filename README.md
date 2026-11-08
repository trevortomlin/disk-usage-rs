# Disk Usage

A fast disk usage analyzer for Linux, written in Rust on
[GPUI Fast](https://github.com/trevortomlin/gpui-fast).

![Disk Usage showing /usr](docs/screenshot.png)

## Features

- **Fast parallel scanning.** Directories are walked on a work-stealing thread pool.
  Hard links are counted once, and by default the scan doesn't cross into other
  filesystems. It reads about half a million files in `/usr` in roughly 1.5 s.
- **Nested treemap.** A squarified treemap drawn straight to the GPU. Each top-level
  folder gets its own color, and chains of folders that hold only one folder are
  folded into a single header (`src / Windows / Win32`). Tiny items are merged into a
  "N more" tile, so drawing stays fast even for huge trees.
- **The list and the treemap stay in sync.** Hover a row to spotlight that item in the
  map. Hover a tile to highlight its row and see its size in a floating card.
- **Easy to navigate.** Click a tile to zoom in. Use the breadcrumbs, back/forward
  (mouse side buttons work too), or Backspace to go up.
- **On disk / apparent size.** Switch between allocated blocks (`du`) and logical
  file length (`ls -l`).
- **Start screen.** Lists every mounted filesystem with a usage bar. You can also drop
  a folder onto the window or pass a path on the command line.
- **Context menu.** Zoom in, open, show in the file manager, or copy the path.

## Building

```sh
cargo build --release
./target/release/disk-usage            # start screen
./target/release/disk-usage ~/Projects  # scan right away
```

Requires a recent stable Rust toolchain and the usual GPUI Linux dependencies
(Vulkan or OpenGL, Wayland and/or X11, fontconfig, xkbcommon).

## Keyboard

| Key | Action |
| --- | --- |
| `↑` `↓` / `j` `k` | Move selection |
| `Home` / `End` | First / last item |
| `Enter` / `→` / `l` | Open selected folder |
| `Backspace` / `←` / `h` | Go up |
| `Alt+←` / `Alt+→` | Back / forward |
| `Ctrl+R` / `F5` | Rescan |
| `Ctrl+O` | Choose a folder |
| `Ctrl+M` | Toggle on-disk / apparent size |
| `Ctrl+C` | Copy path of selection |
| `Ctrl+E` | Show selection in file manager |
| `Ctrl+N` | Back to the start screen |
| `Esc` | Close menu / cancel scan / clear selection |
| `Ctrl+Q` | Quit |

## Layout of the code

| File | What it does |
| --- | --- |
| `src/scan.rs` | Parallel scanner. Flattens the result into an arena where each node's children sit next to each other, sorted by size. |
| `src/treemap.rs` | Nested squarified treemap layout. |
| `src/app.rs` | The window: start screen, scan progress, list, and treemap painting. |
| `src/mounts.rs` | Mounted filesystems and their capacity, via `statvfs`. |
| `src/theme.rs` | Colors. |

The treemap layout is cached and recomputed only when the folder, size mode or
window size changes. Moving the pointer re-renders only the small hover-card view,
so GPUI Fast's retained mode can reuse everything else.

### Offscreen snapshots

For checking the UI without opening a window, there is a headless renderer:

```sh
cargo run --release --features snapshot -- --snapshot out.png /usr hover=900,400 select=1
```

## License

Apache-2.0
