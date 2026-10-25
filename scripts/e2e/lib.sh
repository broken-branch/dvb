# shellcheck shell=bash
# Sourced by run-e2e.sh and run-upgrade.sh. Callers set polling bounds, event
# format, journal diagnostics and as_origin (local in dev, engine user on install).
# Policy variables are assigned by the callers.
# shellcheck disable=SC2154

fail() {
  echo "FAIL: $*" >&2
  if [ "$failure_journal" = 1 ]; then
    sudo journalctl -u dvb.service --no-pager -o cat | tail -60 >&2
  fi
  exit 1
}
ok() { echo "ok $*"; }
api() { curl -sf --unix-socket "$SOCK" "http://dvb$1"; }

# The event stream never ends; read a bounded snapshot and strip SSE framing.
events() {
  if [ "$#" = 0 ]; then
    set -- --unix-socket "$SOCK" 'http://dvb/events?after=0'
  fi
  { curl -s -N --max-time 3 "$@" || true; } | sed -n 's/^data: //p' |
    if [ "$event_format" = id-kind ]; then
      jq -r '"\(.id) \(.kind)"'
    else
      cat
    fi
}

wait_for() {
  local what=$1
  shift
  for _ in $(seq "$polls"); do
    if "$@" >/dev/null 2>&1; then return 0; fi
    sleep "$poll_interval"
  done
  fail "timed out waiting for $what"
}

seed_origin() {
  # shellcheck disable=SC2016 # expanded by the inner bash
  as_origin bash -c 'set -e; cd /; rm -rf "$1" "$1.seed"
    git init -q --bare -b main "$1"; git init -q -b main "$1.seed"; cd "$1.seed"
    echo "# $2" > README.md; git add README.md
    git -c user.name=seed -c user.email=seed@example.invalid commit -qm Seed
    git push -q "$1" main; rm -rf "$1.seed"' _ "$1" "$2"
}
