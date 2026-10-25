#!/usr/bin/env bash
# The tree the public copy sees: scripts/public-copy.sh <src-dir> <out-dir>
# Copies an exported tree (git archive, no .git) to <out-dir> and, in the copy only,
# removes the private files, renames the repository, runs every self-hosted job on
# GitHub-hosted runners and drops ci.yml's `hosted` input, and prints how much of each
# it rewrote. On a tree that is already public it changes nothing, so the public
# repository's CI runs the same step.
set -euo pipefail
# A file with this exact line stays in this repository only; nothing in the copy names it.
private_line='# Private: scripts/public-copy.sh leaves this file out of the public copy.'
# Built from two parts so that the rename leaves this script's own copy working.
private_repo='dev-bench-com'/dvb public_repo=broken-branch/dvb
(( $# == 2 )) || { echo "usage: scripts/public-copy.sh <src-dir> <out-dir>" >&2; exit 2; }
src=$1 out=$2
[[ -d $src ]] || { echo "public-copy: $src is not a directory" >&2; exit 2; }
[[ ! -e $src/.git ]] || { echo "public-copy: $src is a checkout; export it with git archive" >&2; exit 2; }
if [[ -e $out && -n $(ls -A "$out") ]]; then echo "public-copy: $out is not empty" >&2; exit 2; fi
mkdir -p "$out"
cp -a "$src/." "$out/"
{ grep -rlIZxF -e "$private_line" -- "$out" || (( $? == 1 )); } | xargs -0r rm -f --
names=$({ grep -rIoF -e "$private_repo" -- "$out" || (( $? == 1 )); } | wc -l)
named=$({ grep -rlIF -e "$private_repo" -- "$out" || (( $? == 1 )); } | wc -l)
{ grep -rlIZF -e "$private_repo" -- "$out" || (( $? == 1 )); } | xargs -0r sed -i "s#$private_repo#$public_repo#g"
runs_on=0 runs_on_files=0 hosted=0 hosted_files=0
for workflow in "$out"/.github/workflows/*.yml "$out"/.github/workflows/*.yaml; do
  [[ -f $workflow ]] || continue
  n=$(grep -cE '^[[:space:]]*runs-on:.*self-hosted' "$workflow") || (( $? == 1 ))
  if (( n )); then runs_on=$(( runs_on + n )) runs_on_files=$(( runs_on_files + 1 )); fi
  sed -i -E '/^[[:space:]]*runs-on:.*self-hosted/s/runs-on:.*/runs-on: ubuntu-latest/' "$workflow"
  before=$(grep -c '^ *hosted:' "$workflow") || (( $? == 1 ))
  # Drop a `hosted:` input of workflow_dispatch, and `inputs:` once nothing is under it.
  dropped=$(awk '
    function indent(s) { return match(s, /[^ ]/) ? RSTART - 1 : -1 }
    function parent(i,   k) { for (k = i - 1; k > 0; k--) if (indent(line[k]) >= 0 && indent(line[k]) < indent(line[i])) return k; return 0 }
    function has_child(i,   k) { for (k = i + 1; k <= NR; k++) if (!drop[k] && indent(line[k]) >= 0) return indent(line[k]) > indent(line[i]); return 0 }
    { line[NR] = $0 }
    END {
      for (i = 1; i <= NR; i++) {
        if (line[i] !~ /^ *hosted:/) continue
        inputs = parent(i)
        if (!inputs || line[inputs] !~ /^ *inputs: *$/ || line[parent(inputs)] !~ /^ *workflow_dispatch: *$/) continue
        drop[i] = 1
        for (k = i + 1; k <= NR && (indent(line[k]) < 0 || indent(line[k]) > indent(line[i])); k++) drop[k] = 1
        if (!has_child(inputs)) drop[inputs] = 1
      }
      for (i = 1; i <= NR; i++) if (!drop[i]) print line[i]
    }' "$workflow")
  printf '%s\n' "$dropped" > "$workflow"
  n=$(( before - $(grep -c '^ *hosted:' "$workflow" || (( $? == 1 ))) ))
  if (( n )); then hosted=$(( hosted + n )) hosted_files=$(( hosted_files + 1 )); fi
done
echo "public-copy: rewrote the repository name: $names occurrences in $named files"
echo "public-copy: rewrote runs-on: $runs_on lines in $runs_on_files files"
echo "public-copy: dropped the hosted input: $hosted in $hosted_files files"
