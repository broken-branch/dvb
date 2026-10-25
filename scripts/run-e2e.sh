#!/usr/bin/env bash
# The acceptance check: one fake task end
# to end, then the six checks.
#
#   scripts/run-e2e.sh --dev [--no-browser]  local: starts `dvb engine --dev` in a
#                                            temp dir; runs are child processes
#   scripts/run-e2e.sh [--no-browser]        on an install made by install.sh with
#                                            DVB_FAKE=1 on dvb.service (the e2e job)
#
# The browser step needs playwright-core in scripts/e2e (see browser.mjs) and a
# Chrome; CHROME_PATH picks one. `--provider claude|codex` exercises a dedicated
# configured project on an install; see scripts/provider-e2e.sh.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

mode=install
browser=1
provider=fake
while [ "$#" -gt 0 ]; do
  case $1 in
    --dev) mode=dev; shift ;;
    --no-browser) browser=0; shift ;;
    --provider) [ "$#" -ge 2 ] || { echo "--provider needs claude or codex" >&2; exit 2; }; provider=$2; shift 2 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done
case $provider in fake|claude|codex) ;; *) echo "provider must be claude or codex" >&2; exit 2 ;; esac
if [ "$provider" != fake ] && [ "$mode" = dev ]; then
  echo "real-provider checks use a configured install and its worker logins; omit --dev" >&2; exit 2
fi

source scripts/e2e/lib.sh
polls=400
poll_interval=0.25
event_format=json
failure_journal=0
NEUTRAL='dvb <dvb@dvb.invalid>'
GOAL='Add the build version to dvb --version'
pids=()
slice_max= slice_high=
restore_slice() {
  if [ -n "$slice_max" ]; then
    as_origin systemctl --no-ask-password set-property dvb.slice \
      "MemoryMax=$slice_max" "MemoryHigh=$slice_high"
    slice_max= slice_high=
  fi
}
cleanup() {
  restore_slice
  for p in "${pids[@]}"; do kill "$p" 2>/dev/null || true; done
}
trap cleanup EXIT

if [ "$mode" = dev ]; then
  DVB=${DVB:-$(cargo metadata --format-version 1 --no-deps | jq -r .target_directory)/debug/dvb}
  # Short: a socket path must fit in 108 bytes.
  WORK=$(mktemp -d "${TMPDIR:-/tmp}/dvbe2e.XXXXXX")
  "$DVB" --dev "$WORK/e" engine >"$WORK/engine.log" 2>&1 &
  pids+=($!)
  export DVB_ENGINE_SOCK=$WORK/e/engine.sock
  ORIGIN=$WORK/origin.git
  as_origin() { "$@"; }
else
  DVB=${DVB:-/opt/dvb/current/dvb}
  WORK=$(mktemp -d)
  export DVB_ENGINE_SOCK=/run/dvb/engine.sock
  ORIGIN=/srv/dvb-e2e/origin.git
  if [ "$provider" = fake ]; then sudo install -d -o dvb -g dvb /srv/dvb-e2e; fi
  # The origin belongs to the forge's user (the engine, UMask 0027); read it as dvb.
  as_origin() { sudo -H -u dvb env -u XDG_CONFIG_HOME "$@"; }
fi
echo "e2e: $mode mode, $("$DVB" --version), work in $WORK"
SOCK=$DVB_ENGINE_SOCK
og() { as_origin git -c safe.directory='*' -C "$ORIGIN" "$@"; }
open_questions() { [ "$(api '/questions?open=true' | jq length)" = "$1" ]; }

wait_for "the engine" api /version

if [ "$provider" != fake ]; then
  # Real model calls are opt-in. This uses a dedicated existing project; no repo,
  # service, credential or login is created by the check.
  source scripts/provider-e2e.sh
  provider_e2e
  exit 0
fi

# The project's repo: a bare "origin" with one commit, as the forge's user.
seed_origin "$ORIGIN" dvb
seed=$(og rev-parse main)

