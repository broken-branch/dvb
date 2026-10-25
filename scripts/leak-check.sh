#!/usr/bin/env bash
# The public-release leak gate: scripts/leak-check.sh <dir> [--denylist <file>]
# Checks the files in <dir> (no git history) with gitleaks, the optional denylist,
# the generic rules below and the workflow rules, and exits non-zero on any failure.
# A finding prints file:line only, never the matching text, so a log never repeats
# what it found. GITLEAKS=<binary> uses that gitleaks instead of the pinned download.
set -euo pipefail
shopt -s inherit_errexit
usage() { echo "usage: scripts/leak-check.sh <dir> [--denylist <file>]" >&2; exit 2; }
dir="" denylist="" with_denylist=0
while (( $# )); do
  case "$1" in
    --denylist) (( $# > 1 )) || usage; denylist=$2; with_denylist=1; shift 2 ;;
    -*) usage ;;
    *) [[ -z $dir ]] || usage; dir=$1; shift ;;
  esac
done
[[ -n $dir ]] || usage
[[ -d $dir ]] || { echo "leak-check: $dir is not a directory" >&2; exit 2; }
if (( with_denylist )) && ! [[ -f $denylist && -r $denylist ]]; then
  echo "leak-check: the denylist $denylist is not a readable file" >&2; exit 2
fi
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
failed=0

# hits <path> <exclude ERE> <grep pattern options...>: "file:line" for every text line
# under <dir>/<path> with a match that <exclude> does not cover (it sees "line:match").
hits() {
  local path=$1 exclude=$2 file lines n
  shift 2
  [[ -e $dir/$path ]] || return 0
  (cd "$dir" && grep -rlIZ "$@" -- "$path") > "$work/files" || (( $? == 1 ))
  while IFS= read -r -d '' file; do
    lines=$(cd "$dir" && grep -noI "$@" -- "$file" | { grep -viE "$exclude" || (( $? == 1 )); } | cut -d: -f1 | uniq)
    for n in $lines; do printf '%s:%s\n' "${file#./}" "$n"; done
  done < "$work/files"
}
# check <label> <hits arguments...>: prints each hit under the label and records the failure.
check() {
  local label=$1 found hit
  shift
  found=$(hits "$@")
  [[ -n $found ]] || return 0
  failed=1
  while IFS= read -r hit; do echo "FAIL $label: $hit"; done <<< "$found"
}
none='^$' # an exclude that covers nothing

# The identity job in ci.yml downloads the same gitleaks with these two lines. The
# checksum is from the release's gitleaks_<version>_checksums.txt.
GITLEAKS_VERSION=8.30.1
GITLEAKS_SHA256=551f6fc83ea457d62a0d98237cbad105af8d557003051f41f3e7ca7b3f2470eb
gitleaks=${GITLEAKS:-}
if [[ -z $gitleaks ]]; then
  archive="gitleaks_${GITLEAKS_VERSION}_linux_x64.tar.gz"
  curl -sSfL "https://github.com/gitleaks/gitleaks/releases/download/v${GITLEAKS_VERSION}/$archive" -o "$work/$archive"
  echo "$GITLEAKS_SHA256  $work/$archive" | sha256sum -c --quiet -
  tar -xzf "$work/$archive" -C "$work" gitleaks
  gitleaks=$work/gitleaks
fi
# The default rules, whatever .gitleaks.toml or .gitleaksignore the checked tree or
# the current directory has.
printf '[extend]\nuseDefault = true\n' > "$work/gitleaks.toml"
echo "leak-check: gitleaks"
"$gitleaks" dir --no-banner --redact --exit-code 1 --config "$work/gitleaks.toml" \
  --gitleaks-ignore-path "$work" -- "$dir" || { echo "FAIL gitleaks"; failed=1; }

if (( with_denylist )); then
  echo "leak-check: denylist"
  number=0
  while IFS= read -r -u 3 term || [[ -n $term ]]; do
    number=$((number + 1))
    term=${term%$'\r'}
    term=${term#"${term%%[![:space:]]*}"}
    term=${term%"${term##*[![:space:]]}"}
    [[ -z $term || $term == '#'* ]] && continue
    check "denylist line $number" . "$none" -iF -e "$term"
  done 3< "$denylist"
fi

echo "leak-check: generic rules"
check "home-directory path" . "$none" -E -e '/(home|Users)/[A-Za-z0-9_][A-Za-z0-9._-]*/'
# A URL's user:password@host is not an address; systemd instances (name@x.service) neither.
email_ok='^[0-9]+:(://|no-?reply@)|@users\.noreply\.github\.com$|@([a-z0-9-]+\.)*example\.(com|org|net)$|\.(example|invalid|test|localhost)$|\.(service|socket|timer|target|mount|path|slice|scope)$'
check "e-mail address" . "$email_ok" -E -e '(://[^/@[:space:]]*:)?[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}'
check "private IPv4 address" . "$none" -E -e '(^|[^0-9.])(10(\.[0-9]{1,3}){3}|192\.168(\.[0-9]{1,3}){2}|172\.(1[6-9]|2[0-9]|3[01])(\.[0-9]{1,3}){2})([^0-9.]|$)'
check "dvb run work directory" . "$none" -E -e '/var/lib/dvb/work/[0-9A-HJKMNP-TV-Z]{26}'

# A plain string search, not a parse of runs-on, so no new form slips past.
echo "leak-check: workflow rules"
check "self-hosted runner" .github "$none" -F -e self-hosted
check "pull_request_target or workflow_run trigger" .github "$none" -wE -e 'pull_request_target|workflow_run'

if (( failed )); then echo "leak-check: FAILED"; exit 1; fi
if (( ! with_denylist )); then echo "leak-check: passed (no denylist given)"; else echo "leak-check: passed"; fi
