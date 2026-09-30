#!/bin/bash
# Pools left by management operations cut off after a step, as a power loss
# would leave them, for Windows round trips (tools/put-roundtrip.sh
# DIR/STATE NAME): a base pool of three disks with a mirror and a parity
# space filled with the pattern, then, each on a copy of it:
#   create1   space create cut after its first step (the databases in the
#             metadata space written, the pool database not yet)
#   create2   space create cut after the first copy of the pool database
#   adddisk   disk add cut after the new disk's first update on one copy
#   retire    a fourth disk added, then disk retire cut in the middle of
#             moving a mirror slab: its data copied to the new disk, the
#             move recorded on one of the three pool database copies
# Usage: tools/mgmt-crash.sh DIR
set -euo pipefail
cd "$(dirname "$0")/.."
dir=$1
spaces=$PWD/target/release/spaces
rm -rf -- "${dir:?}"
mkdir -p "$dir/base"
for i in 0 1 2; do truncate -s 8G "$dir/base/disk$i.img"; done
base=("$dir"/base/disk0.img "$dir"/base/disk1.img "$dir"/base/disk2.img)
"$spaces" pool create --name ss-crash --yes "${base[@]}" | tail -1
"$spaces" space create --name cmirror --resiliency mirror --size 1G --yes "${base[@]}" | tail -1
"$spaces" space create --name cparity --resiliency parity --size 2G --yes "${base[@]}" | tail -1
"$spaces" write-pattern "${base[@]}" --space cmirror --offset 0 --length $((1 << 30)) --tag cmirror | tail -1
"$spaces" write-pattern "${base[@]}" --space cparity --offset 0 --length $((2 << 30)) --tag cparity --destage | tail -1
manifest() {
  python3 - "$1/manifest.json" <<'PY'
import json, sys
json.dump({"name": "crash", "pattern": True, "pattern_size": 1 << 30, "pool": {"name": "ss-crash"},
           "space": {"name": "cmirror"}, "extra_spaces": [{"name": "cparity", "size": 2 << 30}]},
          open(sys.argv[1], "w"), indent=1)
PY
}
state() {
  local name=$1 steps=$2
  shift 2
  mkdir -p "$dir/$name"
  for i in 0 1 2; do cp --sparse=always "$dir/base/disk$i.img" "$dir/$name/disk$i.img"; done
  local disks=("$dir/$name"/disk0.img "$dir/$name"/disk1.img "$dir/$name"/disk2.img)
  local extra=()
  if [[ $1 == disk && $2 == add ]]; then
    truncate -s 8G "$dir/$name/disk3.img"
    extra=(--new "$dir/$name/disk3.img")
  fi
  SPACES_STOP_AFTER_STEP=$steps "$spaces" "$@" "${extra[@]}" --yes "${disks[@]}" | grep -E "^stopped" || true
  manifest "$dir/$name"
  local all=("$dir/$name"/disk*.img)
  printf '%s: ' "$name"
  "$spaces" info "${all[@]}" 2>&1 | grep -cE '^ +[0-9]+ +[0-9a-f-]{36} +"' | sed 's/$/ spaces/'
  for s in cmirror cparity; do "$spaces" check-pattern "${all[@]}" --space "$s" --tag "$s" | tail -1; done
}
state create1 1 space create --name cnew --resiliency simple --size 1G
state create2 2 space create --name cnew --resiliency simple --size 1G
state adddisk 2 disk add
# Retire: on four disks (the parity space's rows use three). Steps 1-4 mark
# the disk retired, 5-8 move a hidden slab, 9 copies the mirror's slab and
# 10 records the move on the first database copy.
mkdir -p "$dir/retire"
for i in 0 1 2; do cp --sparse=always "$dir/base/disk$i.img" "$dir/retire/disk$i.img"; done
truncate -s 8G "$dir/retire/disk3.img"
"$spaces" disk add --new "$dir/retire/disk3.img" --yes "$dir"/retire/disk{0,1,2}.img | tail -1
four=("$dir"/retire/disk{0,1,2,3}.img)
retired=$("$spaces" info "${four[@]}" | awk '/device 0$/ {print $1}')
SPACES_STOP_AFTER_STEP=10 "$spaces" disk retire --disk "$retired" --yes "${four[@]}" | grep -E "^stopped|^step (9|10):" || true
manifest "$dir/retire"
for s in cmirror cparity; do "$spaces" check-pattern "${four[@]}" --space "$s" --tag "$s" | tail -1; done
echo "crash states: $dir/{create1,create2,adddisk,retire}"
