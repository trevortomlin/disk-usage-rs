//! Parallel filesystem scanner and the compact tree it produces.
//!
//! The walk talks to the kernel directly: each directory is opened relative to
//! its parent's descriptor (`openat`), listed with `getdents64` into a reused
//! per-thread buffer, and its non-directory entries are `fstatat`ed by the
//! NUL-terminated name straight out of that buffer. Nothing allocates per
//! file; each directory produces one small batch of vectors. Directories are
//! never stat'ed from the parent: a child stats its own descriptor, which
//! saves one syscall per folder and tells us about mount points for free.
//!
//! Subdirectories run on rayon's work-stealing pool. When a subtree finishes it
//! sorts its entries by size, so sorting is parallel too. Afterwards the batches
//! are flattened into a 40-byte-per-node arena whose names live in one buffer
//! and whose children are contiguous.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ffi::{CString, OsStr};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rayon::prelude::*;

pub type NodeId = u32;
pub const ROOT: NodeId = 0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Kind {
    Dir,
    File,
    Symlink,
    Other,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum SizeMode {
    /// Blocks actually allocated on disk (what `du` reports).
    #[default]
    Disk,
    /// The logical length of files (what `ls -l` reports).
    Apparent,
}

const F_ERROR: u8 = 1;
const F_MOUNT: u8 = 2;
const F_DELETED: u8 = 4;

/// One file or directory: 40 bytes. Sizes are atomics only so that deleting
/// an item can update its ancestors through a shared `&Tree`; relaxed loads
/// compile to plain moves.
pub struct Node {
    disk: AtomicU64,
    apparent: AtomicU64,
    name_off: u32,
    pub parent: NodeId,
    pub first_child: NodeId,
    pub child_count: u32,
    items: AtomicU32,
    name_len: u16,
    pub kind: Kind,
    flags: AtomicU8,
}

const _: () = assert!(std::mem::size_of::<Node>() == 40);

impl Node {
    #[inline]
    pub fn disk(&self) -> u64 {
        self.disk.load(Relaxed)
    }

    #[inline]
    pub fn apparent(&self) -> u64 {
        self.apparent.load(Relaxed)
    }

    #[inline]
    pub fn size(&self, mode: SizeMode) -> u64 {
        match mode {
            SizeMode::Disk => self.disk(),
            SizeMode::Apparent => self.apparent(),
        }
    }

    /// Files and directories below this node, not counting itself.
    #[inline]
    pub fn items(&self) -> u64 {
        self.items.load(Relaxed) as u64
    }

    #[inline]
    pub fn is_dir(&self) -> bool {
        self.kind == Kind::Dir
    }

    /// The directory could not be fully read.
    #[inline]
    pub fn has_error(&self) -> bool {
        self.flags.load(Relaxed) & F_ERROR != 0
    }

    /// A mount point of another filesystem that was not descended into.
    #[inline]
    pub fn is_mount(&self) -> bool {
        self.flags.load(Relaxed) & F_MOUNT != 0
    }

    #[inline]
    pub fn is_deleted(&self) -> bool {
        self.flags.load(Relaxed) & F_DELETED != 0
    }

    #[inline]
    pub fn children(&self) -> std::ops::Range<NodeId> {
        self.first_child..self.first_child + self.child_count
    }

    /// A folder the user can open: a directory with something inside.
    #[inline]
    pub fn is_openable(&self) -> bool {
        self.is_dir() && self.child_count > 0
    }
}

/// Children of a node in display order: the stored range when it is still
/// sorted (the common case, no allocation), or a re-sorted list otherwise.
#[derive(Clone)]
pub enum ChildList {
    Range(std::ops::Range<NodeId>),
    List(Arc<[NodeId]>),
}

impl ChildList {
    pub fn len(&self) -> usize {
        match self {
            ChildList::Range(r) => r.len(),
            ChildList::List(l) => l.len(),
        }
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn iter(&self) -> ChildIter<'_> {
        match self {
            ChildList::Range(r) => ChildIter::Range(r.clone()),
            ChildList::List(l) => ChildIter::List(l.iter()),
        }
    }
}

pub enum ChildIter<'a> {
    Range(std::ops::Range<NodeId>),
    List(std::slice::Iter<'a, NodeId>),
}

impl Iterator for ChildIter<'_> {
    type Item = NodeId;
    #[inline]
    fn next(&mut self) -> Option<NodeId> {
        match self {
            ChildIter::Range(r) => r.next(),
            ChildIter::List(l) => l.next().copied(),
        }
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        match self {
            ChildIter::Range(r) => r.size_hint(),
            ChildIter::List(l) => l.size_hint(),
        }
    }
}

impl DoubleEndedIterator for ChildIter<'_> {
    fn next_back(&mut self) -> Option<NodeId> {
        match self {
            ChildIter::Range(r) => r.next_back(),
            ChildIter::List(l) => l.next_back().copied(),
        }
    }
}

/// A directory's children in a computed order.
type ChildOrder = Arc<[NodeId]>;

pub struct Tree {
    pub root_path: PathBuf,
    pub nodes: Vec<Node>,
    names: Vec<u8>,
    pub elapsed: Duration,
    pub errors: u64,
    dir_count: AtomicU64,
    /// Directories whose stored child order is stale (after deletions).
    dirty: Mutex<HashSet<NodeId>>,
    any_dirty: AtomicBool,
    /// Children re-sorted for apparent size, or after deletions; built lazily.
    order: Mutex<HashMap<(NodeId, SizeMode), ChildOrder>>,
}

