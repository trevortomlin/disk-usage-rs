//! The main window: start screen, scan progress, and the browser
//! (breadcrumbs, a sortable list and a nested treemap).

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, Bounds, ClickEvent, ClipboardItem, ContentMask, Context, Corners, ElementId,
    Entity, ExternalPaths, FocusHandle, Focusable, FontWeight, Hsla, MouseButton, MouseDownEvent,
    MouseMoveEvent, NavigationDirection, PathPromptOptions, Pixels, Point, Render, ScrollStrategy,
    SharedString, Task, TextAlign, TextRun, UniformListScrollHandle, Window, actions, anchored,
    canvas, deferred, div, fill, hsla, linear_color_stop, linear_gradient, point, prelude::*, px,
    quad, size, svg, transparent_black, uniform_list,
};

use crate::assets::icon;
use crate::format;
use crate::mounts::{self, Mount};
use crate::scan::{self, Kind, NodeId, Progress, ROOT, ScanOptions, SizeMode, Tree};
use crate::theme;
use crate::treemap::{self, HEADER_H, Rect, Tile, TileKind};

actions!(
    disk_usage,
    [
        SelectNext,
        SelectPrev,
        SelectFirst,
        SelectLast,
        OpenSelected,
        GoUp,
        GoBack,
        GoForward,
        Rescan,
        OpenFolder,
        ToggleSizeMode,
        Dismiss,
        CopyPath,
        RevealSelected,
        StartOver,
        Quit,
    ]
);

#[derive(Clone, Copy, PartialEq, Eq)]
enum SortKey {
    Size,
    Name,
    Items,
}

struct Scanning {
    path: PathBuf,
    progress: Arc<Progress>,
    started: Instant,
    _tasks: (Task<()>, Task<()>),
}

/// Treemap layout cached across frames; recomputed only when its inputs change.
#[derive(Default)]
struct MapCache {
    key: Option<(NodeId, SizeMode, u64, i32, i32)>,
    bounds: Bounds<Pixels>,
    tiles: Vec<Tile>,
}

#[derive(Clone, Copy)]
struct Row {
    node: NodeId,
    /// Position in size order; picks the same color the treemap uses.
    color: u32,
}

/// The floating label that follows the pointer over the treemap. It's its own
/// view so pointer motion re-renders only this, not the whole window.
pub struct HoverCard {
    pos: Point<Pixels>,
    area: Bounds<Pixels>,
    info: Option<(SharedString, SharedString, SharedString, Hsla)>,
}

impl Render for HoverCard {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let Some((name, size, detail, color)) = self.info.clone() else {
            return div().into_any_element();
        };
        let w = px(240.0);
        let h = px(58.0);
        let mut x = self.pos.x - self.area.origin.x + px(16.0);
        let mut y = self.pos.y - self.area.origin.y + px(18.0);
        if x + w > self.area.size.width {
            x = (self.pos.x - self.area.origin.x - w - px(12.0)).max(px(4.0));
        }
        if y + h > self.area.size.height {
            y = (self.pos.y - self.area.origin.y - h - px(12.0)).max(px(4.0));
        }
        div()
            .absolute()
            .left(x)
            .top(y)
            .max_w(w)
            .px_3()
            .py_2()
            .rounded_lg()
            .bg(hsla(225.0 / 360.0, 0.2, 0.08, 0.94))
            .border_1()
            .border_color(theme::border_strong())
            .shadow_lg()
            .flex()
            .flex_col()
            .gap_0p5()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().size(px(8.0)).rounded_full().bg(color).flex_none())
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme::text())
                            .truncate()
                            .child(name),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .text_xs()
                    .child(div().text_color(theme::text()).child(size))
                    .child(div().text_color(theme::muted()).truncate().child(detail)),
            )
            .into_any_element()
    }
}

pub struct DiskUsage {
    focus: FocusHandle,
    mounts: Vec<Mount>,
    home: Option<PathBuf>,
    scanning: Option<Scanning>,
    error: Option<SharedString>,
    tree: Option<Arc<Tree>>,
    generation: u64,
    current: NodeId,
    back: Vec<NodeId>,
    forward: Vec<NodeId>,
    selected: Option<NodeId>,
    hovered_tile: Option<NodeId>,
    hovered_row: Option<NodeId>,
    mode: SizeMode,
    sort: SortKey,
    rows: Rc<[Row]>,
    list_scroll: UniformListScrollHandle,
    map: Rc<RefCell<MapCache>>,
    hover_card: Entity<HoverCard>,
    menu: Option<(Point<Pixels>, NodeId)>,
    one_fs: bool,
    capacity: Option<(u64, u64)>,
}

impl Focusable for DiskUsage {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl DiskUsage {
    pub fn new(initial: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let hover_card = cx.new(|_| HoverCard {
            pos: Point::default(),
            area: Bounds::default(),
            info: None,
        });
        let mut this = Self {
            focus,
            mounts: mounts::list(),
            home: std::env::var_os("HOME").map(PathBuf::from),
            scanning: None,
            error: None,
            tree: None,
            generation: 0,
            current: ROOT,
            back: Vec::new(),
            forward: Vec::new(),
            selected: None,
            hovered_tile: None,
            hovered_row: None,
            mode: SizeMode::Disk,
            sort: SortKey::Size,
            rows: Rc::from(Vec::new()),
            list_scroll: UniformListScrollHandle::new(),
            map: Rc::default(),
            hover_card,
            menu: None,
            one_fs: true,
            capacity: None,
        };
        if let Some(path) = initial {
            this.start_scan(path, cx);
        }
        this
    }

    // ── Scanning ────────────────────────────────────────────────────────

