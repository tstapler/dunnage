# `--json` schema

Every `disk-cleanup --json` run writes exactly one JSON document to stdout (pretty-printed, one trailing
newline) and nothing else there. A usage error writes nothing to stdout. The document is never coloured and
never uses `~`, thousands separators or unit suffixes: bytes are exact integers, paths are absolute.

## Versioning rule

`schema_version` is `1`. Adding a field never bumps it. Removing, renaming or retyping a field does.
`crates/cli/tests/json_golden.rs` compares the key shape of each document with the golden files in
`crates/cli/tests/golden/` and fails when a key disappears without a bump. Regenerate the files only with
`cargo test -p disk-cleanup --test json_golden -- --ignored regen_golden`.

## Common keys

| key | type | meaning |
|---|---|---|
| `schema_version` | integer | `1` |
| `mode` | string | `plan`, `apply`, `status` or `kinds` |
| `exit_code` | integer | the exit code of the run, so a consumer that keeps only stdout sees it |
| `errors` | array | `{code, path, message}`; `code` is a stable lower-case id (`eacces`, `enospc`, `lock_held`, `audit_unwritable`, `roots_unreadable`, `starved`, `state_unreadable`, `command_failed`, `plan_id_missing`, `plan_id_unknown`, `plan_id_stale`, `plan_id_changed`, `plan_id_selection_differs`) |

## `plan` and `apply`

`plan_id` (8 hex), `partial_scan`, `items[]`, `totals`, `next[]`, `fs{before,after}`, `notes[]`, `errors[]`.
A plan document also has `selection` (`only[]`: the `--only` values, sorted, lossy UTF-8; `only_hex[]`, parallel,
only when one is not valid UTF-8), `timings` (`{discover_ms, plan_ms, discover_by_kind_ms}`, the two stages of the run
and, per selector id plus `fs_snapshot`, `availability` and `plan_notes`, the milliseconds spent in each part of
discovery; `scripts/bench-dry-run.sh` prints them on a budget miss) and `plan_saved` (false when `last_plan` could not
be saved; the destructive `next[]`
entries are then absent).

An item: `kind` (exact, kebab-case), `selector`, `family`, `id`, `path` (lossy UTF-8; `null` for docker and
btrfs objects, which carry `name`), `path_hex` (only when the name is not valid UTF-8), `tier`
(`safe|guarded|destructive`), `status` (`would_free|freed|skipped|error|risk_not_selected`), `est_bytes`,
`measured_bytes` (`null` in plan mode), `elapsed_ms`, `reason` (the `SkipReason` kebab id), `reason_text`,
`next_action`.

`totals`: `estimated_bytes`, `attributed_bytes`, `measured_drop_bytes`, `unattributed_bytes`,
`attribution_pct`. The last three are `null` in plan mode and whenever the drop was not measured; an apply
document adds `freed_bytes` and a `summary` object.

`next[]`: `{command, plan_id, explain}`. `plan_id` is set exactly when the command carries `--plan-id`; a command
that echoes `--only` carries it last, shell-quoted.

## `status` and `kinds`

`status` is a superset of the old `metascope status --json`: `fs` (the `/sys/fs/btrfs/<uuid>` directory, null off btrfs),
`allocation`, `unallocated_bytes`, `metadata_headroom_bytes`, `verdict` (`ok|warn|crit|unknown`) and `findings[]` keep
their old values; `path`, `filesystem` (`{before, after}` df snapshots), `notes[]` and `errors[]` are added. Exit 0, 1
and 2 are ok, warn and crit; 74 is an unreadable state (`verdict: "unknown"`, `errors[0].code` `state_unreadable`).
`audit verify --json`: `deletions`, `runs`, `violations[]` of `{run_id, kind, path, reason}`, `unfinished_runs[]`. `kinds`: `kinds[]` of `{id, family, tier,
default, available, min_age, touches}`.

## `explore --json`

One document after the scan ran to completion (a partial scan still exits 0 and sets `partial: true`):
`schema_version` 1, `mode: "explore"`, `roots[]` (`path`, `path_hex` only for a non-UTF-8 name, `usage_bytes`,
`apparent_bytes`, `items`, `at_least`), `filesystem` (`verdict`, `avail_bytes`, and `unallocated_bytes`,
`data_slack_bytes`, `meta_slack_bytes`, which are `null` off btrfs), `entries[]`, `exit_code`, `errors[]`.
An entry is `path`, `path_hex`, `usage_bytes`, `apparent_bytes`, `meta_est_bytes` (`null` off btrfs),
`shared_est_bytes` (always `null`: the shared-extent estimate exists only in the interactive view),
`newest_dir_mtime` (Unix seconds or `null`), `items`, `share_of_parent` (float or `null`),
`state` (`done|at_least|unreadable|other_fs`), `kind` (a cleaner kind id or `null`) and `errors[]` of `{errno}`.
The rows the text view prints and the rows of the interactive view carry the same numbers. A synthetic row
(`(N files)`, `(N smaller entries)`, `(empty directory)`) adds `synthetic` and `name`. Additive keys never bump
the version. A SIGINT during the scan prints no document and exits 130.

## Reading the report by column

```
disk-cleanup --json | jq -r '.items[] | [.status, .kind, .est_bytes, .path] | @tsv'
```
