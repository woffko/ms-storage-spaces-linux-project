#!/bin/bash
# Run on the Linux test VM as root. Attaches every corpus pool through loop
# devices with every backend and verifies the test pattern through
# /dev/mapper, sequentially and with random reads.
# Usage: tools/backend-matrix.sh [corpus dir]   (BACKENDS="dm ublk nbd fuse")
set -uo pipefail
corpus=${1:-/srv/spaces/pools}
backends=${BACKENDS:-"dm ublk nbd fuse"}
# Keep the udev-triggered service from attaching the loop devices itself.
systemctl mask --runtime --quiet storage-spaces-attach.service
trap 'systemctl unmask --runtime --quiet storage-spaces-attach.service' EXIT
json() { python3 -c 'import json,sys; m=json.load(open(sys.argv[1],encoding="utf-8-sig")); print(eval(sys.argv[2]))' "$@"; }
pass=0; fail=0; skip=0
for dir in "$corpus"/*/; do
  dir=${dir%/}; name=$(basename "$dir")
  [[ -f $dir/manifest.json ]] || continue
  space=$(json "$dir/manifest.json" 'm["space"]["name"]')
  plen=$(json "$dir/manifest.json" 'm.get("pattern_size") or m["space"]["size"]')
  loops=()
  for img in "$dir"/disk*.img; do loops+=("$(losetup -r -P -f --show "$img")"); done
  udevadm settle
  parts=(); for l in "${loops[@]}"; do parts+=("${l}p2"); done
  for b in $backends; do
    if ! out=$(spaces attach --backend "$b" "${parts[@]}" 2>&1); then
      if [[ $b == dm && $out == *"cannot map"* ]]; then
        echo "SKIP $name/$b (no linear mapping)"; skip=$((skip + 1)); continue
      fi
      echo "FAIL $name/$b attach: $out"; fail=$((fail + 1)); spaces detach >/dev/null 2>&1; continue
    fi
    dev=$(spaces status | awk '/^\/dev\/mapper/{print $1; exit}')
    if spaces verify-pattern "$dev" --tag "$space" --length "$plen" >/dev/null &&
       spaces verify-pattern "$dev" --tag "$space" --length "$plen" --random 300 >/dev/null; then
      echo "PASS $name/$b ($dev, $(blockdev --getss "$dev")-byte sectors)"; pass=$((pass + 1))
    else
      echo "FAIL $name/$b verify"; fail=$((fail + 1))
    fi
    spaces detach >/dev/null || echo "WARN detach failed for $name/$b"
  done
  for l in "${loops[@]}"; do losetup -d "$l"; done
done
echo "passed $pass, failed $fail, skipped $skip"
((fail == 0))
