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
    and application trees `/usr`, `/etc`, `/boot`, `/bin`, `/sbin`, `/lib*`, `/dev`, `/proc`, `/sys`, `/run`,
    `/System`, `/Library`, `/opt`, `/nix`, `/snap`, `/efi`, `/lost+found`, `/Applications`. Programs installed there
    are not yours to guess at; mark a folder in `/srv`, `/mnt`, `/media` or `/tmp` instead. Inside `/var`, only
    folders below `/var/tmp`, `/var/cache` and `/var/folders` (macOS temp and cache) can be deleted; `/var/lib`,
    `/var/log` and the like hold state that is not yours to guess at. On macOS `/private/etc`, `/private/var` and
    `/private/tmp` count as `/etc`, `/var` and `/tmp`. In `/tmp`, `/tmp/ssh-*` (ssh-agent sockets) and
    `/tmp/.X11-unix` (the X server's sockets) are refused too.
  - `$HOME` itself, every ancestor of it (also by identity, so a bind-mounted alias is refused), any folder
    directly inside it (fewer than two components below `$HOME`, rule F), and everything that belongs to another
    account: any path below `/home/<user>`, `/Users/<user>`, `/var/home/<user>` or `/root` that is not below the real
    `$HOME` (the real `$HOME` is the canonical one, whatever spelling the shell had; when `$HOME` is `/root` or
    `/home/<user>`, its own tree is judged like any home). A `dunnage` run as root (or with
    `sudo`) could otherwise delete another account's files; the explorer is for your own.
  - Credentials, keys and browser profiles below `$HOME`, everything below them and every folder that holds one:
    `~/.ssh`, `~/.gnupg`, `~/.password-store`, `~/.aws`, `~/.kube`, `~/.1password` (1Password CLI and agent state),
    `~/.docker`, `~/.azure`, `~/.thunderbird`, `~/.local/share/kwalletd`, `~/.mozilla` (Firefox profiles hold cookies and saved logins), `~/.config/{1Password,op,gh,gcloud,Bitwarden,sops,
    gnupg,google-chrome,chromium,BraveSoftware,vivaldi,keepassxc,rclone}`, `~/.local/share/{keyrings,gnupg}`, `~/snap/1password`,
    `~/.var/app/com.1password.1Password` and, on macOS, `~/Library/Keychains` and the 1Password group-container,
    application-support and container folders. Chosen conservatively: each is a place where losing a file means
    losing a secret or a login that cannot be re-created by a rebuild, and the cost of a refusal is renaming the
    folder you meant. `XDG_CONFIG_HOME` and `XDG_DATA_HOME` are honoured when they name a folder strictly below
    `$HOME`: the same names are refused below them. Other browsers' profiles (Edge, Opera) and
    application data not listed here are not on the floor.
  - any path with a `.git` component, a scan root itself, the tool's own state, cache and config folders (and the
    folders that hold them), mount points, other filesystems and anything that is not a plain absolute UTF-8
    path (`..`, `.`, `//`, a trailing `/`, a non-UTF-8 name). Symlinks are never followed.

  Second paths to the same folder. The list above names paths, so on its own it would miss a secret directory
  reached by another path (a symlinked `~/.ssh`, a bind mount, a btrfs top-level mount such as
  `/mnt/top/@home/u/.ssh/x`). `RootSet` therefore resolves each secret directory that exists to its
  `(device, inode)` once (following links), and the plan and the apply refuse a folder when it, or any directory on
  the way to it, has that identity; a folder that holds a secret directory (`~/.local/share`) is refused by its own
  identity; the tool's own state, cache and config folders are compared the same way. The plan reads the chain with
  `lstat`; the apply reads it from the directories it opens (`fstat` of each, `O_NOFOLLOW`), so it is the same
  check on what is opened, not on what the path says. Limits: a secret directory created after the roots were
  resolved is not known by identity (the lexical list still names it); `XDG_DATA_HOME` is lexical only; and a
  file-level hard link or a bind mount of a single file is not a directory and is out of scope, as everywhere in the
  explorer.

  Known gaps: a bind mount of a sub-directory of a secret directory, and any FUSE or network re-export (new inode numbers), are not recognised by identity; `XDG_*_HOME` outside `$HOME` is ignored; the lexical account rule treats `/home/linuxbrew` and `/Users/Shared` as other accounts; the agent-socket rule covers `/tmp/ssh-*` only.

  Limits of the floor. Names are matched ASCII case-insensitively (macOS volumes are case-insensitive), not by
  Unicode normalisation. A folder full of work is not on the floor: the screen says `git repository: unpushed work
  is lost` but does not block it. The audit oracle (`dunnage audit verify`) applies the same lexical floor to
  `explorer-selection` rows, against the canonical `$HOME` (as the engine resolves it); it cannot test identity
  (the folder may be gone), and it no longer needs the row to be below a configured root, so it does not flag an
  explorer delete outside the configured roots as such. `ci/allowed-roots.txt` is unchanged for the explorer: the
  cleaner kinds' rows keep their roots. Its `cargo-home` rows now name the cargo home: `~/.cargo`, or the
  `CARGO_HOME` of the shell that runs `audit verify` when that is a clean folder strictly below `$HOME`
  (`under-cargo-home`); a `registry/src/<index>/<crate>` below any other folder is a violation.
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