# Steps 2-4: project, goal, the plan question, the yes.
"$DVB" project add dvb --repo "$ORIGIN" --no-ci
# Adding an existing repository pushes nothing to it.
[ "$(og for-each-ref --format='%(refname)' refs/heads)" = refs/heads/main ] ||
  fail "project setup pushed a branch"
"$DVB" goal dvb "$GOAL"
project=$(api /projects | jq -r '.[] | select(.name == "dvb") | .id')
wait_for "the plan question" open_questions 1
question=$(api '/questions?open=true' | jq -r '.[0].id')
task=$(api "/tasks?project=$project" | jq -r '.[0].id')

if [ "$browser" = 1 ]; then
  "$DVB" desk --port 0 >"$WORK/desk1.out" 2>&1 &
  pids+=($!)
  wait_for "the desk" test -s "$WORK/desk1.out"
  node scripts/e2e/browser.mjs "$(head -1 "$WORK/desk1.out")" "$task" 120000 >"$WORK/browser.out" 2>&1 &
  browser_pid=$!
  wait_for "the browser on the task page" grep -q ready "$WORK/browser.out"
fi

"$DVB" answer "$question" 1

# Steps 5-8 run on their own; wait for the next question and every run to end.
task_done() { api "/tasks/$task" | jq -e '.state.state == "done"'; }
wait_for "the task to be done" task_done
wait_for "the next question" open_questions 1
idle() { api /status | jq -e '.running == 0'; }
wait_for "every run to end" idle

if [ "$mode" = install ]; then
  slice_max=$(systemctl show dvb.slice -p MemoryMax --value)
  slice_high=$(systemctl show dvb.slice -p MemoryHigh --value)
  "$DVB" settings set runs_memory_gb 2 >"$WORK/memory-save.out" ||
    fail "engine could not set dvb.slice under polkit"
  [ "$(systemctl show dvb.slice -p MemoryMax --value)" = 2147483648 ] || fail "slice MemoryMax"
  [ "$(systemctl show dvb.slice -p MemoryHigh --value)" = 1879048192 ] || fail "slice MemoryHigh"
  [ "$(api /settings | jq '.values.runs_memory_gb')" = 2 ] || fail "live slice read"
  if sudo -H -u dvb-worker systemctl --no-ask-password set-property dvb.slice \
    MemoryMax=2G MemoryHigh=1792M >"$WORK/memory-denied.out" 2>&1; then
    fail "polkit let dvb-worker set dvb.slice"
  fi
  ram_gb=$(api /settings | jq '.ram_gb')
  "$DVB" settings set runs_memory_gb "$ram_gb" >"$WORK/memory-warning.out" || fail "save above half RAM"
  grep -Fxq 'Several runs building at once can use this much memory and freeze the desktop.' \
    "$WORK/memory-warning.out" || fail "CLI memory warning"
  restore_slice
  ok "engine saved dvb.slice through polkit: MemoryMax=2G MemoryHigh=1792M; dvb-worker denied; CLI warned above half RAM; original limits restored"
fi

all=$(events --unix-socket "$SOCK" "http://dvb/events?after=0")
builder=$(jq -r 'select(.kind == "task.started") | .payload.run' <<<"$all" | head -1)

# 1. One neutral squash commit on main; only the task's branch was added.
[ "$(og rev-list --count "$seed..main")" = 1 ] || fail "1: main does not have exactly one new commit"
who=$(og log -1 --format='%an <%ae>|%cn <%ce>' main)
[ "$who" = "$NEUTRAL|$NEUTRAL" ] || fail "1: main's new commit is by $who"
og cat-file -e main:docs/skeleton.md || fail "1: the edit is not on main"
branches=$(og for-each-ref --format='%(objectname) %(refname)' refs/heads | sort)
want_branches=$(printf '%s\n' \
  "$(og rev-parse "dvb/$task-1") refs/heads/dvb/$task-1" \
  "$(og rev-parse main) refs/heads/main" | sort)
