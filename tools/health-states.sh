#!/bin/bash
# Pools with disks missing, for Windows' health states: a pool of four disks
# created on Linux with a one-column simple space, a four-column simple
# space, a one- and a two-column two-way mirror and a parity space. Then
# DIR/<state> holds the disks Windows gets (links into DIR/base) and a
# manifest without patterns:
#   all      every disk
#   no<N>    all but base disk N (N = 0..3)
#   only01   base disks 0 and 1
#   only0    base disk 0
# The metadata of the base pool is captured as fixtures into
# crates/storage-spaces/tests/scenarios/c11health.
# Usage: tools/health-states.sh DIR, then for each state
#   tools/put-roundtrip.sh DIR/STATE health_STATE -NoRepair
set -euo pipefail
cd "$(dirname "$0")/.."
dir=$1
spaces=$PWD/target/release/spaces
rm -rf -- "${dir:?}"
mkdir -p "$dir/base"
for i in 0 1 2 3; do truncate -s 8G "$dir/base/disk$i.img"; done
base=("$dir"/base/disk{0,1,2,3}.img)
q() { "$spaces" "$@" --yes | grep -vE "^step [0-9]+:" || true; }
q pool create --name ss-health "${base[@]}"
q space create --name hsimple --resiliency simple --size 1G --columns 1 "${base[@]}"
q space create --name hwide --resiliency simple --size 1G "${base[@]}"
q space create --name hmirror --resiliency mirror --size 1G --columns 1 "${base[@]}"
q space create --name hmirror2 --resiliency mirror --size 1G "${base[@]}"
q space create --name hparity --resiliency parity --size 2G "${base[@]}"
"$spaces" info "${base[@]}"
python3 - "$dir/base/manifest.json" <<'PY'
import json, sys
json.dump({"name": "health", "pattern": False, "pool": {"name": "ss-health"}, "space": {"name": "hsimple"},
           "extra_spaces": []}, open(sys.argv[1], "w"), indent=1)
PY
rm -rf crates/storage-spaces/tests/scenarios/c11health
"$spaces" fixture --all-pages -o crates/storage-spaces/tests/scenarios/c11health "$dir/base" | tail -1
state() {
  local name=$1
  shift
  mkdir -p "$dir/$name"
  local n=0
  for i in "$@"; do
    ln -s "../base/disk$i.img" "$dir/$name/disk$n.img"
    n=$((n + 1))
  done
  python3 - "$dir/base/manifest.json" "$dir/$name/manifest.json" "$@" <<'PY'
import json, sys
m = json.load(open(sys.argv[1]))
m["present"] = [int(i) for i in sys.argv[3:]]
json.dump(m, open(sys.argv[2], "w"), indent=1)
PY
}
state all 0 1 2 3
state no0 1 2 3
state no1 0 2 3
state no2 0 1 3
state no3 0 1 2
state only01 0 1
state only0 0
echo "health states: $dir/{all,no0,no1,no2,no3,only01,only0}"
