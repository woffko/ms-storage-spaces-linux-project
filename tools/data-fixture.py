#!/usr/bin/env python3
"""Turn a small pool (an impulse pool) into a data fixture for the tests.

Every non-zero 4 KiB page of each raw disk image is stored in the sparse
fixture format of storage_spaces::io::SparseImage ("SSFIXT01", u64 size,
u32 range count, then u64 offset, u32 length and the bytes of each range).

Usage: tools/data-fixture.py POOL_DIR OUT_DIR [--no-cache-slots]

--no-cache-slots leaves out write-back cache slots (pages starting with
"SPSLOT"); use it only when the data under test was destaged, since the
fixture then describes an empty cache.
"""
import os
import shutil
import struct
import sys


def pages(path, skip_slots):
    """Yields (offset, page) for the non-zero pages of a sparse file."""
    with open(path, "rb") as f:
        fd = f.fileno()
        pos = 0
        while True:
            try:
                data = os.lseek(fd, pos, os.SEEK_DATA)
            except OSError:
                return
            hole = os.lseek(fd, data, os.SEEK_HOLE)
            start = data // 4096 * 4096
            f.seek(start)
            blob = f.read(hole - start)
            for p in range(0, len(blob), 4096):
                page = blob[p:p + 4096]
                if any(page) and not (skip_slots and page.startswith(b"SPSLOT")):
                    yield start + p, page
            pos = hole


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    skip_slots = "--no-cache-slots" in sys.argv
    src, dst = args
    os.makedirs(dst, exist_ok=True)
    i = 0
    while os.path.exists(f"{src}/disk{i}.img"):
        path = f"{src}/disk{i}.img"
        ranges = []
        for offset, page in pages(path, skip_slots):
            if ranges and ranges[-1][0] + len(ranges[-1][1]) == offset:
                ranges[-1][1].extend(page)
            else:
                ranges.append([offset, bytearray(page)])
        with open(f"{dst}/disk{i}.fixture", "wb") as out:
            out.write(b"SSFIXT01" + struct.pack("<QI", os.path.getsize(path), len(ranges)))
            for offset, data in ranges:
                out.write(struct.pack("<QI", offset, len(data)) + data)
        i += 1
    shutil.copy(f"{src}/manifest.json", f"{dst}/manifest.json")
    print(f"{dst}: {i} disks")


if __name__ == "__main__":
    main()