    fn start_scan(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if let Some(s) = self.scanning.take() {
            s.progress.cancel.store(true, Ordering::Relaxed);
        }
        self.error = None;
        self.menu = None;
        let progress = Arc::new(Progress::default());
        let options = ScanOptions {
            one_file_system: self.one_fs,
        };
        let job = {
            let progress = progress.clone();
            let path = path.clone();
            cx.background_executor()
                .spawn(async move { scan::scan(&path, options, &progress) })
        };
        let finisher = cx.spawn({
            let progress = progress.clone();
            async move |this, cx| {
                let result = job.await;
                this.update(cx, |this, cx| {
                    let ours = this
                        .scanning
                        .as_ref()
                        .is_some_and(|s| Arc::ptr_eq(&s.progress, &progress));
                    if ours {
                        this.finish_scan(result, cx);
                    }
                })
                .ok();
            }
        });
        let ticker = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(66))
                    .await;
                let alive = this
                    .update(cx, |this, cx| {
                        cx.notify();
                        this.scanning.is_some()
                    })
                    .unwrap_or(false);
                if !alive {
                    break;
                }
            }
        });
        self.scanning = Some(Scanning {
            path,
            progress,
            started: Instant::now(),
            _tasks: (finisher, ticker),
        });
        cx.notify();
    }

    fn finish_scan(&mut self, result: anyhow::Result<Tree>, cx: &mut Context<Self>) {
        let scanning = self.scanning.take();
        match result {
            Ok(tree) => {
                // Keep the user's place when rescanning the same location.
                let previous = self.tree.as_ref().and_then(|old| {
                    (old.root_path == tree.root_path).then(|| old.path_of(self.current))
                });
                self.capacity = mounts::capacity(&tree.root_path);
                self.tree = Some(Arc::new(tree));
                self.generation += 1;
                self.back.clear();
                self.forward.clear();
                self.current = previous.and_then(|p| self.find_path(&p)).unwrap_or(ROOT);
                self.selected = None;
                self.hovered_tile = None;
                self.hovered_row = None;
                self.rebuild_rows();
            }
            Err(err) => {
                let cancelled = scanning.is_some_and(|s| s.progress.cancelled());
                if !cancelled {
                    self.error = Some(format!("{err:#}").into());
                }
            }
        }
        cx.notify();
    }

    fn cancel_scan(&mut self, cx: &mut Context<Self>) {
        if let Some(s) = self.scanning.take() {
            s.progress.cancel.store(true, Ordering::Relaxed);
        }
        cx.notify();
    }

    fn find_path(&self, path: &Path) -> Option<NodeId> {
        let tree = self.tree.as_ref()?;
        let rel = path.strip_prefix(&tree.root_path).ok()?;
        let mut cur = ROOT;
        for part in rel.components() {
            let name = part.as_os_str().to_string_lossy();
            cur = tree
                .node(cur)
                .children()
                .find(|&c| *tree.node(c).name == *name)?;
        }
        Some(cur)
    }

    fn choose_folder(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Analyze".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await
                && let Some(path) = paths.into_iter().next()
            {
                this.update(cx, |this, cx| this.start_scan(path, cx)).ok();
            }
        })
        .detach();
    }

    // ── Navigation ──────────────────────────────────────────────────────

    fn rebuild_rows(&mut self) {
        let Some(tree) = &self.tree else {
            self.rows = Rc::from(Vec::new());
            return;
        };
        let by_size = tree.sorted_children(self.current, self.mode);
        let mut rows: Vec<Row> = by_size
            .iter()
            .enumerate()
            .map(|(i, &node)| Row {
                node,
                color: i as u32,
            })
            .collect();
        match self.sort {
            SortKey::Size => {}
            SortKey::Name => rows.sort_by_cached_key(|r| tree.node(r.node).name.to_lowercase()),
            SortKey::Items => rows.sort_by_key(|r| std::cmp::Reverse(tree.node(r.node).items)),
        }
        self.rows = rows.into();
    }

    fn navigate(&mut self, node: NodeId, cx: &mut Context<Self>) {
        let Some(tree) = &self.tree else { return };
        let node = if tree.node(node).is_dir() {
            node
        } else {
            tree.node(node).parent
        };
        if node == self.current {
            return;
        }
        self.back.push(self.current);
        self.forward.clear();
        self.set_current(node, None, cx);
    }

    fn set_current(&mut self, node: NodeId, select: Option<NodeId>, cx: &mut Context<Self>) {
        self.current = node;
        self.selected = select;
        self.hovered_row = None;
        self.hovered_tile = None;
        self.menu = None;
        self.rebuild_rows();
        let ix = select.and_then(|s| self.rows.iter().position(|r| r.node == s));
        self.list_scroll
            .scroll_to_item(ix.unwrap_or(0), ScrollStrategy::Center);
        self.hover_card.update(cx, |c, cx| {
            c.info = None;
            cx.notify();
        });
        cx.notify();
    }

    fn go_up(&mut self, _: &GoUp, _: &mut Window, cx: &mut Context<Self>) {
        let Some(tree) = &self.tree else { return };
        if self.current == ROOT {
            return;
        }
        let from = self.current;
        let parent = tree.node(from).parent;
        self.back.push(self.current);
        self.forward.clear();
        self.set_current(parent, Some(from), cx);
    }

    fn go_back(&mut self, _: &GoBack, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(prev) = self.back.pop() {
            self.forward.push(self.current);
            let from = self.current;
            let select = self
                .tree
                .as_ref()
                .is_some_and(|t| t.node(from).parent == prev)
                .then_some(from);
            self.set_current(prev, select, cx);
        }
    }

    fn go_forward(&mut self, _: &GoForward, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(next) = self.forward.pop() {
            self.back.push(self.current);
            self.set_current(next, None, cx);
        }
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.rows.is_empty() {
            return;
        }
        let cur = self
            .selected
            .and_then(|s| self.rows.iter().position(|r| r.node == s));
        let last = self.rows.len() as isize - 1;
        let next = match cur {
            None if delta >= 0 => 0,
            None => last,
            Some(i) => (i as isize + delta).clamp(0, last),
        } as usize;
        self.selected = Some(self.rows[next].node);
        self.list_scroll
            .scroll_to_item(next, ScrollStrategy::Nearest);
        cx.notify();
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(1, cx);
    }
    fn select_prev(&mut self, _: &SelectPrev, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(-1, cx);
    }
    fn select_first(&mut self, _: &SelectFirst, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(isize::MIN / 2, cx);
    }
    fn select_last(&mut self, _: &SelectLast, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(isize::MAX / 2, cx);
    }

    fn open_selected(&mut self, _: &OpenSelected, _: &mut Window, cx: &mut Context<Self>) {
        let (Some(sel), Some(tree)) = (self.selected, &self.tree) else {
            return;
        };
        if tree.node(sel).is_dir() && tree.node(sel).child_count > 0 {
            self.navigate(sel, cx);
        }
    }

    fn rescan(&mut self, _: &Rescan, _: &mut Window, cx: &mut Context<Self>) {
        let path = self
            .tree
            .as_ref()
            .map(|t| t.root_path.clone())
            .or_else(|| self.scanning.as_ref().map(|s| s.path.clone()));
        if let Some(path) = path {
            self.start_scan(path, cx);
        }
    }

    fn open_folder(&mut self, _: &OpenFolder, _: &mut Window, cx: &mut Context<Self>) {
        self.choose_folder(cx);
    }

    fn toggle_mode(&mut self, _: &ToggleSizeMode, _: &mut Window, cx: &mut Context<Self>) {
        self.set_mode(
            match self.mode {
                SizeMode::Disk => SizeMode::Apparent,
                SizeMode::Apparent => SizeMode::Disk,
            },
            cx,
        );
    }

    fn set_mode(&mut self, mode: SizeMode, cx: &mut Context<Self>) {
        if self.mode != mode {
            self.mode = mode;
            self.rebuild_rows();
            cx.notify();
        }
    }

    fn set_sort(&mut self, sort: SortKey, cx: &mut Context<Self>) {
        if self.sort != sort {
            self.sort = sort;
            self.rebuild_rows();
            cx.notify();
        }
    }

    fn dismiss(&mut self, _: &Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        if self.menu.take().is_some() {
            cx.notify();
        } else if self.scanning.is_some() {
            self.cancel_scan(cx);
        } else if self.selected.take().is_some() {
            cx.notify();
        }
    }

    fn start_over(&mut self, _: &StartOver, _: &mut Window, cx: &mut Context<Self>) {
        self.cancel_scan(cx);
        self.tree = None;
        self.error = None;
        self.mounts = mounts::list();
        self.rows = Rc::from(Vec::new());
        cx.notify();
    }

    fn target(&self) -> Option<NodeId> {
        self.selected
            .or(Some(self.current))
            .filter(|_| self.tree.is_some())
    }

    fn copy_path(&mut self, _: &CopyPath, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(node) = self.target() {
            self.copy_node_path(node, cx);
        }
    }

    fn reveal_selected(&mut self, _: &RevealSelected, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(node) = self.target() {
            self.reveal(node, cx);
        }
    }

    fn copy_node_path(&mut self, node: NodeId, cx: &mut Context<Self>) {
        if let Some(tree) = &self.tree {
            let p = tree.path_of(node);
            cx.write_to_clipboard(ClipboardItem::new_string(p.to_string_lossy().into_owned()));
        }
    }

    fn reveal(&mut self, node: NodeId, cx: &mut Context<Self>) {
        if let Some(tree) = &self.tree {
            cx.reveal_path(&tree.path_of(node));
        }
    }

    fn open_with_system(&mut self, node: NodeId, cx: &mut Context<Self>) {
        if let Some(tree) = &self.tree {
            cx.open_with_system(&tree.path_of(node));
        }
    }

    #[cfg(feature = "snapshot")]
    pub fn is_scanning(&self) -> bool {
        self.scanning.is_some()
    }

    #[cfg(feature = "snapshot")]
    pub fn debug_select(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(r) = self.rows.get(ix) {
            self.selected = Some(r.node);
            cx.notify();
        }
    }

    #[cfg(feature = "snapshot")]
    pub fn debug_zoom(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(r) = self.rows.get(ix) {
            let node = r.node;
            self.navigate(node, cx);
        }
    }

    #[cfg(feature = "snapshot")]
    pub fn debug_hover(&mut self, pos: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let e = MouseMoveEvent {
            position: pos,
            pressed_button: None,
            modifiers: Default::default(),
        };
        self.on_map_move(&e, window, cx);
    }

    // ── Treemap interaction ─────────────────────────────────────────────

    fn hit_test(&self, pos: Point<Pixels>) -> Option<Tile> {
        let map = self.map.borrow();
        let local = pos - map.bounds.origin;
        let (x, y) = (f32::from(local.x), f32::from(local.y));
        map.tiles
            .iter()
            .rev()
            .find(|t| t.rect.contains(x, y))
            .copied()
    }

    fn on_map_move(&mut self, e: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        let hit = self.hit_test(e.position);
        let node = hit.map(|t| t.node);
        if node != self.hovered_tile {
            self.hovered_tile = node;
            cx.notify();
        }
        let info = match (hit, &self.tree) {
            (Some(t), Some(tree)) => Some(self.describe_tile(tree, &t)),
            _ => None,
        };
        let area = self.map.borrow().bounds;
        self.hover_card.update(cx, |card, cx| {
            card.pos = e.position;
            card.area = area;
            card.info = info;
            cx.notify();
        });
    }

    fn describe_tile(
        &self,
        tree: &Tree,
        t: &Tile,
    ) -> (SharedString, SharedString, SharedString, Hsla) {
        let n = tree.node(t.node);
        let color = theme::branch(t.branch);
        let current = tree.node(self.current).size(self.mode);
        if let TileKind::Rest { count } = t.kind {
            return (
                format!("{} smaller items", format::count(count as u64)).into(),
                "".into(),
                format!("in {}", n.name).into(),
                color,
            );
        }
        let size = n.size(self.mode);
        let detail = if n.is_dir() {
            format!(
                "{} · {} items",
                format::percent(size, current),
                format::count(n.items)
            )
        } else {
            format!("{} of view", format::percent(size, current))
        };
        (
            n.name.to_string().into(),
            format::bytes(size).into(),
            detail.into(),
            color,
        )
    }

    fn clear_hover(&mut self, cx: &mut Context<Self>) {
        if self.hovered_tile.take().is_some() {
            cx.notify();
        }
        self.hover_card.update(cx, |c, cx| {
            if c.info.take().is_some() {
                cx.notify();
            }
        });
    }

    fn on_map_click(&mut self, e: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(hit) = self.hit_test(e.position()) else {
            return;
        };
        let Some(tree) = self.tree.clone() else {
            return;
        };
        match hit.kind {
            TileKind::Frame => self.navigate(hit.inner, cx),
            TileKind::Dir => self.navigate(hit.node, cx),
            TileKind::Rest { .. } => self.navigate(hit.node, cx),
            TileKind::File => {
                let parent = tree.node(hit.node).parent;
                if parent != self.current {
                    self.back.push(self.current);
                    self.forward.clear();
                    self.set_current(parent, Some(hit.node), cx);
                } else {
                    self.selected = Some(hit.node);
                    if let Some(ix) = self.rows.iter().position(|r| r.node == hit.node) {
                        self.list_scroll.scroll_to_item(ix, ScrollStrategy::Center);
                    }
                    cx.notify();
                }
            }
        }
    }

    fn on_map_right_click(&mut self, e: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(hit) = self.hit_test(e.position) {
            if matches!(hit.kind, TileKind::Rest { .. }) {
                return;
            }
            self.selected = Some(hit.node);
            self.menu = Some((e.position, hit.node));
            cx.notify();
        }
    }
}

