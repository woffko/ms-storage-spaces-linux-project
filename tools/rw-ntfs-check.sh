#!/bin/bash
# NTFS written from Linux on a copy of a corpus pool (run on the Linux test
# VM as root, from the repository):
#   1. copy /srv/spaces/pools/POOL to /srv/spaces/work/POOL-ntfs
#   2. expose the space writable through ublk, give it a GPT with one
#      Microsoft basic data partition and an NTFS file system (mkntfs)
#   3. mount it read-write with DRIVER (ntfs3 or ntfs-3g) and run
#      tools/ntfs-stress.py, leaving out the operations in SKIP
#   4. expose it read-only, mount it read-only and check every file's hash
# The images and files.json (for the Windows round trip) stay in the work
# directory. ntfs3 of Linux 6.8 truncates small resident files to zero in a
# way Windows' chkdsk reports as corrupt, also on plain disks, so run it
# with SKIP=truncate.
# PART_MB limits the partition (default: the whole space), e.g. to what a
# thin pool can allocate. TRIM_MB adds a step after 3: write a file of that
# size, delete it and fstrim (thin spaces give rows back); trim.txt records
# the slabs allocated before and after.
# Usage: sudo tools/rw-ntfs-check.sh POOL [DRIVER] [SKIP] [COPY_MB] [OPS]
set -euo pipefail
cd "$(dirname "$0")/.."
pool=$1 driver=${2:-ntfs3} skip=${3:-} copy_mb=${4:-300} ops=${5:-3000}
spaces=$PWD/target/release/spaces
src=/srv/spaces/pools/$pool
work=/srv/spaces/work/$pool-$driver
mnt=/mnt/rw-ntfs-check
space=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1],encoding="utf-8-sig"))["space"]["name"])' "$src/manifest.json")
rm -rf -- "${work:?}"
mkdir -p "$work" "$mnt"
cp --sparse=always "$src"/disk*.img "$src"/manifest.json "$work/"
disks=("$work"/disk*.img)
pid=
cleanup() {
  mountpoint -q "$mnt" && umount "$mnt" || true
  [[ -n $pid ]] && kill "$pid" 2>/dev/null && wait "$pid" 2>/dev/null || true
  pid=
}
trap cleanup EXIT

expose() {
  local ready=$work/ready
  rm -f "$ready"
  "$spaces" serve-ublk "${disks[@]}" --space "$space" "$@" --ready-file "$ready" >/dev/null 2>>"$work/server.log" &
  pid=$!
  for _ in $(seq 6000); do [[ -s $ready ]] && break; kill -0 "$pid" 2>/dev/null || break; sleep 0.1; done
  dev=$(cat "$ready")
}

expose --rw
end=0
[[ -n ${PART_MB:-} ]] && end=+${PART_MB}M
sgdisk -o -n 1:2048:$end -t 1:0700 -c 1:linux "$dev" >/dev/null
partprobe "$dev"
udevadm settle
mkntfs -f -Q -L linux "${dev}p1" >/dev/null
mount -t "$driver" "${dev}p1" "$mnt"
python3 tools/ntfs-stress.py "$mnt" /usr/lib/x86_64-linux-gnu "$copy_mb" "$ops" 42 "$work/files.json" "$skip"
umount "$mnt"
cleanup
# The slabs the pool database lists for the space.
slabs() { "$spaces" extents --space "$space" "${disks[@]}" | grep -cE '^ *[0-9]'; }
if [[ -n ${TRIM_MB:-} ]]; then
  expose --rw
  mount -t "$driver" "${dev}p1" "$mnt"
  dd if=/dev/urandom of="$mnt/trim.bin" bs=1M count="$TRIM_MB" status=none
  umount "$mnt"
  cleanup
  before=$(slabs)
  expose --rw
  mount -t "$driver" "${dev}p1" "$mnt"
  rm "$mnt/trim.bin"
  sync
  fstrim -v "$mnt"
  umount "$mnt"
  cleanup
  echo "slabs allocated with a $TRIM_MB MiB file: $before; after deleting it and fstrim: $(slabs)" | tee "$work/trim.txt"
  (($(slabs) < before)) || { echo "fstrim gave nothing back" >&2; exit 1; }
fi
# A failed write (a thin pool out of slabs, say) can hide in the file
# system's cache until it is too late for the model check.
if grep -q "failed" "$work/server.log"; then
  echo "writes failed:" >&2
  grep "failed" "$work/server.log" | head -5 >&2
  exit 1
fi

expose
mount -t "$driver" -o ro "${dev}p1" "$mnt"
python3 - "$mnt" "$work/files.json" <<'PY'
import hashlib, json, os, sys
root, files = sys.argv[1], json.load(open(sys.argv[2]))
bad = [f["path"] for f in files
       if hashlib.sha256(open(os.path.join(root, f["path"]), "rb").read()).hexdigest() != f["sha256"]]
print("read-only again: %d files, %d mismatching" % (len(files), len(bad)))
sys.exit(1 if bad else 0)
PY
cleanup
echo "$pool $driver: PASS ($work)"
