#!/usr/bin/env python3
"""A file system stress run that keeps a model of every file it writes.

Copies a tree of real files, then applies seeded random operations (create,
overwrite a range, append, truncate, rename, delete, make directories),
checking every file against the model at the end. Writes the final list of
files with their SHA-256 as JSON (the "files" format of the test pool
manifests), so that Windows can check them after a round trip.

Usage: ntfs-stress.py ROOT SOURCE_DIR COPY_MB OPS SEED FILES_JSON [SKIP_OPS]
SKIP_OPS lists operations to leave out, separated by commas. The history of
every final file (operation, sizes) is written next to FILES_JSON with
the suffix .history.
"""
import hashlib
import json
import os
import random
import sys


def main():
    root, source, copy_mb, ops, seed, out = sys.argv[1:7]
    skip = set(sys.argv[7].split(",")) if len(sys.argv) > 7 else set()
    history = {}
    copy_bytes, ops = int(copy_mb) << 20, int(ops)
    rng = random.Random(int(seed))
    model = {}  # relative path -> bytes

    # Real files first (shared libraries in name order).
    os.makedirs(os.path.join(root, "lib"), exist_ok=True)
    total = 0
    for name in sorted(os.listdir(source)):
        path = os.path.join(source, name)
        if not os.path.isfile(path) or os.path.islink(path):
            continue
        data = open(path, "rb").read()
        if total + len(data) > copy_bytes:
            break
        rel = "lib/" + name
        with open(os.path.join(root, rel), "wb") as f:
            f.write(data)
        model[rel] = data
        total += len(data)

    dirs = ["", "lib"]
    for i in range(ops):
        op = rng.choice(["create", "create", "overwrite", "append", "truncate", "rename", "delete", "mkdir"])
        if op in skip:
            continue
        files = sorted(model)
        if op == "mkdir" or not dirs:
            d = rng.choice(dirs)
            new = (d + "/" if d else "") + "d%d" % i
            os.makedirs(os.path.join(root, new))
            dirs.append(new)
        elif op == "create" or not files:
            d = rng.choice(dirs)
            rel = (d + "/" if d else "") + "f%d.bin" % i
            size = rng.choice([0, 1, 511, 512, 4095, 4096, 4097, 65537, rng.randrange(1, 3 << 20)])
            data = rng.randbytes(size)
            with open(os.path.join(root, rel), "wb") as f:
                f.write(data)
            model[rel] = data
            history[rel] = [("create", size)]
        else:
            rel = rng.choice(files)
            path = os.path.join(root, rel)
            data = model[rel]
            before = len(data)
            if op == "overwrite":
                at = rng.randrange(0, len(data) + 1)
                chunk = rng.randbytes(rng.randrange(1, 70000))
                with open(path, "r+b") as f:
                    f.seek(at)
                    f.write(chunk)
                data = data[:at] + chunk + data[at + len(chunk):]
            elif op == "append":
                chunk = rng.randbytes(rng.randrange(1, 200000))
                with open(path, "ab") as f:
                    f.write(chunk)
                data = data + chunk
            elif op == "truncate":
                size = rng.randrange(0, len(data) + 1)
                os.truncate(path, size)
                data = data[:size]
            elif op == "rename":
                d = rng.choice(dirs)
                new = (d + "/" if d else "") + "r%d-" % i + os.path.basename(rel)
                os.rename(path, os.path.join(root, new))
                del model[rel]
                history[new] = history.pop(rel, [])
                rel = new
            elif op == "delete":
                os.remove(path)
                del model[rel]
                history.pop(rel, None)
                continue
            model[rel] = data
            history.setdefault(rel, []).append((op, before, len(data)))

    os.sync()
    bad = [rel for rel, data in model.items() if open(os.path.join(root, rel), "rb").read() != data]
    if bad:
        sys.exit("mismatching files: %s" % bad[:10])
    files = [
        {"path": rel, "size": len(data), "sha256": hashlib.sha256(data).hexdigest()}
        for rel, data in sorted(model.items())
    ]
    json.dump(files, open(out, "w"), indent=0)
    json.dump(history, open(out + ".history", "w"), indent=0)
    print("%d files, %d MiB, %d operations, all as modelled" % (len(files), sum(f["size"] for f in files) >> 20, ops))


if __name__ == "__main__":
    main()
