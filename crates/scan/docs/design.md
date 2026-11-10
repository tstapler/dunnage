# metascope design

## Problem

On 2026-11-01 a btrfs root froze the machine. Hypothesis (from sysfs, not yet proven to be
caused by any directory): metadata chunks were full (about 25.0 GiB used of 25.5 GiB, DUP so about
51 GiB on disk) and the device had about 1 MiB unallocated, so metadata could not grow even though
`df` showed about 250 GiB free (slack inside data chunks). `btrfs balance -dusage=0` freed
nothing. ncdu ranks by bytes and cannot answer "which directories cost the most metadata".

## Health (`status`)

Source: `/sys/fs/btrfs/<uuid>/allocation/{data,metadata,system}/{total_bytes,bytes_used,disk_total,disk_used}`,
`allocation/global_rsv_{size,reserved}`, and `devices/*/size` (512-byte sectors). All EXACT.

- unallocated = sum(device size) - (data + metadata + system `disk_total`)
- metadata headroom = metadata `total_bytes` - `bytes_used` (logical, inside allocated chunks).
  The global reserve is carved out of metadata, so effective headroom is lower when
  `global_rsv_reserved` is held; reported separately, not subtracted.
- Verdict (`health::assess`, strict `<`): crit if unallocated < 3 GiB; warn if unallocated < 10 GiB or
  headroom < 2 GiB. Exit code 0/1/2.
- Filesystem auto-detection: longest btrfs prefix in `/proc/self/mountinfo` gives the source
  device; the sysfs dir is the one whose `devices/` contains that name. btrfs reports an anonymous
  `st_dev`, so `stat` cannot be used. LVM/dm/LUKS sources (name differs from the sysfs device name)
  are not handled; `--fs` overrides.

## Metadata attribution (`scan`)

Each directory entry is `lstat`ed once. Per-bucket (depth-limited roll-up) counters:

| Quantity | Status | Source |
|---|---|---|
| inodes, files, dirs, symlinks | exact at scan time | lstat; hard links counted once by (dev, ino) |
| directory entries, name bytes | exact | one per link |
| size, allocated bytes | exact | `st_size`, `st_blocks*512` |
| inline files/bytes | inferred | `st_blocks == 0 && st_size > 0` |
| extent count | walker: **estimate, lower bound**; tree search: exact | walker: `ceil(size / 128 MiB)`, ignores fragmentation, compression (128 KiB extents), reflinks; tree search: count of `EXTENT_DATA` items |
| metadata bytes | **estimate** | item-size model below |

Model (logical bytes; every item pays a 25-byte leaf item header):

- inode item: 25 + 160
- per link: INODE_REF (25 + 10 + name) + DIR_ITEM (25 + 30 + name) + DIR_INDEX (25 + 30 + name)
- per regular extent: file extent item 25 + 53 (fs tree) and extent-tree item 25 + 24 + 29
- inline data: 25 + 21 + data bytes
- csum tree: about 1 byte per KiB of allocated data (crc32c, 4 B per 4 KiB). Separate tree, same metadata space.

Not modeled: xattrs/ACLs, leaf fill slack (leaves are not 100% packed), internal nodes, free-space
and block-group trees, nodatacow (no csums), DUP/RAID (on-disk is 2x logical for DUP).
The model's item sizes come from the btrfs on-disk format as I recall it and are unverified
against `btrfs inspect-internal dump-tree`; calibration (below) is the planned check.

Why not FIEMAP: it returns exact extent counts per file but costs one ioctl per file (millions
here). The tree-search backend below reads the same facts for the whole subvolume in a few
thousand large ioctls instead, at the price of needing root.

## Backends

`scan` consumes per-directory counters (`index::DirInfo`) from a `Backend` (`index.rs`). Everything
after collection (build-dir and worktree classification, duplicate clusters, reporting) is shared
and backend-independent, so a TUI or daemon can reuse either backend.

### Walker (no root) with incremental cache

Implemented in `walker.rs`, `cache.rs`.

- Traversal: custom thread pool (LIFO work stack, 3x CPUs capped at 64 because the work is I/O
  bound). Per directory: `fstatat(AT_FDCWD, path)` for inode and mtime; on a miss, `openat` with
  `O_RDONLY|O_DIRECTORY|O_NOFOLLOW|O_NOATIME` (falls back without `O_NOATIME` on EPERM, which the
  kernel returns for files you do not own), one `getdents` pass, and `fstatat(dirfd, name)` for each
  non-directory child. Subdirectories are identified by `d_type` and are not lstat'ed by their
  parent, so each directory costs exactly one lstat from its own work item.
