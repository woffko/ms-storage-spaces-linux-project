#!/bin/bash
# Write ranges into a copy of a corpus pool with spaces write-pattern and
# check on the Windows VM that Windows accepts the pool and reads exactly
# those ranges (tools/vm/Test-RoundTrip.ps1 -Written).
# Usage: tools/rw-roundtrip.sh NAME POOL "OFFKB:LENKB:TAG;..."
#   NAME   round-trip name (C:\sstest\roundtrip\NAME, testdata/work/NAME)
#   POOL   corpus pool in testdata/pools (never written: a copy is made)
# WRITE_ARGS adds arguments to every write-pattern call (e.g. --destage, or
# --crash-after-writes N to leave a crash state).
# The result lands in testdata/work/NAME/roundtrip.json.
set -euo pipefail
cd "$(dirname "$0")/.."
name=$1 pool=$2 ranges=$3
[[ $name =~ ^[a-z0-9_]+$ && $pool =~ ^[a-z0-9_]+$ ]] || { echo "bad name" >&2; exit 1; }
spaces=${SPACES:-target/release/spaces}
work=testdata/work/$name
rm -rf -- "${work:?}"
mkdir -p "$work"
for f in testdata/pools/$pool/disk*.img testdata/pools/$pool/manifest.json; do
  cp --sparse=always "$f" "$work/"
done
space=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1],encoding="utf-8-sig"))["space"]["name"])' "$work/manifest.json")
IFS=';' read -ra list <<<"$ranges"
for r in "${list[@]}"; do
  IFS=':' read -r off len tag <<<"$r"
  rc=0
  "$spaces" write-pattern "$work"/disk*.img --space "$space" --offset $((off * 1024)) --length $((len * 1024)) --tag "$tag" ${WRITE_ARGS:-} || rc=$?
  # 99: stopped by --crash-after-writes, as asked.
  [[ $rc == 0 || $rc == 99 ]] || exit "$rc"
done
n=$(ls "$work"/disk*.img | wc -l)
for ((i = 0; i < n; i++)); do
  python3 tools/raw2vhdx.py "$work/disk$i.img" "$work/disk$i.vhdx" >/dev/null
done
tools/vm.sh "New-Item -ItemType Directory -Force C:\\sstest\\roundtrip\\$name | Out-Null" >/dev/null
for ((i = 0; i < n; i++)); do
  tools/vm.sh -put "$work/disk$i.vhdx" "C:/sstest/roundtrip/$name/disk$i.vhdx"
  rm -f -- "$work/disk$i.vhdx"
done
tools/vm.sh -put "$work/manifest.json" "C:/sstest/roundtrip/$name/manifest.json"
tools/vm.sh -f tools/vm/Test-RoundTrip.ps1 -Name "$name" -Written "$ranges" || true
tools/vm.sh -get "C:/sstest/roundtrip/$name/roundtrip.json" "$work/roundtrip.json"
python3 - "$work/roundtrip.json" <<'PY'
import json, sys
r = json.load(open(sys.argv[1], encoding='utf-8-sig'))
if r.get('error'):
    print('error:', r['error'])
for k in ('attached', 'repaired'):
    s = r.get(k)
    if s:
        print(k, s['pool']['health'], [(x['name'], x['health'], x['operational']) for x in s['spaces']])
for c in r.get('checks', []):
    print('check', c['space'], c['kind'], 'ok' if c['ok'] else 'FAILED', c.get('first_mismatch'))
PY
