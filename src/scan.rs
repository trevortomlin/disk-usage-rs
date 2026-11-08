//! Parallel filesystem scanner and the compact tree it produces.
//!
//! The scan walks directories on rayon's work-stealing pool, building a
//! temporary owned tree, which is then flattened into a cache-friendly arena
//! where every node's children are stored contiguously and sorted by size.

use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rayon::prelude::*;

pub type NodeId = u32;
pub const ROOT: NodeId = 0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Dir,
    File,
    Symlink,
    Other,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SizeMode {
    /// Blocks actually allocated on disk (what `du` reports).
    #[default]
    Disk,
    /// The logical length of files (what `ls -l` reports).
    Apparent,
}

pub struct Node {
    pub name: Box<str>,
    pub disk: u64,
    pub apparent: u64,
    /// Number of files and directories below this node (excluding itself).
    pub items: u64,
    pub parent: NodeId,
    pub first_child: NodeId,
    pub child_count: u32,
    pub kind: Kind,
    /// The directory could not be fully read.
    pub error: bool,
}

impl Node {
    #[inline]
    pub fn size(&self, mode: SizeMode) -> u64 {
        match mode {
            SizeMode::Disk => self.disk,
            SizeMode::Apparent => self.apparent,
        }
    }

    #[inline]
    pub fn is_dir(&self) -> bool {
        self.kind == Kind::Dir
    }

    #[inline]
    pub fn children(&self) -> std::ops::Range<NodeId> {
        self.first_child..self.first_child + self.child_count
    }
}

pub struct Tree {
    pub root_path: PathBuf,
    pub nodes: Vec<Node>,
    pub elapsed: Duration,
    pub errors: u64,
    /// Children re-sorted by apparent size, created lazily per directory.
    apparent_order: Mutex<std::collections::HashMap<NodeId, std::sync::Arc<[NodeId]>>>,
}

impl Tree {
    #[inline]
    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id as usize]
    }

    /// Children of `id`, ordered by descending size under `mode`.
    pub fn sorted_children(&self, id: NodeId, mode: SizeMode) -> std::sync::Arc<[NodeId]> {
        let node = self.node(id);
        match mode {
            SizeMode::Disk => node.children().collect(),
            SizeMode::Apparent => {
                let mut cache = self.apparent_order.lock().unwrap();
                cache
                    .entry(id)
                    .or_insert_with(|| {
                        let mut ids: Vec<NodeId> = node.children().collect();
                        ids.sort_unstable_by(|a, b| {
                            self.node(*b).apparent.cmp(&self.node(*a).apparent)
                        });
                        ids.into()
                    })
                    .clone()
            }
        }
    }

    pub fn path_of(&self, id: NodeId) -> PathBuf {
        let mut parts = Vec::new();
        let mut cur = id;
        while cur != ROOT {
            parts.push(&*self.node(cur).name);
            cur = self.node(cur).parent;
        }
        let mut path = self.root_path.clone();
        for part in parts.iter().rev() {
            path.push(part);
        }
        path
    }

    /// Ancestors of `id` from the root down to and including `id`.
    pub fn ancestry(&self, id: NodeId) -> Vec<NodeId> {
        let mut chain = vec![id];
        let mut cur = id;
        while cur != ROOT {
            cur = self.node(cur).parent;
            chain.push(cur);
        }
        chain.reverse();
        chain
    }

    pub fn is_ancestor(&self, ancestor: NodeId, mut id: NodeId) -> bool {
        loop {
            if id == ancestor {
                return true;
            }
            if id == ROOT {
                return false;
            }
            id = self.node(id).parent;
        }
    }

    pub fn dir_count(&self) -> usize {
        self.nodes.iter().filter(|n| n.is_dir()).count()
    }
}

/// Live counters shared with the UI while a scan runs.
#[derive(Default)]
pub struct Progress {
    pub files: AtomicU64,
    pub dirs: AtomicU64,
    pub bytes: AtomicU64,
    pub errors: AtomicU64,
    pub cancel: AtomicBool,
    pub current: Mutex<PathBuf>,
}