impl Tree {
    #[inline]
    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id as usize]
    }

    #[inline]
    pub fn name_bytes(&self, id: NodeId) -> &[u8] {
        let n = self.node(id);
        &self.names[n.name_off as usize..n.name_off as usize + n.name_len as usize]
    }

    /// The display name. Borrowed unless the name isn't valid UTF-8.
    #[inline]
    pub fn name(&self, id: NodeId) -> Cow<'_, str> {
        String::from_utf8_lossy(self.name_bytes(id))
    }

    pub fn dir_count(&self) -> u64 {
        self.dir_count.load(Relaxed)
    }

    /// Bytes of memory held by the tree itself.
    pub fn heap_bytes(&self) -> usize {
        self.nodes.capacity() * std::mem::size_of::<Node>() + self.names.capacity()
    }

    /// Children of `id`, largest first under `mode`, without deleted items.
    pub fn sorted_children(&self, id: NodeId, mode: SizeMode) -> ChildList {
        let node = self.node(id);
        if mode == SizeMode::Disk
            && (!self.any_dirty.load(Relaxed) || !self.dirty.lock().unwrap().contains(&id))
        {
            return ChildList::Range(node.children());
        }
        let mut cache = self.order.lock().unwrap();
        let list = cache
            .entry((id, mode))
            .or_insert_with(|| {
                let mut ids: Vec<NodeId> = node
                    .children()
                    .filter(|&c| !self.node(c).is_deleted())
                    .collect();
                ids.sort_by_key(|&c| std::cmp::Reverse(self.node(c).size(mode)));
                ids.into()
            })
            .clone();
        ChildList::List(list)
    }

    pub fn path_of(&self, id: NodeId) -> PathBuf {
        let mut parts = Vec::new();
        let mut cur = id;
        while cur != ROOT {
            parts.push(self.name_bytes(cur));
            cur = self.node(cur).parent;
        }
        let mut path = self.root_path.clone();
        for part in parts.iter().rev() {
            path.push(OsStr::from_bytes(part));
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

    pub fn find_child(&self, parent: NodeId, name: &[u8]) -> Option<NodeId> {
        self.node(parent)
            .children()
            .find(|&c| !self.node(c).is_deleted() && self.name_bytes(c) == name)
    }

    /// Remove `id` from the tree after it was deleted from disk: subtract its
    /// size and item count from every ancestor and re-sort what changed.
    pub fn mark_deleted(&self, id: NodeId) {
        let n = self.node(id);
        if id == ROOT || n.is_deleted() {
            return;
        }
        n.flags.fetch_or(F_DELETED, Relaxed);
        let (disk, apparent, items) = (n.disk(), n.apparent(), n.items.load(Relaxed) + 1);
        self.any_dirty.store(true, Relaxed);
        let mut dirty = self.dirty.lock().unwrap();
        let mut order = self.order.lock().unwrap();
        let mut cur = n.parent;
        loop {
            let a = self.node(cur);
            a.disk.fetch_sub(disk, Relaxed);
            a.apparent.fetch_sub(apparent, Relaxed);
            a.items.fetch_sub(items, Relaxed);
            dirty.insert(cur);
            order.remove(&(cur, SizeMode::Disk));
            order.remove(&(cur, SizeMode::Apparent));
            if cur == ROOT {
                break;
            }
            cur = a.parent;
        }
        if n.is_dir() {
            // Directory count is used only for the status bar; keep it close.
            let dirs = 1 + self.count_dirs_below(id);
            self.dir_count.fetch_sub(dirs, Relaxed);
        }
    }

    fn count_dirs_below(&self, id: NodeId) -> u64 {
        let mut stack = vec![id];
        let mut count = 0;
        while let Some(d) = stack.pop() {
            for c in self.node(d).children() {
                let cn = self.node(c);
                if cn.is_dir() && !cn.is_deleted() {
                    count += 1;
                    stack.push(c);
                }
            }
        }
        count
    }
}

/// Live counters shared with the UI while a scan runs. Workers batch their
/// updates, so these trail the truth by at most a few thousand entries.
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
        self.cancel.load(Relaxed)
    }
}

pub struct ScanOptions {
    pub one_file_system: bool,
}

/// How many threads walk the tree; `DU_THREADS` overrides it. On a spinning
/// disk the elevator walk separately limits how many of them read at once.
fn thread_count(dev: u64) -> usize {
    if let Some(n) = std::env::var("DU_THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        return n;
    }
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    if is_rotational(dev) {
        // The elevator walk throttles itself when the disk is the bottleneck;
        // one thread per core keeps cached reads from being preempted.
        cores.max(4)
    } else {
        // SSDs want many reads queued when cold (measured on a SATA SSD:
        // 16 threads 1.68 s, 32 threads 1.33 s); cached, it makes no difference.
        (cores * 4).max(16)
    }
}

/// Whether the block device behind `dev` is a spinning disk.
fn is_rotational(dev: u64) -> bool {
    let (major, minor) = (libc::major(dev), libc::minor(dev));
    let base = format!("/sys/dev/block/{major}:{minor}");
    // Partitions keep `queue/` on their parent device.
    for p in [
        format!("{base}/queue/rotational"),
        format!("{base}/../queue/rotational"),
    ] {
        if let Ok(v) = std::fs::read_to_string(p) {
            return v.trim() == "1";
        }
    }
    false
}

// ── Scan-time representation ───────────────────────────────────────────

const NO_SUB: u32 = u32::MAX;

/// One entry of a directory being scanned (32 bytes).
struct Entry {
    disk: u64,
    apparent: u64,
    /// Offset of the NUL-terminated name in the directory's `names`.
    name_off: u32,
    items: u32,
    /// Index into the directory's `subs` for subdirectories.
    sub: u32,
    name_len: u16,
    kind: Kind,
    flags: u8,
}

