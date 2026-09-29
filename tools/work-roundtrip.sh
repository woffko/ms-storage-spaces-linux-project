#!/bin/bash
# Bring a pool changed on the Linux test VM (a work directory of
# tools/rw-ntfs-check.sh or tools/rw-kernel-check.sh) to the Windows VM and
# check it there with tools/vm/Test-RoundTrip.ps1. With a files.json in the
# work directory the manifest lists those files instead of the pattern.
# Usage: tools/work-roundtrip.sh VM_WORK_DIR NAME [Test-RoundTrip args...]
set -euo pipefail
cd "$(dirname "$0")/.."
remote=$1 name=$2
shift 2
[[ $name =~ ^[a-z0-9_]+$ ]] || { echo "bad name" >&2; exit 1; }
work=testdata/work/$name
rm -rf -- "${work:?}"
mkdir -p "$work"
rsync -a --sparse -e "ssh -o BatchMode=yes -o IdentitiesOnly=yes -o HostKeyAlias=192.168.189.144 -i $HOME/.ssh/rustadmin_vm_ed25519" \
  --bwlimit="${BWLIMIT:-40000}" "codex@${LINUX_VM_HOST:-192.168.189.142}:$remote/" "$work/"
if [[ -f $work/files.json ]]; then
  python3 - "$work" <<'PY'
import json, sys
w = sys.argv[1]
m = json.load(open(f"{w}/manifest.json", encoding="utf-8-sig"))
m["pattern"] = False
m["files"] = json.load(open(f"{w}/files.json"))
json.dump(m, open(f"{w}/manifest.json", "w"), indent=1)
PY
fi
n=$(ls "$work"/disk*.img | wc -l)
tools/vm.sh "New-Item -ItemType Directory -Force C:\\sstest\\roundtrip\\$name | Out-Null" >/dev/null
for ((i = 0; i < n; i++)); do
  python3 tools/raw2vhdx.py "$work/disk$i.img" "$work/disk$i.vhdx" >/dev/null
  tools/vm.sh -put "$work/disk$i.vhdx" "C:/sstest/roundtrip/$name/disk$i.vhdx"
  rm -f -- "$work/disk$i.vhdx"
done
tools/vm.sh -put "$work/manifest.json" "C:/sstest/roundtrip/$name/manifest.json"
tools/vm.sh -f tools/vm/Test-RoundTrip.ps1 -Name "$name" "$@" || true
tools/vm.sh -get "C:/sstest/roundtrip/$name/roundtrip.json" "$work/roundtrip.json"
python3 - "$work/roundtrip.json" <<'PY'
import json, re, sys
r = json.load(open(sys.argv[1], encoding="utf-8-sig"))
if r.get("error"):
    print("error:", r["error"])
for k in ("attached", "repaired"):
    s = r.get(k)
    if s:
        print(k, s["pool"]["health"], [(x["name"], x["health"], x["operational"]) for x in s["spaces"]])
for c in r.get("checks", []):
    extra = ""
    if c["kind"] == "ntfs":
        extra = "chkdsk %s, %d files, %d mismatching %s" % (
            c["chkdsk_exit"], c["files"], len(c["mismatching"]),
            re.findall(r"file record segment \w+", c["chkdsk"]))
    print("check", c["space"], c["kind"], "ok" if c["ok"] else "FAILED", extra)
PY
