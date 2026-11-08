//! Headless benchmark: `disk-usage --bench PATH [--no-layout]`.
//!
//! Scans PATH once in this process and prints one line of `key=value` metrics:
//! wall time, CPU time, peak and retained RSS, and treemap layout time.

use std::path::PathBuf;
use std::time::Instant;

use crate::scan::{self, ROOT, ScanOptions, SizeMode};
use crate::treemap::{self, Rect};

fn rusage() -> (f64, f64, u64) {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    let t = |tv: libc::timeval| tv.tv_sec as f64 + tv.tv_usec as f64 / 1e6;
    (t(ru.ru_utime), t(ru.ru_stime), ru.ru_maxrss as u64 * 1024)
}

fn rss_now() -> u64 {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    let pages: u64 = statm
        .split_whitespace()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    pages * 4096
}

pub fn run() {
    let path = std::env::args_os()
        .nth(2)
        .map(PathBuf::from)
        .unwrap_or_else(|| ".".into());
    let layout = !std::env::args().any(|a| a == "--no-layout");
    let rss0 = rss_now();
    let (u0, s0, _) = rusage();
    let start = Instant::now();
    let progress = scan::Progress::default();
    let tree = scan::scan(
        &path,
        ScanOptions {
            one_file_system: true,
        },
        &progress,
    )
    .expect("scan");
    let wall = start.elapsed().as_secs_f64();
    let (u1, s1, peak) = rusage();
    let retained = rss_now().saturating_sub(rss0);
    let items = tree.node(ROOT).items() + 1;

    let mut layout_ms = f64::NAN;
    let mut tiles = 0;
    if layout {
        // Average over several runs at a typical window size.
        let n = 20;
        let t = Instant::now();
        for _ in 0..n {
            let v = treemap::layout(
                &tree,
                ROOT,
                Rect {
                    x: 0.0,
                    y: 0.0,
                    w: 1100.0,
                    h: 760.0,
                },
                SizeMode::Disk,
            );
            tiles = v.len();
            std::hint::black_box(v);
        }
        layout_ms = t.elapsed().as_secs_f64() * 1000.0 / n as f64;
    }
    println!(
        "items={items} dirs={} disk={} wall_s={wall:.3} user_s={:.3} sys_s={:.3} cpu_pct={:.0} peak_rss_mb={:.1} retained_mb={:.1} bytes_per_item={:.1} layout_ms={layout_ms:.3} tiles={tiles} tree_mb={:.1}",
        tree.dir_count(),
        tree.node(ROOT).disk(),
        u1 - u0,
        s1 - s0,
        (u1 - u0 + s1 - s0) / wall * 100.0,
        peak as f64 / 1048576.0,
        retained as f64 / 1048576.0,
        retained as f64 / items as f64,
        tree.heap_bytes() as f64 / 1048576.0,
    );
}