#[derive(Default)]
struct DirOut {
    names: Vec<u8>,
    entries: Vec<Entry>,
    subs: Vec<DirOut>,
    /// The directory's own size, then its total once children are summed.
    disk: u64,
    apparent: u64,
    items: u64,
    flags: u8,
    /// Set when the child turned out to be another filesystem.
    mount: bool,
}

/// A path as a chain of borrowed names on the stack, materialized only when
/// progress wants to show it.
struct Chain<'a> {
    name: &'a [u8],
    parent: Option<&'a Chain<'a>>,
}

impl Chain<'_> {
    fn to_path(&self) -> PathBuf {
        let mut parts = vec![self.name];
        let mut p = self.parent;
        while let Some(c) = p {
            parts.push(c.name);
            p = c.parent;
        }
        let mut path = PathBuf::new();
        for part in parts.iter().rev() {
            path.push(OsStr::from_bytes(part));
        }
        path
    }
}

struct Ctx<'a> {
    progress: &'a Progress,
    root_dev: u64,
    one_fs: bool,
    /// (dev, inode) of multiply-linked files already counted, sharded to
    /// keep threads from contending.
    hardlinks: [Mutex<HashSet<(u64, u64)>>; 16],
    /// Batch each directory's stats through io_uring (`DU_URING=1`).
    uring: bool,
}

/// The parts of a stat result the scan uses.
struct Meta {
    mode: u32,
    nlink: u64,
    dev: u64,
    ino: u64,
    size: u64,
    blocks: u64,
}

impl Meta {
    fn from_stat(st: &libc::stat) -> Self {
        Meta {
            mode: st.st_mode,
            nlink: st.st_nlink,
            dev: st.st_dev,
            ino: st.st_ino,
            size: st.st_size as u64,
            blocks: st.st_blocks as u64,
        }
    }

    fn from_statx(st: &libc::statx) -> Self {
        Meta {
            mode: st.stx_mode as u32,
            nlink: st.stx_nlink as u64,
            dev: libc::makedev(st.stx_dev_major, st.stx_dev_minor),
            ino: st.stx_ino,
            size: st.stx_size,
            blocks: st.stx_blocks,
        }
    }
}

const URING_BATCH: usize = 64;

struct Uring {
    ring: io_uring::IoUring,
    bufs: Vec<libc::statx>,
}

impl Uring {
    fn new() -> Option<Self> {
        let ring = io_uring::IoUring::new(URING_BATCH as u32 * 2).ok()?;
        // Bound the kernel worker threads that run the (blocking) statx calls.
        let _ = ring.submitter().register_iowq_max_workers(&mut [16, 16]);
        Some(Uring {
            ring,
            bufs: vec![unsafe { std::mem::zeroed() }; URING_BATCH],
        })
    }

    /// Stat a batch of names relative to `dirfd`; `f` gets each result.
    fn stat_batch(
        &mut self,
        dirfd: RawFd,
        names: &[*const libc::c_char],
        mut f: impl FnMut(usize, Option<Meta>),
    ) -> bool {
        use io_uring::{opcode, types};
        let mask = libc::STATX_TYPE
            | libc::STATX_MODE
            | libc::STATX_NLINK
            | libc::STATX_INO
            | libc::STATX_SIZE
            | libc::STATX_BLOCKS;
        for (i, &name) in names.iter().enumerate() {
            let sqe = opcode::Statx::new(
                types::Fd(dirfd),
                name,
                &mut self.bufs[i] as *mut libc::statx as *mut types::statx,
            )
            .flags(libc::AT_SYMLINK_NOFOLLOW)
            .mask(mask)
            .build()
            .user_data(i as u64);
            if unsafe { self.ring.submission().push(&sqe) }.is_err() {
                return false;
            }
        }
        let mut done = 0;
        while done < names.len() {
            if self.ring.submit_and_wait(names.len() - done).is_err() {
                // Interrupted: retry, the requests are still in flight.
                continue;
            }
            for cqe in self.ring.completion() {
                let i = cqe.user_data() as usize;
                let meta = (cqe.result() >= 0).then(|| Meta::from_statx(&self.bufs[i]));
                f(i, meta);
                done += 1;
            }
        }
        true
    }
}

thread_local! {
    static DENTS: RefCell<Vec<u8>> = RefCell::new(vec![0u8; 64 * 1024]);
    /// (inode, entry index) pairs waiting to be stat'ed, reused per thread.
    static TO_STAT: RefCell<Vec<(u64, u32)>> = const { RefCell::new(Vec::new()) };
    static URING: RefCell<Option<Option<Uring>>> = const { RefCell::new(None) };
    static PENDING: Cell<(u64, u64, u64, u32)> = const { Cell::new((0, 0, 0, 0)) };
}

