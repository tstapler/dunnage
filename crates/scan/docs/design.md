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
| extent count | **estimate, lower bound** | `ceil(size / 128 MiB)`; ignores fragmentation, compression (128 KiB extents), reflinks |
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

Why not FIEMAP / `compsize`: FIEMAP returns exact extent counts per file but costs one ioctl per
file (hundreds of thousands here) and needs a libc dependency; `compsize`/`btrfs inspect-internal
dump-tree` need root. v0 trades accuracy for a root-free, single-pass walk. FIEMAP is the next
accuracy step, applied only to the top buckets.

### Attribution model

`scan` takes roots (default `~/Programming ~/code ~/dotfiles`, those that exist) and walks them with
the `ignore` crate's parallel walker. Gitignore filters are off by default because they hide
`node_modules/` and `target/`; `--respect-gitignore` turns them on. The walk records only
per-directory counters plus marker flags (`package.json`, `Cargo.toml`, `.git` file). A pure pass
over that map then classifies, so results do not depend on thread scheduling:

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
- Each build root also carries `age_secs` (its own directory mtime), for the future `clean` guard.

Hard links are counted once per `(dev, ino)`; with parallel threads the owning directory of a
multiply-linked inode is whichever thread sees it first (matters for pnpm-style stores).
The default same-filesystem rule stops at subvolume boundaries (distinct `st_dev`, for example `/`
vs `/home`); `--cross-fs` lifts it, including onto non-btrfs mounts.

### Performance

The walk is I/O-bound (one `lstat` per entry, random fs-tree reads), so the default thread count is
3x CPUs capped at 64, not CPU count. Measured on this machine (24 CPUs) during the incident, with
a concurrent cleanup deleting files, so inode counts drifted between runs:

| Run | Result |
|---|---|
| prior single-threaded walk, `~/Programming`, cold cache | about 411k inodes in 292 s, truncated |
| parallel, 16 threads vs 64 threads, 60 s cap, same tree | 67k vs 94k inodes/s |
| parallel, 64 threads, `~/Programming` | 7.49M inodes complete in 98 s |
| parallel, default roots, warm cache | 8.96M inodes in 14 s |

The Python prototype's discovery phase reportedly took about 1000 s on a cold cache (reported by
the coordinator, not re-run here). Cold-cache numbers depend on what the page cache holds.

### Calibration plan

Sum scans of every subvolume and compare `total_metadata_cost` with
metadata `bytes_used`. The ratio is the unmodeled share (slack, xattrs, other trees). Apply it as a
documented scale factor only after it is measured on more than one machine state.

## Roadmap (designed, not built)

Cost classes: S small, M medium, L large. Blockers are named, not timed.

| Item | Class | Blocker |
|---|---|---|
| ratatui TUI over `ScanReport` (drill-down, sort toggle) | M | none; consume `metascope-core` only |
| FIEMAP extent counts for top buckets | S | libc dependency decision |
| Guided remediation: `btrfs balance -dusage=0/5/10/20/40` escalation with `-dlimit`, idle IO class, skip if balance or scrub running | L | needs root path (polkit or sudo helper); owner decision; dry-run default |
| Alerting on state changes only (`notify-send`; ntfy push) | M | owner decision: ntfy topic/server and auth |
| Dead-man's-switch heartbeat | S | same ntfy decision |
| `btrfs device stats` error counters (sysfs `devinfo/<id>/error_stats`) | S | none |
| Git-worktree-aware cleanup (stale > 14 d since last commit, skip dirty/locked, `git worktree remove`, `target/`, `node_modules`) | L | port the logic in `dotfiles/stapler-scripts/disk-cleanup`; confirmation UX |
| Cache cleanup (npm, cargo registry, go build, gradle, pip, docker prune) | M | per-cache safety review |
| `clean` subcommand (below) | L | port the discovery rules; owner review of the guard list |
| MCP server | S | owner decision whether wanted |

Rules for mutating features: dry-run by default, explicit flag to apply, never act while a balance
or scrub is running, and re-read the changed state afterwards.

## `clean` subcommand (design only, not implemented)

Purpose: reclaim metadata by deleting rebuildable build dirs, newest-duplicate-first, using the
same classification as `scan`. Replaces `stapler-scripts/disk-cleanup` and the Python prototype.

```
metascope clean [PATHS...] [--kinds node-modules,cargo-target,worktrees] [--min-age 60m]
                [--keep-newest N] [--only-duplicates] [--apply] [--json]
```

- **Dry run by default.** Without `--apply` it prints what would be removed with inode and
  estimated-metadata savings, and exits 0. `--apply` is the only way anything is deleted; there is
  no `--force` shorthand and no interactive "yes to all".
- **Selection** (reuses `scan::BuildRoot`): a `node_modules` with a sibling `package.json`, or a
  `target` with a sibling `Cargo.toml`, and nothing else. A path is never deleted just for being
  named `target`.
- **Guards, all checked again immediately before each delete** (the scan can be minutes old):
  - skip build dirs whose mtime is newer than `--min-age` (default 60 minutes, from the prototype);
  - skip if the owning checkout has a running build (a process holds a file open under it, or
    `Cargo.lock`/`.cargo-lock` is held);
  - skip worktrees with uncommitted changes or a `locked` marker (logic from `disk-cleanup`);
  - refuse paths that resolve outside the scan roots, symlinks, and other filesystems;
  - never touch `.git`, source files, or lockfiles.
- **Duplicate-aware ordering:** with `--only-duplicates`, keep the `--keep-newest N` (default 1)
  members of each lockfile cluster and delete the rest, biggest redundancy first.
- **Worktree removal** is a separate kind: `git worktree remove` (not `rm -rf`), only for clean
  worktrees older than 14 days since last commit, and `git worktree prune` afterwards.
- **Execution:** parallel unlink per build dir, deepest-first; per-path result recorded (removed,
  skipped with reason, error). Stop and report on the first `ENOSPC`-style failure rather than
  retrying in a loop, since deletes on a metadata-full btrfs can themselves fail until a
  transaction commits.
- **Verification:** after `--apply`, re-read `status` and print before/after unallocated and
  metadata `bytes_used`. btrfs frees deleted metadata at transaction commit and only returns
  chunks to unallocated via balance, so a successful delete does not by itself raise unallocated;
  the output says so and suggests the remediation step instead of claiming success.
- **Audit:** `--json` and an append-only log under `$XDG_STATE_HOME/metascope/clean.jsonl`.
- **Tests:** guard predicates are pure functions over a scan report and are unit-tested; deletion
  runs only against a tempdir fixture in tests.
