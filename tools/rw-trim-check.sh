#!/bin/bash
# Discards (TRIM) through a kernel block device on a copy of a thin simple
# or mirror corpus pool (run on the Linux test VM as root, from the
# repository):
#   1. copy /srv/spaces/pools/POOL to /srv/spaces/work/POOL-trim
#   2. expose the space writable through BACKEND (ublk or nbd), write rows
#      4 to 9, then discard rows 4 and 5 in 128 MiB pieces out of order, row
#      6 but for its first 4 KiB, and row 7 in one discard with 4 KiB of each
#      neighbour; rows 4, 5 and 7 must read zeros, 6, 8 and 9 their data
#   3. expose it read-only: the same again, the pool database lists rows 6,
#      8 and 9 and not 4, 5 and 7, and the verification pattern below row 4
#      is intact
#   4. ext4: mkfs (which discards the whole space), write files, delete all
#      but one, fstrim: rows are given back, e2fsck is clean and the file
#      kept is intact
# Usage: sudo tools/rw-trim-check.sh POOL BACKEND
set -euo pipefail
cd "$(dirname "$0")/.."
pool=$1 backend=$2
spaces=$PWD/target/release/spaces
src=/srv/spaces/pools/$pool
work=/srv/spaces/work/$pool-trim
mnt=/mnt/rw-trim-check
space=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1],encoding="utf-8-sig"))["space"]["name"])' "$src/manifest.json")
pattern=$(python3 -c 'import json,sys; m=json.load(open(sys.argv[1],encoding="utf-8-sig")); print(m["pattern_size"] if m["pattern"] else 0)' "$src/manifest.json")
rm -rf -- "${work:?}"
mkdir -p "$work" "$mnt"
cp --sparse=always "$src"/disk*.img "$src"/manifest.json "$work/"
disks=("$work"/disk*.img)

pid= dev=
cleanup() {
  mountpoint -q "$mnt" && umount "$mnt" || true
  [[ $backend == nbd && -n $dev ]] && nbd-client -d "$dev" >/dev/null 2>&1 || true
  [[ -n $pid ]] && kill "$pid" 2>/dev/null && wait "$pid" 2>/dev/null || true
  pid= dev=
}
trap cleanup EXIT

# Exposes the space; $1 = rw or ro. Sets $dev.
expose() {
  local mode=$1 ready=$work/ready rw=()
  rm -f "$ready"
  [[ $mode == rw ]] && rw=(--rw)
  case $backend in
    ublk)
      "$spaces" serve-ublk "${disks[@]}" --space "$space" "${rw[@]}" --ready-file "$ready" >/dev/null 2>>"$work/server.log" &
      pid=$!
      for _ in $(seq 6000); do [[ -s $ready ]] && break; kill -0 "$pid" 2>/dev/null || break; sleep 0.1; done
      dev=$(cat "$ready")
      ;;
    nbd)
      rm -f "$work/sock"
      "$spaces" serve-nbd "${disks[@]}" --space "$space" "${rw[@]}" --socket "$work/sock" --ready-file "$ready" >/dev/null 2>>"$work/server.log" &
      pid=$!
      for _ in $(seq 6000); do [[ -s $ready ]] && break; kill -0 "$pid" 2>/dev/null || break; sleep 0.1; done
      local d ro=()
      [[ $mode == ro ]] && ro=(-R)
      for d in /dev/nbd*; do
        [[ $d =~ ^/dev/nbd[0-9]+$ && $(cat "/sys/block/${d#/dev/}/size") == 0 ]] || continue
        nbd-client -N "$space" -u "$work/sock" "$d" -b 512 "${ro[@]}" >/dev/null && dev=$d && break
      done
      ;;
  esac
  [[ -n $dev && -b $dev ]] || { echo "no device" >&2; exit 1; }
}

# The rows the pool database lists for the space, one per line.
rows() {
  "$spaces" extents --space "$space" "${disks[@]}" | awk -v c="$columns" '$1 ~ /^[0-9]+$/ {print $1 / c}' | sort -nu
}
columns=$("$spaces" extents --space "$space" "${disks[@]}" | awk '$1 ~ /^[0-9]+$/ {print $2}' | sort -u | wc -l)
row=$((columns << 28))
echo "$pool: $columns column(s), rows of $((row >> 20)) MiB; allocated rows: $(rows | tr '\n' ' ')"