[ "$branches" = "$want_branches" ] || fail "1: branch refs are $branches"
if og log --all --format='%ae %ce' | grep -q 'worker@dvb.invalid'; then
  fail "1: a commit by the worker reached the repo"
fi
ok "1: main has one squash commit by $NEUTRAL with the edit; only dvb/$task-1 was added"

# 2. Status hides finished work unless --all is given. Events keep their order.
status=$("$DVB" status dvb)
grep -qx '1 finished goals not shown (dvb status dvb --all)' <<<"$status" || fail "2: status: $status"
if grep -qE '^(Goal[: ]|Task )' <<<"$status"; then
  fail "2: default status shows finished work: $status"
fi
status=$("$DVB" status dvb --all)
grep -Fxq "Task $task: $GOAL - Done" <<<"$status" || fail "2: status: $status"
goal=$(api "/goals?project=$project" | jq -r '.[0].id')
grep -Fxq "Goal $goal: $GOAL - Done" <<<"$status" || fail "2: status: $status"
want='project.added goal.stated plan.proposed question.opened question.answered goal.approved task.started run.preflight run.usage run.collected run.ended branch.published pr.opened verdict.recorded pr.merged task.done goal.done question.opened'
got=$(jq -r --arg b "$builder" 'select((.kind | startswith("run.") | not) or .payload.run == $b) | .kind' <<<"$all" |
  grep -xE "$(tr ' ' '|' <<<"$want")" | tr '\n' ' ' | sed 's/ $//')
[ "$got" = "$want" ] || fail "2: events in order: $got"
watched=$("$DVB" watch --once --after 0 | awk '{print $1, $2}')
[ "$watched" = "$(jq -r '"\(.id) \(.kind)"' <<<"$all")" ] || fail "2: dvb watch differs from the stream"
ok "2: status matches finished-work visibility and shows Done; dvb watch prints the events in order"

# 3. Early starts are refused; engine and orchestrator admission use the active hire.
refused=$(jq -r 'select(.kind == "command.refused" and .payload.code == "GOAL_NOT_APPROVED"
  and .payload.command == "task start") | .id' <<<"$all" | head -1)
answered=$(jq -r 'select(.kind == "question.answered") | .id' <<<"$all" | head -1)
[ -n "$refused" ] && [ "$refused" -lt "$answered" ] || fail "3: no GOAL_NOT_APPROVED before the yes"
ok "3: task start before the yes was refused with GOAL_NOT_APPROVED (event $refused)"
started=$(jq -r --arg b "$builder" 'select(.kind == "task.started" and .payload.run == $b) | .id' <<<"$all" | head -1)
automatic=$(jq -r --arg b "$builder" 'select(.kind == "run.started" and .payload.run == $b
  and .actor == "engine") | .id' <<<"$all" | head -1)
if [ -n "$automatic" ]; then
  [ "$automatic" -gt "$answered" ] && [ "$automatic" -lt "$started" ] ||
    fail "3: automatic builder admission did not follow approval"
  ok "3: the engine admitted the approved task after a run ended"
else
  alias_refusal=$(jq -r 'select(.kind == "command.refused" and .payload.code == "MODEL_NOT_IN_POOL"
    and .payload.command == "task start" and (.payload.detail | contains("sonnet"))
    and (.payload.detail | contains("valid model ids: fake-1"))) | .id' <<<"$all" | head -1)
  [ -n "$alias_refusal" ] && [ "$alias_refusal" -gt "$answered" ] && [ "$alias_refusal" -lt "$started" ] ||
    fail "3: no alias refusal with valid ids before the orchestrator's builder start"
  ok "3: sonnet refused with MODEL_NOT_IN_POOL (valid model ids: fake-1)"
