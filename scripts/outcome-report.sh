#!/usr/bin/env bash
# outcome-report.sh [--audit F] [--outcome F] [--wrapper-log F] [--cutover YYYY-MM-DD] [--bin PATH] | --selftest
# Reads the state directory of dunnage and prints the evidence the owner needs at Gates B, C and D:
# SM-2 (the first three qualifying real runs, classified from the recorded run_end), O-1, the reach table
# (CV-1), O-2 and O-3. It reports and never acts: every outcome, a miss included, exits 0 (2 means a file or a
# tool it needs is missing or unparsable). Classification reads run_end.{attribution_pct,causes,explained},
# never the summary's prose. ADR-004 (amended 2026-11-10), Story 2.3.5.
# shellcheck disable=SC2016,SC2001,SC2086
set -u -o pipefail

MIN_DROP=5368709120   # 5 GiB: below this, concurrent writes can exceed the 5% tolerance (triad 3, P-j)

state_dir=${XDG_STATE_HOME:-${HOME:-.}/.local/state}/dunnage

JQ_LIB='
def r1: (. * 10 | round) as $t | "\(($t / 10) | floor).\($t % 10)";
def pct: if . < 0 then "-" + ((-. ) | r1) else r1 end;
def gib: if . < 0 then "-" + ((-. / 1073741824) | r1) + " GiB" else ((. / 1073741824) | r1) + " GiB" end;
def sgib: (if . < 0 then "-" else "+" end) + ((if . < 0 then -. else . end) / 1073741824 | r1) + " GiB";
def day: strftime("%Y-%m-%d");
def klass:
  if .pct == null then "FAIL"
  elif .pct >= 95 and .pct <= 105 then "PASS"
  elif .explained then "EXPLAINED"
  elif (.pct >= 90 and .pct < 95) or (.pct > 105 and .pct <= 110) then "AMBER"
  else "FAIL" end;
'

