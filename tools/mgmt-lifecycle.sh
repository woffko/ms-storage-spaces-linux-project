#!/bin/bash
# Every management operation of `spaces` on one pool of sparse images, for a
# Windows round trip (tools/put-roundtrip.sh DIR NAME -Optimize):
#   pool create (3 disks); spaces: simple, mirror, parity, thin simple and one
#   to delete; simple and parity grown; the pattern written into each (the
#   tag is the space's final name); the extra space deleted; the pool and a
#   space renamed; a fourth disk added and set to SSD; disk 1 retired (its
#   data moved) and removed. DIR then holds the three remaining disks as
#   disk0-2.img (the removed one as removed.img) and manifest.json.
# Usage: tools/mgmt-lifecycle.sh DIR
set -euo pipefail
cd "$(dirname "$0")/.."
dir=$1
spaces=$PWD/target/release/spaces
rm -rf -- "${dir:?}"
mkdir -p "$dir"
for i in 0 1 2 3; do truncate -s 8G "$dir/d$i.img"; done
pool=("$dir"/d0.img "$dir"/d1.img "$dir"/d2.img)
q() { "$spaces" "$@" --yes | grep -vE "^step [0-9]+:" || true; }

q pool create --name ss-lifecycle "${pool[@]}"
q space create --name lsimple --resiliency simple --size 2G "${pool[@]}"
q space create --name lmirror --resiliency mirror --size 1G "${pool[@]}"
q space create --name lparity --resiliency parity --size 2G "${pool[@]}"
q space create --name lthin --resiliency simple --size 1500M --thin "${pool[@]}"
q space create --name ldel --resiliency simple --size 1G "${pool[@]}"
q space resize --space lsimple --size 5G "${pool[@]}"
q space resize --space lparity --size 4G "${pool[@]}"
q space rename --space lsimple --name lsimple2 "${pool[@]}"
q space delete --space ldel "${pool[@]}"
q pool rename --name ss-lifecycle2 "${pool[@]}"
declare -A sizes
while read -r name size; do sizes[$name]=$size; done < <(
  "$spaces" info "${pool[@]}" | awk '/"l[a-z0-9]+"/ {gsub(/"/, "", $3); name=$3} /^ +size / && name {print name, $2 * ($3 ~ /GiB/ ? 1073741824 : 1048576); name=""}')
for name in lsimple2 lmirror lparity lthin; do
  "$spaces" write-pattern "${pool[@]}" --space "$name" --offset 0 --length "${sizes[$name]}" --tag "$name" --destage | tail -1
done
added=$("$spaces" disk add --new "$dir/d3.img" --yes "${pool[@]}" | sed -n 's/.* as disk id \([0-9]*\)$/\1/p')
pool+=("$dir/d3.img")
q disk set --disk "$added" --media ssd "${pool[@]}"
# The disk on d1.img.
second=$("$spaces" info "${pool[@]}" | awk '/device 1$/ {print $1}')
q disk retire --disk "$second" "${pool[@]}"
q disk remove --disk "$second" "${pool[@]}"
mv "$dir/d1.img" "$dir/removed.img"
mv "$dir/d0.img" "$dir/disk0.img"
mv "$dir/d2.img" "$dir/disk1.img"
mv "$dir/d3.img" "$dir/disk2.img"
final=("$dir"/disk0.img "$dir"/disk1.img "$dir"/disk2.img)
"$spaces" info "${final[@]}"
for name in lsimple2 lmirror lparity lthin; do
  printf '%s: ' "$name"
  "$spaces" check-pattern "${final[@]}" --space "$name" --tag "$name" | tail -1
done
python3 - "$dir" "${sizes[lsimple2]}" "${sizes[lmirror]}" "${sizes[lparity]}" "${sizes[lthin]}" <<'PY'
import json, sys
d, s, m, p, t = sys.argv[1], *map(int, sys.argv[2:])
json.dump({"name": "lifecycle", "pattern": True, "pattern_size": s,
           "pool": {"name": "ss-lifecycle2"}, "space": {"name": "lsimple2"},
           "extra_spaces": [{"name": "lmirror", "size": m}, {"name": "lparity", "size": p}, {"name": "lthin", "size": t}]},
          open(f"{d}/manifest.json", "w"), indent=1)
PY
echo "lifecycle: PASS ($dir)"