// ── Rendering ───────────────────────────────────────────────────────────

struct TooltipText(SharedString);

impl Render for TooltipText {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .rounded_md()
            .bg(theme::raised())
            .border_1()
            .border_color(theme::border_strong())
            .shadow_md()
            .text_xs()
            .text_color(theme::text())
            .child(self.0.clone())
    }
}

fn section_label(label: &'static str) -> impl IntoElement {
    div()
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme::faint())
        .child(label.to_uppercase())
}

fn icon_el(name: &str, color: Hsla, size_px: f32) -> impl IntoElement {
    svg()
        .path(icon(name))
        .size(px(size_px))
        .flex_none()
        .text_color(color)
}

fn icon_button(id: impl Into<ElementId>, name: &str, enabled: bool) -> gpui::Stateful<gpui::Div> {
    let b = div()
        .id(id)
        .size(px(30.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .child(icon_el(
            name,
            if enabled {
                theme::text()
            } else {
                theme::faint()
            },
            16.0,
        ));
    if enabled {
        b.cursor_pointer()
            .hover(|s| s.bg(theme::hover()))
            .active(|s| s.bg(theme::raised()))
    } else {
        b
    }
}

fn button(
    id: impl Into<ElementId>,
    label: &str,
    icon_name: Option<&str>,
    primary: bool,
) -> gpui::Stateful<gpui::Div> {
    let mut b = div()
        .id(id)
        .h(px(30.0))
        .px_3()
        .flex()
        .items_center()
        .gap_2()
        .rounded_md()
        .text_sm()
        .cursor_pointer()
        .border_1();
    if primary {
        b = b
            .bg(theme::accent())
            .border_color(theme::accent())
            .text_color(gpui::rgb(0x0b1020))
            .font_weight(FontWeight::SEMIBOLD)
            .hover(|s| s.opacity(0.9));
    } else {
        b = b
            .bg(theme::raised())
            .border_color(theme::border())
            .text_color(theme::text())
            .hover(|s| s.bg(theme::border()).border_color(theme::border_strong()));
    }
    if let Some(i) = icon_name {
        b = b.child(icon_el(
            i,
            if primary {
                gpui::rgb(0x0b1020).into()
            } else {
                theme::muted()
            },
            14.0,
        ));
    }
    b.child(SharedString::from(label.to_string()))
}

fn usage_bar(fraction: f32, color: Hsla, height: f32) -> impl IntoElement {
    div()
        .w_full()
        .h(px(height))
        .rounded_full()
        .bg(theme::border())
        .overflow_hidden()
        .child(
            div()
                .h_full()
                .rounded_full()
                .bg(color)
                .w(gpui::relative(fraction.clamp(0.0, 1.0))),
        )
}

impl DiskUsage {
    fn render_home(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let mut locations = div().flex().flex_col().gap_2().w_full();

        if let Some(home) = self.home.clone() {
            let cap = mounts::capacity(&home);
            locations = locations.child(self.location_card(
                "home",
                "home".into(),
                "Home folder".into(),
                home.to_string_lossy().into_owned().into(),
                cap,
                home,
                cx,
            ));
        }
        for (i, m) in self.mounts.clone().into_iter().enumerate() {
            let title: SharedString = if m.path == Path::new("/") {
                "System".into()
            } else {
                m.path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| m.path.to_string_lossy().into_owned())
                    .into()
            };
            let sub = format!("{} · {} · {}", m.path.display(), m.fs_type, m.device);
            locations = locations.child(self.location_card(
                ("mount", i),
                "drive".into(),
                title,
                sub.into(),
                Some((m.total, m.free)),
                m.path.clone(),
                cx,
            ));
        }

        let error = self.error.clone().map(|e| {
            div()
                .w_full()
                .px_3()
                .py_2()
                .rounded_md()
                .bg(hsla(355.0 / 360.0, 0.8, 0.6, 0.12))
                .border_1()
                .border_color(hsla(355.0 / 360.0, 0.8, 0.6, 0.3))
                .text_sm()
                .text_color(theme::danger())
                .child(e)
        });

        div()
            .id("home-scroll")
            .size_full()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .items_center()
            .child(
                div()
                    .w(px(620.0))
                    .max_w_full()
                    .px_6()
                    .py(px(56.0))
                    .flex()
                    .flex_col()
                    .gap_6()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_4()
                            .child(
                                div()
                                    .size(px(52.0))
                                    .rounded_xl()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .bg(linear_gradient(
                                        135.0,
                                        linear_color_stop(theme::branch(0), 0.0),
                                        linear_color_stop(theme::branch(4), 1.0),
                                    ))
                                    .shadow_lg()
                                    .child(icon_el("chart", gpui::rgb(0x0b1020).into(), 26.0)),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .child(
                                        div()
                                            .text_2xl()
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(theme::text())
                                            .child("Disk Usage"),
                                    )
                                    .child(
                                        div().text_sm().text_color(theme::muted()).child(
                                            "Pick a location to see what's taking up space.",
                                        ),
                                    ),
                            ),
                    )
                    .children(error)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(section_label("Locations"))
                            .child(locations),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_3()
                            .child(
                                button("choose", "Choose a folder…", Some("folder-search"), true)
                                    .on_click(cx.listener(|this, _, _, cx| this.choose_folder(cx))),
                            )
                            .child(self.one_fs_toggle(cx)),
                    )
                    .child(div().text_xs().text_color(theme::faint()).child(
                        "Tip: drop a folder onto this window, or pass a path on the command line.",
                    )),
            )
            .into_any_element()
    }

    fn one_fs_toggle(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let on = self.one_fs;
        div()
            .id("one-fs")
            .flex()
            .items_center()
            .gap_2()
            .cursor_pointer()
            .text_xs()
            .text_color(theme::muted())
            .hover(|s| s.text_color(theme::text()))
            .on_click(cx.listener(|this, _, _, cx| {
                this.one_fs = !this.one_fs;
                cx.notify();
            }))
            .child(
                div()
                    .w(px(28.0))
                    .h(px(16.0))
                    .rounded_full()
                    .p(px(2.0))
                    .bg(if on {
                        theme::accent()
                    } else {
                        theme::border_strong()
                    })
                    .flex()
                    .when(on, |d| d.justify_end())
                    .child(div().size(px(12.0)).rounded_full().bg(theme::text())),
            )
            .child("Stay on one filesystem")
    }

    #[allow(clippy::too_many_arguments)]
    fn location_card(
        &self,
        id: impl Into<ElementId>,
        icon_name: SharedString,
        title: SharedString,
        subtitle: SharedString,
        capacity: Option<(u64, u64)>,
        path: PathBuf,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (bar, numbers) = match capacity {
            Some((total, free)) if total > 0 => {
                let used = total.saturating_sub(free);
                let f = used as f32 / total as f32;
                let color = if f > 0.9 {
                    theme::danger()
                } else if f > 0.75 {
                    theme::branch(2)
                } else {
                    theme::accent()
                };
                (
                    Some(usage_bar(f, color, 5.0)),
                    Some(format!(
                        "{} free of {}",
                        format::bytes(free),
                        format::bytes(total)
                    )),
                )
            }
            _ => (None, None),
        };
        div()
            .id(id)
            .w_full()
            .p_3()
            .rounded_lg()
            .bg(theme::panel())
            .border_1()
            .border_color(theme::border())
            .cursor_pointer()
            .hover(|s| s.bg(theme::raised()).border_color(theme::border_strong()))
            .active(|s| s.border_color(theme::accent()))
            .on_click(cx.listener(move |this, _, _, cx| this.start_scan(path.clone(), cx)))
            .flex()
            .items_center()
            .gap_3()
            .child(
                div()
                    .size(px(36.0))
                    .rounded_md()
                    .bg(theme::raised())
                    .flex()
                    .items_center()
                    .justify_center()
                    .flex_none()
                    .child(icon_el(&icon_name, theme::accent(), 18.0)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .gap_2()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme::text())
                                    .child(title),
                            )
                            .children(
                                numbers
                                    .map(|n| div().text_xs().text_color(theme::muted()).child(n)),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme::faint())
                            .truncate()
                            .child(subtitle),
                    )
                    .children(bar),
            )
    }

    fn render_scanning(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let s = self.scanning.as_ref().unwrap();
        let p = &s.progress;
        let files = p.files.load(Ordering::Relaxed);
        let dirs = p.dirs.load(Ordering::Relaxed);
        let bytes = p.bytes.load(Ordering::Relaxed);
        let errors = p.errors.load(Ordering::Relaxed);
        let elapsed = s.started.elapsed();
        let current: SharedString = p
            .current
            .lock()
            .map(|c| c.to_string_lossy().into_owned())
            .unwrap_or_default()
            .into();
        let rate = (files + dirs) as f64 / elapsed.as_secs_f64().max(0.001);

        // Indeterminate sweep, driven by the progress ticker.
        let t = (elapsed.as_secs_f32() * 0.8).fract();
        let sweep_x = t * 1.4 - 0.4;

        let stat = |label: &'static str, value: String| {
            div()
                .flex()
                .flex_col()
                .gap_0p5()
                .flex_1()
                .child(div().text_xs().text_color(theme::faint()).child(label))
                .child(
                    div()
                        .text_lg()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme::text())
                        .child(value),
                )
        };

        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .w(px(520.0))
                    .max_w_full()
                    .p_6()
                    .rounded_xl()
                    .bg(theme::panel())
                    .border_1()
                    .border_color(theme::border())
                    .shadow_lg()
                    .flex()
                    .flex_col()
                    .gap_5()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(div().text_xs().text_color(theme::muted()).child("Scanning"))
                            .child(
                                div()
                                    .text_lg()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme::text())
                                    .text_ellipsis_middle()
                                    .whitespace_nowrap()
                                    .overflow_hidden()
                                    .child(SharedString::from(
                                        s.path.to_string_lossy().into_owned(),
                                    )),
                            ),
                    )
                    .child(
                        div()
                            .text_3xl()
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme::text())
                            .child(format::bytes(bytes)),
                    )
                    .child(
                        div()
                            .relative()
                            .w_full()
                            .h(px(4.0))
                            .rounded_full()
                            .bg(theme::border())
                            .overflow_hidden()
                            .child(
                                div()
                                    .absolute()
                                    .top_0()
                                    .h_full()
                                    .w(gpui::relative(0.4))
                                    .left(gpui::relative(sweep_x))
                                    .rounded_full()
                                    .bg(linear_gradient(
                                        90.0,
                                        linear_color_stop(theme::accent().opacity(0.0), 0.0),
                                        linear_color_stop(theme::accent(), 1.0),
                                    )),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_4()
                            .child(stat("Files", format::count(files)))
                            .child(stat("Folders", format::count(dirs)))
                            .child(stat("Items / s", format::count(rate as u64)))
                            .child(stat("Elapsed", format::duration(elapsed))),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme::faint())
                            .text_ellipsis_middle()
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .child(current),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(if errors > 0 {
                                        theme::danger()
                                    } else {
                                        theme::faint()
                                    })
                                    .child(if errors > 0 {
                                        format!("{} unreadable", format::count(errors))
                                    } else {
                                        String::new()
                                    }),
                            )
                            .child(
                                button("cancel", "Cancel", Some("x"), false)
                                    .on_click(cx.listener(|this, _, _, cx| this.cancel_scan(cx))),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_toolbar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let has_tree = self.tree.is_some();
        let can_back = !self.back.is_empty();
        let can_fwd = !self.forward.is_empty();
        let can_up = has_tree && self.current != ROOT;
        let mode = self.mode;

        let seg = |id: &'static str, label: &'static str, active: bool| {
            div()
                .id(id)
                .px_2p5()
                .h(px(24.0))
                .flex()
                .items_center()
                .rounded(px(5.0))
                .text_xs()
                .cursor_pointer()
                .when(active, |d| {
                    d.bg(theme::border_strong()).text_color(theme::text())
                })
                .when(!active, |d| {
                    d.text_color(theme::muted())
                        .hover(|s| s.text_color(theme::text()))
                })
                .child(label)
        };

        div()
            .flex_none()
            .h(px(48.0))
            .px_2()
            .flex()
            .items_center()
            .gap_1()
            .bg(theme::panel())
            .border_b_1()
            .border_color(theme::border())
            .child(
                icon_button("home", "home", true)
                    .tooltip(|_, cx| {
                        cx.new(|_| TooltipText("Back to locations  ·  Ctrl+N".into()))
                            .into()
                    })
                    .on_click(cx.listener(|this, _, w, cx| this.start_over(&StartOver, w, cx))),
            )
            .child(div().w(px(1.0)).h(px(18.0)).mx_1().bg(theme::border()))
            .child(
                icon_button("back", "arrow-left", can_back).when(can_back, |b| {
                    b.on_click(cx.listener(|this, _, w, cx| this.go_back(&GoBack, w, cx)))
                }),
            )
            .child(
                icon_button("fwd", "arrow-right", can_fwd).when(can_fwd, |b| {
                    b.on_click(cx.listener(|this, _, w, cx| this.go_forward(&GoForward, w, cx)))
                }),
            )
            .child(icon_button("up", "arrow-up", can_up).when(can_up, |b| {
                b.on_click(cx.listener(|this, _, w, cx| this.go_up(&GoUp, w, cx)))
            }))
            .child(self.render_breadcrumbs(cx))
            .when(has_tree, |d| {
                d.child(
                    div()
                        .flex()
                        .items_center()
                        .p(px(3.0))
                        .gap(px(2.0))
                        .rounded_md()
                        .bg(theme::bg())
                        .border_1()
                        .border_color(theme::border())
                        .child(
                            seg("mode-disk", "On disk", mode == SizeMode::Disk).on_click(
                                cx.listener(|this, _, _, cx| this.set_mode(SizeMode::Disk, cx)),
                            ),
                        )
                        .child(
                            seg("mode-apparent", "Apparent", mode == SizeMode::Apparent).on_click(
                                cx.listener(|this, _, _, cx| this.set_mode(SizeMode::Apparent, cx)),
                            ),
                        ),
                )
                .child(div().w_1())
                .child(
                    button("rescan", "Rescan", Some("refresh"), false)
                        .on_click(cx.listener(|this, _, w, cx| this.rescan(&Rescan, w, cx))),
                )
            })
            .child(
                button("open", "Open…", Some("folder-search"), false)
                    .on_click(cx.listener(|this, _, _, cx| this.choose_folder(cx))),
            )
    }

    fn render_breadcrumbs(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut crumbs = div()
            .id("crumbs")
            .flex_1()
            .min_w_0()
            .h_full()
            .mx_2()
            .flex()
            .items_center()
            .gap_0p5()
            .overflow_x_scroll();
        let Some(tree) = self.tree.clone() else {
            return crumbs;
        };
        let chain = tree.ancestry(self.current);
        let n = chain.len();
        for (i, id) in chain.into_iter().enumerate() {
            let last = i + 1 == n;
            let label: SharedString = if id == ROOT {
                tree.root_path.to_string_lossy().into_owned().into()
            } else {
                tree.node(id).name.to_string().into()
            };
            if i > 0 {
                crumbs = crumbs.child(icon_el("chevron-right", theme::faint(), 12.0));
            }
            crumbs = crumbs.child(
                div()
                    .id(("crumb", id as usize))
                    .flex_none()
                    .px_1p5()
                    .py_0p5()
                    .rounded(px(4.0))
                    .text_sm()
                    .whitespace_nowrap()
                    .when(last, |d| {
                        d.text_color(theme::text())
                            .font_weight(FontWeight::SEMIBOLD)
                    })
                    .when(!last, |d| {
                        d.text_color(theme::muted())
                            .cursor_pointer()
                            .hover(|s| s.bg(theme::hover()).text_color(theme::text()))
                            .on_click(cx.listener(move |this, _, _, cx| this.navigate(id, cx)))
                    })
                    .child(label),
            );
        }
        crumbs
    }

    fn render_browser(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .child(self.render_sidebar(window, cx))
            .child(self.render_map_panel(cx))
            .into_any_element()
    }

    fn render_sidebar(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tree = self.tree.clone().unwrap();
        let cur = tree.node(self.current);
        let total = cur.size(self.mode);
        let root_total = tree.node(ROOT).size(self.mode);
        let title: SharedString = if self.current == ROOT {
            tree.root_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| tree.root_path.to_string_lossy().into_owned())
                .into()
        } else {
            cur.name.to_string().into()
        };

        // A stacked bar of the biggest children, colored like the treemap.
        let mut stack = div()
            .w_full()
            .h(px(8.0))
            .rounded_full()
            .overflow_hidden()
            .flex()
            .bg(theme::border());
        if total > 0 {
            for (i, &c) in tree
                .sorted_children(self.current, self.mode)
                .iter()
                .take(12)
                .enumerate()
            {
                let s = tree.node(c).size(self.mode);
                let f = s as f32 / total as f32;
                if f < 0.004 {
                    break;
                }
                stack = stack.child(
                    div()
                        .h_full()
                        .w(gpui::relative(f))
                        .bg(theme::branch(i as u32))
                        .border_r(px(1.0))
                        .border_color(theme::panel()),
                );
            }
        }

        let sort = self.sort;
        let header_cell = |id: &'static str, label: &'static str, key: SortKey| {
            let active = sort == key;
            div()
                .id(id)
                .flex()
                .items_center()
                .gap_1()
                .cursor_pointer()
                .text_color(if active {
                    theme::text()
                } else {
                    theme::faint()
                })
                .hover(|s| s.text_color(theme::text()))
                .child(label)
                .when(active, |d| d.child("↓"))
        };

        let rows = self.rows.clone();
        let count = rows.len();

        div()
            .w(px(440.0))
            .min_w(px(320.0))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .bg(theme::panel())
            .border_r_1()
            .border_color(theme::border())
            .child(
                div()
                    .flex_none()
                    .p_4()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .border_b_1()
                    .border_color(theme::border())
                    .child(
                        div()
                            .flex()
                            .items_end()
                            .justify_between()
                            .gap_3()
                            .child(
                                div()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .child(
                                        div()
                                            .text_lg()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(theme::text())
                                            .truncate()
                                            .child(title),
                                    )
                                    .child(div().text_xs().text_color(theme::muted()).child(
                                        format!(
                                            "{} items{}",
                                            format::count(cur.items),
                                            if self.current != ROOT {
                                                format!(
                                                    " · {} of scan",
                                                    format::percent(total, root_total)
                                                )
                                            } else {
                                                String::new()
                                            }
                                        ),
                                    )),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .text_2xl()
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(theme::text())
                                    .child(format::bytes(total)),
                            ),
                    )
                    .child(stack),
            )
            .child(
                div()
                    .flex_none()
                    .h(px(28.0))
                    .px_3()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_xs()
                    .border_b_1()
                    .border_color(theme::border())
                    .child(div().w(px(14.0)))
                    .child(div().flex_1().child(
                        header_cell("h-name", "Name", SortKey::Name).on_click(
                            cx.listener(|this, _, _, cx| this.set_sort(SortKey::Name, cx)),
                        ),
                    ))
                    .child(div().w(px(64.0)).flex().justify_end().child(
                        header_cell("h-items", "Items", SortKey::Items).on_click(
                            cx.listener(|this, _, _, cx| this.set_sort(SortKey::Items, cx)),
                        ),
                    ))
                    .child(div().w(px(126.0)).flex().justify_end().child(
                        header_cell("h-size", "Size", SortKey::Size).on_click(
                            cx.listener(|this, _, _, cx| this.set_sort(SortKey::Size, cx)),
                        ),
                    )),
            )
            .child(if count == 0 {
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_sm()
                    .text_color(theme::faint())
                    .child(if cur.error {
                        "Couldn't read this folder"
                    } else {
                        "Empty folder"
                    })
                    .into_any_element()
            } else {
                uniform_list(
                    "rows",
                    count,
                    cx.processor(move |this, range: std::ops::Range<usize>, _w, cx| {
                        range
                            .map(|ix| this.render_row(ix, rows[ix], cx))
                            .collect::<Vec<_>>()
                    }),
                )
                .track_scroll(&self.list_scroll)
                .flex_1()
                .into_any_element()
            })
    }

    fn render_row(&self, ix: usize, row: Row, cx: &mut Context<Self>) -> AnyElement {
        let tree = self.tree.as_ref().unwrap();
        let n = tree.node(row.node);
        let total = tree.node(self.current).size(self.mode);
        let size = n.size(self.mode);
        let frac = if total > 0 {
            size as f32 / total as f32
        } else {
            0.0
        };
        let selected = self.selected == Some(row.node);
        let linked = self
            .hovered_tile
            .is_some_and(|h| tree.is_ancestor(row.node, h));
        let color = theme::branch(row.color);
        let node = row.node;
        let (icon_name, icon_color) = match n.kind {
            Kind::Dir => ("folder", color),
            Kind::Symlink => ("link", theme::muted()),
            Kind::Other if n.disk == 0 && n.child_count == 0 && n.items == 0 => {
                ("drive", theme::muted())
            }
            _ => ("file", theme::muted()),
        };
        let openable = n.is_dir() && n.child_count > 0;

        div()
            .id(("row", ix))
            .w_full()
            .h(px(32.0))
            .mx_1()
            .px_2()
            .flex()
            .items_center()
            .gap_2()
            .rounded_md()
            .cursor_pointer()
            .when(selected, |d| d.bg(theme::accent_soft()))
            .when(!selected && linked, |d| d.bg(theme::hover()))
            .when(!selected, |d| d.hover(|s| s.bg(theme::hover())))
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                let next = if *hovered { Some(node) } else { None };
                if (*hovered || this.hovered_row == Some(node)) && this.hovered_row != next {
                    this.hovered_row = next;
                    cx.notify();
                }
            }))
            .on_click(cx.listener(move |this, e: &ClickEvent, _, cx| {
                if e.click_count() >= 2 && openable {
                    this.navigate(node, cx);
                } else {
                    this.selected = Some(node);
                    cx.notify();
                }
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, e: &MouseDownEvent, _, cx| {
                    this.selected = Some(node);
                    this.menu = Some((e.position, node));
                    cx.notify();
                }),
            )
            .child(icon_el(icon_name, icon_color, 14.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .truncate()
                    .text_color(if n.is_dir() {
                        theme::text()
                    } else {
                        gpui::rgb(0xc4cad6).into()
                    })
                    .child(SharedString::from(n.name.to_string())),
            )
            .child(
                div()
                    .w(px(64.0))
                    .flex_none()
                    .text_xs()
                    .text_right()
                    .text_color(theme::faint())
                    .child(if n.is_dir() {
                        format::count(n.items)
                    } else {
                        String::new()
                    }),
            )
            .child(
                div()
                    .w(px(56.0))
                    .flex_none()
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .child(
                        div()
                            .text_xs()
                            .text_right()
                            .text_color(theme::muted())
                            .child(format::percent(size, total)),
                    )
                    .child(usage_bar(frac, color.opacity(0.9), 3.0)),
            )
            .child(
                div()
                    .w(px(66.0))
                    .flex_none()
                    .text_sm()
                    .text_right()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme::text())
                    .child(format::bytes(size)),
            )
            .into_any_element()
    }

    fn render_map_panel(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let tree = self.tree.clone().unwrap();
        let cache = self.map.clone();
        let current = self.current;
        let mode = self.mode;
        let generation = self.generation;
        let hovered = self.hovered_tile;
        let focus = self.hovered_row.or(self.selected);

        let map = canvas(
            move |bounds, _window, _cx| {
                let mut c = cache.borrow_mut();
                let w = f32::from(bounds.size.width);
                let h = f32::from(bounds.size.height);
                let key = (current, mode, generation, w as i32, h as i32);
                if c.key != Some(key) {
                    c.tiles = treemap::layout(
                        &tree,
                        current,
                        Rect {
                            x: 0.0,
                            y: 0.0,
                            w,
                            h,
                        },
                        mode,
                    );
                    c.key = Some(key);
                }
                c.bounds = bounds;
                drop(c);
                (cache, tree)
            },
            move |bounds, (cache, tree), window, cx| {
                let c = cache.borrow();
                paint_treemap(&c.tiles, &tree, bounds, hovered, focus, mode, window, cx);
            },
        )
        .size_full();

        div()
            .flex_1()
            .min_w_0()
            .h_full()
            .p_3()
            .flex()
            .flex_col()
            .gap_2()
            .bg(theme::bg())
            .child(
                div()
                    .id("treemap")
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .rounded_lg()
                    .overflow_hidden()
                    .bg(theme::panel())
                    .cursor_pointer()
                    .on_mouse_move(cx.listener(Self::on_map_move))
                    .on_hover(cx.listener(|this, h: &bool, _, cx| {
                        if !*h {
                            this.clear_hover(cx);
                        }
                    }))
                    .on_click(cx.listener(Self::on_map_click))
                    .on_mouse_down(MouseButton::Right, cx.listener(Self::on_map_right_click))
                    .child(map)
                    .child(self.hover_card.clone()),
            )
            .child(self.render_legend())
    }

    fn render_legend(&self) -> impl IntoElement {
        let tree = self.tree.as_ref().unwrap();
        let mut row = div()
            .flex_none()
            .h(px(18.0))
            .flex()
            .items_center()
            .gap_4()
            .overflow_hidden()
            .text_xs()
            .text_color(theme::muted());
        for (i, &c) in tree
            .sorted_children(self.current, self.mode)
            .iter()
            .take(8)
            .enumerate()
        {
            let n = tree.node(c);
            if n.size(self.mode) == 0 {
                break;
            }
            row = row.child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap_1p5()
                    .child(
                        div()
                            .size(px(8.0))
                            .rounded(px(2.0))
                            .bg(theme::branch(i as u32)),
                    )
                    .child(SharedString::from(n.name.to_string())),
            );
        }
        row.child(div().flex_1()).child(
            div()
                .min_w_0()
                .truncate()
                .text_color(theme::faint())
                .child("Click to zoom · Right-click for actions · Backspace to go up"),
        )
    }

    fn render_status(&self) -> impl IntoElement {
        let mut bar = div()
            .flex_none()
            .h(px(28.0))
            .px_3()
            .flex()
            .items_center()
            .gap_3()
            .bg(theme::panel())
            .border_t_1()
            .border_color(theme::border())
            .text_xs()
            .text_color(theme::faint());
        if let Some(tree) = &self.tree {
            let root = tree.node(ROOT);
            let dirs = tree.dir_count();
            bar = bar
                .child(format!(
                    "{} files · {} folders · {} total",
                    format::count(root.items + 1 - dirs as u64),
                    format::count(dirs as u64),
                    format::bytes(root.size(self.mode))
                ))
                .child(format!("scanned in {}", format::duration(tree.elapsed)));
            if tree.errors > 0 {
                bar = bar.child(
                    div()
                        .text_color(theme::danger())
                        .child(format!("{} unreadable", format::count(tree.errors))),
                );
            }
        }
        bar = bar.child(div().flex_1());
        if let (Some((total, free)), Some(_)) = (self.capacity, &self.tree) {
            let used = total.saturating_sub(free);
            bar = bar
                .child(format!(
                    "{} free of {}",
                    format::bytes(free),
                    format::bytes(total)
                ))
                .child(div().w(px(90.0)).child(usage_bar(
                    used as f32 / total.max(1) as f32,
                    theme::accent(),
                    4.0,
                )));
        }
        bar
    }

    fn render_menu(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let (pos, node) = self.menu?;
        let tree = self.tree.as_ref()?;
        let n = tree.node(node);
        let is_dir = n.is_dir() && n.child_count > 0;

        let item = |id: &'static str, icon_name: &'static str, label: &'static str| {
            div()
                .id(id)
                .h(px(28.0))
                .px_2()
                .flex()
                .items_center()
                .gap_2()
                .rounded(px(5.0))
                .text_sm()
                .text_color(theme::text())
                .cursor_pointer()
                .hover(|s| s.bg(theme::accent_soft()))
                .child(icon_el(icon_name, theme::muted(), 14.0))
                .child(label)
        };

        Some(deferred(
            anchored().position(pos).snap_to_window().child(
                div()
                    .id("menu")
                    .occlude()
                    .w(px(220.0))
                    .p_1()
                    .rounded_lg()
                    .bg(theme::raised())
                    .border_1()
                    .border_color(theme::border_strong())
                    .shadow_lg()
                    .flex()
                    .flex_col()
                    .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                        this.menu = None;
                        cx.notify();
                    }))
                    .child(
                        div()
                            .px_2()
                            .py_1()
                            .text_xs()
                            .text_color(theme::faint())
                            .truncate()
                            .child(SharedString::from(n.name.to_string())),
                    )
                    .when(is_dir, |d| {
                        d.child(item("m-zoom", "layers", "Zoom in").on_click(cx.listener(
                            move |this, _, _, cx| {
                                this.menu = None;
                                this.navigate(node, cx);
                            },
                        )))
                    })
                    .child(item("m-open", "external", "Open").on_click(cx.listener(
                        move |this, _, _, cx| {
                            this.menu = None;
                            this.open_with_system(node, cx);
                            cx.notify();
                        },
                    )))
                    .child(item("m-reveal", "folder", "Show in file manager").on_click(
                        cx.listener(move |this, _, _, cx| {
                            this.menu = None;
                            this.reveal(node, cx);
                            cx.notify();
                        }),
                    ))
                    .child(item("m-copy", "copy", "Copy path").on_click(cx.listener(
                        move |this, _, _, cx| {
                            this.menu = None;
                            this.copy_node_path(node, cx);
                            cx.notify();
                        },
                    ))),
            ),
        ))
    }
}

