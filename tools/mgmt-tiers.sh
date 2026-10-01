#!/bin/bash
# Tiered spaces created on Linux, for Windows round trips (tools/put-roundtrip.sh
# DIR/KIND NAME -Optimize): pools of sparse 8 GiB images, the first two SSD
# and the rest HDD (spaces disk set --media), two tier templates (spaces tier
# create) and a tiered space over them:
#   tier    four disks, an SSD mirror and an HDD two-column simple tier
#   mapar   five disks, an SSD mirror and an HDD three-column parity tier
#           (mirror-accelerated parity)
# Usage: tools/mgmt-tiers.sh DIR
set -euo pipefail
cd "$(dirname "$0")/.."
dir=$1
spaces=$PWD/target/release/spaces
q() { "$spaces" "$@" --yes | grep -vE "^step [0-9]+:" || true; }
make() {
  local kind=$1 n=$2 hdd=$3 columns=$4
  local d=$dir/$kind
  rm -rf -- "${d:?}"
  mkdir -p "$d"
  local disks=()
  for ((i = 0; i < n; i++)); do truncate -s 8G "$d/disk$i.img"; disks+=("$d/disk$i.img"); done
  q pool create --name "ss-l$kind" "${disks[@]}"
  for ((i = 0; i < n; i++)); do
    id=$("$spaces" info "${disks[@]}" | awk -v dev="device $i" '$0 ~ dev"$" {print $1}')
    q disk set --disk "$id" --media "$([[ $i -lt 2 ]] && echo ssd || echo hdd)" "${disks[@]}"
  done
  q tier create --name "l${kind}ssd" --media ssd --resiliency mirror "${disks[@]}"
  q tier create --name "l${kind}hdd" --media hdd --resiliency "$hdd" --columns "$columns" "${disks[@]}"
  q space create --name "l$kind" --tier "l${kind}ssd=1G" --tier "l${kind}hdd=2G" "${disks[@]}"
  "$spaces" pool health "${disks[@]}"
  python3 - "$d/manifest.json" "$kind" <<'PY'
import json, sys
k = sys.argv[2]
json.dump({"name": "l" + k, "pattern": False, "pool": {"name": "ss-l" + k}, "space": {"name": "l" + k},
           "extra_spaces": []}, open(sys.argv[1], "w"), indent=1)
PY
}
make tier 4 simple 2
make mapar 5 parity 3
echo "tiered pools: $dir/{tier,mapar}"
