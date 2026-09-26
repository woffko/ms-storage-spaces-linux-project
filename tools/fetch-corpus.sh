#!/bin/bash
# Copy test pools from the VM into testdata/pools/<name>/ as sparse raw images.
# Usage: tools/fetch-corpus.sh name...
set -euo pipefail
cd "$(dirname "$0")/.."
for name in "$@"; do
  out=testdata/pools/$name
  mkdir -p "$out"
  tools/vm.sh -get "C:/sstest/$name/manifest.json" "$out/manifest.json"
  n=$(python3 -c 'import json,sys; print(len(json.load(open(sys.argv[1],encoding="utf-8-sig"))["disks"]))' "$out/manifest.json")
  for ((i = 0; i < n; i++)); do
    tools/vm.sh -get "C:/sstest/$name/disk$i.vhdx" "$out/disk$i.vhdx"
    qemu-img convert -f vhdx -O raw "$out/disk$i.vhdx" "$out/disk$i.img"
    rm "$out/disk$i.vhdx"
  done
  echo "$name: $n disks, $(du -sh "$out" | cut -f1)"
done