impl Render for DiskUsage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = if self.scanning.is_some() {
            self.render_scanning(cx)
        } else if self.tree.is_some() {
            self.render_browser(window, cx)
        } else {
            self.render_home(cx)
        };

        div()
            .id("root")
            .key_context("DiskUsage")
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_prev))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::open_selected))
            .on_action(cx.listener(Self::go_up))
            .on_action(cx.listener(Self::go_back))
            .on_action(cx.listener(Self::go_forward))
            .on_action(cx.listener(Self::rescan))
            .on_action(cx.listener(Self::open_folder))
            .on_action(cx.listener(Self::toggle_mode))
            .on_action(cx.listener(Self::dismiss))
            .on_action(cx.listener(Self::copy_path))
            .on_action(cx.listener(Self::reveal_selected))
            .on_action(cx.listener(Self::start_over))
            .on_mouse_down(
                MouseButton::Navigate(NavigationDirection::Back),
                cx.listener(|this, _, w, cx| this.go_back(&GoBack, w, cx)),
            )
            .on_mouse_down(
                MouseButton::Navigate(NavigationDirection::Forward),
                cx.listener(|this, _, w, cx| this.go_forward(&GoForward, w, cx)),
            )
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                if let Some(p) = paths.paths().first() {
                    this.start_scan(p.clone(), cx);
                }
            }))
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::bg())
            .text_color(theme::text())
            .when(self.tree.is_some() || self.scanning.is_some(), |d| {
                d.child(self.render_toolbar(cx))
            })
            .child(body)
            .when(self.tree.is_some() && self.scanning.is_none(), |d| {
                d.child(self.render_status())
            })
            .children(self.render_menu(cx))
    }
}

