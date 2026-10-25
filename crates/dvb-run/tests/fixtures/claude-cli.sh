#!/bin/sh
set -eu
printf '%s\n' "$*" > "$HOME/args"
printf '%s' "${RUSTUP_AUTO_INSTALL-unset}" > "$HOME/rustup-auto-install"
cat > "$HOME/stdin"
cat "$HOME/stream.jsonl"
