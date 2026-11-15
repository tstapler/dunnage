# Security policy

dunnage deletes files, so its path guards are the security boundary. These count as security bugs:

- a deletion outside the per-kind allowed roots (`ci/allowed-roots.txt`), or, for `dunnage explore`, a deletion of anything on the explorer's hard floor (below),
- following a symlink or crossing a filesystem during a delete,
- an `--apply` that deleted a destructive kind without a matching `--plan-id`,
- a binary that reaches users with the `test-support` feature compiled in.

## Known limits

- **The explorer is not limited to the cleaner's roots.** The cleaner kinds (caches, build directories, worktrees,
  Steam, ...) stay inside their narrow roots. In `dunnage explore`, you can mark and delete any folder that you may
  delete and that is on one filesystem with the folder the delete starts from (the scan root that holds it, else
  the mount point that holds it), except the hard floor below. Everything else about an explorer delete is
  unchanged: dry-run view, plan id, typed count, audit `intent` before each delete, fd-relative deletes that refuse
  symlinks and never cross a filesystem, identity and birth-time checks, and a re-check right before each delete.
  The floor, never markable and shown with a reason (one function, `explorer_floor` in
  `crates/core/src/explorer_floor.rs`, used by the screen, the planner, the engine and `audit verify`):
  - `/`, every direct child of `/` (`/home`, `/var`, `/tmp`, `/srv`, `/opt`, `/mnt`, `/Users`, ...), and the system
    trees `/usr`, `/etc`, `/boot`, `/bin`, `/sbin`, `/lib*`, `/dev`, `/proc`, `/sys`, `/run`, `/System`, `/Library`.
    Inside `/var`, only folders below `/var/tmp`, `/var/cache` and `/var/folders` (macOS temp and cache) can be
    deleted; `/var/lib`, `/var/log` and the like hold state that is not yours to guess at. On macOS
    `/private/etc`, `/private/var` and `/private/tmp` count as `/etc`, `/var` and `/tmp`.
  - `$HOME` itself, every ancestor of it (also by identity, so a bind-mounted alias is refused), any folder
    directly inside it (fewer than two components below `$HOME`, rule F), and other accounts' home directories
    (`/home/<user>`, `/Users/<user>`, `/var/home/<user>`).
  - `~/.ssh`, `~/.gnupg`, `~/.1password` and `~/.config/1Password` (the 1Password agent socket and app state),
    `~/.config/op`, the macOS 1Password group container and application support folders, everything below them
    and every folder that holds one.
  - any path with a `.git` component, a scan root itself, the tool's own state, cache and config folders (and the
    folders that hold them), mount points, other filesystems and anything that is not a plain absolute UTF-8
    path (`..`, `.`, `//`, a trailing `/`, a non-UTF-8 name). Symlinks are never followed.

  Limits of the floor. It is lexical, so it names paths, not inodes (except `$HOME` and its ancestors): a directory
  that is bind-mounted at a second path is refused at the mount point but not recognised below it. Names are
  matched ASCII case-insensitively (macOS volumes are case-insensitive), not by Unicode normalisation. A folder
  full of work is not on the floor: the screen says `git repository: unpushed work is lost` but does not block it.
  The audit oracle (`dunnage audit verify`) applies the same floor to `explorer-selection` rows; it no longer
  needs the row to be below a configured root, and `ci/allowed-roots.txt` is unchanged because the cleaner kinds'
  rows keep their roots.
- **Recreated directories.** The explorer binds each reviewed item to `(device, inode, file type, birth time)` and
  hashes all four into the plan id, so a directory deleted and recreated under the same path after review is skipped
  as changed, even where the filesystem hands the old inode number back (ext4 does). The birth time is the kernel's
  `statx` `stx_btime`. Two residual windows: where the filesystem or platform reports no birth time (some
  filesystems; the delete path's own `stat` on macOS), only `(device, inode, file type)` is compared, as before; and
  a directory recreated within one kernel timer tick (a few milliseconds) of the original's removal can get the same
  inode and the same birth time. Neither is a window a review-then-apply gap falls into. The check is one-sided
  on purpose: if either the plan or the fresh read has no birth time, the birth time is not compared.

## Reporting

Do not open a public issue. Report privately to **TODO-OWNER: security contact address** (or use GitHub's
"Report a vulnerability" button under the repository's Security tab once the owner has enabled private
vulnerability reporting).

Include the version (`dunnage --version`), the command line, and the audit log lines
(`$XDG_STATE_HOME/dunnage/audit.jsonl`) if a deletion was involved.

## Supported versions

Only the latest release receives fixes.

## Response

TODO-OWNER: state a response-time expectation you can keep (for example "acknowledged within 7 days").
