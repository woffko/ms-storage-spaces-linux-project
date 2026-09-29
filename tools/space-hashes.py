#!/usr/bin/env python3
"""One MD5 per chunk of a space image (from `spaces export`), in the format
of tools/vm/Get-SpaceHashes.ps1, or the chunks where it differs from such a
file.

Usage: tools/space-hashes.py IMAGE [CHUNK_KB] [--compare HASHES.txt]
"""
import hashlib
import sys

args = [a for a in sys.argv[1:] if not a.startswith("--")]
compare = None
if "--compare" in sys.argv:
    compare = sys.argv[sys.argv.index("--compare") + 1]
    args.remove(compare)
image = args[0]
chunk = int(args[1] if len(args) > 1 else 128) * 1024
ours = {}
with open(image, "rb") as f:
    i = 0
    while True:
        b = f.read(chunk)
        if not b:
            break
        ours[i] = hashlib.md5(b).hexdigest()
        i += 1
if compare is None:
    for i, h in ours.items():
        print(i, h)
    sys.exit(0)
theirs = {}
with open(compare, encoding="utf-8-sig") as f:
    for line in f:
        i, h = line.split()
        theirs[int(i)] = h
diff = [i for i in ours if theirs.get(i) != ours[i]]
print("%d of %d chunks differ" % (len(diff), len(ours)))
for i in diff:
    print(i, hex(i * chunk))
sys.exit(1 if diff else 0)