impl Progress {
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

pub struct ScanOptions {
    pub one_file_system: bool,
}

struct Raw {
    name: Box<str>,
    disk: u64,
    apparent: u64,
    items: u64,
    kind: Kind,
    error: bool,
    children: Vec<Raw>,
}

struct Ctx<'a> {
    progress: &'a Progress,
    root_dev: u64,
    one_fs: bool,
    /// (dev, inode) of multiply-linked files already counted.
    hardlinks: Mutex<HashSet<(u64, u64)>>,
}

pub fn scan(path: &Path, options: ScanOptions, progress: &Progress) -> anyhow::Result<Tree> {
    let start = Instant::now();
    let path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let meta = fs::symlink_metadata(&path)?;
    let ctx = Ctx {
        progress,
        root_dev: meta.dev(),
        one_fs: options.one_file_system,
        hardlinks: Mutex::new(HashSet::new()),
    };

    let mut root = if meta.is_dir() {
        let mut raw = Raw {
            name: path.to_string_lossy().into(),
            disk: meta.blocks() * 512,
            apparent: meta.len(),
            items: 0,
            kind: Kind::Dir,
            error: false,
            children: Vec::new(),
        };
        scan_dir(&path, &mut raw, &ctx);
        raw
    } else {
        Raw {
            name: path.to_string_lossy().into(),
            disk: meta.blocks() * 512,
            apparent: meta.len(),
            items: 0,
            kind: Kind::File,
            error: false,
            children: Vec::new(),
        }
    };
    if progress.cancelled() {
        anyhow::bail!("scan cancelled");
    }

    sort_raw(&mut root);
    let nodes = flatten(root);
    Ok(Tree {
        root_path: path,
        nodes,
        elapsed: start.elapsed(),
        errors: progress.errors.load(Ordering::Relaxed),
        apparent_order: Mutex::default(),
    })
}

