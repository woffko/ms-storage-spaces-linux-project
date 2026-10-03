#!/usr/bin/env python3
"""What changed between two states of a ReFS volume (write experiments,
tools/vm/Invoke-RefsSteps.ps1): every cluster that differs, in runs, with
what `refs map` says it is in the old and in the new state.

Usage: tools/refs-diff.py OLD.img NEW.img [--offset BYTES] [--cluster N]
(the offset defaults to 16 MiB, the partition of New-RefsVolume.ps1; the
cluster size is read from the boot sector).
"""
import argparse
import bisect
import os
import struct
import subprocess
import sys

REFS = os.path.join(os.path.dirname(__file__), "..", "target", "release", "refs")


def data_ranges(fd, start, end):
    """The (from, to) byte ranges of `fd` holding data within start..end."""
    at = start
    while at < end:
        try:
            data = os.lseek(fd, at, os.SEEK_DATA)
        except OSError:
            return
        if data >= end:
            return
        hole = min(os.lseek(fd, data, os.SEEK_HOLE), end)
        yield data, hole
        at = hole


def merged(ranges):
    out = []
    for a, b in sorted(ranges):
        if out and a <= out[-1][1]:
            out[-1][1] = max(out[-1][1], b)
        else:
            out.append([a, b])
    return out


def changed_clusters(old, new, offset, size, cluster):
    fo, fn = os.open(old, os.O_RDONLY), os.open(new, os.O_RDONLY)
    ranges = merged(list(data_ranges(fo, offset, offset + size)) + list(data_ranges(fn, offset, offset + size)))
    changed = []
    for a, b in ranges:
        a -= (a - offset) % cluster
        at = a
        while at < b:
            n = min(b - at, 1 << 22)
            n += (-n) % cluster
            x, y = os.pread(fo, n, at), os.pread(fn, n, at)
            if x != y:
                for k in range(0, n, cluster):
                    if x[k:k + cluster] != y[k:k + cluster]:
                        changed.append((at + k - offset) // cluster)
            at += n
    return changed


def runs(lcns):
    out = []
    for c in lcns:
        if out and out[-1][1] == c:
            out[-1][1] = c + 1
        else:
            out.append([c, c + 1])
    return out


class Map:
    def __init__(self, image, offset):
        text = subprocess.run([REFS, "map", image, "--offset", str(offset)], check=True,
                              capture_output=True, text=True).stdout
        self.items = []
        for line in text.splitlines():
            lcn, count, what = line.split(" ", 2)
            self.items.append((int(lcn, 16), int(count), what))
        self.items.sort()
        self.starts = [i[0] for i in self.items]

    def at(self, lcn):
        """What the cluster is (several owners when trees share it)."""
        k = bisect.bisect_right(self.starts, lcn)
        found = []
        for start, count, what in self.items[max(0, k - 64):k]:
            if start <= lcn < start + count:
                found.append(what if count == 1 else f"{what} +{lcn - start}")
        return "; ".join(found) or "-"


def main():
    p = argparse.ArgumentParser()
    p.add_argument("old")
    p.add_argument("new")
    p.add_argument("--offset", type=int, default=16 << 20)
    p.add_argument("--cluster", type=int, default=0)
    a = p.parse_args()
    with open(a.new, "rb") as f:
        f.seek(a.offset)
        boot = f.read(512)
    if boot[3:7] != b"ReFS":
        sys.exit("no ReFS boot sector at the offset")
    sectors, bps, spc = struct.unpack_from("<QII", boot, 0x18)
    cluster = a.cluster or bps * spc
    size = sectors * bps
    old_map, new_map = Map(a.old, a.offset), Map(a.new, a.offset)
    for start, end in runs(changed_clusters(a.old, a.new, a.offset, size, cluster)):
        before, after = old_map.at(start), new_map.at(start)
        n = end - start
        print(f"{start:#x} +{n}: old [{before}] new [{after}]")


if __name__ == "__main__":
    main()