pub fn scan(path: &Path, options: ScanOptions, progress: &Progress) -> anyhow::Result<Tree> {
    let start = Instant::now();
    raise_fd_limit();
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let cpath = CString::new(path.as_os_str().as_bytes())?;
    let st = lstat_path(&cpath)?;
    let root_name = path.as_os_str().as_bytes().to_vec();

    let ctx = Ctx {
        progress,
        root_dev: st.st_dev,
        one_fs: options.one_file_system,
        hardlinks: Default::default(),
        // Measured slower on a cold spinning disk (its workers add queues
        // for the scheduler to idle on) and only equal when cached, so off
        // unless asked for.
        uring: std::env::var("DU_URING").ok().as_deref() == Some("1"),
    };

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(thread_count(st.st_dev))
        .thread_name(|i| format!("scan-{i}"))
        .stack_size(1 << 20)
        .build()?;

    let mut root = DirOut::default();
    let is_dir = (st.st_mode & libc::S_IFMT) == libc::S_IFDIR;
    if is_dir {
        let fd = unsafe {
            libc::open(
                cpath.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let walk = std::env::var("DU_WALK").ok();
        let walk = walk.as_deref().unwrap_or(if is_rotational(st.st_dev) {
            "elevator"
        } else {
            "dfs"
        });
        if walk == "bfs" {
            drop(fd);
            root = walk_levels(&root_name, &ctx, &pool);
        } else if walk == "elevator" {
            drop(fd);
            root = walk_elevator(&root_name, &ctx, &pool);
        } else {
            let chain = Chain {
                name: &root_name,
                parent: None,
            };
            pool.install(|| scan_dir(fd, &mut root, &ctx, &chain));
        }
    } else {
        root.disk = st.st_blocks as u64 * 512;
        root.apparent = st.st_size as u64;
    }
    let walked = start.elapsed();
    pool.install(|| flush_progress(progress));
    drop(pool);
    if progress.cancelled() {
        anyhow::bail!("scan cancelled");
    }

    let (nodes, names, dirs) = flatten(root, &root_name, is_dir);
    // The scan's temporary batches are gone; hand their pages back to the OS
    // instead of keeping them in the allocator's free lists.
    let flattened = start.elapsed();
    unsafe { libc::malloc_trim(0) };
    if std::env::var_os("DU_TRACE").is_some() {
        eprintln!(
            "walk={:.1}ms flatten={:.1}ms trim={:.1}ms",
            walked.as_secs_f64() * 1e3,
            (flattened - walked).as_secs_f64() * 1e3,
            (start.elapsed() - flattened).as_secs_f64() * 1e3
        );
    }
    Ok(Tree {
        root_path: path,
        nodes,
        names,
        elapsed: start.elapsed(),
        errors: progress.errors.load(Relaxed),
        dir_count: AtomicU64::new(dirs),
        dirty: Mutex::default(),
        any_dirty: AtomicBool::new(false),
        order: Mutex::default(),
    })
}

fn raise_fd_limit() {
    let mut rl: libc::rlimit = unsafe { std::mem::zeroed() };
    unsafe {
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) == 0 && rl.rlim_cur < rl.rlim_max {
            rl.rlim_cur = rl.rlim_max.min(65536);
            libc::setrlimit(libc::RLIMIT_NOFILE, &rl);
        }
    }
}

fn lstat_path(p: &CString) -> std::io::Result<libc::stat> {
    let mut st = MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::lstat(p.as_ptr(), st.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { st.assume_init() })
}

#[inline]
fn fstat_fd(fd: RawFd) -> Option<libc::stat> {
    let mut st = MaybeUninit::<libc::stat>::uninit();
    (unsafe { libc::fstat(fd, st.as_mut_ptr()) } == 0).then(|| unsafe { st.assume_init() })
}

#[inline]
fn fstatat_name(dirfd: RawFd, name: *const libc::c_char) -> Option<libc::stat> {
    let mut st = MaybeUninit::<libc::stat>::uninit();
    let r = unsafe { libc::fstatat(dirfd, name, st.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW) };
    (r == 0).then(|| unsafe { st.assume_init() })
}

/// Add to this thread's pending counters and publish them every few thousand
/// entries, so threads don't fight over the shared cache line.
#[inline]
fn add_progress(p: &Progress, files: u64, bytes: u64, path: impl FnOnce() -> PathBuf) {
    PENDING.with(|c| {
        let (f, b, d, n) = c.get();
        let (f, b, d, n) = (f + files, b + bytes, d + 1, n + 1);
        if n >= 256 {
            p.files.fetch_add(f, Relaxed);
            p.bytes.fetch_add(b, Relaxed);
            p.dirs.fetch_add(d, Relaxed);
            if let Ok(mut cur) = p.current.try_lock() {
                *cur = path();
            }
            c.set((0, 0, 0, 0));
        } else {
            c.set((f, b, d, n));
        }
    });
}

fn flush_progress(p: &Progress) {
    let flush = || {
        PENDING.with(|c| {
            let (f, b, d, _) = c.replace((0, 0, 0, 0));
            p.files.fetch_add(f, Relaxed);
            p.bytes.fetch_add(b, Relaxed);
            p.dirs.fetch_add(d, Relaxed);
        })
    };
    rayon::broadcast(|_| flush());
    flush();
}

/// List one open directory into `out` and stat its non-directory entries.
/// Returns its subdirectories as (inode, entry index), in inode order, plus
/// the number of files and their bytes for progress.
fn read_dir(dirfd: RawFd, out: &mut DirOut, ctx: &Ctx) -> (Vec<(u64, u32)>, u64, u64) {
    // (inode, entry index) of subdirectories.
    let mut dirs: Vec<(u64, u32)> = Vec::new();
    let mut local_bytes = 0u64;
    let mut files = 0u64;

    TO_STAT.with(|to_stat| {
        let mut to_stat = to_stat.borrow_mut();
        to_stat.clear();

        // Pass 1: list the directory. Names go into `out.names`, NUL-terminated
        // so later syscalls can use them in place.
        DENTS.with(|buf| {
            let mut buf = buf.borrow_mut();
            loop {
                let n = unsafe {
                    libc::syscall(libc::SYS_getdents64, dirfd, buf.as_mut_ptr(), buf.len())
                };
                if n < 0 {
                    out.flags |= F_ERROR;
                    ctx.progress.errors.fetch_add(1, Relaxed);
                    break;
                }
                if n == 0 {
                    break;
                }
                let mut off = 0usize;
                while off < n as usize {
                    // struct linux_dirent64 { u64 d_ino; i64 d_off; u16 d_reclen; u8 d_type; char d_name[]; }
                    let rec = unsafe { buf.as_ptr().add(off) };
                    let ino = unsafe { std::ptr::read_unaligned(rec as *const u64) };
                    let reclen =
                        unsafe { std::ptr::read_unaligned(rec.add(16) as *const u16) } as usize;
                    let d_type = unsafe { *rec.add(18) };
                    let name =
                        unsafe { std::ffi::CStr::from_ptr(rec.add(19) as *const libc::c_char) }
                            .to_bytes();
                    off += reclen;
                    if name == b"." || name == b".." {
                        continue;
                    }
                    let name_off = out.names.len() as u32;
                    out.names.extend_from_slice(name);
                    out.names.push(0);
                    let ix = out.entries.len() as u32;
                    out.entries.push(Entry {
                        disk: 0,
                        apparent: 0,
                        name_off,
                        items: 0,
                        sub: NO_SUB,
                        name_len: name.len().min(u16::MAX as usize) as u16,
                        kind: Kind::Dir,
                        flags: 0,
                    });
                    if d_type == libc::DT_DIR {
                        dirs.push((ino, ix));
                    } else {
                        to_stat.push((ino, ix));
                    }
                }
            }
        });

        // Pass 2: stat in inode order. ext4 lists entries in hash order, which
        // scatters reads across the inode table; inode order makes them
        // nearly sequential on a cold disk and is free when cached.
        if to_stat.len() > 1 {
            to_stat.sort_unstable_by_key(|&(ino, _)| ino);
        }
        let mut apply = |ino: u64, ix: u32, meta: Option<Meta>, out: &mut DirOut| {
            let e = &mut out.entries[ix as usize];
            let Some(st) = meta else {
                ctx.progress.errors.fetch_add(1, Relaxed);
                e.kind = Kind::Other;
                e.flags = F_ERROR;
                return;
            };
            let fmt = st.mode & libc::S_IFMT;
            if fmt == libc::S_IFDIR {
                // DT_UNKNOWN on filesystems that don't fill in d_type.
                dirs.push((ino, ix));
                return;
            }
            e.kind = match fmt {
                libc::S_IFREG => Kind::File,
                libc::S_IFLNK => Kind::Symlink,
                _ => Kind::Other,
            };
            let mut disk = st.blocks * 512;
            let mut apparent = st.size;
            if st.nlink > 1 {
                let shard = &ctx.hardlinks[(st.ino as usize ^ (st.ino as usize >> 7)) & 15];
                if !shard.lock().unwrap().insert((st.dev, st.ino)) {
                    disk = 0;
                    apparent = 0;
                }
            }
            e.disk = disk;
            e.apparent = apparent;
            local_bytes += disk;
            files += 1;
        };

        // Entries handled by io_uring; the rest (or all, without it) use fstatat.
        let mut done = 0usize;
        if ctx.uring && to_stat.len() > 1 {
            URING.with(|u| {
                let mut u = u.borrow_mut();
                let Some(ring) = u.get_or_insert_with(Uring::new).as_mut() else {
                    return;
                };
                let mut ptrs: [*const libc::c_char; URING_BATCH] = [std::ptr::null(); URING_BATCH];
                for chunk in to_stat.chunks(URING_BATCH) {
                    for (p, &(_, ix)) in ptrs.iter_mut().zip(chunk) {
                        let e = &out.entries[ix as usize];
                        *p = out.names[e.name_off as usize..].as_ptr() as *const libc::c_char;
                    }
                    let mut results: [Option<Meta>; URING_BATCH] = [const { None }; URING_BATCH];
                    if !ring.stat_batch(dirfd, &ptrs[..chunk.len()], |i, m| results[i] = m) {
                        return;
                    }
                    for (&(ino, ix), m) in chunk.iter().zip(results.iter_mut()) {
                        apply(ino, ix, m.take(), out);
                    }
                    done += chunk.len();
                }
            });
        }
        {
            for &(ino, ix) in to_stat[done..].iter() {
                let e = &out.entries[ix as usize];
                let name_ptr = out.names[e.name_off as usize..].as_ptr() as *const libc::c_char;
                let meta = fstatat_name(dirfd, name_ptr).map(|st| Meta::from_stat(&st));
                apply(ino, ix, meta, out);
            }
        }
    });
    if dirs.len() > 1 {
        dirs.sort_unstable_by_key(|&(ino, _)| ino);
    }
    (dirs, files, local_bytes)
}

fn scan_dir(fd: OwnedFd, out: &mut DirOut, ctx: &Ctx, chain: &Chain) {
    let dirfd = fd.as_raw_fd();
    let Some(own) = fstat_fd(dirfd) else {
        out.flags |= F_ERROR;
        ctx.progress.errors.fetch_add(1, Relaxed);
        return;
    };
    if ctx.one_fs && own.st_dev != ctx.root_dev {
        out.mount = true;
        return;
    }
    out.disk = own.st_blocks as u64 * 512;
    out.apparent = own.st_size as u64;
    if ctx.progress.cancelled() {
        return;
    }

    let (dirs, files, local_bytes) = read_dir(dirfd, out, ctx);
    add_progress(ctx.progress, files, local_bytes, || chain.to_path());

    // Recurse. Each child opens itself relative to our descriptor, inside
    // its own task, so a slow open (cold disk) doesn't stall its siblings.
    let names = &out.names;
    let entries = &out.entries;
    let open_child = |(_, ix): (u64, u32), sub: &mut DirOut| {
        let e = &entries[ix as usize];
        let name_ptr = names[e.name_off as usize..].as_ptr() as *const libc::c_char;
        let fd = unsafe {
            libc::openat(
                dirfd,
                name_ptr,
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            sub.flags |= F_ERROR;
            ctx.progress.errors.fetch_add(1, Relaxed);
            return;
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let chain = Chain {
            name: &names[e.name_off as usize..e.name_off as usize + e.name_len as usize],
            parent: Some(chain),
        };
        scan_dir(fd, sub, ctx, &chain);
    };
    let mut subs: Vec<DirOut> = Vec::with_capacity(dirs.len());
    subs.resize_with(dirs.len(), DirOut::default);
    if dirs.len() > 1 {
        subs.par_iter_mut()
            .zip(dirs.par_iter())
            .for_each(|(sub, &d)| open_child(d, sub));
    } else if let Some(&d) = dirs.first() {
        open_child(d, &mut subs[0]);
    }

    for (si, (sub, &(_, ix))) in subs.iter_mut().zip(&dirs).enumerate() {
        let e = &mut out.entries[ix as usize];
        if sub.mount {
            e.kind = Kind::Other;
            e.flags = F_MOUNT;
            continue;
        }
        e.disk = sub.disk;
        e.apparent = sub.apparent;
        e.items = sub.items.min(u32::MAX as u64) as u32;
        e.flags = sub.flags;
        e.sub = si as u32;
    }
    out.subs = subs;
    finish_dir(out);
}

/// A read directory and the subdirectories it revealed.
type JobResult = (DirOut, Vec<Job>);

/// A directory waiting to be read by the level-order walk.
struct Job {
    /// Absolute path, NUL-terminated.
    path: Vec<u8>,
    ino: u64,
    /// Index of the parent in the walk's arena, and of this directory among
    /// the parent's entries.
    parent: u32,
    entry: u32,
}

/// Level-order walk for spinning disks.
///
/// Nearly every read a cold scan does is a directory's first block, and ext4
/// places those near the directory's inode. So instead of a depth-first walk,
/// where each thread wanders a different subtree and the head seeks back and
/// forth across the platter, read the tree one depth at a time: sort each level
/// by inode number and have every thread take the next directory from a shared
/// cursor. The requests in flight are then always neighbours on disk and the
/// head sweeps forward, like an elevator.
fn walk_levels(root_path: &[u8], ctx: &Ctx, pool: &rayon::ThreadPool) -> DirOut {
    let mut path = root_path.to_vec();
    path.push(0);
    let mut level = vec![Job {
        path,
        ino: 0,
        parent: u32::MAX,
        entry: 0,
    }];
    let mut arena: Vec<DirOut> = Vec::new();
    let mut links: Vec<(u32, u32)> = Vec::new();

    while !level.is_empty() && !ctx.progress.cancelled() {
        level.sort_unstable_by_key(|j| j.ino);
        let n = level.len();
        let slots: Vec<Mutex<Option<JobResult>>> = (0..n).map(|_| Mutex::new(None)).collect();
        let cursor = std::sync::atomic::AtomicUsize::new(0);
        let work = || {
            loop {
                let i = cursor.fetch_add(1, Relaxed);
                if i >= n || ctx.progress.cancelled() {
                    break;
                }
                let result = read_job(&level[i], ctx);
                *slots[i].lock().unwrap() = Some(result);
            }
        };
        if n < 4 {
            work();
        } else {
            pool.broadcast(|_| work());
        }

        let base = arena.len() as u32;
        let mut next = Vec::new();
        for (i, (slot, job)) in slots.into_iter().zip(level).enumerate() {
            let (out, mut kids) = slot.into_inner().unwrap().unwrap_or_default();
            for k in &mut kids {
                k.parent = base + i as u32;
            }
            next.extend(kids);
            arena.push(out);
            links.push((job.parent, job.entry));
        }
        level = next;
    }

    fold(arena, links)
}

/// Directories in the order they were read, and each one's (parent, entry).
type Arena = (Vec<DirOut>, Vec<(u32, u32)>);

/// Fold a walk's arena (parents before children) back into a tree, deepest
/// directories first, so every directory has its children's totals before it
/// sums and sorts itself.
fn fold(mut arena: Vec<DirOut>, mut links: Vec<(u32, u32)>) -> DirOut {
    while arena.len() > 1 {
        let mut dir = arena.pop().unwrap();
        let (parent, entry) = links.pop().unwrap();
        finish_dir(&mut dir);
        let p = &mut arena[parent as usize];
        let e = &mut p.entries[entry as usize];
        if dir.mount {
            e.kind = Kind::Other;
            e.flags = F_MOUNT;
            continue;
        }
        e.disk = dir.disk;
        e.apparent = dir.apparent;
        e.items = dir.items.min(u32::MAX as u64) as u32;
        e.flags = dir.flags;
        e.sub = p.subs.len() as u32;
        p.subs.push(dir);
    }
    let mut root = arena.pop().unwrap_or_default();
    finish_dir(&mut root);
    root
}

/// The elevator walk for spinning disks (C-SCAN).
///
/// Every directory found so far waits in one set ordered by inode number.
/// Threads take the next one at or above a cursor that only moves forward,
/// wrapping to the lowest when it runs off the end. Requests in flight are
/// neighbours on disk and the head sweeps one way, as in the level walk, but
/// with no barrier between depths: a thread that finishes never waits for a
/// slow sibling, so a cached scan keeps every core busy.
fn walk_elevator(root_path: &[u8], ctx: &Ctx, pool: &rayon::ThreadPool) -> DirOut {
    use std::collections::BTreeMap;
    use std::sync::Condvar;

    struct State {
        pending: BTreeMap<(u64, u64), Job>,
        cursor: u64,
        active: usize,
        seq: u64,
        /// How many directories may be read at once, and how many reads in a
        /// row came back fast (cached).
        limit: usize,
        fast_streak: u32,
        /// One bit per recent read, set when it was slow.
        recent_slow: u16,
        switches: u32,
    }
    let threads = pool.current_num_threads();
    // When reads are slow the disk is seeking: keep a single read in flight,
    // strictly in inode order, so the head only ever hops forward. Measured
    // cold on a 7200 rpm drive (3.6 M items, fixed widths): 1 in flight took
    // 145 s, 2 took 164 s, 4 took 182 s, 8 took 195 s. When reads are fast
    // everything is cached and the work is CPU-bound, so use every thread.
    let narrow: usize = std::env::var("DU_COLD_WIDTH")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1)
        .clamp(1, threads);
    let mut path = root_path.to_vec();
    path.push(0);
    let mut pending = BTreeMap::new();
    pending.insert(
        (0, 0),
        Job {
            path,
            ino: 0,
            parent: u32::MAX,
            entry: 0,
        },
    );
    let state = Mutex::new(State {
        pending,
        cursor: 0,
        active: 0,
        seq: 1,
        // Start narrow: a cached tree proves itself fast within a few dozen
        // directories, while starting wide on a cold disk scatters the first
        // requests all over it.
        limit: narrow,
        fast_streak: 0,
        recent_slow: 0,
        switches: 0,
    });
    let wake = Condvar::new();
    // (directory, parent index, entry in parent); a directory is always
    // pushed after its parent, so parents have lower indices.
    let results: Mutex<Arena> = Mutex::default();

    pool.broadcast(|_| {
        loop {
            let job = {
                let mut s = state.lock().unwrap();
                loop {
                    if ctx.progress.cancelled() {
                        wake.notify_all();
                        return;
                    }
                    let next = s
                        .pending
                        .range((s.cursor, 0)..)
                        .next()
                        .or_else(|| s.pending.iter().next())
                        .map(|(k, _)| *k);
                    match next {
                        Some(_) if s.active >= s.limit => s = wake.wait(s).unwrap(),
                        Some(k) => {
                            let job = s.pending.remove(&k).unwrap();
                            s.cursor = k.0;
                            s.active += 1;
                            break job;
                        }
                        None if s.active == 0 => {
                            wake.notify_all();
                            return;
                        }
                        None => s = wake.wait(s).unwrap(),
                    }
                }
            };
            let started = Instant::now();
            let (out, kids) = read_job(&job, ctx);
            // Big directories take longer even when cached; count per 64 entries.
            let cost =
                started.elapsed().as_secs_f64() * 1e6 / (1.0 + out.entries.len() as f64 / 64.0);
            let id = {
                let mut r = results.lock().unwrap();
                r.0.push(out);
                r.1.push((job.parent, job.entry));
                (r.0.len() - 1) as u32
            };
            let mut s = state.lock().unwrap();
            let woke = !kids.is_empty();
            for mut k in kids {
                k.parent = id;
                s.seq += 1;
                let key = (k.ino, s.seq);
                s.pending.insert(key, k);
            }
            s.active -= 1;
            // Two slow reads among the last 16 mean the disk is seeking (one
            // alone can be a cached read that got preempted): go narrow. Go
            // wide only after a long run of fast reads, because on a cold disk
            // short runs of fast ones are common (readahead brought them in),
            // and widening on those scatters requests and breaks the sweep.
            let slow = cost > 2500.0;
            s.recent_slow = (s.recent_slow << 1) | slow as u16;
            let limit = if slow {
                s.fast_streak = 0;
                if s.recent_slow.count_ones() >= 2 {
                    narrow
                } else {
                    s.limit
                }
            } else {
                s.fast_streak += 1;
                if s.fast_streak >= 1024 {
                    threads
                } else {
                    s.limit
                }
            };
            let widened = limit > s.limit;
            if limit != s.limit {
                s.switches += 1;
            }
            s.limit = limit;
            if widened || (s.active == 0 && s.pending.is_empty()) {
                wake.notify_all();
            } else if woke || s.active < s.limit {
                // One slot freed (or work arrived): wake one waiter, not all.
                wake.notify_one();
            }
        }
    });

    if std::env::var_os("DU_TRACE").is_some() {
        let s = state.lock().unwrap();
        eprintln!("elevator: width switches={}", s.switches);
    }
    let (arena, links) = results.into_inner().unwrap();
    fold(arena, links)
}

/// Read one directory of the level-order walk; returns it and its subdirectories.
fn read_job(job: &Job, ctx: &Ctx) -> (DirOut, Vec<Job>) {
    let mut out = DirOut::default();
    let fd = unsafe {
        libc::open(
            job.path.as_ptr() as *const libc::c_char,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        out.flags |= F_ERROR;
        ctx.progress.errors.fetch_add(1, Relaxed);
        return (out, Vec::new());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let dirfd = fd.as_raw_fd();
    let Some(own) = fstat_fd(dirfd) else {
        out.flags |= F_ERROR;
        ctx.progress.errors.fetch_add(1, Relaxed);
        return (out, Vec::new());
    };
    if ctx.one_fs && own.st_dev != ctx.root_dev {
        out.mount = true;
        return (out, Vec::new());
    }
    out.disk = own.st_blocks as u64 * 512;
    out.apparent = own.st_size as u64;
    let (dirs, files, bytes) = read_dir(dirfd, &mut out, ctx);
    drop(fd);

    let base = &job.path[..job.path.len() - 1];
    let sep = !base.ends_with(b"/");
    let kids = dirs
        .into_iter()
        .map(|(ino, ix)| {
            let e = &out.entries[ix as usize];
            let name = &out.names[e.name_off as usize..e.name_off as usize + e.name_len as usize];
            let mut path = Vec::with_capacity(base.len() + name.len() + 2);
            path.extend_from_slice(base);
            if sep {
                path.push(b'/');
            }
            path.extend_from_slice(name);
            path.push(0);
            Job {
                path,
                ino,
                parent: 0,
                entry: ix,
            }
        })
        .collect();
    add_progress(ctx.progress, files, bytes, || {
        PathBuf::from(OsStr::from_bytes(base))
    });
    (out, kids)
}

/// Sum a directory whose children are final, then order it largest first.
/// A stable sort on the integer key alone: ties keep the directory's order,
/// and we avoid comparing names (equal sizes, such as many 4 KiB files, are
/// very common, and name tie-breaks were the scan's hottest code).
fn finish_dir(out: &mut DirOut) {
    for e in &out.entries {
        out.disk += e.disk;
        out.apparent += e.apparent;
        out.items += e.items as u64 + 1;
    }
    out.entries.sort_by_key(|e| std::cmp::Reverse(e.disk));
}

/// Breadth-first flatten so each node's children occupy one contiguous range.
fn flatten(root: DirOut, root_name: &[u8], is_dir: bool) -> (Vec<Node>, Vec<u8>, u64) {
    let total = 1 + root.items as usize;
    let mut nodes: Vec<Node> = Vec::with_capacity(total);
    let mut names: Vec<u8> = Vec::with_capacity(total * 16);
    let mut dirs = 0u64;

    let mk = |names: &mut Vec<u8>, name: &[u8], parent, kind, flags, disk, apparent, items| {
        let off = names.len() as u32;
        names.extend_from_slice(name);
        Node {
            disk: AtomicU64::new(disk),
            apparent: AtomicU64::new(apparent),
            name_off: off,
            parent,
            first_child: 0,
            child_count: 0,
            items: AtomicU32::new(items),
            name_len: name.len() as u16,
            kind,
            flags: AtomicU8::new(flags),
        }
    };

    nodes.push(mk(
        &mut names,
        root_name,
        ROOT,
        if is_dir { Kind::Dir } else { Kind::File },
        root.flags,
        root.disk,
        root.apparent,
        root.items.min(u32::MAX as u64) as u32,
    ));
    if is_dir {
        dirs += 1;
    }

    let mut last_trim = 0usize;
    let mut queue = std::collections::VecDeque::with_capacity(1024);
    queue.push_back((root, ROOT));
    while let Some((mut dir, id)) = queue.pop_front() {
        let first = nodes.len() as NodeId;
        nodes[id as usize].first_child = first;
        nodes[id as usize].child_count = dir.entries.len() as u32;
        for e in &dir.entries {
            let name = &dir.names[e.name_off as usize..e.name_off as usize + e.name_len as usize];
            let cid = nodes.len() as NodeId;
            nodes.push(mk(
                &mut names, name, id, e.kind, e.flags, e.disk, e.apparent, e.items,
            ));
            if e.kind == Kind::Dir {
                dirs += 1;
            }
            if e.sub != NO_SUB {
                let sub = std::mem::take(&mut dir.subs[e.sub as usize]);
                if !sub.entries.is_empty() {
                    queue.push_back((sub, cid));
                }
            }
        }
        // `dir` (its names and entries) is freed here, as we go. Every so
        // often hand those freed pages back so the peak stays near the size
        // of the final tree instead of scan batches + tree.
        drop(dir);
        if nodes.len() - last_trim > 1 << 18 {
            last_trim = nodes.len();
            unsafe { libc::malloc_trim(0) };
        }
    }
    names.shrink_to_fit();
    (nodes, names, dirs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_temp_tree() {
        let dir = std::env::temp_dir().join(format!("du-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("a/b")).unwrap();
        std::fs::write(dir.join("a/b/big"), vec![1u8; 100_000]).unwrap();
        std::fs::write(dir.join("a/small"), vec![1u8; 10]).unwrap();
        std::fs::write(dir.join("top"), vec![1u8; 5000]).unwrap();
        std::fs::hard_link(dir.join("top"), dir.join("a/top-link")).unwrap();

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
        assert_eq!(root.items(), 6);
        assert!(root.apparent() >= 105_010);
        // The hard link is counted once.
        assert!(root.apparent() < 110_000);
        assert_eq!(tree.dir_count(), 3);
        let first = root.first_child;
        assert_eq!(tree.name(first), "a");
        assert!(tree.node(first).is_dir());
        let b = tree.node(first).first_child;
        assert_eq!(tree.name(b), "b");
        let big = tree.node(b).first_child;
        assert_eq!(
            tree.path_of(big),
            dir.canonicalize().unwrap().join("a/b/big")
        );
        for (i, n) in tree.nodes.iter().enumerate() {
            for c in n.children() {
                assert_eq!(tree.node(c).parent as usize, i);
            }
        }

        // Deleting updates ancestors and hides the node.
        let before = root.disk();
        let big_disk = tree.node(big).disk();
        tree.mark_deleted(b);
        let _ = big_disk;
        assert_eq!(root.disk(), before - tree.node(b).disk());
        assert_eq!(root.items(), 4);
        let kids: Vec<_> = tree.sorted_children(first, SizeMode::Disk).iter().collect();
        assert!(!kids.contains(&b));
        assert_eq!(tree.dir_count(), 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
