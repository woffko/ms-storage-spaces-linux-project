#!/bin/bash
# Upload a local pool directory (disk<N>.img and manifest.json, e.g. a
# snapshot state or a work copy) to the Windows VM and run
# tools/vm/Test-RoundTrip.ps1 on it, without changing the pool first.
# Usage: tools/put-roundtrip.sh DIR NAME [Test-RoundTrip args...]
set -euo pipefail
cd "$(dirname "$0")/.."
dir=$1 name=$2
shift 2
[[ $name =~ ^[a-z0-9_]+$ ]] || { echo "bad name" >&2; exit 1; }
out=testdata/work/$name
mkdir -p "$out"
n=$(ls "$dir"/disk*.img | wc -l)
tools/vm.sh "New-Item -ItemType Directory -Force C:\\sstest\\roundtrip\\$name | Out-Null" >/dev/null
for ((i = 0; i < n; i++)); do
  python3 tools/raw2vhdx.py "$dir/disk$i.img" "$out/disk$i.vhdx" >/dev/null
  tools/vm.sh -put "$out/disk$i.vhdx" "C:/sstest/roundtrip/$name/disk$i.vhdx"
  rm -f -- "$out/disk$i.vhdx"
done
tools/vm.sh -put "$dir/manifest.json" "C:/sstest/roundtrip/$name/manifest.json"
tools/vm.sh -f tools/vm/Test-RoundTrip.ps1 -Name "$name" "$@" || true
tools/vm.sh -get "C:/sstest/roundtrip/$name/roundtrip.json" "$out/roundtrip.json"
python3 - "$out/roundtrip.json" <<'PY'
import json, sys
r = json.load(open(sys.argv[1], encoding="utf-8-sig"))
if r.get("error"):
    print("error:", r["error"])
for k in ("attached", "connected", "waited", "repaired"):
    s = r.get(k)
    if s:
        print(k, s["pool"]["health"], [(x["name"], x["health"], x["operational"]) for x in s["spaces"]],
              [(j["name"], j["state"], j["total"]) for j in s.get("jobs", [])])
for c in r.get("checks", []):
    print("check", c["space"], c["kind"], "ok" if c["ok"] else "FAILED")
PY
