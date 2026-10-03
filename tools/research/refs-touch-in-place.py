"""Research (Track B4, docs/refs-format.md "Writing"); writes only to the
image file given, never to a device.

Experiment: change a file's modification time in place (no copy on
write, no allocation) and fix every checksum on the way up: the directory
page, the object table rows naming it (roots 0 and 5), those pages, the
checkpoint's root references and its own checksum. Does Windows take it?

Usage: tools/research/refs-touch-in-place.py IMAGE NAME FILETIME   (IMAGE is changed in place)
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
PAGE = 16384 if CL == 4096 else CL
PER = PAGE // CL

def table(poly, bits):
    t = []
    for i in range(256):
        c = i
        for _ in range(8):
            c = (c >> 1) ^ poly if c & 1 else c >> 1
        t.append(c)
    return t
T64, T32 = table(0x9A6C9329AC4BC9B5, 64), table(0x82F63B78, 32)
def crc(b, t, ones):
    c = ones
    for x in b:
        c = t[(c ^ x) & 0xff] ^ (c >> 8)
    return c ^ ones
crc64 = lambda b: crc(b, T64, (1 << 64) - 1)
crc32c = lambda b: crc(b, T32, (1 << 32) - 1)
assert crc64(b"123456789") == 0xAE8B14860A799888 and crc32c(b"123456789") == 0xE3069283

def rd(lcn, n=1):
    f.seek(P + lcn * CL); return bytearray(f.read(n * CL))
def wr(lcn, data):
    f.seek(P + lcn * CL); f.write(data)

def set_checksum(ref, off, data):
    """Store data's checksum in the page reference at ref[off:]."""
    kind, at = ref[off + 0x22], off + 0x20 + ref[off + 0x23]
    if kind == 2:
        struct.pack_into("<Q", ref, at, crc64(data))
    elif kind == 1:
        struct.pack_into("<I", ref, at, crc32c(data))
    else:
        raise SystemExit(f"checksum kind {kind}")

def verify(ref, off, data):
    kind, at = ref[off + 0x22], off + 0x20 + ref[off + 0x23]
    return (kind == 2 and crc64(data) == u64(ref, at)) or (kind == 1 and crc32c(data) == u32(ref, at))

# Superblock -> current checkpoint.
s = rd(0x1e)
cps = [u64(s, u32(s, 0x70) + 8 * i) for i in range(2)]
cp_lcn = max(cps, key=lambda c: u64(rd(c), 0x60))
chk = rd(cp_lcn, PER)
REF = u32(chk, 0x5c)
arr = u32(chk, 0x94) if u32(chk, 0x78) & 0x200 else 0x94
root_at = [u32(chk, arr + 4 * i) for i in range(u32(chk, 0x90))]

CPC, CMAP = None, {}
def translate(v):
    return CMAP[v >> CPC.bit_length()] + (v & (CPC - 1))

class Page:
    """A metadata page read through a reference, kept for writing back."""
    def __init__(self, ref, off, virtual=True):
        lcns = [u64(ref, off + 8 * i) for i in range(PER)]
        self.phys = [translate(l) if virtual else l for l in lcns]
        self.data = bytearray(b"".join(rd(l) for l in self.phys))
        assert self.data[:4] == b"MSB+" and verify(ref, off, self.data)
    def write(self):
        for i, l in enumerate(self.phys):
            wr(l, self.data[i * CL:(i + 1) * CL])

def rows(buf, base=0x50, leaf=True):
    """(row offset, key offset, key, value offset, value) of a node."""
    h = base + u32(buf, base)
    if leaf:
        assert buf[h + 0x0c] == 0, "index nodes: not handled by this experiment"
    start, count = u32(buf, h + 0x10), u32(buf, h + 0x14)
    for i in range(count):
        r = h + (u32(buf, h + start + 4 * i) & 0xffff)
        ko, kl, vo, vl = u16(buf, r + 4), u16(buf, r + 6), u16(buf, r + 0x0a), u16(buf, r + 0x0c)
        yield r, r + ko, bytes(buf[r + ko:r + ko + kl]), r + vo, buf[r + vo:r + vo + vl]

# Containers (root 7, physical clusters; read only).
def leaves(ref, off, virtual):
    page = Page(ref, off, virtual)
    h = 0x50 + u32(page.data, 0x50)
    if page.data[h + 0x0c] == 0:
        yield from rows(page.data)
    else:
        for _, _, _, vo, _ in rows(page.data, leaf=False):
            yield from leaves(page.data, vo, virtual)
for _, _, k, _, v in leaves(chk, root_at[7], False):
    CPC = u32(v, 0x18); CMAP[u64(k, 0)] = u64(v, len(v) - 16)

# Object tables (root 0 and its copy, root 5): the row of the root directory.
tables = {i: Page(chk, root_at[i]) for i in (0, 5)}
def dir_ref(t):
    for _, _, k, vo, v in rows(t.data):
        if u64(k, 8) == 0x600:
            return vo + 0x20
    raise SystemExit("no root directory row")
d_at = {i: dir_ref(t) for i, t in tables.items()}
directory = Page(tables[0].data, d_at[0])
assert bytes(tables[0].data[d_at[0]:d_at[0] + 32]) == bytes(tables[5].data[d_at[5]:d_at[5] + 32])

# The file's name row (type 0x30, embedded record): its modification time.
for _, ko, k, vo, v in rows(directory.data):
    if u16(k, 0) == 0x30 and k[4:].decode("utf-16-le") == NAME:
        assert u16(k, 2) == 1, "not an embedded record"
        old = u64(directory.data, vo + 0x30)
        struct.pack_into("<Q", directory.data, vo + 0x30, NEWTIME)
        print(f"{NAME}: modified {old} -> {NEWTIME}")
        break
else:
    raise SystemExit(f"no {NAME}")

# Checksums upwards: directory page -> both object tables -> checkpoint.
for i, t in tables.items():
    set_checksum(t.data, d_at[i], directory.data)
    set_checksum(chk, root_at[i], t.data)
desc = u32(chk, 0x58)
first = bytearray(chk[:CL]); first[desc:desc + REF] = bytes(REF)
set_checksum(chk, desc, first)
directory.write()
for t in tables.values():
    t.write()
wr(cp_lcn, chk[:CL])
f.close()
print(f"checkpoint {cp_lcn:#x} clock {u64(chk, 0x60):#x} rewritten")
