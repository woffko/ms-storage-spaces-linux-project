"""Research (Track B4, docs/refs-format.md "Writing"); writes only to the
image file given, never to a device.

Experiment: change a file's modification time with copy on write, the
way Windows commits: the changed directory page, both object tables and
both allocator tables go to newly allocated clusters (pages of roots 0
and 5 and directories from the medium allocator, root 1; pages of roots 1
and 2 from the container allocator, root 2), the old clusters are freed,
and a new checkpoint with the next clock replaces the older of the two.
Nothing the current checkpoint references is overwritten.

Usage: tools/research/refs-touch-cow.py IMAGE NAME FILETIME   (IMAGE is changed in place)
"""
import struct, sys

IMG, NAME, NEWTIME = sys.argv[1], sys.argv[2], int(sys.argv[3])
f = open(IMG, "r+b")
P = 16 << 20
u16 = lambda b, o: struct.unpack_from("<H", b, o)[0]
u32 = lambda b, o: struct.unpack_from("<I", b, o)[0]
u64 = lambda b, o: struct.unpack_from("<Q", b, o)[0]
f.seek(P); vbr = f.read(512)
CL = u32(vbr, 0x20) * u32(vbr, 0x24)
assert CL == 4096, "4 KiB clusters only"
PAGE, PER = 16384, 4

def table(poly):
    t = []
    for i in range(256):
        c = i
        for _ in range(8):
            c = (c >> 1) ^ poly if c & 1 else c >> 1
        t.append(c)
    return t
T64, T32 = table(0x9A6C9329AC4BC9B5), table(0x82F63B78)
def crc(b, t, ones):
    c = ones
    for x in b:
        c = t[(c ^ x) & 0xff] ^ (c >> 8)
    return c ^ ones
crc64 = lambda b: crc(b, T64, (1 << 64) - 1)
crc32c = lambda b: crc(b, T32, (1 << 32) - 1)

def rd(lcn, n=1):
    f.seek(P + lcn * CL); return bytearray(f.read(n * CL))
