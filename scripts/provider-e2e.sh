# shellcheck shell=bash
# Opt-in real-provider acceptance, sourced by run-e2e.sh --provider claude|codex.
# Needs an idle dedicated project, configured forge and worker subscription logins.
provider_e2e_drain() {
  local project=$1 after=$2 done_seen=0 calm=0 open question stop answer_failed=0
  local polls=600 poll_interval=2
  provider_e2e_settled() {
    # A failed read is not a quiet poll.
    if ! open=$(api '/questions?open=true') || ! question=$(jq -r --arg p "$project" '[.[] | select(.project == $p)] | first | .id // empty' <<<"$open"); then
      calm=0
      return 1
    fi
    if [ -n "$question" ]; then
      calm=0
      stop=$(jq -r --arg q "$question" '.[] | select(.id == $q) | [.options[] | select((.text + " " + .effect) | test("stop"; "i")) | .id] | first // empty' <<<"$open") || stop=''
      if [ -n "$stop" ]; then
        "$DVB" answer "$question" "$stop" || answer_failed=1
      else
        "$DVB" answer "$question" --reply stop || answer_failed=1
      fi
      # End the wait immediately on an answer failure; the caller reports it.
      [ "$answer_failed" = 1 ]
      return
    fi
    if [ "$done_seen" = 0 ]; then
      events --unix-socket "$SOCK" "http://dvb/events?after=$after" | jq -se --arg p "$project" 'any(.[]; .kind == "goal.done" and .project == $p)' >/dev/null && done_seen=1
    fi
    if [ "$done_seen" = 1 ] && api /status | jq -e '.running == 0' >/dev/null; then
      calm=$((calm + 1))
    else
      calm=0
    fi
    [ "$calm" -ge 3 ]
  }
  wait_for "the test project to settle with every question closed" provider_e2e_settled
  [ "$answer_failed" = 0 ]
}

provider_e2e() {
  local name=${DVB_E2E_PROJECT:?set DVB_E2E_PROJECT to a dedicated existing test project}
  local model project selected role hire question collected
  local polls=240 poll_interval=2
  case $provider in
    claude) model=${DVB_E2E_MODEL:-claude-sonnet-5-5} ;;
    codex) model=${DVB_E2E_MODEL:-gpt-6.1-sol} ;;
  esac
  api /models | jq -e --arg id "$model" --arg vendor "$provider" \
    '.models[] | select(.id == $id and .vendor == $vendor and (.efforts | index("medium")))' >/dev/null || fail "model $model is unavailable"
  project=$(api /projects | jq -r --arg name "$name" '.[] | select(.name == $name or .id == $name) | .id')
  [ -n "$project" ] || fail "no dedicated test project $name"
  api /status | jq -e '.running == 0' >/dev/null || fail "wait for existing work to finish"
  [ "$(api '/questions?open=true' | jq --arg p "$project" '[.[] | select(.project == $p)] | length')" = 0 ] || fail "answer the test project's existing questions first"
  selected=$(api /settings | jq -c --arg model "$model" '{values:{model_mode:"Custom",model_pool:((.values.model_pool + [$model]) | unique)}}')
  curl -fsS --unix-socket "$SOCK" -X PUT -H 'content-type: application/json' -d "$selected" http://dvb/settings >/dev/null
  while read -r role; do
    while read -r hire; do "$DVB" retire "$hire"; done < <(api "/hires?project=$project" | jq -r --arg role "$role" '.[] | select(.role == $role and .status == "active") | .id')
    "$DVB" hire "$role" "$model" --effort medium
  done < <(api "/projects/$project/roles" | jq -r '.[].id')
  local after
  after=$(events --unix-socket "$SOCK" http://dvb/events | jq -s 'map(.id) | max // 0')
  "$DVB" goal "$project" 'Create docs/provider-e2e.md with one sentence describing this project. Commit the change.'
  provider_e2e_question() {
    question=$(api '/questions?open=true' | jq -r --arg p "$project" '.[] | select(.project == $p) | .id' | head -1) || return 1
    [ -n "$question" ]
  }
  wait_for "the real orchestrator to ask for plan approval" provider_e2e_question
  local choice
  choice=$(api '/questions?open=true' | jq -r --arg q "$question" '.[] | select(.id == $q) | .recommended')
  "$DVB" answer "$question" "$choice"
  provider_e2e_collected() {
    local log
    log=$(events --unix-socket "$SOCK" "http://dvb/events?after=$after") || return 1
    collected=$(jq -sr --arg p "$project" --arg vendor "$provider" '
      [ .[] | select(.kind == "run.started" and .project == $p and .payload.vendor == $vendor and .payload.task != null and .payload.reviews == null) | .payload.run ] as $builders |
      .[] | select(.kind == "run.collected" and (.payload.run as $r | $builders | index($r))) | .payload.head' <<<"$log" | head -1) || return 1
    [ -n "$collected" ]
  }
  wait_for "the real builder to collect a head (check preflight and run output)" provider_e2e_collected
  ok "$provider builder collected $collected; Custom pool and hires saved in $name"
  # The project had no open questions at the start, so any open now came from this goal's runs.
  provider_e2e_drain "$project" "$after" || fail "the test project did not settle with every question closed"
  "$DVB" usage "$project"
}
