# Security policy

dunnage deletes files, so its path guards are the security boundary. These count as security bugs:

- a deletion outside the per-kind allowed roots (`ci/allowed-roots.txt`),
- following a symlink or crossing a filesystem during a delete,
- an `--apply` that deleted a destructive kind without a matching `--plan-id`,
- a binary that reaches users with the `test-support` feature compiled in.

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
