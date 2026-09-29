#!/bin/bash
# Write through a kernel block device on a copy of a corpus pool and check
# the data (run on the Linux test VM as root, from the repository):
#   1. copy /srv/spaces/pools/POOL to /srv/spaces/work/POOL-BACKEND
#   2. expose the space writable through BACKEND (ublk, nbd, or dm on loop
#      devices of the images; dm only for simple spaces)
#   3. fio: random writes of 4 KiB to 1 MiB with crc32c verification over
#      MIB MiB from START MiB on (default 256; beyond the allocated part of
#      a thin space the writes allocate rows), then read everything back
#      and verify
#   4. expose it again read-only and verify the fio data once more (it
#      reached the images), and check that the verification pattern outside
#      the fio range is intact
# Usage: sudo tools/rw-kernel-check.sh POOL BACKEND [MIB] [START]
set -euo pipefail
cd "$(dirname "$0")/.."
pool=$1 backend=$2 mib=${3:-512} start_mib=${4:-256}
spaces=$PWD/target/release/spaces
src=/srv/spaces/pools/$pool
work=/srv/spaces/work/$pool-$backend
space=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1],encoding="utf-8-sig"))["space"]["name"])' "$src/manifest.json")
pattern=$(python3 -c 'import json,sys; m=json.load(open(sys.argv[1],encoding="utf-8-sig")); print(m["pattern_size"] if m["pattern"] else 0)' "$src/manifest.json")
rm -rf -- "${work:?}"
mkdir -p "$work"
cp --sparse=always "$src"/disk*.img "$src"/manifest.json "$work/"
disks=("$work"/disk*.img)
fio_start=$((start_mib << 20))
fio_len=$((mib << 20))

pid=
dev=
loops=()
cleanup() {
  case $backend in
    nbd) [[ -n $dev ]] && nbd-client -d "$dev" >/dev/null 2>&1 || true ;;
    dm) dmsetup remove "ss-rwcheck" 2>/dev/null || true ;;
  esac
  [[ -n $pid ]] && kill "$pid" 2>/dev/null && wait "$pid" 2>/dev/null || true
  for l in "${loops[@]}"; do losetup -d "$l" 2>/dev/null || true; done
  loops=() pid= dev=
}
trap cleanup EXIT

# Exposes the space; $1 = rw or ro. Sets $dev.
expose() {
  local mode=$1 ready=$work/ready
  rm -f "$ready"
  local rw=()
  [[ $mode == rw ]] && rw=(--rw)
  case $backend in
    ublk)
      "$spaces" serve-ublk "${disks[@]}" --space "$space" "${rw[@]}" --ready-file "$ready" >/dev/null &
      pid=$!
      for _ in $(seq 6000); do [[ -s $ready ]] && break; kill -0 "$pid" 2>/dev/null || break; sleep 0.1; done
      dev=$(cat "$ready")
      ;;
    nbd)
      rm -f "$work/sock"
      "$spaces" serve-nbd "${disks[@]}" --space "$space" "${rw[@]}" --socket "$work/sock" --ready-file "$ready" >/dev/null 2>&1 &
      pid=$!
      for _ in $(seq 6000); do [[ -s $ready ]] && break; kill -0 "$pid" 2>/dev/null || break; sleep 0.1; done
      dev=
      for d in /dev/nbd*; do
        [[ $d =~ ^/dev/nbd[0-9]+$ ]] || continue
        [[ $(cat /sys/block/${d#/dev/}/size) == 0 ]] || continue
        local ro=()
        [[ $mode == ro ]] && ro=(-R)
        nbd-client -N "$space" -u "$work/sock" "$d" -b 512 "${ro[@]}" >/dev/null && dev=$d && break
      done
      ;;
    dm)
      for d in "${disks[@]}"; do
        local lopt=()
        [[ $mode == ro ]] && lopt=(-r)
        loops+=("$(losetup "${lopt[@]}" -f --show "$d")")
      done
      local table
      table=$("$spaces" dm-table "${loops[@]}" --space "$space")
      local ro=()
      [[ $mode == ro ]] && ro=(--readonly)
      dmsetup create "${ro[@]}" ss-rwcheck <<<"$table"
      dev=/dev/mapper/ss-rwcheck
      ;;
  esac
  [[ -n $dev && -b $dev ]] || { echo "no device" >&2; exit 1; }
}

fio_job() {
  fio --name=rwcheck --filename="$dev" --offset="$fio_start" --size="$fio_len" --direct=1 \
    --ioengine=libaio --iodepth=32 --numjobs=1 --bsrange=4k-1m --randrepeat=1 \
    --verify=crc32c --verify_fatal=1 --output-format=json "$@" |
    python3 -c 'import json,sys
text = sys.stdin.read(); j = json.loads(text[text.find("{"):])
job = j["jobs"][0]
print("fio: wrote %d MiB, read %d MiB, error %d" % (job["write"]["io_kbytes"] >> 10, job["read"]["io_kbytes"] >> 10, job["error"]))
sys.exit(1 if job["error"] else 0)'
}

expose rw
echo "$backend: $dev (writable)"
fio_job --rw=randwrite --do_verify=1
cleanup
expose ro
echo "$backend: $dev (read-only again)"
fio_job --rw=randwrite --verify_only
cleanup
# The pattern outside the fio range, through the library.
if ((pattern > 0)); then
  "$spaces" check-pattern "${disks[@]}" --space "$space" --length $((pattern < fio_start ? pattern : fio_start)) >/dev/null
  echo "pattern intact below the fio range"
fi
rm -rf -- "${work:?}"
echo "$pool $backend: PASS"