# Writes rows $1 to $2 with blocks naming their offset.
fill() {
  python3 -c '
import sys
start, end = int(sys.argv[1]), int(sys.argv[2])
for o in range(start, end, 1 << 20):
    sys.stdout.buffer.write(("trim:%016x:" % o).encode().ljust(64, b".") * 16384)
' $(($1 * row)) $((($2 + 1) * row)) |
    dd of="$dev" bs=1M seek=$(($1 * row >> 20)) oflag=direct conv=fsync iflag=fullblock status=none
}
# Prints what row $1 reads: zeros, data (as written by fill) or mixed.
content() {
  dd if="$dev" bs=1M skip=$(($1 * row >> 20)) count=$((row >> 20)) iflag=direct status=none |
    python3 -c '
import sys
start = int(sys.argv[1])
kinds = set()
o = start
while block := sys.stdin.buffer.read(1 << 20):
    if block == bytes(len(block)):
        kinds.add("zeros")
    elif block == ("trim:%016x:" % o).encode().ljust(64, b".") * 16384:
        kinds.add("data")
    else:
        kinds.add("other")
    o += 1 << 20
print(kinds.pop() if len(kinds) == 1 else "mixed")
' $(($1 * row))
}
expect() {
  local r got
  for r in 4 5 6 7 8 9; do
    got=$(content "$r")
    case $r in 4 | 5 | 7) want=zeros ;; *) want=data ;; esac
    [[ $got == "$want" ]] || { echo "row $r reads $got, not $want" >&2; exit 1; }
  done
  echo "rows 4, 5 and 7 read zeros, rows 6, 8 and 9 their data"
}

expose rw
echo "$backend: $dev (writable)"
fill 4 9
pieces=()
for r in 4 5; do
  for ((o = 0; o < row; o += 128 << 20)); do pieces+=($((r * row + o))); done
done
for p in $(printf '%s\n' "${pieces[@]}" | shuf --random-source=<(yes)); do
  blkdiscard -o "$p" -l $((128 << 20)) "$dev"
done
blkdiscard -o $((6 * row + 4096)) -l $((row - 4096)) "$dev"
blkdiscard -o $((7 * row - 4096)) -l $((row + 8192)) "$dev"
expect
cleanup

got=$(rows | tr '\n' ' ')
echo "allocated rows: $got"
for r in 4 5 7; do grep -qw "$r" <<<"$got" && { echo "row $r still allocated" >&2; exit 1; }; done
for r in 6 8 9; do grep -qw "$r" <<<"$got" || { echo "row $r not allocated" >&2; exit 1; }; done
expose ro
expect
cleanup
if ((pattern > 0)); then
  "$spaces" check-pattern "${disks[@]}" --space "$space" --length $((pattern < 4 * row ? pattern : 4 * row)) >/dev/null
  echo "pattern intact below row 4"
fi

# ext4 trims by block groups of 128 MiB.
expose rw
mkfs.ext4 -q -F -E lazy_itable_init=0,lazy_journal_init=0 "$dev"
echo "mkfs.ext4: allocated rows after its discard: $(rows | wc -l) (read while serving)"
mount "$dev" "$mnt"
for i in 1 2 3 4 5 6; do dd if=/dev/urandom of="$mnt/big$i" bs=1M count=256 status=none; done
dd if=/dev/urandom of="$mnt/kept" bs=1M count=16 status=none
sum=$(sha256sum <"$mnt/kept")
umount "$mnt"
cleanup
before=$(rows | wc -l)
expose rw
mount "$dev" "$mnt"
rm "$mnt"/big*
# ext4 trims freed blocks only once the transaction freeing them commits.
sync
fstrim -v "$mnt"
umount "$mnt"
cleanup
after=$(rows | wc -l)
echo "ext4: $before rows allocated with the files, $after after deleting them and fstrim"
((after < before)) || { echo "fstrim gave nothing back" >&2; exit 1; }
expose ro
e2fsck -fn "$dev" >/dev/null
mount -o ro "$dev" "$mnt"
[[ $(sha256sum <"$mnt/kept") == "$sum" ]] || { echo "kept file differs" >&2; exit 1; }
cleanup
echo "e2fsck clean, file kept intact"
if "$spaces" info "${disks[@]}" | grep -iE "warning|stale|torn"; then
  echo "pool warnings" >&2
  exit 1
fi
if grep -q "failed" "$work/server.log"; then
  grep "failed" "$work/server.log" | head -5 >&2
  exit 1
fi
rm -rf -- "${work:?}"
echo "$pool $backend: PASS"
