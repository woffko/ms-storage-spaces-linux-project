#!/bin/bash
# A pool Windows created, changed on Linux, for a Windows round trip
# (tools/put-roundtrip.sh DIR NAME -Optimize): on a sparse copy of
# testdata/pools/POOL a blank disk is added, a mirror space created on the
# pool and filled with the pattern, the pool's own space grown (the new rows
# stay unwritten), and its first disk retired (its data moved) and removed.
# DIR then holds the remaining disks as disk<N>.img and a manifest checking
# the original pattern and the new space.
# Usage: tools/mgmt-corpus.sh POOL DIR
set -euo pipefail
cd "$(dirname "$0")/.."
name=$1 dir=$2
src=testdata/pools/$name
spaces=$PWD/target/release/spaces
rm -rf -- "${dir:?}"
mkdir -p "$dir"
n=0
for f in "$src"/disk*.img; do cp --sparse=always "$f" "$dir/d$n.img"; n=$((n + 1)); done
truncate -s 8G "$dir/d$n.img"
pool=()
for ((i = 0; i < n; i++)); do pool+=("$dir/d$i.img"); done
q() { "$spaces" "$@" --yes | grep -vE "^step [0-9]+:" || true; }
space=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1],encoding="utf-8-sig"))["space"]["name"])' "$src/manifest.json")
size=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1],encoding="utf-8-sig"))["space"]["size"])' "$src/manifest.json")

q disk add --new "$dir/d$n.img" "${pool[@]}"
pool+=("$dir/d$n.img")
# One column: its rows must fit the disks left after the retirement.
q space create --name lnew --resiliency mirror --size 1G --columns 1 "${pool[@]}"
"$spaces" write-pattern "${pool[@]}" --space lnew --offset 0 --length $((1 << 30)) --tag lnew | tail -1
q space resize --space "$space" --size $((size + (2 << 30))) "${pool[@]}"
first=$("$spaces" info "${pool[@]}" | awk '/device 0$/ {print $1}')
q disk retire --disk "$first" "${pool[@]}"
q disk remove --disk "$first" "${pool[@]}"
mv "$dir/d0.img" "$dir/removed.img"
final=()
for ((i = 1; i <= n; i++)); do mv "$dir/d$i.img" "$dir/disk$((i - 1)).img"; final+=("$dir/disk$((i - 1)).img"); done
"$spaces" info "${final[@]}"
"$spaces" check-pattern "${final[@]}" --space "$space" --length "$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1],encoding="utf-8-sig"))["pattern_size"])' "$src/manifest.json")" | tail -1
"$spaces" check-pattern "${final[@]}" --space lnew --tag lnew | tail -1
python3 - "$src/manifest.json" "$dir/manifest.json" <<'PY'
import json, sys
m = json.load(open(sys.argv[1], encoding="utf-8-sig"))
m["extra_spaces"] = (m.get("extra_spaces") or []) + [{"name": "lnew", "size": 1 << 30}]
json.dump(m, open(sys.argv[2], "w"), indent=1)
PY
echo "$name changed on Linux: PASS ($dir)"
