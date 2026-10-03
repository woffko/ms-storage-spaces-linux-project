#!/bin/bash
# Mount every ReFS corpus volume (testdata/refs/NAME, or the NAMEs given)
# with `refs mount` and compare what Linux sees with Windows' listing
# (manifest.json): every path and nothing else, kinds, sizes, modification
# times, the SHA-256 of every file and of every named stream up to 64 KiB
# (extended attributes user.<name>; stream snapshots are not mounted),
# link targets, and one inode for all
# names of a hard-linked file. Needs FUSE; nothing is written to the images.
# Usage: tools/refs-mount-check.sh [NAME...]
set -uo pipefail
cd "$(dirname "$0")/.."
cargo build --release -q -p refs-cli || exit 1
names=("$@")
((${#names[@]})) || mapfile -t names < <(ls testdata/refs)
mnt=$(mktemp -d)
trap 'fusermount -u "$mnt" 2>/dev/null; rmdir "$mnt"' EXIT
trap 'exit 130' INT TERM
failed=0
for name in "${names[@]}"; do
  dir=testdata/refs/$name
  [[ -f $dir/disk.img && -f $dir/manifest.json ]] || continue
  target/release/refs mount "$dir/disk.img" "$mnt" &
  pid=$!
  for _ in $(seq 100); do mountpoint -q "$mnt" && break; sleep 0.1; done
  python3 - "$dir/manifest.json" "$mnt" <<'EOF' || failed=1
import hashlib, json, os, sys
manifest = json.load(open(sys.argv[1], encoding="utf-8-sig"))
mnt = sys.argv[2]
problems = []
listed = {e["path"]: e for e in manifest["entries"]}
seen = set()
for root, dirs, files in os.walk(mnt):
    rel = os.path.relpath(root, mnt)
    for n in dirs + files:
        p = n if rel == "." else f"{rel}/{n}"
        if p != "System Volume Information" and not p.startswith("System Volume Information/"):
            seen.add(p)
problems += [f"{p}: not listed by Windows" for p in sorted(seen - set(listed))]
inodes = {}
for path, e in listed.items():
    full = os.path.join(mnt, path)
    if not os.path.lexists(full):
        problems.append(f"{path}: missing")
        continue
    st = os.lstat(full)
    if "link_target" in e:
        target = os.readlink(full)
        windows = e["link_target"].replace("\\", "/")
        if len(windows) > 1 and windows[1] == ":":
            windows = os.path.join(mnt, windows[3:])
        if target != windows:
            problems.append(f"{path}: link to {target!r}, Windows {e['link_target']!r}")
        continue
    if (e["kind"] == "dir") != os.path.isdir(full):
        problems.append(f"{path}: kind")
    if st.st_mtime_ns != (e["written"] - 116444736000000000) * 100:
        problems.append(f"{path}: mtime {st.st_mtime_ns}")
    if e["kind"] != "file":
        continue
    if st.st_size != e["size"]:
        problems.append(f"{path}: size {st.st_size} != {e['size']}")
    h = hashlib.sha256()
    with open(full, "rb") as f:
        while chunk := f.read(1 << 20):
            h.update(chunk)
    if h.hexdigest() != e["sha256"]:
        problems.append(f"{path}: data differs")
    for s in e.get("streams", []):
        # Snapshots are not in the mount; Linux takes 64 KiB attributes.
        if s["name"].endswith(":$SNAPSHOT") or s["size"] > 65536:
            continue
        try:
            value = os.getxattr(full, "user." + s["name"], follow_symlinks=False)
        except OSError as err:
            problems.append(f"{path}:{s['name']}: {err}")
            continue
        if hashlib.sha256(value).hexdigest() != s["sha256"]:
            problems.append(f"{path}:{s['name']}: data differs")
    inodes.setdefault(e.get("hard_link_of", path), set()).add(st.st_ino)
problems += [f"{p}: hard links with inodes {sorted(i)}" for p, i in inodes.items() if len(i) > 1]
print(f"{manifest['name']}: {len(listed)} entries listed by Windows, {len(seen)} mounted, {len(problems)} problems")
for p in problems[:30]:
    print("  " + p)
sys.exit(1 if problems else 0)
EOF
  fusermount -u "$mnt"
  wait "$pid"
done
exit $failed
