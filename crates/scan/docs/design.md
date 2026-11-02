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

Ignore handling: default honors `.gitignore`/`.ignore` (the `ignore` crate), which hides
`node_modules/` and `target/`. `--include-ignored` counts everything. Subvolume boundaries have a
distinct `st_dev`, so the default same-filesystem rule also stops at subvolumes (for example `/`
vs `/home`); `--cross-fs` lifts it, including onto non-btrfs mounts.

### Calibration plan

Sum scans of every subvolume with `--include-ignored` and compare `total_metadata_cost` with
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
| MCP server | S | owner decision whether wanted |

Rules for mutating features: dry-run by default, explicit flag to apply, never act while a balance
or scrub is running, and re-read the changed state afterwards.
