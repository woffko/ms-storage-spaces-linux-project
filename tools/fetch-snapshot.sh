#!/bin/bash
# Copy member disk snapshots taken by tools/vm/Invoke-Scenario.ps1 into
# testdata/snapshots/<name>/<label>/ as raw images (git-ignored), with the
# state Windows reported and the pool's manifest.
# Usage: tools/fetch-snapshot.sh NAME LABEL...
set -euo pipefail
cd "$(dirname "$0")/.."
name=$1; shift
[[ $name =~ ^[a-z0-9_]+$ ]] || { echo "bad name $name" >&2; exit 1; }
spaces=${SPACES:-target/release/spaces}
for label in "$@"; do
  [[ $label =~ ^[A-Za-z0-9_-]+$ ]] || { echo "bad label $label" >&2; exit 1; }
  final=testdata/snapshots/$name/$label
  if [[ -e $final ]]; then echo "$final exists, skipping" >&2; continue; fi
  out=testdata/snapshots/$name/.$label.partial
  rm -rf -- "${out:?}"
  mkdir -p "$out"
  remote="C:\\sstest\\$name\\snap-$label"
  files=$(tools/vm.sh "Get-ChildItem '$remote' -File | ForEach-Object Name" | tr -d '\r')
  for f in $files; do
    tools/vm.sh -get "C:/sstest/$name/snap-$label/$f" "$out/$f"
  done
  [[ -e testdata/snapshots/$name/manifest.json ]] ||
    tools/vm.sh -get "C:/sstest/$name/manifest.json" "testdata/snapshots/$name/manifest.json"
  # Each state is a pool directory of its own (spaces fixture, corpus tools).
  cp "testdata/snapshots/$name/manifest.json" "$out/"
  "$spaces" snapshot-to-raw "$out"/disk*.snap
  rm -f -- "$out"/disk*.snap
  mv "$out" "$final"
done
