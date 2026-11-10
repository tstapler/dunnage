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
A plan document also has `plan_saved` (false when `last_plan` could not be saved; the destructive `next[]`
entries are then absent).

An item: `kind` (exact, kebab-case), `selector`, `family`, `id`, `path` (lossy UTF-8; `null` for docker and
btrfs objects, which carry `name`), `path_hex` (only when the name is not valid UTF-8), `tier`
(`safe|guarded|destructive`), `status` (`would_free|freed|skipped|error|risk_not_selected`), `est_bytes`,
`measured_bytes` (`null` in plan mode), `elapsed_ms`, `reason` (the `SkipReason` kebab id), `reason_text`,
`next_action`.

`totals`: `estimated_bytes`, `attributed_bytes`, `measured_drop_bytes`, `unattributed_bytes`,
`attribution_pct`. The last three are `null` in plan mode and whenever the drop was not measured; an apply
document adds `freed_bytes` and a `summary` object.

`next[]`: `{command, plan_id, explain}`. `plan_id` is set exactly when the command carries `--plan-id`.

## `status` and `kinds`

`status`: `path`, `verdict` (`ok|warn|crit|unknown`), `fs`. `kinds`: `kinds[]` of `{id, family, tier,
default, available, min_age, touches}`.

## Reading the report by column

```
disk-cleanup --json | jq -r '.items[] | [.status, .kind, .est_bytes, .path] | @tsv'
```
