#!/bin/bash
# Copy test pools from the VM into testdata/pools/<name>/ as sparse raw images.
# Usage: tools/fetch-corpus.sh [--crash] [--remove-remote] name...
#   --crash          fetch the crashed disks of a New-CrashPool.ps1 pool into testdata/crash/
#   --remove-remote  delete the pool's VHDX files on the VM after a complete copy
set -euo pipefail
cd "$(dirname "$0")/.."
remove=0
kind=pools
src_sub=""
while [[ ${1:-} == --* ]]; do
  case $1 in
    --remove-remote) remove=1 ;;
    --crash) kind=crash; src_sub=crash/ ;;
  esac
  shift
done
for name in "$@"; do
  [[ $name =~ ^[a-z0-9]+$ ]] || { echo "bad name $name" >&2; exit 1; }
  final=testdata/$kind/$name
  if [[ -e $final ]]; then echo "$final exists, skipping" >&2; continue; fi
  # Download into a temporary directory so tests never see a partial pool.
  out=testdata/$kind/.$name.partial
  mkdir -p "$out"
  tools/vm.sh -get "C:/sstest/$name/manifest.json" "$out/manifest.json"
  n=$(python3 -c 'import json,sys; print(len(json.load(open(sys.argv[1],encoding="utf-8-sig"))["disks"]))' "$out/manifest.json")
  for ((i = 0; i < n; i++)); do
    tools/vm.sh -get "C:/sstest/$name/${src_sub}disk$i.vhdx" "$out/disk$i.vhdx"
    qemu-img convert -f vhdx -O raw "$out/disk$i.vhdx" "$out/disk$i.img"
    rm "$out/disk$i.vhdx"
  done
  mv "$out" "$final"
  echo "$name: $n disks, $(du -sh "$final" | cut -f1)"
  if ((remove)); then
    tools/vm.sh "\$ErrorActionPreference='Stop'; \$d='C:\\sstest\\$name'; if (Get-ChildItem \$d -Filter *.vhdx | Where-Object { (Get-DiskImage -ImagePath \$_.FullName).Attached }) { throw 'attached' }; Remove-Item -Recurse -Force \$d; 'removed on VM: $name'" \
      || echo "warning: could not remove $name on the VM" >&2
  fi
done
