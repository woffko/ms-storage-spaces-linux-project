#!/bin/bash
# Windows' health states with a disk lost while the pool is in use, for a
# pool created on Linux (tools/health-states.sh, DIR_LINUX) and the same
# layout created by Windows (scenario c11ctl, DIR_WINDOWS: four disks, a
# one-column and a four-column simple space, a one- and a two-column mirror,
# a parity space): for each disk N, the pool is attached whole and image N
# detached (tools/vm/Test-RoundTrip.ps1 -DropDisk N -NoRepair), then
# Windows' view ("dropped") is printed next to `spaces pool health` of the
# other three disks. The round trips land in testdata/work/health_drop_*.
# N may list several disks (0,1).
# Usage: tools/health-drop.sh DIR_LINUX DIR_WINDOWS [N...]
set -uo pipefail
cd "$(dirname "$0")/.."
spaces=$PWD/target/release/spaces
linux=$1 windows=$2
shift 2
drops=("$@")
((${#drops[@]})) || drops=(0 1 2 3)
for n in "${drops[@]}"; do
  for kind in lnx win; do
    src=$linux
    [[ $kind == win ]] && src=$windows
    name=health_drop_${kind}_${n//,/}
    echo "=== $name"
    tools/put-roundtrip.sh "$src" "$name" -NoRepair -DropDisk "$n" 2>&1 | grep -vE "heartbeat|^scp attempt"
    others=()
    for i in 0 1 2 3; do [[ ,$n, == *,$i,* ]] || others+=("$src/disk$i.img"); done
    echo "--- spaces pool health without disk$n"
    "$spaces" pool health "${others[@]}" 2>/dev/null
  done
done
