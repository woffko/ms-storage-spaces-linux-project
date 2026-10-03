#!/bin/bash
# Copy ReFS volumes made by tools/vm/New-RefsVolume.ps1 from the Windows VM
# to testdata/refs/NAME/: disk.img (a sparse raw image of the whole VHDX;
# volumes inside a space: disk0.img ... of the pool's disks) and
# manifest.json. The VHDX files stay on the VM. WIN_VM_HOST and
# WIN_VM_HOSTKEY_ALIAS select the VM as for tools/vm.sh.
# Usage: tools/fetch-refs.sh NAME...
set -euo pipefail
cd "$(dirname "$0")/.."
for name in "$@"; do
  [[ $name =~ ^[a-z0-9_]+$ ]] || { echo "bad name $name" >&2; exit 1; }
  out=testdata/refs/$name
  mkdir -p "$out"
  tools/vm.sh -get "C:/sstest/refs/$name/manifest.json" "$out/manifest.json"
  disks=$(python3 -c 'import json, sys; p = json.load(open(sys.argv[1], encoding="utf-8-sig")).get("pool"); print(" ".join(f"disk{i}" for i in range(p["disks"])) if p else "disk")' "$out/manifest.json")
  for d in $disks; do
    tools/vm.sh -get "C:/sstest/refs/$name/$d.vhdx" "$out/$d.vhdx"
    python3 tools/vhdx2raw.py "$out/$d.vhdx" "$out/$d.img" >/dev/null
    rm -f -- "$out/$d.vhdx"
  done
  echo "$name: $(du -ch --apparent-size "$out"/*.img | tail -1 | cut -f1) apparent, $(du -ch "$out"/*.img | tail -1 | cut -f1) on disk"
done
