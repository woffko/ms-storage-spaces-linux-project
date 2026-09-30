#!/bin/bash
# The write checks of Stage 2 on spaces created on Linux (run on the Linux
# test VM as root, from the repository): pools of three sparse 8 GiB images
# made by `spaces pool create` and `spaces space create` in
# /srv/spaces/work/lnxpools, one space each, the first PATTERN MiB filled
# with the verification pattern, then
#   tools/rw-kernel-check.sh through ublk and nbd (and dm for the simple
#   space): fio with crc32c verification above the pattern, read back
#   read-only, the pattern intact;
#   tools/rw-ntfs-check.sh with ntfs-3g on the mirror and the parity space,
#   the work copies (images and files.json) kept for a Windows round trip.
# Usage: sudo tools/linux-created-checks.sh [KIND...]
#   (kinds: simple mirror parity thin; default all)
set -euo pipefail
cd "$(dirname "$0")/.."
spaces=$PWD/target/release/spaces
export POOLS=/srv/spaces/work/lnxpools
pattern_mib=256
kinds=("$@")
((${#kinds[@]})) || kinds=(simple mirror parity thin)
make_pool() {
  local kind=$1 dir=$POOLS/lnx_$1
  rm -rf -- "${dir:?}"
  mkdir -p "$dir"
  for i in 0 1 2; do truncate -s 8G "$dir/disk$i.img"; done
  local disks=("$dir"/disk{0,1,2}.img) args
  case $kind in
    simple) args=(--resiliency simple --size 3G) ;;
    mirror) args=(--resiliency mirror --size 2G) ;;
    parity) args=(--resiliency parity --size 2G) ;;
    thin) args=(--resiliency mirror --size 4G --thin) ;;
  esac
  "$spaces" pool create --name "ss-lnx_$kind" --yes "${disks[@]}" | tail -1
  "$spaces" space create --name "lnx_$kind" "${args[@]}" --yes "${disks[@]}" | tail -1
  "$spaces" write-pattern "${disks[@]}" --space "lnx_$kind" --offset 0 --length $((pattern_mib << 20)) \
    --tag "lnx_$kind" --destage | tail -1
  python3 - "$dir/manifest.json" "$kind" $((pattern_mib << 20)) <<'PY'
import json, sys
path, kind, size = sys.argv[1], sys.argv[2], int(sys.argv[3])
json.dump({"name": "lnx_" + kind, "pattern": True, "pattern_size": size,
           "pool": {"name": "ss-lnx_" + kind}, "space": {"name": "lnx_" + kind}},
          open(path, "w"), indent=1)
PY
}
for kind in "${kinds[@]}"; do
  echo "=== $kind"
  make_pool "$kind"
  backends=(ublk nbd)
  [[ $kind == simple ]] && backends+=(dm)
  for b in "${backends[@]}"; do tools/rw-kernel-check.sh "lnx_$kind" "$b" 512 "$pattern_mib" | tail -3; done
  if [[ $kind == mirror || $kind == parity ]]; then
    tools/rw-ntfs-check.sh "lnx_$kind" ntfs-3g | tail -3
  fi
done
echo "linux-created checks: PASS (${kinds[*]})"
