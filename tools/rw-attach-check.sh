#!/bin/bash
# `spaces attach --rw` on a copy of a corpus pool (run on the Linux test VM
# as root, from the repository):
#   1. copy /srv/spaces/pools/POOL to /srv/spaces/work/POOL-attach and put
#      read-write loop devices and linear device-mapper wrappers over the
#      Storage Spaces partitions of the copies
#   2. attach the space read-write (BACKEND, default auto) and run fio
#      random writes with crc32c verification through /dev/mapper
#   3. detach, attach it read-only and verify the fio data again
#   4. detach, and check the verification pattern below the fio range
# Usage: sudo tools/rw-attach-check.sh POOL [BACKEND] [MIB]
set -euo pipefail
cd "$(dirname "$0")/.."
pool=$1 backend=${2:-auto} mib=${3:-256}
spaces=$PWD/target/release/spaces
src=/srv/spaces/pools/$pool
work=/srv/spaces/work/$pool-attach
space=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1],encoding="utf-8-sig"))["space"]["name"])' "$src/manifest.json")
rm -rf -- "${work:?}"
mkdir -p "$work"
cp --sparse=always "$src"/disk*.img "$src"/manifest.json "$work/"
loops=() parts=() dev=
cleanup() {
  [[ -n $dev ]] && "$spaces" detach "$(basename "$dev")" >/dev/null 2>&1 || true
  for p in "${parts[@]}"; do dmsetup remove "$(basename "$p")" 2>/dev/null || true; done
  for l in "${loops[@]}"; do losetup -d "$l" 2>/dev/null || true; done
}
trap cleanup EXIT
k=0
for img in "$work"/disk*.img; do
  for bs in 512 4096; do
    l=$(losetup -b "$bs" -f --show "$img")
    read -r start size < <(sfdisk -d "$l" 2>/dev/null | awk -F'[=,]' '/type=E75CAF8F/{gsub(/ /,""); print $2, $4}')
    [[ -n $start ]] && break
    losetup -d "$l"
  done
  loops+=("$l")
  echo "0 $((size * bs / 512)) linear $l $((start * bs / 512))" | dmsetup create "rwattach-$k"
  parts+=("/dev/mapper/rwattach-$k")
  k=$((k + 1))
done

attach() {
  local out
  out=$("$spaces" attach --backend "$backend" --space "$space" "$@" "${parts[@]}")
  dev=$(awk '/^  \/dev\/mapper/{print $1; exit}' <<<"$out")
  echo "$(head -1 <<<"$out") $dev"
}
detach() {
  "$spaces" detach "$(basename "$dev")" >/dev/null
  dev=
}
fio_job() {
  fio --name=rwattach --filename="$dev" --offset=$((256 << 20)) --size=$((mib << 20)) --direct=1 \
    --ioengine=libaio --iodepth=32 --numjobs=1 --bsrange=4k-1m --randrepeat=1 \
    --verify=crc32c --verify_fatal=1 --output-format=json "$@" |
    python3 -c 'import json,sys
text = sys.stdin.read(); j = json.loads(text[text.find("{"):])
job = j["jobs"][0]
print("fio: wrote %d MiB, read %d MiB, error %d" % (job["write"]["io_kbytes"] >> 10, job["read"]["io_kbytes"] >> 10, job["error"]))
sys.exit(1 if job["error"] else 0)'
}

attach --rw
fio_job --rw=randwrite --do_verify=1
detach
attach
fio_job --rw=randwrite --verify_only
detach
cleanup
loops=() parts=()
trap - EXIT
"$spaces" check-pattern "$work"/disk*.img --space "$space" --length $((256 << 20)) >/dev/null
echo "pattern intact below the fio range"
echo "$pool $backend: PASS ($work)"
