//! Nested squarified treemap layout (Bruls, Huizing & van Wijk).

use crate::scan::{NodeId, SizeMode, Tree};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && py >= self.y && px < self.x + self.w && py < self.y + self.h
    }

    fn inset(&self, l: f32, t: f32, r: f32, b: f32) -> Rect {
        Rect {
            x: self.x + l,
            y: self.y + t,
            w: (self.w - l - r).max(0.0),
            h: (self.h - t - b).max(0.0),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum TileKind {
    /// A directory drawn as a frame with a header and nested children.
    Frame,
    /// A directory too small to open up, drawn solid.
    Dir,
    File,
    /// Many small items aggregated into one tile.
    Rest {
        count: u32,
    },
}

#[derive(Clone, Copy, Debug)]
pub struct Tile {
    pub node: NodeId,
    /// For frames: the deepest folder of a single-child chain starting at
    /// `node`, whose contents are what the frame shows. Equal to `node` otherwise.
    pub inner: NodeId,
    pub rect: Rect,
    pub depth: u8,
    /// Index of the top-level child this tile descends from (for coloring).
    pub branch: u32,
    pub kind: TileKind,
}

pub const HEADER_H: f32 = 17.0;
const PAD: f32 = 2.0;
const MIN_SIDE: f32 = 3.0;
const MAX_TILES: usize = 12_000;
const MAX_DEPTH: u8 = 10;

pub fn layout(tree: &Tree, root: NodeId, bounds: Rect, mode: SizeMode) -> Vec<Tile> {
    let mut out = Vec::with_capacity(2048);
    lay_children(tree, root, bounds, mode, 0, None, &mut out);
    out
}

fn lay_children(
    tree: &Tree,
    parent: NodeId,
    rect: Rect,
    mode: SizeMode,
    depth: u8,
    branch: Option<u32>,
    out: &mut Vec<Tile>,
) {
    if rect.w < MIN_SIDE || rect.h < MIN_SIDE || out.len() >= MAX_TILES {
        return;
    }
    let total = tree.node(parent).size(mode) as f64;
    if total <= 0.0 {
        return;
    }
    let children = tree.sorted_children(parent, mode);
    let area = (rect.w * rect.h) as f64;
    let scale = area / total;

    // Keep children until the remaining ones would be specks, then merge the tail.
    let min_area = (MIN_SIDE * MIN_SIDE * 2.0) as f64;
    let mut items: Vec<(Option<NodeId>, f64)> = Vec::with_capacity(children.len().min(512));
    let mut rest_size = 0.0f64;
    let mut rest_count = 0u32;
    for &c in children.iter() {
        let s = tree.node(c).size(mode) as f64;
        if s <= 0.0 {
            continue;
        }
        if rest_count == 0 && s * scale >= min_area && items.len() < 2000 {
            items.push((Some(c), s * scale));
        } else {
            rest_size += s;
            rest_count += 1;
        }
    }
    if rest_count > 0 {
        let a = rest_size * scale;
        if rest_count == 1 && a >= 1.0 {
            // A single leftover is still worth showing as itself.
            let last = children
                .iter()
                .rev()
                .copied()
                .find(|&c| tree.node(c).size(mode) > 0);
            items.push((last, a));
            rest_count = 0;
        } else if a >= 1.0 {
            items.push((None, a));
        }
    }
    if items.is_empty() {
        return;
    }

    let mut rects = Vec::with_capacity(items.len());
    squarify(&items, rect, &mut rects);

    for (i, (&(node, _), r)) in items.iter().zip(rects).enumerate() {
        let br = branch.unwrap_or(i as u32);
        let Some(node) = node else {
            out.push(Tile {
                node: parent,
                inner: parent,
                rect: r,
                depth,
                branch: br,
                kind: TileKind::Rest { count: rest_count },
            });
            continue;
        };
        let n = tree.node(node);
        if n.is_dir() && n.child_count > 0 {
            let can_open = depth < MAX_DEPTH && r.w >= 28.0 && r.h >= 28.0;
            if can_open {
                let header = if r.h >= 44.0 && r.w >= 48.0 {
                    HEADER_H
                } else {
                    PAD
                };
                // Fold chains of folders that contain just one folder into one frame.
                let mut inner = node;
                loop {
                    let n = tree.node(inner);
                    let mut live = n.children().filter(|&c| tree.node(c).size(mode) > 0);
                    match (live.next(), live.next()) {
                        (Some(only), None)
                            if tree.node(only).is_dir() && tree.node(only).child_count > 0 =>
                        {
                            inner = only
                        }
                        _ => break,
                    }
                }
                out.push(Tile {
                    node,
                    inner,
                    rect: r,
                    depth,
                    branch: br,
                    kind: TileKind::Frame,
                });
                let area = r.inset(PAD, header, PAD, PAD);
                lay_children(tree, inner, area, mode, depth + 1, Some(br), out);
            } else {
                out.push(Tile {
                    node,
                    inner: node,
                    rect: r,
                    depth,
                    branch: br,
                    kind: TileKind::Dir,
                });
            }
        } else {
            out.push(Tile {
                node,
                inner: node,
                rect: r,
                depth,
                branch: br,
                kind: if n.is_dir() {
                    TileKind::Dir
                } else {
                    TileKind::File
                },
            });
        }
    }
}

/// Lay out `items` (areas already scaled to `rect`, sorted descending) into `out`.
fn squarify(items: &[(Option<NodeId>, f64)], rect: Rect, out: &mut Vec<Rect>) {
    let mut x = rect.x as f64;
    let mut y = rect.y as f64;
    let mut w = rect.w as f64;
    let mut h = rect.h as f64;
    let mut i = 0;
    let n = items.len();

    while i < n {
        let side = w.min(h);
        if side <= 0.0 {
            // Degenerate space left: give remaining items zero-size rects.
            for _ in i..n {
                out.push(Rect {
                    x: x as f32,
                    y: y as f32,
                    w: 0.0,
                    h: 0.0,
                });
            }
            return;
        }
        // Grow the row while the worst aspect ratio improves.
        let mut sum = items[i].1;
        let mut min = sum;
        let mut max = sum;
        let mut worst = worst_ratio(sum, min, max, side);
        let mut j = i + 1;
        while j < n {
            let a = items[j].1;
            let s2 = sum + a;
            let mn = min.min(a);
            let mx = max.max(a);
            let wr = worst_ratio(s2, mn, mx, side);
            if wr > worst {
                break;
            }
            sum = s2;
            min = mn;
            max = mx;
            worst = wr;
            j += 1;
        }
        // Last row takes all remaining space to absorb rounding error.
        let thickness = if j == n {
            if w >= h { w } else { h }
        } else {
            sum / side
        };
        let mut offset = 0.0;
        for (k, item) in items[i..j].iter().enumerate() {
            let len = if k + 1 == j - i {
                side - offset
            } else {
                item.1 / thickness
            };
            let r = if w >= h {
                Rect {
                    x: x as f32,
                    y: (y + offset) as f32,
                    w: thickness as f32,
                    h: len as f32,
                }
            } else {
                Rect {
                    x: (x + offset) as f32,
                    y: y as f32,
                    w: len as f32,
                    h: thickness as f32,
                }
            };
            out.push(snap(r));
            offset += len;
        }
        if w >= h {
            x += thickness;
            w -= thickness;
        } else {
            y += thickness;
            h -= thickness;
        }
        i = j;
    }
}

#[inline]
fn worst_ratio(sum: f64, min: f64, max: f64, side: f64) -> f64 {
    let s2 = sum * sum;
    let w2 = side * side;
    (w2 * max / s2).max(s2 / (w2 * min))
}

/// Snap to whole pixels so neighbouring tiles share crisp edges.
fn snap(r: Rect) -> Rect {
    let x0 = r.x.round();
    let y0 = r.y.round();
    let x1 = (r.x + r.w).round();
    let y1 = (r.y + r.h).round();
    Rect {
        x: x0,
        y: y0,
        w: (x1 - x0).max(0.0),
        h: (y1 - y0).max(0.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn squarify_fills_rect() {
        let items: Vec<(Option<NodeId>, f64)> = [6.0, 6.0, 4.0, 3.0, 2.0, 2.0, 1.0]
            .iter()
            .map(|a| (None, a * 1000.0 / 24.0))
            .collect();
        let rect = Rect {
            x: 0.0,
            y: 0.0,
            w: 50.0,
            h: 20.0,
        };
        let mut out = vec![];
        squarify(&items, rect, &mut out);
        assert_eq!(out.len(), items.len());
        let total: f32 = out.iter().map(|r| r.w * r.h).sum();
        assert!((total - 1000.0).abs() < 60.0, "{total}");
        for r in &out {
            assert!(r.x >= 0.0 && r.y >= 0.0 && r.x + r.w <= 50.0 && r.y + r.h <= 20.0);
        }
    }
}
