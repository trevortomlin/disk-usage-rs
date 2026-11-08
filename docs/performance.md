# Performance

Everything below was measured on one machine: an Intel i7-6700K (4 cores, 8 threads),
16 GB of RAM and Linux 7.3. There are three drives:

| Drive | Model | Filesystem | Test tree |
| --- | --- | --- | --- |
| 1 TB hard disk | Seagate ST1000DM003 (7200 rpm, SATA, BFQ scheduler) | ext4 | the whole drive: 3.62 M items, 411 k folders |
| SATA SSD | Samsung 850 EVO 250 GB | btrfs | `/usr`: 547 k items, 36 k folders |
| NVMe SSD | Samsung 970 EVO 1 TB | ext4 | the whole drive: 519 k items, 59 k folders |

"Before" is the first commit (`ae51f71`). "After" is this version. Both were measured with
the same harness, `disk-usage --bench PATH`. For cold runs the kernel caches were dropped
before every run (`echo 3 > /proc/sys/vm/drop_caches`), and each run's device reads were
counted from `/proc/diskstats` to confirm it really came from disk. With dropped caches,
repeated cold runs agree within about 1%. Without dropping them, the same binary varied
from 458 s to 896 s on the hard disk, because a full scan doesn't fit in the cache and
whatever the previous run left behind changes the result.

## Results

| | Before | After | Change |
| --- | --- | --- | --- |
| **1 TB HDD, cold** (3.62 M items) | 900 s | **149 s** | **6.0× faster** |
| ↳ disk reads served per second | 505 | 3,000 | 5.9× |
| ↳ peak memory | 674 MB | 414 MB | −39% |
| ↳ memory kept for the tree | 661 MB (191 B/item) | 218 MB (63 B/item) | −67% |
| SATA SSD `/usr`, cold | 1.89 s | **1.28 s** | 1.5× faster |
| NVMe drive, cold | 1.20–1.55 s | **0.75–1.05 s** | ~1.5× faster |
| `/usr`, cached | 0.277 s | **0.243 s** | −12% |
| ↳ user CPU | 0.73 s | 0.52 s | −29% |
| ↳ peak / kept memory | 93 / 88 MB | 66 / 36 MB | −29% / −59% |
| HDD `coding` tree, cached (1.95 M items) | 1.15 s | 1.27 s | +10% (see below) |
| ↳ kept memory | 351 MB | 120 MB | −66% |
| Treemap layout (`/usr` root, 1100×760) | 0.29 ms | 0.28 ms | — |
| UI frame CPU while hovering the treemap¹ | 2.55 ms | **1.97 ms** | −23% |
| UI frame CPU while changing the selection¹ | 3.93 ms | **2.65 ms** | −33% |
| Treemap repaints while hovering | every frame | none | |

¹ Main-thread CPU per frame, measured with the headless renderer (`DU_FRAMES=200`,
`--features snapshot`), minus the ~5.15 ms the headless renderer spends copying each
frame back from the GPU, which is the same in both builds. In the final build, moving the
pointer over the treemap re-renders only the small hover-card view.