# finished runs of an audit log, oldest first; each carries its result lines
RUNS_JQ='
  ($a | map(select(.type == "run_start"))) as $s
  | ($a | map(select(.type == "run_end"))) as $e
  | ($a | map(select(.type == "result")) | group_by(.run_id)
     | map({key: .[0].run_id, value: .}) | from_entries) as $res
  | [ $e[] as $r
      | ($s | map(select(.run_id == $r.run_id)) | .[0]) as $st
      | { id: $r.run_id, ts: $r.ts, drop: $r.measured_drop, pct: $r.attribution_pct,
          explained: ($r.explained // false), causes: ($r.causes // []),
          vstart: ($st.verdict_start // null), vend: $r.verdict_after,
          kinds: ($st.kinds // ""), results: ($res[$r.run_id] // []) } ]
  | sort_by(.ts)
'

fail() { echo "outcome-report: $*" >&2; exit 2; }

# $1 audit path -> JSON array of finished runs on stdout
load_runs() {
  if [ ! -r "$1" ]; then echo '[]'; return 0; fi
  jq -Rn "[inputs | fromjson?] as \$a | $RUNS_JQ" "$1" || fail "cannot parse $1"
}

section_sm2() { # $1 runs json, $2 flags file, $3 plan binary
  local runs=$1 flags=$2 bin=$3 n pass
  n=$(jq "$JQ_LIB"' [ .[] | select((.drop // 0) >= '"$MIN_DROP"') ] | length' <<<"$runs")
  jq -r "$JQ_LIB
    def cause: \"\(.kind) \(.bytes | sgib) (\(.source))\";
    . as \$all
    | reduce .[] as \$r ({n: 0, out: []};
        if (\$r.drop // 0) >= $MIN_DROP then
          .n += 1
          | if .n <= 3 then
              .out += [ \"run \(\$r.id) \(\$r.ts | day): \(\$r | klass) \(if \$r.pct == null then \"n/a\" else (\$r.pct | pct) + \"%\" end) of a \(\$r.drop | gib) drop; causes: \" + (if (\$r.causes | length) == 0 then \"none recorded\" else (\$r.causes | map(cause) | join(\"; \")) end) ]
              + (if (\$r | klass) == \"FAIL\" then
                  [ \"  FAIL-and-investigate: the gate stays open. Compare this run's audit log and its shared_est_bytes (the FIEMAP-aware attribution of Story 1.2.2, ADR-004 as amended 2026-11-10) with the counter drop and record the finding in spikes.md.\" ]
                 else [] end)
            else .out += [ \"run \(\$r.id) \(\$r.ts | day): \(\$r | klass) \(if \$r.pct == null then \"n/a\" else (\$r.pct | pct) + \"%\" end) (qualifying, after the fixed set of three: not counted)\" ] end
        elif \$r.drop == null then
          .out += [ \"run \(\$r.id) \(\$r.ts | day): no measured drop recorded: not counted\" ]
        else
          .out += [ \"run \(\$r.id) \(\$r.ts | day): TOO SMALL (\(\$r.drop | gib), minimum 5 GiB): not counted\" ]
        end)
    | .out[]" <<<"$runs"
  pass=$(jq "$JQ_LIB"' [ .[] | select((.drop // 0) >= '"$MIN_DROP"') ] | .[0:3] | map(select(klass == "PASS")) | length' <<<"$runs")
  if [ "$pass" -ge 1 ]; then
    echo "SM-2: met ($pass of the first 3 qualifying runs PASS; an EXPLAINED run does not count on its own)"
  elif [ "$n" -ge 3 ]; then
    echo "SM-2: not met (no PASS among the first 3 qualifying runs; an EXPLAINED run does not count on its own)"
    echo mm >>"$flags"; echo SM-2 >>"$flags.names"
  else
    echo "SM-2: pending ($n of 3 qualifying runs recorded, none PASS yet)"
  fi
  : "$bin"
}

section_o1() { # runs flags bin
  local runs=$1 flags=$2 bin=$3 tot ok not_counted plan
  tot=$(jq '[ .[] | select(.vstart == "warn" or .vstart == "crit") ] | .[0:3] | length' <<<"$runs")
  if [ "$tot" -eq 0 ]; then
    echo "O-1: not measurable (no run started WARN or CRIT)"
    return 0
  fi
  ok=$(jq '[ .[] | select(.vstart == "warn" or .vstart == "crit") ] | .[0:3] | map(select(.vend == "ok")) | length' <<<"$runs")
  not_counted=$(jq '[ .[] | select(.vstart == "warn" or .vstart == "crit" | not) ] | length' <<<"$runs")
  local tail=""
  if [ "$not_counted" -eq 1 ]; then tail=" (1 run started OK and is not counted)"
  elif [ "$not_counted" -gt 1 ]; then tail=" ($not_counted runs started OK and are not counted)"; fi
  echo "O-1: $ok of $tot qualifying runs ended OK$tail"
  [ "$ok" -lt "$tot" ] || return 0
  echo mm >>"$flags"; echo O-1 >>"$flags.names"
  # the three largest unselected RISK rows of a fresh dry run, once, for every qualifying run that did not end OK
  local bad
  bad=$(jq -r '[ .[] | select(.vstart == "warn" or .vstart == "crit") ] | .[0:3] | map(select(.vend != "ok") | .id) | .[]' <<<"$runs")
  plan=$("$bin" --json 2>/dev/null) || plan=""
  while read -r id; do
    [ -n "$id" ] || continue
    echo "  run $id did not end OK; the next consumers (largest unselected RISK rows):"
    if [ -z "$plan" ] || ! jq -e . >/dev/null 2>&1 <<<"$plan"; then
      echo "    (no readable dry-run plan from '$bin --json')"
      continue
    fi
    jq -r "$JQ_LIB"' [ .items[]? | select(.status == "risk_not_selected") ] | sort_by(-(.est_bytes // 0)) | .[0:3]
      | if length == 0 then "    (no risk_not_selected rows in the plan)"
        else .[] | "    \(.selector // .kind) \(.path // .name // "-") \((.est_bytes // 0) | gib)" end' <<<"$plan"
  done <<<"$bad"
}

section_cv1() { # runs flags
  local runs=$1 flags=$2 run
  run=$(jq '[ .[] | select(
      ((.kinds | split(",") | map(select(. != "caches" and . != "cache-apt" and . != "")) | length) > 0)
      or ([ .results[] | select(.selector != "caches" and .selector != "cache-apt") ] | length) > 0) ] | .[0] // empty' <<<"$runs")
  if [ -z "$run" ]; then
    echo "CV-1: no run has selected the new kinds yet"
    return 0
  fi
  jq -r "$JQ_LIB"'
    . as $r
    | ($r.drop // 0) as $d
    | ([ $r.results[] | select(.status == "removed") ] | group_by(.selector)
       | map({sel: .[0].selector, meas: (map(.measured_bytes // 0) | add), est: (map(.est_bytes // 0) | add)})) as $k
    | "Reach of run \($r.id) (share of the measured drop \($d | gib)):",
      ($k[] | "  \(.sel) \(.meas | gib) (est \(.est | gib)) \(if $d > 0 then (.meas * 100 / $d | pct) + "%" else "n/a" end) of the drop"),
      "  bash baseline 4.6% (11 of 241 GiB)",
      ([ $k[] | select(.sel != "caches" and .sel != "cache-apt") | .meas ] | add // 0) as $new
      | "  new kinds: \(if $d > 0 then ($new * 100 / $d | pct) + "%" else "n/a" end) of the drop"' <<<"$run"
  local share
  share=$(jq -r '
    (.drop // 0) as $d
    | ([ .results[] | select(.status == "removed" and .selector != "caches" and .selector != "cache-apt") | (.measured_bytes // 0) ] | add // 0) as $new
    | if $d > 0 then ($new * 1000 / $d | round) / 10 else empty end' <<<"$run")
  if [ -n "$share" ] && awk -v s="$share" 'BEGIN { exit !(s < 25) }'; then
    echo "CV-1: new kinds $(jq -r "$JQ_LIB"'(.drop) as $d | ([ .results[] | select(.status == "removed" and .selector != "caches" and .selector != "cache-apt") | (.measured_bytes // 0) ] | add // 0) * 100 / $d | pct' <<<"$run")%, under the 25% line: record an owner decision at the Gate D review"
    echo mm >>"$flags"; echo CV-1 >>"$flags.names"
  fi
}

section_o2() { # wrapper-log outcome cutover
  local wlog=$1 outc=$2 cutover=$3 start now notes direct window=""
  if [ -z "$cutover" ]; then echo "O-2: no cutover date given (--cutover YYYY-MM-DD)"; return 0; fi
  start=$(date -u -d "$cutover" +%s 2>/dev/null) || fail "bad --cutover '$cutover'"
  now=${OUTCOME_REPORT_NOW:-$(date +%s)}
  [ "$now" -ge $((start + 31 * 86400)) ] || window=" (window still open)"
  notes=0
  if [ -r "$wlog" ]; then
    notes=$(while read -r iso _ kind; do
      [ -n "${iso:-}" ] || continue
      case $kind in note | retired) ;; *) continue ;; esac
      t=$(date -u -d "$iso" +%s 2>/dev/null) || continue
      d=$(((t - start) / 86400)); [ "$t" -ge "$start" ] || continue
      [ "$d" -ge 21 ] && [ "$d" -le 30 ] && echo x
    done <"$wlog" | wc -l)
  fi
  direct=0
  if [ -n "$outc" ]; then
    direct=$(jq -Rn --argjson s "$start" '[ inputs | fromjson? | select(.via == null and .ts != null)
      | select(((.ts - $s) / 86400 | floor) >= 21 and ((.ts - $s) / 86400 | floor) <= 30) ] | length' <<<"$outc") || direct=0
  fi
  if [ "$notes" -gt 0 ]; then
    local w=calls; [ "$notes" -eq 1 ] && w=call
    echo "O-2: $notes NOTE $w in days 21-30 (target 0), $direct direct uses (minimum 5)$window"
  elif [ "$direct" -lt 5 ]; then
    echo "O-2: not measurable ($direct direct uses in days 21-30, minimum 5)$window"
  else
    echo "O-2: met$window"
  fi
}

section_o3() { # outcome-content
  local outc=$1 out
  if [ -z "$outc" ]; then echo "O-3: no record"; return 0; fi
  out=$(jq -Rn '[ inputs | fromjson? | select(.ts != null and .verdict != null) ] | sort_by(.ts) as $l
    | if ($l | length) == 0 then "O-3: no record" else
      ($l[0].ts / 86400 | floor) as $d0
      | [ $l[] | . + {day: (((.ts / 86400 | floor) - $d0) + 1)} | select(.day <= 30) ] as $w
      | ($w | group_by(.day) | map(.[0])) as $days
      | ([ $days[] | select(.verdict == "warn" or .verdict == "crit") ] | length) as $bad
      | ([ $w[] | select(.verdict == "ok") ] | .[0].day) as $first
      | "O-3 (reported): \($bad) of 30 days started WARN or CRIT, " + (if $first == null then "no OK start yet" else "first OK start on day \($first)" end)
      end' <<<"$outc") || fail "cannot parse outcome.jsonl"
  echo "$out"
}

report() {
  local audit=$1 outcome=$2 wlog=$3 cutover=$4 bin=$5 runs flags outc="" names
  flags=$(mktemp)
  command -v jq >/dev/null || fail "jq is required"
  runs=$(load_runs "$audit") || return 2
  if [ -r "$outcome.1" ] || [ -r "$outcome" ]; then outc=$(cat "$outcome.1" "$outcome" 2>/dev/null); fi
  echo "outcome-report ($audit)"
  section_sm2 "$runs" "$flags" "$bin"
  section_o1 "$runs" "$flags" "$bin"
  section_cv1 "$runs" "$flags"
  section_o2 "$wlog" "$outc" "$cutover"
  section_o3 "$outc"
  if [ -s "$flags.names" ]; then
    names=$(sort -u "$flags.names" | paste -sd'/' - | sed 's#/# / #g')
    echo "Gate D review: record a decision for $names in spikes.md"
  fi
  rm -f "$flags" "$flags.names"
  return 0
}

# ------------------------------------------------------------------------------------------------ selftest
selftest() {
  local self bad=0 n=0 out
  self=$(readlink -f "$0"); d=$(mktemp -d); trap 'rm -rf "${d:?}"' EXIT INT TERM
  GIB=1073741824
  has() { local name=$1 pat=$2; if grep -qF -- "$pat" <<<"$out"; then n=$((n+1)); echo "ok: $name"; else bad=$((bad+1)); echo "SELFTEST BAD: $name: want '$pat' in:"; sed 's/^/    /' <<<"$out"; fi; }
  hasnt() { local name=$1 pat=$2; if ! grep -qF -- "$pat" <<<"$out"; then n=$((n+1)); echo "ok: $name"; else bad=$((bad+1)); echo "SELFTEST BAD: $name: unwanted '$pat' in:"; sed 's/^/    /' <<<"$out"; fi; }
  last() { local name=$1 pat=$2 l; l=$(tail -n1 <<<"$out"); if [ "$l" = "$pat" ]; then n=$((n+1)); echo "ok: $name"; else bad=$((bad+1)); echo "SELFTEST BAD: $name: last line '$l'"; fi; }
  run() { out=$("$self" "$@" 2>&1); rc=$?; if [ "$rc" -ne 0 ]; then bad=$((bad+1)); echo "SELFTEST BAD: exit $rc for: $*"; sed 's/^/    /' <<<"$out"; fi; }
  # one run: id, ts, drop GiB, pct, explained, vstart, vend, causes-json
  rec() { jq -cn --arg id "$1" --argjson ts "$2" --argjson drop "$3" --argjson pct "$4" --argjson ex "$5" --arg vs "$6" --arg ve "$7" --argjson causes "${8:-[]}" --arg kinds "${9:-caches}" '
    {type:"run_start", run_id:$id, ts:($ts-60), verdict_start:(if $vs=="-" then null else $vs end), kinds:$kinds},
    {type:"run_end", run_id:$id, ts:$ts, verdict_after:$ve, attributed_bytes:0, measured_drop:$drop, attribution_pct:$pct, causes:$causes, explained:$ex}'; }
  res() { jq -cn --arg id "$1" --arg sel "$2" --argjson est "$3" --argjson m "$4" '{type:"result", run_id:$id, ts:1, selector:$sel, kind:"x", status:"removed", est_bytes:$est, measured_bytes:$m}'; }
  stub="$d/stub"; printf '#!/bin/sh\ncat <<EOF\n{"items":[{"selector":"docker","kind":"docker-image","path":null,"name":"img-a","status":"risk_not_selected","est_bytes":9663676416},{"selector":"build-dirs","kind":"node-modules","path":"/h/a/node_modules","status":"risk_not_selected","est_bytes":3221225472},{"selector":"steam","kind":"steam-prefix","path":"/h/s","status":"risk_not_selected","est_bytes":1073741824},{"selector":"btrfs","kind":"btrfs-balance","path":null,"name":"bal","status":"risk_not_selected","est_bytes":107374182},{"selector":"caches","kind":"cache","path":"/h/c","status":"removed","est_bytes":99999999999}]}\nEOF\n' >"$stub"; chmod +x "$stub"
  E="$d/none"  # no outcome / wrapper log

  # --- classification at the boundaries, one run each
  for c in "100.0 PASS" "95 PASS" "105 PASS" "94.9 AMBER" "90 AMBER" "105.1 AMBER" "110 AMBER" "89.9 FAIL" "110.1 FAIL" "40 FAIL"; do
    set -- $c; rec r1 1000000 $((6*GIB)) "$1" false ok ok >"$d/a.jsonl"
    run --audit "$d/a.jsonl" --outcome "$E" --wrapper-log "$E" --bin "$stub"; has "pct $1 is $2" "$2 $(jq -rn --argjson p "$1" "$JQ_LIB"'$p | pct')%"
  done
  rec r1 1000000 $((6*GIB)) 130 true ok ok '[{"kind":"snapshot-held","bytes":-1500000000,"source":"snapshot path probe"},{"kind":"concurrent-write-delta","bytes":-1000000000,"source":"two data.bytes_used samples"}]' >"$d/a.jsonl"
  run --audit "$d/a.jsonl" --outcome "$E" --wrapper-log "$E" --bin "$stub"
  has "explained run outside the range is EXPLAINED" "EXPLAINED 130.0%"; has "causes list their bytes and source" "snapshot-held -1.4 GiB (snapshot path probe); concurrent-write-delta -0.9 GiB (two data.bytes_used samples)"
  has "an EXPLAINED run alone does not meet SM-2" "SM-2: pending (1 of 3"
  rec r1 1000000 $((6*GIB)) 40 true ok ok >"$d/a.jsonl"; run --audit "$d/a.jsonl" --outcome "$E" --wrapper-log "$E" --bin "$stub"
  has "explained outside the 110 line is still EXPLAINED (reads the flag, not the number)" "EXPLAINED 40.0%"
  rec r1 1000000 $((6*GIB)) 60 false ok ok >"$d/a.jsonl"; run --audit "$d/a.jsonl" --outcome "$E" --wrapper-log "$E" --bin "$stub"
  has "FAIL names the owner action with shared_est_bytes" "FAIL-and-investigate: the gate stays open. Compare this run's audit log and its shared_est_bytes"
  has "FAIL names spikes.md" "record the finding in spikes.md"
  rec r1 1000000 $((6*GIB)) null false ok ok >"$d/a.jsonl"; run --audit "$d/a.jsonl" --outcome "$E" --wrapper-log "$E" --bin "$stub"
  has "a qualifying run with no attribution_pct is FAIL, never silently passed" "FAIL n/a of a 6.0 GiB drop"

  # --- qualifying runs and SM-2
  { rec s1 1000000 $((2*GIB+107374182)) 99 false ok ok; rec q1 2000000 $((5*GIB)) 130 false ok ok; rec q2 3000000 $((8*GIB)) 100 false ok ok
    rec q3 4000000 $((9*GIB)) 99 false ok ok; rec q4 5000000 $((9*GIB)) 99 false ok ok; } >"$d/a.jsonl"
  run --audit "$d/a.jsonl" --outcome "$E" --wrapper-log "$E" --bin "$stub"
  has "under 5 GiB is TOO SMALL" "TOO SMALL (2.1 GiB, minimum 5 GiB): not counted"
  has "exactly 5 GiB qualifies (FAIL at 130 unexplained is printed)" "run q1 1970-01-24: FAIL 130.0% of a 5.0 GiB drop"
  has "all three qualifying lines print whatever their class" "run q2 1970-02-04: PASS 100.0%"; has "third qualifying run printed" "run q3 1970-02-16: PASS 99.0%"
  has "a fourth qualifying run is shown and not counted" "run q4 1970-02-27: PASS 99.0% (qualifying, after the fixed set of three: not counted)"
  has "one PASS of three meets SM-2" "SM-2: met (2 of the first 3 qualifying runs PASS"
  { rec q1 2000000 $((5*GIB)) 130 false ok ok; rec q2 3000000 $((8*GIB)) 94 false ok ok; rec q3 4000000 $((9*GIB)) 120 true ok ok; } >"$d/a.jsonl"
  run --audit "$d/a.jsonl" --outcome "$E" --wrapper-log "$E" --bin "$stub"
  has "three qualifying runs, none PASS: not met" "SM-2: not met"
  last "a missed SM-2 ends with the Gate D line" "Gate D review: record a decision for SM-2 in spikes.md"
  hasnt "the fixed set never drops a run: no TOO SMALL here" "TOO SMALL"

  # --- CV-1 reach table
  { rec k1 1000000 $((100*GIB)) 100 false ok ok '[]' 'caches,build-dirs,docker-images'
    res k1 build-dirs $((31*GIB+214748365)) $((30*GIB)); res k1 docker-images $((10*GIB)) $((10*GIB)); res k1 caches $((4*GIB)) $((4*GIB)); } >"$d/a.jsonl"
  run --audit "$d/a.jsonl" --outcome "$E" --wrapper-log "$E" --bin "$stub"
  has "per-kind line with est and share" "build-dirs 30.0 GiB (est 31.2 GiB) 30.0% of the drop"
  has "docker line" "docker-images 10.0 GiB (est 10.0 GiB) 10.0% of the drop"; has "caches are itemised too" "caches 4.0 GiB (est 4.0 GiB) 4.0% of the drop"
  has "reference row" "bash baseline 4.6% (11 of 241 GiB)"; has "new kinds share" "new kinds: 40.0% of the drop"
  hasnt "40% is over the line: no CV-1 miss" "CV-1: new kinds"
  { rec k1 1000000 $((100*GIB)) 100 false ok ok '[]' 'caches,build-dirs'
    res k1 build-dirs $((12*GIB)) $((12*GIB)); res k1 caches $((4*GIB)) $((4*GIB)); } >"$d/a.jsonl"
  run --audit "$d/a.jsonl" --outcome "$E" --wrapper-log "$E" --bin "$stub"
  has "under 25% adds the CV-1 decision line" "CV-1: new kinds 12.0%, under the 25% line: record an owner decision at the Gate D review"
  last "CV-1 miss ends with the Gate D line" "Gate D review: record a decision for CV-1 in spikes.md"
  { rec k1 1000000 $((100*GIB)) 100 false ok ok '[]' 'caches'; res k1 caches $((4*GIB)) $((4*GIB)); } >"$d/a.jsonl"
  run --audit "$d/a.jsonl" --outcome "$E" --wrapper-log "$E" --bin "$stub"; has "a caches-only run has no reach verdict" "CV-1: no run has selected the new kinds yet"

  # --- O-1
  { rec o1 1000000 $((6*GIB)) 100 false warn ok; rec o2 2000000 $((6*GIB)) 100 false crit warn; rec o3 3000000 $((6*GIB)) 100 false ok ok; rec o4 4000000 $((6*GIB)) 100 false warn crit; } >"$d/a.jsonl"
  run --audit "$d/a.jsonl" --outcome "$E" --wrapper-log "$E" --bin "$stub"
  has "O-1 counts only runs that started WARN or CRIT" "O-1: 1 of 3 qualifying runs ended OK (1 run started OK and is not counted)"
  has "a run that did not end OK prints the next consumers, largest first" "    docker img-a 9.0 GiB"
  has "second largest" "    build-dirs /h/a/node_modules 3.0 GiB"; has "third largest" "    steam /h/s 1.0 GiB"
  hasnt "only three are printed" "bal"; hasnt "removed rows are not risk rows" "/h/c"
  has "a miss puts O-1 on the Gate D line" "Gate D review: record a decision for O-1 in spikes.md"
  { rec o1 1000000 $((6*GIB)) 60 false warn ok; rec o2 2000000 $((6*GIB)) 70 false crit warn; rec o3 3000000 $((6*GIB)) 80 false warn crit; } >"$d/a.jsonl"
  run --audit "$d/a.jsonl" --outcome "$E" --wrapper-log "$E" --bin "$stub"
  last "O-1 and SM-2 both missed: one closing line names both" "Gate D review: record a decision for O-1 / SM-2 in spikes.md"
  hasnt "an executed missed O-1 does not stop the report" "SELFTEST"
  { rec o1 1000000 $((6*GIB)) 100 false ok ok; rec o2 2000000 $((6*GIB)) 100 false ok ok; } >"$d/a.jsonl"
  run --audit "$d/a.jsonl" --outcome "$E" --wrapper-log "$E" --bin "$stub"
  has "only OK starts: not measurable" "O-1: not measurable (no run started WARN or CRIT)"; hasnt "and it is never met" "O-1: met"
  { rec o1 1000000 $((6*GIB)) 100 false warn ok; } >"$d/a.jsonl"
  run --audit "$d/a.jsonl" --outcome "$E" --wrapper-log "$E" --bin "$stub"; has "all qualifying runs ended OK" "O-1: 1 of 1 qualifying runs ended OK"
  hasnt "no miss, no Gate D line" "Gate D review"
  run --audit "$d/absent.jsonl" --outcome "$E" --wrapper-log "$E" --bin "$stub"; has "no audit log: nothing recorded, still exit 0" "SM-2: pending (0 of 3"

  # --- O-2 (cutover 1970-01-01 = ts 0; day n starts at n*86400)
  day() { echo $(($1 * 86400 + 3600)); }
  iso() { date -u -d "@$(day "$1")" +%Y-%m-%dT%H:%M:%SZ; }
  { for k in 3 12 25; do echo "$(iso $k) disk-usage note"; done; } >"$d/w.log"
  for i in 1 2 3 4 5 6 7; do jq -cn --argjson t "$(day $((20 + i)))" '{ts:$t, via:null, verdict:"ok", mode:"plan"}'; done >"$d/o.jsonl"
  jq -cn --argjson t "$(day 22)" '{ts:$t, via:"wrapper", verdict:"ok", mode:"plan"}' >>"$d/o.jsonl"
  OUTCOME_REPORT_NOW=$((40 * 86400)) run --audit "$d/absent.jsonl" --outcome "$d/o.jsonl" --wrapper-log "$d/w.log" --cutover 1970-01-01 --bin "$stub"
  has "O-2 counts the one NOTE call in days 21-30 and 7 direct uses (wrapper calls are not direct)" "O-2: 1 NOTE call in days 21-30 (target 0), 7 direct uses (minimum 5)"
  : >"$d/w.log"; head -n3 "$d/o.jsonl" >"$d/o3.jsonl"
  OUTCOME_REPORT_NOW=$((40 * 86400)) run --audit "$d/absent.jsonl" --outcome "$d/o3.jsonl" --wrapper-log "$d/w.log" --cutover 1970-01-01 --bin "$stub"
  has "zero calls by not using the tool is not a pass" "O-2: not measurable (3 direct uses in days 21-30, minimum 5)"
  head -n5 "$d/o.jsonl" >"$d/o5.jsonl"
  OUTCOME_REPORT_NOW=$((40 * 86400)) run --audit "$d/absent.jsonl" --outcome "$d/o5.jsonl" --wrapper-log "$d/w.log" --cutover 1970-01-01 --bin "$stub"
  has "0 NOTE calls and 5 direct uses is met" "O-2: met"; hasnt "window closed: no open-window note" "window still open"
  OUTCOME_REPORT_NOW=$((25 * 86400)) run --audit "$d/absent.jsonl" --outcome "$d/o5.jsonl" --wrapper-log "$d/w.log" --cutover 1970-01-01 --bin "$stub"
  has "a window that is not over says so" "O-2: met (window still open)"
  echo "$(iso 12) disk-usage note" >"$d/w.log"
  OUTCOME_REPORT_NOW=$((40 * 86400)) run --audit "$d/absent.jsonl" --outcome "$d/o5.jsonl" --wrapper-log "$d/w.log" --cutover 1970-01-01 --bin "$stub"
  has "a NOTE before day 21 is outside the window" "O-2: met"

  # --- O-3: 30 days, day 1 = 1970-01-01; 6 days start WARN or CRIT, an OK start on day 19, nothing after day 30 counted
  { for k in 1 2 3 4 5 6; do jq -cn --argjson t "$(day $((k - 1)))" --arg v "$([ $((k % 2)) = 0 ] && echo crit || echo warn)" '{ts:$t, via:null, verdict:$v, mode:"plan"}'; done
    jq -cn --argjson t "$(day 18)" '{ts:$t, via:null, verdict:"ok", mode:"plan"}'
    jq -cn --argjson t "$(day 18)" '{ts:($t+60), via:null, verdict:"warn", mode:"plan"}'  # same day, second run: the day counts by its first
    jq -cn --argjson t "$(day 40)" '{ts:$t, via:null, verdict:"warn", mode:"plan"}'; } >"$d/o3.jsonl"
  run --audit "$d/absent.jsonl" --outcome "$d/o3.jsonl" --wrapper-log "$d/w.log" --bin "$stub"
  has "O-3 counts days that started WARN or CRIT and the first OK day" "O-3 (reported): 6 of 30 days started WARN or CRIT, first OK start on day 19"
  : >"$d/empty.jsonl"; run --audit "$d/absent.jsonl" --outcome "$d/empty.jsonl" --wrapper-log "$d/w.log" --bin "$stub"; has "an empty outcome.jsonl is no record" "O-3: no record"
  run --audit "$d/absent.jsonl" --outcome "$E" --wrapper-log "$d/w.log" --bin "$stub"; has "a missing outcome.jsonl is no record" "O-3: no record"

  # --- failure modes fail closed
  printf 'not json\n{"type":"run_end"' >"$d/bad.jsonl"
  run --audit "$d/bad.jsonl" --outcome "$E" --wrapper-log "$E" --bin "$stub"; has "torn and garbage audit lines are skipped" "SM-2: pending (0 of 3"
  out=$(PATH=/nonexistent "$BASH" "$self" --audit "$d/a.jsonl" 2>&1); rc=$?
  if [ "$rc" = 2 ] && grep -q "jq is required" <<<"$out"; then n=$((n+1)); echo "ok: no jq exits 2, never a silent pass"; else bad=$((bad+1)); echo "SELFTEST BAD: no jq rc=$rc: $out"; fi
  echo "selftest: $n ok, $bad bad"; [ $bad = 0 ]
}

main() {
  local audit="" outcome="" wlog="" cutover="" bin=${DUNNAGE_BIN:-dunnage}
  while [ $# -gt 0 ]; do
    case $1 in
      --selftest) selftest; exit $? ;;
      --audit) audit=${2:?}; shift 2 ;;
      --outcome) outcome=${2:?}; shift 2 ;;
      --wrapper-log) wlog=${2:?}; shift 2 ;;
      --cutover) cutover=${2:?}; shift 2 ;;
      --bin) bin=${2:?}; shift 2 ;;
      -h | --help) sed -n 2,7p "$0"; exit 0 ;;
      *) echo "outcome-report: unknown argument $1" >&2; exit 64 ;;
    esac
  done
  report "${audit:-$state_dir/audit.jsonl}" "${outcome:-$state_dir/outcome.jsonl}" "${wlog:-$state_dir/wrapper-use.log}" "$cutover" "$bin"
}

# the selftest re-enters this script, so run main only when executed, not when it is sourced
main "$@"