fn scan_dir(path: &Path, into: &mut Raw, ctx: &Ctx) {
    if ctx.progress.cancelled() {
        return;
    }
    let read = match fs::read_dir(path) {
        Ok(r) => r,
        Err(_) => {
            into.error = true;
            ctx.progress.errors.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };
    ctx.progress.dirs.fetch_add(1, Ordering::Relaxed);

    let mut subdirs: Vec<(PathBuf, Raw)> = Vec::new();
    let mut files: Vec<Raw> = Vec::new();
    let mut local_bytes = 0u64;

    for entry in read {
        let Ok(entry) = entry else {
            into.error = true;
            ctx.progress.errors.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        // On Unix, DirEntry::metadata does not follow symlinks.
        let Ok(meta) = entry.metadata() else {
            ctx.progress.errors.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        let name: Box<str> = entry.file_name().to_string_lossy().into();
        let ft = meta.file_type();
        let mut disk = meta.blocks() * 512;
        let mut apparent = meta.len();

        if ft.is_dir() {
            if ctx.one_fs && meta.dev() != ctx.root_dev {
                // A mount point: show it, but don't descend into another filesystem.
                files.push(Raw {
                    name,
                    disk: 0,
                    apparent: 0,
                    items: 0,
                    kind: Kind::Other,
                    error: false,
                    children: Vec::new(),
                });
                continue;
            }
            subdirs.push((
                entry.path(),
                Raw {
                    name,
                    disk,
                    apparent,
                    items: 0,
                    kind: Kind::Dir,
                    error: false,
                    children: Vec::new(),
                },
            ));
        } else {
            if meta.nlink() > 1 && !ft.is_dir() {
                let fresh = ctx
                    .hardlinks
                    .lock()
                    .unwrap()
                    .insert((meta.dev(), meta.ino()));
                if !fresh {
                    disk = 0;
                    apparent = 0;
                }
            }
            let kind = if ft.is_file() {
                Kind::File
            } else if ft.is_symlink() {
                Kind::Symlink
            } else {
                Kind::Other
            };
            local_bytes += disk;
            files.push(Raw {
                name,
                disk,
                apparent,
                items: 0,
                kind,
                error: false,
                children: Vec::new(),
            });
        }
    }

    let file_count = files.len() as u64;
    ctx.progress.files.fetch_add(file_count, Ordering::Relaxed);
    ctx.progress.bytes.fetch_add(local_bytes, Ordering::Relaxed);
    if let Ok(mut cur) = ctx.progress.current.try_lock() {
        cur.clear();
        cur.push(path);
    }

    if subdirs.len() > 1 {
        subdirs
            .par_iter_mut()
            .for_each(|(p, raw)| scan_dir(p, raw, ctx));
    } else {
        for (p, raw) in subdirs.iter_mut() {
            scan_dir(p, raw, ctx);
        }
    }

    let mut children = files;
    children.reserve(subdirs.len());
    children.extend(subdirs.into_iter().map(|(_, raw)| raw));

    for c in &children {
        into.disk += c.disk;
        into.apparent += c.apparent;
        into.items += c.items + 1;
    }
    into.children = children;
}

fn sort_raw(raw: &mut Raw) {
    // Sort every level in parallel; large trees benefit noticeably.
    raw.children
        .sort_unstable_by(|a, b| b.disk.cmp(&a.disk).then_with(|| a.name.cmp(&b.name)));
    raw.children.par_iter_mut().for_each(|c| {
        if !c.children.is_empty() {
            sort_raw(c);
        }
    });
}

/// Breadth-first flatten so each node's children occupy one contiguous range.
fn flatten(root: Raw) -> Vec<Node> {
    let total = 1 + root.items as usize;
    let mut nodes: Vec<Node> = Vec::with_capacity(total);
    let mut queue: std::collections::VecDeque<(Raw, NodeId)> =
        std::collections::VecDeque::with_capacity(1024);

    nodes.push(make_node(&root, ROOT));
    queue.push_back((root, ROOT));

    while let Some((raw, id)) = queue.pop_front() {
        let first = nodes.len() as NodeId;
        let count = raw.children.len() as u32;
        {
            let n = &mut nodes[id as usize];
            n.first_child = first;
            n.child_count = count;
        }
        for child in raw.children {
            let cid = nodes.len() as NodeId;
            nodes.push(make_node(&child, id));
            if !child.children.is_empty() {
                queue.push_back((child, cid));
            } else if let Some(n) = nodes.last_mut() {
                n.first_child = cid + 1;
            }
        }
    }
    nodes
}

fn make_node(raw: &Raw, parent: NodeId) -> Node {
    Node {
        name: raw.name.clone(),
        disk: raw.disk,
        apparent: raw.apparent,
        items: raw.items,
        parent,
        first_child: 0,
        child_count: 0,
        kind: raw.kind,
        error: raw.error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_temp_tree() {
        let dir = std::env::temp_dir().join(format!("du-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("a/b")).unwrap();
        fs::write(dir.join("a/b/big"), vec![1u8; 100_000]).unwrap();
        fs::write(dir.join("a/small"), vec![1u8; 10]).unwrap();
        fs::write(dir.join("top"), vec![1u8; 5000]).unwrap();

        let progress = Progress::default();
        let tree = scan(
            &dir,
            ScanOptions {
                one_file_system: true,
            },
            &progress,
        )
        .unwrap();
        let root = tree.node(ROOT);
        assert_eq!(root.items, 5);
        assert_eq!(root.apparent >= 105_010, true);
        let first = tree.node(root.first_child);
        assert_eq!(&*first.name, "a");
        assert!(first.is_dir());
        let b = tree.node(first.first_child);
        assert_eq!(&*b.name, "b");
        assert_eq!(
            tree.path_of(b.first_child),
            dir.canonicalize().unwrap().join("a/b/big")
        );
        for (i, n) in tree.nodes.iter().enumerate() {
            for c in n.children() {
                assert_eq!(tree.node(c).parent as usize, i);
            }
        }
        fs::remove_dir_all(&dir).unwrap();
    }
}
