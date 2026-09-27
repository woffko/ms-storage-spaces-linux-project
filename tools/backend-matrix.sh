#!/bin/bash
# Run on the Linux test VM as root. Attaches every corpus pool through loop
# devices with every backend and verifies the test pattern through
# /dev/mapper: sequentially, with random reads, with random O_DIRECT reads
# from 4 threads, and (when fio is installed) under a 20 s fio random-read
# load of queue depth 32 x 4 jobs, which must finish without I/O errors.
# Usage: tools/backend-matrix.sh [corpus dir] [pool...]
#        (BACKENDS="dm ublk nbd fuse", FIO_SECONDS=20)
set -uo pipefail
corpus=${1:-/srv/spaces/pools}
shift || true
only=" $* "
backends=${BACKENDS:-"dm ublk nbd fuse"}
fio_seconds=${FIO_SECONDS:-20}
json() { python3 -c 'import json,sys; m=json.load(open(sys.argv[1],encoding="utf-8-sig")); print(eval(sys.argv[2]))' "$@"; }
pass=0; fail=0; skip=0
for dir in "$corpus"/*/; do
  dir=${dir%/}; name=$(basename "$dir")
  [[ -f $dir/manifest.json ]] || continue
  [[ $only != "  " && $only != *" $name "* ]] && continue
  space=$(json "$dir/manifest.json" 'm["space"]["name"]')
  plen=$(json "$dir/manifest.json" 'm.get("pattern_size") or m["space"]["size"]')
  # Loop devices without partition scanning (so the udev rule does not
  # attach them) plus a linear device-mapper wrapper over the Storage
  # Spaces partition of each image.
  loops=(); parts=(); k=0
  for img in "$dir"/disk*.img; do
    l=$(losetup -r -f --show "$img"); loops+=("$l")
    read -r start size < <(sfdisk -d "$l" | awk -F'[=,]' '/type=E75CAF8F/{gsub(/ /,""); print $2, $4}')
    echo "0 $size linear $l $start" | dmsetup create --readonly "matrix-$k"
    parts+=("/dev/mapper/matrix-$k"); k=$((k + 1))
  done
  for b in $backends; do
    if ! out=$(spaces attach --backend "$b" "${parts[@]}" 2>&1); then
      if [[ $b == dm && $out == *"cannot map"* ]]; then
        echo "SKIP $name/$b (no linear mapping)"; skip=$((skip + 1)); continue
      fi
      echo "FAIL $name/$b attach: $out"; fail=$((fail + 1)); continue
    fi
    dev=$(awk '/^  \/dev\/mapper/{print $1; exit}' <<<"$out")
    fio_ok=1
    if command -v fio >/dev/null; then
      fio --name=matrix --filename="$dev" --readonly --rw=randread --bsrange=4k-1m --direct=1 \
        --ioengine=libaio --iodepth=32 --numjobs=4 --time_based --runtime="$fio_seconds" \
        --group_reporting --output-format=terse --terse-version=3 >"/tmp/matrix-fio.$$" 2>&1 || fio_ok=0
      # Terse v3: field 5 is the error code of the job group.
      [[ $(awk -F';' 'NR==1{print $5}' "/tmp/matrix-fio.$$") == 0 ]] || fio_ok=0
      rm -f "/tmp/matrix-fio.$$"
    fi
    if spaces verify-pattern "$dev" --tag "$space" --length "$plen" >/dev/null &&
       spaces verify-pattern "$dev" --tag "$space" --length "$plen" --random 300 >/dev/null &&
       spaces verify-pattern "$dev" --tag "$space" --length "$plen" --random 2000 --jobs 4 --direct >/dev/null &&
       ((fio_ok)); then
      echo "PASS $name/$b ($dev, $(blockdev --getss "$dev")-byte sectors)"; pass=$((pass + 1))
    else
      echo "FAIL $name/$b verify"; fail=$((fail + 1))
    fi
    spaces detach "$(basename "$dev")" >/dev/null || echo "WARN detach failed for $name/$b"
  done
  for ((i = 0; i < k; i++)); do dmsetup remove "matrix-$i"; done
  for l in "${loops[@]}"; do losetup -d "$l"; done
done
echo "passed $pass, failed $fail, skipped $skip"
((fail == 0))