- Cache (`~/.cache/metascope/walker.cache`, one file for all roots): a record per directory with
  inode, mtime (sec, nsec), listing time, the counters for its non-directory children, marker
  flags, and subdirectory names. A record is reused when inode and mtime match and it is younger
  than `--cache-max-age-hours` (default 24). A hit costs one lstat and no open/readdir/per-file
  lstat. Atomic replace on save (temp file + rename).
- What stays exact on a hit: entry names, entry counts, subdirectories, marker files. Creating,
  deleting or renaming an entry bumps the parent's mtime.
- What can go stale on a hit: size, allocated bytes and extents of files edited in place (a write
  does not change the directory mtime), and hard-link ownership. The max-age bound and `--rescan`
  limit this; counts that drive the ranking (inodes, dirs, entries) are unaffected.
- Racy timestamps: a directory modified within 2 s of being listed is stored as already expired,
  because a second change in the same timestamp tick would be invisible (same issue as git's racy
  index). The unit test `recently_modified_dirs_are_not_trusted` covers it.
- Truncated runs keep old records for unvisited directories; complete runs drop records under the
  scanned roots that were not seen (deleted directories).

Why not inotify to keep the index live: it needs one watch per directory. This machine has
`fs.inotify.max_user_watches` = 532,480 (VERIFIED: `/proc/sys/fs/inotify/max_user_watches`), and a
walk of `~/Programming` alone lists 413,720 directories today (VERIFIED, scan output below) with
about 610K reported for the full tree (coordinator's prototype). Raising the limit needs root and
costs kernel memory per watch (INFERRED). The cache's mtime check gets most of the benefit without
any watches.

### btrfs tree search (root)

Implemented in `btrfs_tree.rs`; **prototype**. Read-only: `BTRFS_IOC_TREE_SEARCH_V2` only searches.

- Needs `CAP_SYS_ADMIN`. Subvolume id comes from the `subvolid=` mount option; the mount root is
  assumed to be the subvolume root (not true for bind mounts of a subdirectory).
- Streams keys `(objectid, type, offset)` from `(256, 0, 0)` in 4 MiB batches (grows on
  `EOVERFLOW`), resuming at the successor of the last key. Modern kernels compare whole keys against
  the min/max, so resuming is exact (INFERRED from kernel source recollection; not exercised live).
- One pass suffices because an inode's items are contiguous and ordered
  INODE_ITEM < INODE_REF < INODE_EXTREF < XATTR < DIR_ITEM < DIR_INDEX < EXTENT_DATA.
- Exact outputs per directory: inode count by type, links (every INODE_REF/EXTREF, so hard links
  are visible), name bytes, size and `nbytes` from the inode item, regular/prealloc extent count
  (holes excluded), inline extent bytes. Paths come from parent inode plus ref name; orphans and
  other-subvolume stubs are dropped.
- Not counted: xattr items and their bytes, DIR_ITEM/DIR_INDEX item counts (the cost model derives
  them from links), the csum tree itself (still modeled as about 1 B per KiB).
- Cost shape (INFERRED): the tree is ordered by inode number, not path, so the pass always reads
  the whole subvolume and filters to the requested roots afterwards. It should beat the walker on
  whole-home or whole-fs scans and lose on a small subtree.
- Incremental option (not built, INFERRED): the search key has `min_transid`, so a later pass can
  ask only for items changed since the last generation. Deletions are invisible to that, so it
  needs an occasional full pass or the fanotify daemon below.
- Verified here: the ioctl request number is accepted by the running kernel (an unprivileged call
  returns EPERM, not ENOTTY/EINVAL), and framing, aggregation and path reconstruction pass unit
  tests on synthetic buffers. Not verified: any run against a real tree, because this session has no
  root (`sudo -n` asks for a password). Run `sudo metascope scan ~/Programming --backend btrfs` to
  get the real timing and exact counts.

### Optional later daemon: fanotify (needs root)

A small root daemon keeps the walker cache live, so a scan is nearly free. It is an optimization,
not a correctness dependency, because the cache is revalidated by mtime anyway.

- `fanotify_init(FAN_CLASS_NOTIF | FAN_REPORT_DFID_NAME)` with a `FAN_MARK_FILESYSTEM` mark on the
  mount: one mark covers every directory, avoiding the inotify per-directory watch limit.
  `FAN_REPORT_DFID_NAME` (kernel 5.9+; this kernel is 6.6) reports the parent directory's file
  handle plus the entry name for `FAN_CREATE`, `FAN_DELETE`, `FAN_MOVED_FROM`, `FAN_MOVED_TO` and
  `FAN_ATTRIB`; `FAN_CLOSE_WRITE` marks size changes without the storm that `FAN_MODIFY` causes.
- Needs `CAP_SYS_ADMIN` for the filesystem mark and `CAP_DAC_READ_SEARCH` for
  `open_by_handle_at` to turn the handle into a path. On btrfs each subvolume has its own `f_fsid`
  and handles are subvolume-relative, so multi-subvolume behavior must be tested (INFERRED risk).
- The daemon marks touched directories dirty in the cache (or re-lists just them). On
  `FAN_Q_OVERFLOW` it drops back to the mtime revalidation pass. It must ignore its own cache
  writes (excluded path or a bounded debounce).
- Alerting from the daemon reuses the state-change-only design in the roadmap.

### Attribution model

`scan` takes roots (default `~/Programming ~/code ~/dotfiles`, those that exist). There is no
gitignore filtering: ignored directories (`node_modules/`, `target/`) are the point. A backend
records only per-directory counters plus marker flags (`package.json`, `Cargo.toml`, `.git`
file). A pure pass over that map then classifies, so results do not depend on thread scheduling:

- **Build dirs** (rules from `disk-cleanup.py`): `node_modules` with a sibling `package.json`;
  `target` with a sibling `Cargo.toml`. Outermost match wins, and everything beneath it is counted
  to that category. `.git` directories are their own category.
- **Checkouts and worktrees**: a directory with a `.git` directory is a checkout. A `.git` file
  containing `gitdir: <main>/.git/worktrees/<name>` marks a linked worktree of `<main>`; any other
  `.git` file (submodules) is treated as a checkout. Each entry belongs to its nearest enclosing
  checkout, so a worktree nested under `<repo>/.claude/worktrees/` is not charged to `<repo>`.
  Rows are rolled up to a family per main repo (`--by repo`, default) or listed per checkout
  (`--by checkout`). Paths outside any checkout group by first component under the scan root.
- **Duplicate clusters**: build dirs of one kind whose sibling lockfile (`package-lock.json`,
  `npm-shrinkwrap.json`, `yarn.lock`, `pnpm-lock.yaml`, `bun.lock[b]`, `Cargo.lock`) has identical
  content. This is evidence of the same dependency set, not a tree comparison: member inode counts
  can differ (the output shows min..max). "Redundant" is total minus the largest member. Build dirs
  with no readable lockfile are never clustered.
- Each build root also carries `age_secs` (its own directory mtime).

Hard links are counted once per `(dev, ino)`; with parallel threads the owning directory of a
multiply-linked inode is whichever thread sees it first (matters for pnpm-style stores).
The default same-filesystem rule stops at subvolume boundaries (distinct `st_dev`, for example `/`
vs `/home`); `--cross-fs` lifts it, including onto non-btrfs mounts.

### Performance

All numbers are from this machine (24 CPUs, NVMe, kernel 6.6.128) on 2026-11-01 while a cleanup was
deleting `node_modules`, so tree sizes drift between sessions. Page cache was warm for every run
below; a cold-cache run needs `drop_caches` (root), so **cold timings are UNMEASURED here**.

| Run (`~/Programming`, release build) | Result |
|---|---|
| first single-threaded walker, `~/Programming`, cold-ish cache | 411k inodes in 292 s, then truncated |
| earlier `ignore`-crate parallel walk, 7.49M inodes, 64 threads | 98 s complete (tree since shrunk by cleanup) |
| current walker, `--no-cache`, 413,720 dirs, 3.4M inodes | walk 806 ms, total 2.2 s |
| current walker, first cached run (builds a 70 MB cache) | walk 1.44 s |
| current walker, warm cache, 0 dirs listed | walk 1.24 s, total 2.8 s |
| warm cache after adding and removing one directory | 1 dir listed, 413,719 cached, walk 1.25 s |
| btrfs tree search | UNMEASURED (needs root) |

Reading this honestly: with a warm page cache the cache is **not** faster than a plain walk
(0.8 s vs 1.2 s), because decoding 70 MB of records costs about as much as the kernel serving
cached `readdir`/`lstat`. The cache is a bet on cold I/O, where each skipped `readdir` and file
lstat is a random metadata read; that benefit is INFERRED from the earlier 85 s warm versus 17 min
cold figures for a Python walk (coordinator's measurements), not measured with this tool. The
2 s gap between "walk" and "total" is classification, lockfile hashing and sorting, and is the next
thing to optimize now that the walk is fast.

### Calibration plan

Sum scans of every subvolume and compare `total_metadata_cost` with
metadata `bytes_used`. The ratio is the unmodeled share (slack, xattrs, other trees). Apply it as a
documented scale factor only after it is measured on more than one machine state.

## Roadmap (designed, not built)

Cost classes: S small, M medium, L large. Blockers are named, not timed.

| Item | Class | Blocker |
|---|---|---|
| ratatui TUI over `ScanReport` (drill-down, sort toggle) | M | none; consume `metascope-core` only |
| Run and time the btrfs tree-search backend; add `min_transid` incremental passes | M | root session (sudo) to run it |
| fanotify daemon keeping the walker cache live (design above) | L | root service; owner decision on running a daemon |
| Guided remediation: `btrfs balance -dusage=0/5/10/20/40` escalation with `-dlimit`, idle IO class, skip if balance or scrub running | L | needs root path (polkit or sudo helper); owner decision; dry-run default |
| Alerting on state changes only (`notify-send`; ntfy push) | M | owner decision: ntfy topic/server and auth |
| Dead-man's-switch heartbeat | S | same ntfy decision |
| `btrfs device stats` error counters (sysfs `devinfo/<id>/error_stats`) | S | none |
| MCP server | S | owner decision whether wanted |

Rules for mutating features: dry-run by default, explicit flag to apply, never act while a balance
or scrub is running, and re-read the changed state afterwards.

## Cleaning

There is no `metascope clean`: cleaning lives in `tstapler/disk-cleanup`; this tool stays read-only.

## Streaming walk (`Backend::collect_streaming`)

`collect_streaming(&StreamRequest, &(dyn Fn(Vec<DirEvent>) + Sync)) -> io::Result<StreamSummary>` delivers the walk as
batches of per-directory events while it runs, so a UI can draw the first rows in the first second and stop the scan.
`collect`, `CollectRequest`, `Collected` and `DirInfo` are unchanged. A backend that implements only `collect` gets a default
method that replays its result (one `Dir` event per entry, ids dense from 0, parents before children, `dev` 0).

- **Events** (`#[non_exhaustive]`): `Dir`, `Unreadable { errno }`, `Boundary { dev }`, `Gone`, each with a dense `id`, the
  `parent` id the parent handed out when it queued the child (`NO_PARENT` for roots) and a name (full path for roots,
  basename otherwise). Every id handed out gets exactly one terminal event, unless the walk is stopped first.
  `Dir.info` is what `collect` returns for the same path; `from_cache` marks cache hits.
- **Batching**: each worker owns a batch behind its own mutex, handed to the sink at `batch_max` (256) events. One tick
  thread wakes every 25 ms and flushes batches whose oldest event is 50 ms old, so a worker blocked in a slow `readdir`
  cannot strand its parent event (worst wait about 75 ms). Sink calls never hold a batch lock.
- **Cancel**: `StreamRequest.cancel` is read next to the walker's `stop` flag; the tick thread turns it into `stop` and
  `notify_all`s idle workers. The end of every walk is one order: workers stop, the tick thread is told `done` and joined,
  the caller flushes every batch, the cache is saved if `StreamRequest.save` is still set (clearing it during the write
  aborts it through `save_atomic`'s `abort`), then the call returns. A truncated walk saves the visited records plus the old
  records of directories it did not reach, as `collect` does.
- **Paint**: with `paint_from_cache` the walker's own loaded cache map (one `cache::load` per run) is emitted first as
  `painted: true` events with ids from `PAINT_ID_BASE`; the live walk then emits `painted: false` events for the same
  directories. A painted `info` counts the directory's own inode but not its size, which the cache record does not keep.
- **Test seams**: `WalkHooks` (doc-hidden) replaces `lstat`, the directory listing and the cache loader and observes
  workers, so the 200,000- and 1,000,000-directory cases run without a disk.

### Baseline for the "cleaner is not slower" bound

`collect` over a generated 2,221-directory tree (20 x 10 x 10 directories with 5 small files each, tmpfs, 4 threads, no
cache, release build), median of 9 runs, three repetitions, harness `crates/scan/tests/walk_baseline.rs` (run with
`--ignored`):

| | repetition 1 | repetition 2 | repetition 3 |
|---|---|---|---|
| before the change (`052da1e`) | 12.42 ms | 12.67 ms | 12.48 ms |
| after the change | 13.01 ms | 12.57 ms | 12.51 ms |

Mean of the medians: 12.52 ms before, 12.70 ms after (+1.4%, inside the +5% bound and inside the run-to-run spread). This was
measured on the development machine only; the real-tree run and the Gate F harness (Story 6.5.6) are not done here.
