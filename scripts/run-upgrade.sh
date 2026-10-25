#!/usr/bin/env bash
# The upgrade check on an install made with the Fake provider and
# forge. Three stages, run in order by the upgrade job:
#
#   scripts/run-upgrade.sh seed
#       the installed (previous) release finishes a task, so there is real data;
#   scripts/run-upgrade.sh bootstrapped <version>
#       after a newer release was installed over it (the way off a release that
#       cannot update itself): data intact, backup kept, both accounts can run the
#       binary, a Fake run completes;
#   scripts/run-upgrade.sh cases <broken> <other-key> <good>
#       self-update from there: a release signed with another key is refused; one
#       whose migration fails is undone with nothing lost; a good one updates.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
DVB=/opt/dvb/current/dvb
export DVB_ENGINE_SOCK=/run/dvb/engine.sock
SOCK=$DVB_ENGINE_SOCK
T=${RUNNER_TEMP:-/tmp}

source scripts/e2e/lib.sh
polls=240
poll_interval=0.5
event_format=id-kind
failure_journal=1
as_origin() { sudo -H -u dvb env -u XDG_CONFIG_HOME "$@"; }
db() { sudo sqlite3 /var/lib/dvb/state/dvb.sqlite "$1"; }
has_table() { [ "$(db "SELECT count(*) FROM sqlite_master WHERE name = '$1'")" = 1 ]; }

# One finished task on a new project, started and answered through the CLI: proof
# that the engine (and the binary the workers run) works as it is now installed.
fake_run() {
  local name=$1 origin=/srv/dvb-e2e/$1.git
  sudo install -d -o dvb -g dvb /srv/dvb-e2e
  seed_origin "$origin" "$name"
  "$DVB" project add "$name" --repo "$origin" --no-ci >/dev/null
  "$DVB" goal "$name" "Add the build version to dvb --version" >/dev/null
  project=$(api /projects | jq -r --arg n "$name" '.[] | select(.name == $n) | .id')
  question() { [ "$(api "/questions?open=true" | jq --arg p "$project" '[.[] | select(.project == $p)] | length')" -ge 1 ]; }
  wait_for "the plan question of $name" question
  "$DVB" answer "$(api '/questions?open=true' | jq -r --arg p "$project" '[.[] | select(.project == $p)][0].id')" 1 >/dev/null
  task_done() { api "/tasks?project=$project" | jq -e 'length > 0 and all(.[]; .state.state == "done")'; }
  wait_for "the task of $name to be done" task_done
  wait_for "every run to end" bash -c "curl -sf --unix-socket $SOCK http://dvb/status | jq -e '.running == 0'"
}

# The new binary runs for the owner and for both service accounts (the engine's
# umask must not have made it owner-only), and a Fake run completes on it.
usable() {
  local version=$1
  [ "$("$DVB" --version)" = "dvb $version" ] || fail "$version: the owner cannot run /opt/dvb/current/dvb"
  for u in dvb dvb-worker; do
    [ "$(sudo -u "$u" "$DVB" --version)" = "dvb $version" ] || fail "$version: $u cannot run /opt/dvb/current/dvb"
  done
  [ "$(stat -c %a /opt/dvb/versions/"$version")" = 755 ] || fail "$version: the version directory is $(stat -c %a /opt/dvb/versions/"$version")"
  [ "$(stat -c %a /opt/dvb/versions/"$version"/dvb)" = 755 ] || fail "$version: the binary is $(stat -c %a /opt/dvb/versions/"$version"/dvb)"
  fake_run "after-$(echo "$version" | tr . -)"
  ok "$version: runnable by the owner, dvb and dvb-worker; a Fake run completed"
}

case ${1:?stage} in
seed)
  prev=$("$DVB" --version | awk '{print $2}')
  scripts/run-e2e.sh --no-browser
  events > "$T/seeded.txt"
  [ "$(wc -l < "$T/seeded.txt")" -gt 20 ] || fail "the seed run recorded only $(wc -l < "$T/seeded.txt") events"
  ok "seeded $(wc -l < "$T/seeded.txt") events on $prev"
  ;;