NVMe timings shift with run order (the drive's power states); the two ranges are the
back-to-back and the interleaved series. The speedup is about 1.5× in both.

The one regression is a fully cached scan on the hard disk, about 10% slower. The elevator
walk described below shares a single ordered queue, and that costs a little when nothing
has to come off the disk. On this drive a full scan never stays cached (3.6 M inodes
don't fit), so cold is the case that matters.

## What changed, and what each step bought

Cold HDD times; each row adds to the one before.

| Step | Cold 1 TB HDD |
| --- | --- |
| Baseline: `std::fs` walk on rayon, 8 threads | 900 s |
| Raw `getdents64` + `fstatat` relative to the directory, with files stat'ed in inode order | 564 s |
| …the same with 64 threads | 564 s (thread count didn't matter) |
| …with io_uring batched `statx` | 643 s (worse; kept as `DU_URING=1`) |
| Level-by-level walk, each level sorted by inode, 64 threads | 287 s |
| …8 threads (4, 8 and 16 all ≈ 221 s; 32 → 256 s; 128 → 315 s) | 222 s |
| C-SCAN elevator: one queue ordered by inode with a forward-only cursor, 8 in flight | 195 s |
| …4 / 2 / 1 in flight | 183 / 164 / 145 s |
| Adaptive: 1 in flight while the disk is seeking, all threads once reads are cached | **149 s** |

### Why the order matters so much

The cold scan reads about 2.5 GB of metadata in about 448 k requests. That's roughly one
4 KiB read per folder, since a folder's first block is where its entries live. File inodes
are cheap by comparison, because ext4 reads the inode table ahead in 128 KiB chunks.

A depth-first walk, even a parallel one, gives the disk those reads in tree order, and
ext4 spreads a tree across the platter (the Orlov allocator puts top-level folders in
different block groups). On top of that, the BFQ scheduler gives every thread its own
queue and idles up to 8 ms waiting for each one's next request. So the drive mostly saw
one far-away request at a time: about 505 reads per second.

Inode numbers follow on-disk position, and ext4 keeps a folder's blocks near its inode.
So the new walk keeps every folder it has discovered in one set ordered by inode. Threads
always take the next one at or above a cursor that only moves forward, wrapping around
at the end (C-SCAN, the classic elevator algorithm). Every request lands just past the
previous one, and the drive serves about 3,000 per second. More requests in flight made
it *slower*: they spread out across the disk again, and the drive's command queue can't
reorder its way back to a sweep.

One request at a time is ideal for a cold disk and bad for a cached tree, where the work
is CPU-bound. So the walk adapts. It starts with one read in flight. Two slow reads
(> 2.5 ms) among the last 16 keep it there, and 1,024 fast reads in a row open it up to
every thread. The thresholds were tuned against both cases: oscillating on runs of
directories that readahead had already pulled in cost about 20 s cold, and narrowing on
cached reads that had merely been preempted cost about 0.7 s warm.

### CPU and memory

* **No per-file allocation.** Directories are listed with `getdents64` into a reused
  per-thread buffer. Files are `fstatat`'d by the NUL-terminated name straight out of
  that buffer. Each directory produces one small batch of vectors.
* **One less syscall per folder.** A folder is never stat'ed from its parent; it stats
  its own descriptor after opening it, which also detects mount points.
* **Sorting.** Profiling (callgrind) showed 45% of user time in sorting, mostly
  `memcmp` breaking ties between equal-sized files by name. Sorting by size alone, with
  a stable sort, removed that.
* **Compact tree.** 40 bytes per node, with all names in one shared buffer, instead of
  56 bytes plus a separate allocation per name. Children are stored next to each other,
  sorted by size.
* **Returning memory.** The scan's temporary batches are freed while the tree is built,
  with `malloc_trim` along the way, so the peak stays close to the final tree's size.
* **Batched progress counters.** Workers count into per-thread counters and publish
  every 256 folders, instead of contending on shared atomics.
* **UI.** The status bar's folder count used to walk every node on every frame; it's now
  kept with the tree. Child lists are no longer allocated per treemap call. The hover
  outline is drawn by the hover-card view, so the treemap and list are reused untouched
  while the pointer moves.

## How fast could the 1 TB drive possibly be?

Measured: a cold scan of the drive needs about 2.5 GB of metadata in about 448 k reads,
for 411 k folders and 3.62 M items.

1. **The theoretical floor is about 15–20 s.** 2.5 GB of metadata read sequentially, at
   this drive's 210 MB/s outer to roughly 100 MB/s inner transfer rate. Getting there
   means skipping the filesystem: read `/dev/sda` directly, parse ext4's inode tables
   and directory blocks in disk order, and rebuild the tree in memory, as WizTree does
   with NTFS's master file table. It needs root and a full ext4 parser, so it isn't
   done here.
2. **Through the filesystem, the floor is about 2 minutes.** Each folder costs at least
   one read that has to wait for its parent's, so about 411 k reads in a dependency
   order. Sorted by inode they come close to a single sweep. In the final build the disk
   was busy for 121 s of the 149 s. The rest is CPU time between strictly serial
   requests, so roughly 2 minutes is the practical floor for this approach.
3. **In random order: 15 minutes to 1½ hours.** At this drive's full-stroke figures
   (8.5 ms seek plus 4.17 ms rotational latency, about 79 random reads per second),
   448 k reads would take about 95 minutes. The baseline's 15 minutes was already helped
   by ext4 clustering metadata.

So this version is within about 25% of the best you can do through the filesystem on
this drive, and about 8× away from the raw-device floor.

One more knob, not tested here because it changes the system: the drive uses BFQ.
`mq-deadline` handles parallel random reads differently and may shift these numbers.

## Reproducing

```sh
cargo build --release
./target/release/disk-usage --bench /path            # one line of metrics
DU_TRACE=1 ./target/release/disk-usage --bench /path # plus phase timings

# For cold runs (as root):
sync; echo 3 > /proc/sys/vm/drop_caches
```

Overrides, for experiments:

| Variable | Effect |
| --- | --- |
| `DU_WALK=dfs\|bfs\|elevator` | Walk strategy. Default: `elevator` on spinning disks, `dfs` otherwise. |
| `DU_THREADS=N` | Thread count. Default: one per core on spinning disks, 4 × cores otherwise. |
| `DU_COLD_WIDTH=N` | Reads in flight while the disk is seeking (elevator walk). Default 1. |
| `DU_URING=1` | Batch file stats through io_uring. |
| `DU_FRAMES=N` | With `--snapshot`, measure UI frame CPU. |