fi
[ "$(jq -s --arg t "$task" '[.[] | select(.kind == "task.started" and .payload.task == $t)] | length' <<<"$all")" = 1 ] ||
  fail "3: the automatic and orchestrator starts produced duplicate runs"
assignment=$(api "/runs/$builder")
hire=$(api "/hires?project=$project" | jq -c --arg h "$(jq -r .hire <<<"$assignment")" '.[] | select(.id == $h)')
[ -n "$hire" ] || fail "3: the builder's hire is missing"
jq -e --argjson h "$hire" '.role == $h.role and .model == $h.model and .effort == $h.effort
  and .vendor == "fake" and $h.status == "active"' <<<"$assignment" >/dev/null ||
  fail "3: the builder did not use the role's active hire: $assignment"
ok "3: omitted model/effort used $(jq -r '"\(.model), \(.effort)"' <<<"$assignment") from the active hire"

# 4. The builder ran in its unit as dvb-worker and could not read the store.
pf=$(jq -c --arg b "$builder" 'select(.kind == "run.preflight" and .payload.run == $b) | .payload' <<<"$all")
jq -e '.checks | all(.ok)' <<<"$pf" >/dev/null || fail "4: preflight: $pf"
if [ "$mode" = install ]; then
  jq -e --arg u "dvb-run-fake@$builder.service as dvb-worker" '.boundary == $u' <<<"$pf" >/dev/null ||
    fail "4: boundary: $(jq -r .boundary <<<"$pf")"
  jq -e '.checks[] | select(.code == "PF_STORE_VISIBLE") | .detail | test("^/var/lib/dvb/state not readable")' \
    <<<"$pf" >/dev/null || fail "4: the store was readable: $pf"
  ok "4: $(jq -r .boundary <<<"$pf"); $(jq -r '.checks[] | select(.code == "PF_STORE_VISIBLE") | .detail' <<<"$pf")"
else
  ok "4: skipped in dev mode (no unit, no boundary); the e2e job checks it on an install"
fi

# 5. The same events through the desk on 127.0.0.1, and the app showed Done.
"$DVB" desk --port 0 >"$WORK/desk2.out" 2>&1 &
pids+=($!)
wait_for "the second desk" test -s "$WORK/desk2.out"
launch=$(head -1 "$WORK/desk2.out")
base=${launch%%/launch*}
[ "$(curl -s -o /dev/null -w '%{http_code}' "$base/events?after=0")" = 403 ] || fail "5: the desk let a request without the cookie through"
curl -sf -c "$WORK/jar" -o /dev/null "$launch" || fail "5: the launch link"
desk=$(events -b "$WORK/jar" "$base/events?after=0")
[ "$(jq -r .id <<<"$desk")" = "$(jq -r .id <<<"$all")" ] || fail "5: the desk's events differ"
if [ "$browser" = 1 ]; then
  wait "$browser_pid" || fail "5: browser: $(cat "$WORK/browser.out")"
  ok "5: the desk serves the same $(wc -l <<<"$desk") events; $(tail -1 "$WORK/browser.out")"
else
  ok "5: the desk serves the same $(wc -l <<<"$desk") events (browser step skipped)"
fi

# 6. Every run selected the Fake vendor and emitted its zero-model-call receipt.
# These checks do not measure network use.
jq -e 'select(.kind == "run.started" and .payload.vendor != "fake")' <<<"$all" >/dev/null &&
  fail "6: a run used a real provider"
runs=$(jq -r 'select(.kind == "run.started") | .payload.run' <<<"$all" | sort)
zero=$(jq -r 'select(.kind == "run.output" and .payload.item.text == "fake provider: 0 model calls")
  | .payload.run' <<<"$all" | sort)
[ "$runs" = "$zero" ] || fail "6: not every run reported 0 model calls"
ok "6: $(wc -l <<<"$runs") runs selected the Fake vendor; each emitted the Fake-provider receipt for 0 model calls"

echo "e2e: all six checks passed"
