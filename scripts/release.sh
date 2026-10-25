#!/usr/bin/env bash
# Assembles a release from a built binary; the release workflow and a
# local dry run both use it.
#
#   scripts/release.sh <tag> <dvb-binary> <outdir>
#
# Writes into <outdir>: the tarball, install.sh (stamped with the tag and the
# pinned public key), SHA256SUMS and SHA256SUMS.minisig.
# Requires MINISIGN_KEY_FILE to name a secret key made without a password
# (`minisign -G -W`; the key's protection is the Actions secret).
set -euo pipefail

tag=${1:?usage: scripts/release.sh <tag> <dvb-binary> <outdir>}
bin=${2:?usage: scripts/release.sh <tag> <dvb-binary> <outdir>}
out=${3:?usage: scripts/release.sh <tag> <dvb-binary> <outdir>}
cd "$(git rev-parse --show-toplevel)"

[ -n "${MINISIGN_KEY_FILE:-}" ] || { echo "release.sh: MINISIGN_KEY_FILE is required; refusing to package an unsigned release" >&2; exit 1; }

[[ $tag =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$ ]] || { echo "release.sh: tag must look like v1.2.3: $tag" >&2; exit 1; }
ver=${tag#v}
[ "$("$bin" --version)" = "dvb $ver" ] || { echo "release.sh: $bin says '$("$bin" --version)', expected 'dvb $ver'" >&2; exit 1; }

# The key the binary pins (crates/dvb-engine/src/update/release.pub): its second line.
pub=$(sed -n 2p crates/dvb-engine/src/update/release.pub)
[ -n "$pub" ] || { echo "release.sh: the binary pins no signing key (release.pub is empty)" >&2; exit 1; }

mkdir -p "$out"
out=$(cd "$out" && pwd)
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
name=dvb-$ver-x86_64-linux
top=dvb-$ver # what install.sh and `dvb update` unpack
mkdir -p "$stage/$top/install"
install -m 0755 "$bin" "$stage/$top/dvb"
# The newest database migration this release knows, so `dvb update --to` can tell
# whether going back needs a migration.
ls crates/dvb-store/migrations | sed -n 's/^V\([0-9][0-9]*\)__.*/\1/p' | sort -n | tail -1 > "$stage/$top/schema"
# -L: install/icons links to the app's icons in ui/public/icons.
cp -RL install/* "$stage/$top/install/"
tar --sort=name --mtime=@0 --owner=0 --group=0 --numeric-owner \
  -C "$stage" -czf "$out/$name.tar.gz" "$top"

sed -e "s|@RELEASE_TAG@|$tag|" -e "s|@RELEASE_PUBKEY@|$pub|" \
  install/install.sh > "$out/install.sh"
chmod 0755 "$out/install.sh"

(cd "$out" && sha256sum "$name.tar.gz" install.sh > SHA256SUMS)

rm -f "$out/SHA256SUMS.minisig"
minisign -S -s "$MINISIGN_KEY_FILE" -m "$out/SHA256SUMS" -x "$out/SHA256SUMS.minisig" >/dev/null
minisign -Vm "$out/SHA256SUMS" -x "$out/SHA256SUMS.minisig" -P "$pub" >/dev/null \
  || { echo "release.sh: the signature does not verify with the key the binary pins" >&2; exit 1; }
echo "release.sh: $tag signed"
ls -l "$out"