WRITES = []
def wr(lcn, data):
    WRITES.append((lcn, len(data) // CL))
    f.seek(P + lcn * CL); f.write(data)

def checksum_ok(ref, off, data):
    kind, at = ref[off + 0x22], off + 0x20 + ref[off + 0x23]
    return (kind == 2 and crc64(data) == u64(ref, at)) or (kind == 1 and crc32c(data) == u32(ref, at))
def set_ref(ref, off, lcns, data):
    """Point the page reference at ref[off:] to lcns, with data's checksum."""
    for i, l in enumerate(lcns):
        struct.pack_into("<Q", ref, off + 8 * i, l)
    kind, at = ref[off + 0x22], off + 0x20 + ref[off + 0x23]
    if kind == 2:
        struct.pack_into("<Q", ref, at, crc64(data))
    elif kind == 1:
        struct.pack_into("<I", ref, at, crc32c(data))
    else:
        raise SystemExit(f"checksum kind {kind}")

# Superblock, both checkpoints.
s = rd(0x1e)
slots = [u64(s, u32(s, 0x70) + 8 * i) for i in range(2)]
clocks = {c: u64(rd(c), 0x60) for c in slots}
cur = max(slots, key=clocks.get)
other = min(slots, key=clocks.get)
chk = rd(cur, PER)
REF = u32(chk, 0x5c)
arr = u32(chk, 0x94) if u32(chk, 0x78) & 0x200 else 0x94
root_at = [u32(chk, arr + 4 * i) for i in range(u32(chk, 0x90))]
clock = u64(chk, 0x60) + 1

CPC, CMAP = None, {}
def translate(v):
    return CMAP[v >> CPC.bit_length()] + (v & (CPC - 1))
def virtual(p):
    for cid, start in CMAP.items():
        if start <= p < start + CPC:
            return (cid << CPC.bit_length()) + p - start
    raise SystemExit(f"cluster {p:#x} in no container")

class Page:
    def __init__(self, ref, off, physical=False):
        self.virtual = [u64(ref, off + 8 * i) for i in range(PER)]
        self.physical = physical
        self.phys = [l if physical else translate(l) for l in self.virtual]
        self.data = bytearray(b"".join(rd(l) for l in self.phys))
        assert self.data[:4] == b"MSB+" and checksum_ok(ref, off, self.data)
    def move(self, phys):
        """Gives the page new clusters (its header names them)."""
        self.old = self.phys
        self.phys = phys
        self.virtual = phys if self.physical else [virtual(p) for p in phys]
        for i, l in enumerate(self.virtual):
            struct.pack_into("<Q", self.data, 0x20 + 8 * i, l)
        struct.pack_into("<Q", self.data, 0x10, clock)
    def write(self):
        for i, l in enumerate(self.phys):
            wr(l, self.data[i * CL:(i + 1) * CL])

def node(buf, base=0x50):
    h = base + u32(buf, base)
    start, count = u32(buf, h + 0x10), u32(buf, h + 0x14)
    out = []
    for i in range(count):
        r = h + (u32(buf, h + start + 4 * i) & 0xffff)
        ko, kl, vo, vl = u16(buf, r + 4), u16(buf, r + 6), u16(buf, r + 0x0a), u16(buf, r + 0x0c)
        out.append((bytes(buf[r + ko:r + ko + kl]), r + vo, vl))
    return buf[h + 0x0c], out

def tree(ref, off, physical=False):
    """Every page of a tree: (page, parent page or None, offset of its reference in the parent)."""
    pages = []
    def go(ref, off, parent):
        pg = Page(ref, off, physical)
        pages.append((pg, parent, off))
        level, rows = node(pg.data)
        if level:
            for _, vo, _ in rows:
                go(pg.data, vo, pg)
    go(ref, off, None)
    return pages

for pg, _, _ in tree(chk, root_at[7], physical=True):
    level, rows = node(pg.data)
    if level == 0:
        for k, vo, vl in rows:
            v = pg.data[vo:vo + vl]
            CPC = u32(v, 0x18); CMAP[u64(k, 0)] = u64(v, len(v) - 16)

# Allocators: bitmap rows (value: start, count, free u16 at 0x10, bitmap from 0x18).
alloc = {i: tree(chk, root_at[i]) for i in (1, 2)}
def bitmap_rows(i):
    for pg, _, _ in alloc[i]:
        level, rows = node(pg.data)
        if level == 0:
            for k, vo, vl in rows:
                if vl == 24 + 2048:
                    yield pg, vo
DIRTY = set()
def take(i, n=PER):
    """n free consecutive clusters of allocator i (marked used)."""
    for pg, vo in bitmap_rows(i):
        d = pg.data
        start, free = u64(d, vo), u16(d, vo + 0x10)
        if free < n:
            continue
        bits = lambda j: d[vo + 0x18 + j // 8] >> (j % 8) & 1
        for j in range(0, 16384 - n + 1, n):
            if not any(bits(j + k) for k in range(n)):
                for k in range(n):
                    d[vo + 0x18 + (j + k) // 8] |= 1 << ((j + k) % 8)
                struct.pack_into("<H", d, vo + 0x10, free - n)
                DIRTY.add(id(pg))
                return [start + j + k for k in range(n)]
    raise SystemExit(f"allocator {i} full")
def give_back(i, clusters):
    for p in clusters:
        for pg, vo in bitmap_rows(i):
            d = pg.data
            start = u64(d, vo)
            if start <= p < start + 16384:
                j = p - start
                assert d[vo + 0x18 + j // 8] >> (j % 8) & 1, f"{p:#x} not allocated"
                d[vo + 0x18 + j // 8] &= ~(1 << (j % 8)) & 0xff
                struct.pack_into("<H", d, vo + 0x10, u16(d, vo + 0x10) + 1)
                DIRTY.add(id(pg))
                break
        else:
            raise SystemExit(f"{p:#x} in no row of allocator {i}")

# The change: the file's time in its directory page.
tables = {i: Page(chk, root_at[i]) for i in (0, 5)}
def dir_row(t):
    for k, vo, _ in node(t.data)[1]:
        if u64(k, 8) == 0x600:
            return vo
d_at = {i: dir_row(t) for i, t in tables.items()}
directory = Page(tables[0].data, d_at[0] + 0x20)
for k, vo, _ in node(directory.data)[1]:
    if u16(k, 0) == 0x30 and k[4:].decode("utf-16-le") == NAME:
        assert u16(k, 2) == 1, "not an embedded record"
        struct.pack_into("<Q", directory.data, vo + 0x30, NEWTIME)
        break
else:
    raise SystemExit(f"no {NAME}")

# New clusters: the directory and both object tables from the medium
# allocator; their old clusters freed.
for pg in [directory, tables[0], tables[5]]:
    pg.move(take(1))
    give_back(1, pg.old)
# The allocator pages that changed, and the pages above them, from the
# container allocator (root 2 last: its own page is in it).
def path_up(pages):
    """Changed pages and their ancestors, children first."""
    changed = [(pg, parent, off) for pg, parent, off in pages if id(pg) in DIRTY]
    out, seen = [], set()
    while changed:
        pg, parent, off = changed.pop(0)
        if id(pg) in seen:
            continue
        seen.add(id(pg)); out.append((pg, parent, off))
        if parent is not None:
            changed.append(next(x for x in pages if x[0] is parent))
    return out
root1 = path_up(alloc[1])
for pg, _, _ in root1:
    pg.move(take(2))
    give_back(2, pg.old)
root2 = alloc[2]
assert len(root2) == 1, "one page of root 2 expected"
r2 = root2[0][0]
r2.move(take(2))
give_back(2, r2.old)
DIRTY.add(id(r2))

# References, children before parents, checksums last.
for i, t in tables.items():
    set_ref(t.data, d_at[i] + 0x20, directory.virtual, directory.data)
for pg, parent, off in root1:
    if parent is not None:
        set_ref(parent.data, off, pg.virtual, pg.data)
new = bytearray(chk)
for i, pg in [(0, tables[0]), (5, tables[5]), (1, root1[-1][0]), (2, r2)]:
    set_ref(new, root_at[i], pg.virtual, pg.data)
for pg in [directory, tables[0], tables[5], *(p for p, _, _ in root1), r2]:
    pg.write()

# The checkpoint, into the older slot: its clusters, the next clock.
old_hdr = rd(other)
assert all(u64(old_hdr, 0x28 + 8 * i) == 0 for i in range(3)), "checkpoint of one cluster expected"
new[0x20:0x40] = old_hdr[0x20:0x40]
struct.pack_into("<Q", new, 0x10, clock)
struct.pack_into("<Q", new, 0x60, clock)
struct.pack_into("<Q", new, 0x68, u64(chk, 0x68) + 1)
desc = u32(new, 0x58)
for i in range(PER):
    struct.pack_into("<Q", new, desc + 8 * i, u64(old_hdr, 0x20 + 8 * i))
first = bytearray(new[:CL]); first[desc:desc + REF] = bytes(REF)
set_ref(new, desc, [u64(old_hdr, 0x20 + 8 * i) for i in range(PER)], first)
wr(other, new[:CL])
f.close()
print(f"checkpoint {other:#x} clock {clock:#x}; wrote", ", ".join(f"{l:#x}+{n}" for l, n in WRITES))