// ── Treemap painting ────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn paint_treemap(
    tiles: &[Tile],
    tree: &Tree,
    bounds: Bounds<Pixels>,
    hovered: Option<NodeId>,
    focus: Option<NodeId>,
    mode: SizeMode,
    window: &mut Window,
    cx: &mut App,
) {
    let origin = bounds.origin;
    let to_bounds = |r: Rect| {
        Bounds::new(
            point(origin.x + px(r.x), origin.y + px(r.y)),
            size(px(r.w), px(r.h)),
        )
    };
    let font = window.text_style().font();
    let bold = gpui::Font {
        weight: FontWeight::SEMIBOLD,
        ..font.clone()
    };
    let label_size = px(11.0);

    window.paint_quad(fill(bounds, theme::panel()));

    for t in tiles {
        let r = t.rect;
        if r.w < 1.0 || r.h < 1.0 {
            continue;
        }
        match t.kind {
            TileKind::Frame => {
                let b = to_bounds(Rect {
                    w: r.w - 1.0,
                    h: r.h - 1.0,
                    ..r
                });
                window.paint_quad(quad(
                    b,
                    px(3.0),
                    theme::frame(t.branch, t.depth),
                    px(0.0),
                    transparent_black(),
                    Default::default(),
                ));
                if r.h >= 44.0 && r.w >= 48.0 {
                    let n = tree.node(t.node);
                    let header = Rect {
                        x: r.x + 4.0,
                        y: r.y + 1.0,
                        w: r.w - 8.0,
                        h: HEADER_H - 2.0,
                    };
                    let sz = format::bytes(n.size(mode));
                    let name = chain_label(tree, t.node, t.inner);
                    paint_label(
                        origin,
                        &name,
                        Some(&sz),
                        header,
                        &bold,
                        &font,
                        label_size,
                        theme::frame_text(t.branch),
                        theme::frame_text(t.branch).opacity(0.6),
                        window,
                        cx,
                    );
                }
            }
            TileKind::File | TileKind::Dir => {
                let (a, b) = theme::tile(t.branch, t.depth, t.node);
                let inset = if r.w > 3.0 && r.h > 3.0 { 1.0 } else { 0.0 };
                let rb = to_bounds(Rect {
                    w: r.w - inset,
                    h: r.h - inset,
                    ..r
                });
                let radius = if r.w > 8.0 && r.h > 8.0 { 2.0 } else { 0.0 };
                let bg = if matches!(t.kind, TileKind::Dir) {
                    // Unopened folders: a muted, flatter cushion.
                    linear_gradient(
                        160.0,
                        linear_color_stop(a.opacity(0.8), 0.0),
                        linear_color_stop(b.opacity(0.7), 1.0),
                    )
                } else {
                    linear_gradient(160.0, linear_color_stop(a, 0.0), linear_color_stop(b, 1.0))
                };
                window.paint_quad(quad(
                    rb,
                    px(radius),
                    bg,
                    px(0.0),
                    transparent_black(),
                    Default::default(),
                ));
                if r.w >= 44.0 && r.h >= 18.0 {
                    let n = tree.node(t.node);
                    let txt = theme::tile_text(t.branch);
                    let area = Rect {
                        x: r.x + 4.0,
                        y: r.y + 2.0,
                        w: r.w - 8.0,
                        h: 14.0,
                    };
                    paint_label(
                        origin, &n.name, None, area, &font, &font, label_size, txt, txt, window, cx,
                    );
                    if r.h >= 34.0 {
                        let sz = format::bytes(n.size(mode));
                        let area = Rect {
                            x: r.x + 4.0,
                            y: r.y + 16.0,
                            w: r.w - 8.0,
                            h: 14.0,
                        };
                        paint_label(
                            origin,
                            &sz,
                            None,
                            area,
                            &font,
                            &font,
                            label_size,
                            txt.opacity(0.65),
                            txt,
                            window,
                            cx,
                        );
                    }
                }
            }
            TileKind::Rest { count } => {
                let rb = to_bounds(Rect {
                    w: r.w - 1.0,
                    h: r.h - 1.0,
                    ..r
                });
                window.paint_quad(quad(
                    rb,
                    px(2.0),
                    theme::frame(t.branch, t.depth + 2),
                    px(0.0),
                    transparent_black(),
                    Default::default(),
                ));
                if r.w >= 60.0 && r.h >= 18.0 {
                    let label = format!("{} more", format::count(count as u64));
                    let area = Rect {
                        x: r.x + 4.0,
                        y: r.y + 2.0,
                        w: r.w - 8.0,
                        h: 14.0,
                    };
                    let c = theme::frame_text(t.branch).opacity(0.6);
                    paint_label(
                        origin, &label, None, area, &font, &font, label_size, c, c, window, cx,
                    );
                }
            }
        }
    }

    // Spotlight the focused item (from the list) by dimming everything else.
    if let Some(f) = focus
        && let Some(t) = tiles
            .iter()
            .find(|t| t.node == f && !matches!(t.kind, TileKind::Rest { .. }))
    {
        let fb = to_bounds(t.rect);
        let dim = hsla(0.0, 0.0, 0.0, 0.5);
        let (l, tp, r, b) = (bounds.left(), bounds.top(), bounds.right(), bounds.bottom());
        let (fl, ft, fr, fbm) = (fb.left(), fb.top(), fb.right(), fb.bottom());
        for rect in [
            Bounds::from_corners(point(l, tp), point(r, ft)),
            Bounds::from_corners(point(l, fbm), point(r, b)),
            Bounds::from_corners(point(l, ft), point(fl, fbm)),
            Bounds::from_corners(point(fr, ft), point(r, fbm)),
        ] {
            if rect.size.width > px(0.0) && rect.size.height > px(0.0) {
                window.paint_quad(fill(rect, dim));
            }
        }
        window.paint_quad(quad(
            fb,
            px(3.0),
            transparent_black(),
            px(2.0),
            theme::accent(),
            Default::default(),
        ));
    }

    if let Some(h) = hovered
        && let Some(t) = tiles.iter().rev().find(|t| t.node == h)
    {
        let hb = to_bounds(Rect {
            w: t.rect.w - 1.0,
            h: t.rect.h - 1.0,
            ..t.rect
        });
        window.paint_quad(quad(
            hb,
            Corners::all(px(3.0)),
            hsla(0.0, 0.0, 1.0, 0.10),
            px(1.5),
            hsla(0.0, 0.0, 1.0, 0.9),
            Default::default(),
        ));
    }
}

