#!/bin/bash
# A failed disk replaced, then a scrub that repairs, all on Linux, for a
# Windows round trip (tools/put-roundtrip.sh DIR NAME): a pool of three
# sparse images with a two-way mirror and a parity space filled with the
# pattern; disk 2 "fails" (left out), a blank disk is added to the other
# two, the pool repaired onto it and the missing disk removed. Then 1 MiB of
# the mirror's second copy is overwritten, `spaces pool scrub` must find it
# (scrub.txt) and `--repair` make the copies agree again. DIR then holds the three
# member disks as disk0-2.img (the failed one as failed.img) and
# manifest.json.
# Usage: tools/mgmt-replace.sh DIR
set -euo pipefail
cd "$(dirname "$0")/.."
dir=$1
spaces=$PWD/target/release/spaces
rm -rf -- "${dir:?}"
mkdir -p "$dir"
for i in 0 1 2 3; do truncate -s 8G "$dir/d$i.img"; done
q() { "$spaces" "$@" --yes | grep -vE "^step [0-9]+:" || true; }
q pool create --name ss-replace "$dir"/d{0,1,2}.img
q space create --name rmirror --resiliency mirror --size 1G "$dir"/d{0,1,2}.img
q space create --name rparity --resiliency parity --size 2G "$dir"/d{0,1,2}.img
"$spaces" write-pattern "$dir"/d{0,1,2}.img --space rmirror --offset 0 --length $((1 << 30)) --tag rmirror | tail -1
"$spaces" write-pattern "$dir"/d{0,1,2}.img --space rparity --offset 0 --length $((2 << 30)) --tag rparity --destage | tail -1
failed=$("$spaces" info "$dir"/d{0,1,2}.img | awk '/device 2$/ {print $1}')
mv "$dir/d2.img" "$dir/failed.img"
left=("$dir"/d0.img "$dir"/d1.img)
"$spaces" pool health "${left[@]}" 2>/dev/null | head -1
q disk add --new "$dir/d3.img" "${left[@]}"
now=("$dir"/d0.img "$dir"/d1.img "$dir"/d3.img)
q pool repair "${now[@]}"
q disk remove --disk "$failed" "${now[@]}"
"$spaces" pool health "${now[@]}"
# Overwrite 1 MiB at 3 MiB into the mirror's second copy.
read -r disk slab < <("$spaces" extents "${now[@]}" --space rmirror | awk '$2 == 0 && $3 == 1 {print $5, $6; exit}')
device=$("$spaces" info "${now[@]}" | awk -v d="$disk" '$1 == d && /device [0-9]+$/ {print $NF}')
offset=$(((16 << 20) + (512 << 20) + slab + (3 << 20)))
head -c $((1 << 20)) /dev/urandom | dd of="${now[$device]}" bs=1M seek=$((offset >> 20)) conv=notrunc status=none
# The pattern was written on Linux, so the dirty region log lists the
# mirror's run: the difference counts as one where writes were under way.
"$spaces" pool scrub "${now[@]}" | tee "$dir/scrub.txt" | grep -q "no differences" &&
  { echo "scrub missed the damage" >&2; exit 1; }
cat "$dir/scrub.txt"
q pool scrub --repair "${now[@]}"
"$spaces" pool scrub "${now[@]}" | tail -1
for i in 0 1 2; do mv "${now[$i]}" "$dir/disk$i.img"; done
final=("$dir"/disk{0,1,2}.img)
for s in rmirror rparity; do "$spaces" check-pattern "${final[@]}" --space "$s" --tag "$s" | tail -1; done
python3 - "$dir/manifest.json" <<'PY'
import json, sys
json.dump({"name": "replace", "pattern": True, "pattern_size": 1 << 30, "pool": {"name": "ss-replace"},
           "space": {"name": "rmirror"}, "extra_spaces": [{"name": "rparity", "size": 2 << 30}]},
          open(sys.argv[1], "w"), indent=1)
PY
echo "replace and scrub: PASS ($dir)"
