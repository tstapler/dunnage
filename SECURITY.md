# Security policy

dunnage deletes files, so its path guards are the security boundary. These count as security bugs:

- a deletion outside the per-kind allowed roots (`ci/allowed-roots.txt`),
- following a symlink or crossing a filesystem during a delete,
- an `--apply` that deleted a destructive kind without a matching `--plan-id`,
- a binary that reaches users with the `test-support` feature compiled in.

## Known limits

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
