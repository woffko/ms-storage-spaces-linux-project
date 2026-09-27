#!/bin/bash
# Run on the Linux test VM as root with spaces, the udev rule and the systemd
# unit installed. For every NTFS pool (manifest "files") the member images
# are attached as partition-scanned loop devices, so that udev and
# storage-spaces-attach.service assemble the pool like plugged-in disks; the
# NTFS partition of the space is mounted read-only with ntfs3 and every file
# is compared with the SHA-256 Windows recorded.
# Usage: tools/ntfs-check.sh [corpus dir] [pool...]
set -uo pipefail
corpus=${1:-/srv/spaces/pools}
shift || true
only=" $* "
json() { python3 -c 'import json,sys; m=json.load(open(sys.argv[1],encoding="utf-8-sig")); print(eval(sys.argv[2]))' "$@"; }
pass=0; fail=0
for dir in "$corpus"/*/; do
  dir=${dir%/}; name=$(basename "$dir")
  [[ -f $dir/manifest.json ]] || continue
  [[ $only != "  " && $only != *" $name "* ]] && continue
  [[ $(json "$dir/manifest.json" 'len(m.get("files") or [])') -gt 0 ]] || continue
  guid=$(json "$dir/manifest.json" 'm["space"]["guid"]')
  loops=()
  for img in "$dir"/disk*.img; do loops+=("$(losetup -r -P -f --show "$img")"); done
  # udev starts storage-spaces-attach.service for the new members.
  dev=""
  for _ in $(seq 60); do
    dev=$(spaces status 2>/dev/null | awk -v g="$guid" '/^\/dev\/mapper\/ss-/ && $3 == g {print $1; exit}')
    [[ -n $dev ]] && break
    sleep 1
  done
  if [[ -z $dev ]]; then
    echo "FAIL $name: not attached automatically"; fail=$((fail + 1))
  else
    part=""
    for p in "$dev"-p*; do [[ $(blkid -o value -s TYPE "$p") == ntfs ]] && part=$p; done
    mnt=$(mktemp -d)
    if [[ -z $part ]] || ! mount -t ntfs3 -o ro "$part" "$mnt"; then
      echo "FAIL $name: cannot mount the NTFS partition of $dev"; fail=$((fail + 1))
    else
      bad=$(cd "$mnt" && python3 - "$dir/manifest.json" <<'PY'
import hashlib, json, sys
m = json.load(open(sys.argv[1], encoding="utf-8-sig"))
bad = 0
for f in m["files"]:
    h = hashlib.sha256()
    try:
        with open(f["path"], "rb") as fh:
            for chunk in iter(lambda: fh.read(1 << 20), b""):
                h.update(chunk)
    except OSError as e:
        print(f"  {f['path']}: {e}", file=sys.stderr); bad += 1; continue
    if h.hexdigest() != f["sha256"]:
        print(f"  {f['path']}: checksum differs", file=sys.stderr); bad += 1
print(bad)
PY
)
      files=$(json "$dir/manifest.json" 'len(m["files"])')
      if [[ $bad == 0 ]]; then
        echo "PASS $name: $files files match through $part ($(spaces status | awk -v d="$dev" '$1 == d {print $5}'))"; pass=$((pass + 1))
      else
        echo "FAIL $name: $bad of $files files differ"; fail=$((fail + 1))
      fi
      umount "$mnt"
    fi
    rmdir "$mnt"
    spaces detach "$(basename "$dev")" >/dev/null || echo "WARN detach failed for $name"
  fi
  for l in "${loops[@]}"; do losetup -d "$l"; done
  udevadm settle
done
echo "passed $pass, failed $fail"
((fail == 0))