bootstrapped)
  version=${2:?version}
  [ "$(readlink /opt/dvb/current)" = "versions/$version" ] || fail "current is $(readlink /opt/dvb/current)"
  wait_for "the engine after install" "$DVB" status
  n=$(wc -l < "$T/seeded.txt")
  seeded_events() {
    events > "$T/after-install.txt" && [ "$(wc -l < "$T/after-install.txt")" -ge "$n" ]
  }
  wait_for "at least $n seeded events after install" seeded_events
  [ "$(head -n "$n" "$T/after-install.txt" | md5sum)" = "$(md5sum < "$T/seeded.txt")" ] || fail "the install changed the seeded events"
  has_table probe || fail "the new release's migration was not applied"
  sudo ls /var/lib/dvb/state/backups | grep -q '^pre-install-' || fail "no copy of the database was kept"
  systemctl is-active --quiet dvb.service || fail "dvb.service is not active"
  usable "$version"
  ok "installed $version over the previous release: its $n seeded events are intact"
  ;;
cases)
  broken=${2:?broken} other=${3:?other} good=${4:?good}
  prev=$("$DVB" --version | awk '{print $2}')

  # 1. A release signed with another key is refused, and shown.
  if out=$("$DVB" update --to "$other" 2>&1); then fail "1: a release signed with another key was accepted"; fi
  grep -q "UPDATE_REFUSED" <<<"$out" || fail "1: no refusal code in: $out"
  grep -q "not signed with dvb's release key" <<<"$out" || fail "1: refusal not shown: $out"
  [ "$("$DVB" --version)" = "dvb $prev" ] || fail "1: the version changed"
  [ ! -d "/opt/dvb/versions/$other" ] || fail "1: the refused release was unpacked"
  ok "1: a release signed with another key is refused: $out"

  # 2. A failing migration is undone: same version, same data, still writable.
  events > "$T/before2.txt"; n=$(wc -l < "$T/before2.txt")
  if out=$("$DVB" update --to "$broken" 2>&1); then fail "2: a broken migration was reported as an update"; fi
  grep -q "did not take" <<<"$out" || fail "2: failure not shown: $out"
  wait_for "the engine to be back" "$DVB" status
  [ "$("$DVB" --version)" = "dvb $prev" ] || fail "2: the version changed"
  [ "$(readlink /opt/dvb/current)" = "versions/$prev" ] || fail "2: current moved"
  if sudo test -e /var/lib/dvb/state/update.json || sudo test -e /var/lib/dvb/state/rollback; then
    fail "2: handover files left"
  fi
  events > "$T/after2.txt"
  grep -q " engine.update_failed$" "$T/after2.txt" || fail "2: engine.update_failed was not recorded"
  [ "$(head -n "$n" "$T/after2.txt" | md5sum)" = "$(md5sum < "$T/before2.txt")" ] || fail "2: events changed"
  "$DVB" project list | grep -q . || fail "2: the engine does not answer"
  fake_run after-failed
  ok "2: a failing migration was undone; the data is intact and a Fake run completed"

  # 3. The good release updates.
  events > "$T/before3.txt"; n=$(wc -l < "$T/before3.txt")
  out=$("$DVB" update --to "$good" 2>&1) || fail "3: the update failed: $out"
  grep -q "dvb is now $good" <<<"$out" || fail "3: $out"
  wait_for "the new engine" "$DVB" status
  [ "$(readlink /opt/dvb/current)" = "versions/$good" ] || fail "3: current did not move"
  [ "$(api /version | jq -r .version)" = "$good" ] || fail "3: the engine runs another version"
  has_table probe2 || fail "3: the migration was not applied"
  events > "$T/after3.txt"
  [ "$(head -n "$n" "$T/after3.txt" | md5sum)" = "$(md5sum < "$T/before3.txt")" ] || fail "3: events changed"
  [ "$(grep -c ' engine.updated$' "$T/after3.txt")" = 1 ] || fail "3: engine.updated not recorded once"
  [ -d "/opt/dvb/versions/$prev" ] || fail "3: the previous version was not kept"
  sudo ls /var/lib/dvb/state/backups/ | grep -q "^$prev-" || fail "3: no snapshot of $prev"
  ! sudo test -e /var/lib/dvb/state/update.json || fail "3: update.json left"
  systemctl is-active --quiet dvb.service || fail "3: dvb.service is not active"
  usable "$good"
  ok "3: updated $prev to $good: events intact, migration applied, engine.updated recorded"
  ;;
*) echo "usage: $0 seed | bootstrapped <version> | cases <broken> <other> <good>" >&2; exit 2 ;;
esac
