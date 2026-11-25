#!/usr/bin/env bash
# The dvb-bin AUR package for a release: scripts/aur-render.sh <version> <sums file> [<out dir> [<package dir>]]
# Renders <package dir>/PKGBUILD.in (default: this tree's packaging/aur/dvb-bin) for
# <version> (X.Y.Z; prereleases have no AUR package) into <out dir> (default: this
# tree's target/aur/dvb-bin-<version>): PKGBUILD with pkgver, pkgrel=1 and one sum per
# source, .SRCINFO read back from it, and the local files it installs. <sums file> is
# sha256sum output naming the release's files: each downloaded source's sum is the one
# for its URL's last part (the tarball, SHA256SUMS, SHA256SUMS.minisig, LICENSE); the
# sums of local sources are taken from the copied files. Other lines are ignored, so
# the release's own SHA256SUMS can supply the tarball's. Nothing is downloaded.
# public-release.sh, release.yml and ci.yml run it; scripts/test-aur-hooks.py tests it.
# shellcheck disable=SC2016 # pkgbuild's code expands in the PKGBUILD's shell, not here
set -euo pipefail
prog=aur-render
die() { echo "$prog: $*" >&2; exit 1; }
(( $# >= 2 && $# <= 4 )) || { echo "usage: scripts/aur-render.sh <version> <sums file> [<out dir> [<package dir>]]" >&2; exit 2; }
root=$(dirname "$(realpath "$0")")/..
ver=$1 sums_file=$2 out=${3:-$root/target/aur/dvb-bin-$1} package=${4:-$root/packaging/aur/dvb-bin}
[[ $ver =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "the version $ver is not X.Y.Z"
[[ -f $sums_file && -r $sums_file ]] || die "the sums file $sums_file is not a readable file"
[[ -f $package/PKGBUILD.in ]] || die "$package has no PKGBUILD.in"

# What makepkg reads from a PKGBUILD, in a shell with nothing of the caller's.
pkgbuild() {
  env -i PATH="$(getconf PATH)" LC_ALL=C bash --noprofile --norc -ec "cd \"\$1\"; source ./PKGBUILD; $2" _ "$1"
}

mkdir -p "$(dirname "$out")"
work=$(mktemp -d "$(dirname "$out")/.aur-render.XXXXXX")
trap 'rm -rf "$work"' EXIT
sed "s/@PKGVER@/$ver/g" "$package/PKGBUILD.in" > "$work/PKGBUILD"
[[ $(grep -c '^sha256sums=(@SHA256SUMS@)$' "$work/PKGBUILD") == 1 ]] \
  || die "$package/PKGBUILD.in needs exactly one line sha256sums=(@SHA256SUMS@)"
listed=$(pkgbuild "$work" 'printf "%s\n" "${source[@]}"') || die "$package/PKGBUILD.in does not source"
mapfile -t sources <<< "$listed"
install=$(pkgbuild "$work" 'printf "%s\n" "${install:-}"')

sums=()
for entry in "${sources[@]}"; do
  url=${entry#*::}
  if [[ $url == *://* ]]; then
    name=${url##*/}
    sum=$(awk -v f="$name" '$2 == f || $2 == "*" f { print $1 }' "$sums_file")
    [[ $sum =~ ^[0-9a-f]{64}$ ]] || die "$sums_file has no single sum for $name"
  else
    [[ $url != */* && -f $package/$url ]] || die "the local source $url is not a file in $package"
    cp "$package/$url" "$work/"
    sum=$(sha256sum "$work/$url") sum=${sum%% *}
  fi
  sums+=("$sum")
done
if [[ -n $install ]]; then
  [[ $install != */* && -f $package/$install ]] || die "the install file $install is not a file in $package"
  cp "$package/$install" "$work/"
fi

awk -v sums="${sums[*]}" '
  BEGIN { n = split(sums, s, " ") }
  /^sha256sums=\(@SHA256SUMS@\)$/ {
    for (i = 1; i <= n; i++) print (i == 1 ? "sha256sums=(" : "            ") "\047" s[i] "\047" (i == n ? ")" : "")
    next
  }
  { print }' "$work/PKGBUILD" > "$work/PKGBUILD.new"
mv "$work/PKGBUILD.new" "$work/PKGBUILD"
! grep -n '@[A-Z0-9_]*@' "$work/PKGBUILD" || die "PKGBUILD.in has a placeholder other than @PKGVER@ and @SHA256SUMS@"
# makepkg --printsrcinfo for this one-package recipe, without needing makepkg.
pkgbuild "$work" '
printf "pkgbase = %s\n" "${pkgbase:-${pkgname[0]}}"
for field in pkgdesc pkgver pkgrel url install arch license makedepends depends \
             optdepends provides conflicts options source sha256sums; do
  declare -n values="$field"
  for value in "${values[@]}"; do printf "\t%s = %s\n" "$field" "$value"; done
done
printf "\npkgname = %s\n" "${pkgname[0]}"' > "$work/.SRCINFO"

rm -rf "$out"
mkdir "$out"
cp -p "$work"/* "$work/.SRCINFO" "$out/"
echo "$prog: dvb-bin $ver: $out"
