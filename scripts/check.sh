#!/usr/bin/env bash
# The local and CI gates. Each part runs sequentially; default: all three.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
part=${1:-all}
if (( $# > 1 )); then echo "usage: scripts/check.sh [all|rust|ui|contracts]" >&2; exit 2; fi
case "$part" in all|rust|ui|contracts) ;; *) echo "unknown check part: $part" >&2; exit 2 ;; esac
# Reject references to internal design notes absent from the repository.
# The first deployed migration retains its checksummed bytes.
if git grep -nIE '\b(ADR-[0-9]{4}|Q-[0-9]+(\.[0-9]+)?|R-[0-9]+)\b' -- . ':!*.lock' ':!*-lock.json' ':!crates/dvb-store/migrations/V1__events.sql'; then exit 1; fi
: "${CARGO_TARGET_DIR:?Set a private, writable CARGO_TARGET_DIR for this worktree}"
mkdir -p "$CARGO_TARGET_DIR"
[[ -w "$CARGO_TARGET_DIR" ]] || { echo "CARGO_TARGET_DIR is not writable" >&2; exit 1; }
[[ $(node --version) == v26.* ]] || { echo "Node 26 is required" >&2; exit 1; }
[[ $(rustup show active-toolchain) == stable-* ]] || { echo "The stable Rust toolchain is required" >&2; exit 1; }
# Check sources as well as nextest's compiled test inventory (including cfg_attr).
python3 - <<'PYIGNORE'
import pathlib, re, sys
hits = [str(p) for p in pathlib.Path('crates').rglob('*.rs')
        if re.search(r'#\s*\[\s*(?:ignore\b|cfg_attr\([^]]*,\s*ignore\b)', p.read_text())]
if hits:
    sys.exit('Ignored tests are forbidden: ' + ', '.join(hits))
PYIGNORE
ui_build() {
  (cd ui && npm ci && npm run build)
  # rust-embed debug builds must re-read the newly built cockpit.
  touch crates/dvb-desk/src/lib.rs
}
ui_checks() { ui_build; (cd ui && npm run format:check && npm run lint && npx --no-install vitest run); }
rust_checks() {
  [[ $(cargo nextest --version | awk 'NR == 1 {print $2}') == "0.9.148" ]] || { echo "Install cargo-nextest 0.9.148" >&2; exit 1; }
  [[ $(cargo deny --version) == "cargo-deny 0.20.2" ]] || { echo "Install cargo-deny 0.20.2" >&2; exit 1; }
  cargo fmt --all --check
  # Keep long-function findings advisory even with the strict warning gate.
  cargo clippy --locked --workspace --all-targets -- -D warnings --force-warn clippy::too_many_lines
  cargo clippy --locked --workspace --lib --bins -- -D warnings --force-warn clippy::too_many_lines \
    -D clippy::unwrap_used -D clippy::expect_used -D clippy::panic -D clippy::todo -D clippy::dbg_macro
  cargo build --locked --workspace
  inventory=$(mktemp)
  trap 'rm -f "$inventory"' EXIT
  cargo nextest list --locked --workspace --message-format json > "$inventory"
  python3 - "$inventory" <<'PYTESTS'
import json, sys
suites = json.load(open(sys.argv[1]))['rust-suites']
ignored = [f'{name}::{test}' for name, suite in suites.items()
           for test, case in suite['testcases'].items() if case['ignored']]
if ignored:
    sys.exit('Ignored tests are forbidden: ' + ', '.join(ignored))
PYTESTS
  cargo nextest run --locked --workspace
  cargo deny --locked check
  python3 scripts/test-install.py
}
contracts_checks() {
  local before_status before_diff
  before_status=$(git status --porcelain)
  before_diff=$(git diff --binary HEAD)
  scripts/gen-contracts.sh
  # A clean CI checkout must remain clean; local implementation edits may
  # already exist, but generation must not add to or alter them.
  if [[ $(git status --porcelain) != "$before_status" || $(git diff --binary HEAD) != "$before_diff" ]]; then
    echo "Contract regeneration changed the tree; include the generated changes and rerun" >&2
    git status --short
    exit 1
  fi
  # Restrict the clean-tree gate to generated contracts so local implementation
  # edits can be checked before collection/commit.
  if [[ -n $(git status --porcelain -- contracts/openapi.json ui/src/api/schema.ts prompts/verdict.schema.json contracts/wording.json ui/src/api/wording.json) ]]; then
    echo "Contract files are stale; regenerate and include them in the change:" >&2
    git diff -- contracts/openapi.json ui/src/api/schema.ts prompts/verdict.schema.json contracts/wording.json ui/src/api/wording.json
    exit 1
  fi
}
case "$part" in
  all) ui_checks; rust_checks; contracts_checks ;;
  rust) ui_build; rust_checks ;;
  ui) ui_checks ;;
  contracts) contracts_checks ;;
esac