/// "a / b / c" for a folded chain of single-folder directories.
fn chain_label(tree: &Tree, top: NodeId, inner: NodeId) -> String {
    if top == inner {
        return tree.node(top).name.to_string();
    }
    let mut parts = Vec::new();
    let mut cur = inner;
    while cur != top {
        parts.push(&*tree.node(cur).name);
        cur = tree.node(cur).parent;
    }
    parts.push(&*tree.node(top).name);
    parts.reverse();
    parts.join(" / ")
}

#[allow(clippy::too_many_arguments)]
fn paint_label(
    base: Point<Pixels>,
    text: &str,
    suffix: Option<&str>,
    area: Rect,
    font: &gpui::Font,
    suffix_font: &gpui::Font,
    font_size: Pixels,
    color: Hsla,
    suffix_color: Hsla,
    window: &mut Window,
    cx: &mut App,
) {
    if area.w < 8.0 {
        return;
    }
    // Truncate by an average glyph width so we shape each label only once.
    let avg = f32::from(font_size) * 0.56;
    let max_chars = ((area.w / avg) as usize).max(1);
    let mut s = String::with_capacity(text.len() + 16);
    let char_count = text.chars().count();
    if char_count > max_chars {
        s.extend(text.chars().take(max_chars.saturating_sub(1)));
        s.push('…');
    } else {
        s.push_str(text);
    }
    let name_len = s.len();
    let mut runs = vec![TextRun {
        len: name_len,
        font: font.clone(),
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    }];
    if let Some(suffix) = suffix {
        let room = max_chars.saturating_sub(s.chars().count() + 2);
        if room >= suffix.chars().count() {
            s.push_str("  ");
            s.push_str(suffix);
            runs.push(TextRun {
                len: s.len() - name_len,
                font: suffix_font.clone(),
                color: suffix_color,
                background_color: None,
                underline: None,
                strikethrough: None,
            });
        }
    }
    let line = window
        .text_system()
        .shape_line(s.into(), font_size, &runs, None);
    let origin = point(base.x + px(area.x), base.y + px(area.y));
    let clip = Bounds::new(origin, size(px(area.w), px(area.h)));
    window.with_content_mask(Some(ContentMask { bounds: clip }), |window| {
        let _ = line.paint(origin, px(area.h), TextAlign::Left, None, window, cx);
    });
}
