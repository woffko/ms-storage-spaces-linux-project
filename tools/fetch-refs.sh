#!/bin/bash
# Copy ReFS volumes made by tools/vm/New-RefsVolume.ps1 from the Windows VM
# to testdata/refs/NAME/: disk.img (a sparse raw image of the whole VHDX)
# and manifest.json. The VHDX stays on the VM. WIN_VM_HOST and
# WIN_VM_HOSTKEY_ALIAS select the VM as for tools/vm.sh.
# Usage: tools/fetch-refs.sh NAME...
set -euo pipefail
cd "$(dirname "$0")/.."
for name in "$@"; do
  [[ $name =~ ^[a-z0-9_]+$ ]] || { echo "bad name $name" >&2; exit 1; }
  out=testdata/refs/$name
  mkdir -p "$out"
  tools/vm.sh -get "C:/sstest/refs/$name/manifest.json" "$out/manifest.json"
  tools/vm.sh -get "C:/sstest/refs/$name/disk.vhdx" "$out/disk.vhdx"
  python3 tools/vhdx2raw.py "$out/disk.vhdx" "$out/disk.img" >/dev/null
  rm -f -- "$out/disk.vhdx"
  echo "$name: $(du -h --apparent-size "$out/disk.img" | cut -f1) apparent, $(du -h "$out/disk.img" | cut -f1) on disk"
done
