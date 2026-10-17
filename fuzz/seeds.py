#!/usr/bin/env python3
"""Writes the seed inputs of fuzz targets that start from crafted cases
into their corpus (fuzz/corpus/<target>/seed-*), and the dictionaries
(fuzz/dicts/<target>.dict, which libFuzzer gets as -dict). The cases are
those of the code audit of 2026-10-16: partition tables and boot sectors
with numbers past any disk (guard_userdata), record fields at the edges of
their types (policy_fields). tools/fuzz-long.sh runs it before fuzzing:
python3 fuzz/seeds.py
"""
import os
import struct
import zlib

HERE = os.path.dirname(os.path.abspath(__file__))
CORPUS = os.path.join(HERE, "corpus")
DICTS = os.path.join(HERE, "dicts")
PART = 64 << 10

# guard_userdata: the first byte picks the fixture (0 simple1c, 512-byte
# sectors, a space of 1 GiB; 1 sect4k, 4 KiB sectors, 2 GiB; 2 mirror2; 3
# parity3), then 64 KiB for the start of the space and up to 64 KiB for
# its end.
BASIC_DATA = bytes([0xa2, 0xa0, 0xd0, 0xeb, 0xe5, 0xb9, 0x33, 0x44, 0x87, 0xc0, 0x68, 0xb6, 0xb7, 0x26, 0x99, 0xc7])


def mbr(entries=(), protective=False, lbas=0, sector0=b""):
    s = bytearray(512)
    s[: len(sector0)] = sector0
    if protective:
        s[446 + 4] = 0xEE
        s[446 + 8 : 446 + 12] = struct.pack("<I", 1)
        s[446 + 12 : 446 + 16] = struct.pack("<I", min(lbas - 1, 0xFFFFFFFF))
    for i, (kind, start, count) in enumerate(entries):
        o = 446 + i * 16
        s[o + 4] = kind
        s[o + 8 : o + 12] = struct.pack("<I", start)
        s[o + 12 : o + 16] = struct.pack("<I", count)
    s[510:512] = b"\x55\xaa"
    return bytes(s)


def entries(parts, fill=None):
    t = bytearray(128 * 128)
    for i, (first, last) in enumerate(parts):
        e = bytearray(128)
        e[0:16] = BASIC_DATA
        e[16:32] = bytes([i + 1]) * 16
        e[32:40] = struct.pack("<Q", first)
        e[40:48] = struct.pack("<Q", last)
        t[i * 128 : (i + 1) * 128] = e
    if fill is not None:
        t[0:128] = bytes([fill]) * 128
    return bytes(t)


def header(my, alt, entries_lba, table, lbas, sector):
    h = bytearray(92)
    h[0:8] = b"EFI PART"
    h[8:12] = struct.pack("<I", 0x00010000)
    h[12:16] = struct.pack("<I", 92)
    h[24:32] = struct.pack("<Q", my)
    h[32:40] = struct.pack("<Q", alt)
    h[40:48] = struct.pack("<Q", 34)
    h[48:56] = struct.pack("<Q", lbas - 34)
    h[56:72] = bytes([0x5A]) * 16
    h[72:80] = struct.pack("<Q", entries_lba)
    h[80:84] = struct.pack("<I", 128)
    h[84:88] = struct.pack("<I", 128)
    h[88:92] = struct.pack("<I", zlib.crc32(table))
    h[16:20] = struct.pack("<I", zlib.crc32(bytes(h)))
    return bytes(h).ljust(sector, b"\0")


def ntfs_boot(sector, bps=512, sectors=1 << 20):
    b = bytearray(sector)
    b[0:3] = b"\xeb\x52\x90"
    b[3:11] = b"NTFS    "
    b[0x0B:0x0D] = struct.pack("<H", bps)
    b[0x0D] = 8
    b[0x28:0x30] = struct.pack("<Q", sectors)
    b[510:512] = b"\x55\xaa"
    return bytes(b)


def refs_boot(sector, sectors):
    b = bytearray(sector)
    b[3:11] = b"ReFS\0\0\0\0"
    b[0x10:0x14] = b"FSRS"
    b[0x18:0x20] = struct.pack("<Q", sectors)
    b[0x20:0x24] = struct.pack("<I", 512)
    b[0x24:0x28] = struct.pack("<I", 8)
    b[0x28] = 3
    b[0x29] = 14
    # Its checksum (refs::checksum::boot_sum), which fuzzing builds skip.
    c = 0
    for i in range(3, 512):
        if i not in (0x16, 0x17):
            c = (((c >> 1) | ((c & 1) << 15)) + b[i]) & 0xFFFF
    b[0x16:0x18] = struct.pack("<H", c)
    return bytes(b)


def space(start, end=b""):
    return start[:PART].ljust(PART, b"\0") + end[:PART]


def guard_seeds():
    out = {}
    for which, (sector, size) in [(0, (512, 1 << 30)), (1, (4096, 2 << 30))]:
        lbas = size // sector
        first = 64 if sector == 512 else 8
        part = entries([(first, lbas - 64)])
        table_lba = 2
        start = bytearray(PART)
        start[0:512] = mbr(protective=True, lbas=lbas)
        start[sector : 2 * sector] = header(1, lbas - 1, table_lba, part, lbas, sector)
        start[table_lba * sector : table_lba * sector + len(part)] = part
        good = bytes(start)
        # The table with an NTFS boot sector, and with a ReFS one claiming
        # 2^58 sectors, in the partition.
        for name, boot in [("ntfs", ntfs_boot(sector, bps=sector)), ("refs58", refs_boot(sector, 1 << 58))]:
            s = bytearray(good)
            s[first * sector : first * sector + len(boot)] = boot
            out[f"{which}-{name}"] = bytes([which]) + space(bytes(s))
        # Its first entry all ones.
        s = bytearray(good)
        bad = entries([(first, lbas - 64)], fill=0xFF)
        s[sector : 2 * sector] = header(1, lbas - 1, table_lba, bad, lbas, sector)
        s[table_lba * sector : table_lba * sector + len(bad)] = bad
        out[f"{which}-entry-ones"] = bytes([which]) + space(bytes(s))
        # The primary header naming its entries at LBA 2^63.
        s = bytearray(good)
        s[sector : 2 * sector] = header(1, lbas - 1, 1 << 63, part, lbas, sector)
        out[f"{which}-entries-lba"] = bytes([which]) + space(bytes(s))
        # A valid primary, the backup header at the last LBA naming 2^63.
        end = bytearray(PART)
        end[PART - sector :] = header(lbas - 1, 1, 1 << 63, part, lbas, sector)
        out[f"{which}-backup-lba"] = bytes([which]) + space(good, bytes(end))
        # The primary header lost, the backup and its entries intact.
        start = bytearray(PART)
        start[0:512] = mbr(protective=True, lbas=lbas)
        end = bytearray(PART)
        at = PART - 33 * sector
        end[at : at + len(part)] = part
        end[PART - sector :] = header(lbas - 1, 1, lbas - 33, part, lbas, sector)
        out[f"{which}-backup-only"] = bytes([which]) + space(bytes(start), bytes(end))
    # An MBR partition (start 0, one sector) over a sector 0 that looks
    # like an NTFS boot sector of 4096-byte sectors: NTFS check's underflow.
    boot = ntfs_boot(512, bps=4096, sectors=0)
    s0 = mbr([(0x07, 0, 1)], sector0=boot[:446])
    for which in range(4):
        out[f"{which}-ntfs-underflow"] = bytes([which]) + space(s0)
        out[f"{which}-empty"] = bytes([which])
    return out


# policy_fields: the first byte picks the fixture (0 simple2c, 1 mirror2, 2
# mirror3, 3 parity3, 4 dual7, 5 lrc11, 6 tiered, 7 wc64), then edits of
# (field, record, value index): fields 0 size, 1 columns, 2 copies, 3
# redundancy, 4 groups, 5 interleave, 6 allocation unit; 7 slabs, 8
# virtual slab, 9 column, 10 copy, 11 physical slab, 12 disk, 13 stale.
# The list of fuzz_targets/policy_fields.rs, in its order.
VALUES = [
    0, 1, 2, 3, 4, 7, 8, 15, 16, 17, 31, 32, 33, 63, 64, 255, 256, 1023, 1024, 1025, 0xFFFF, 1 << 16,
    (1 << 31) - 1, 1 << 31, 0xFFFFFFFF, 1 << 32, 1 << 36, 1 << 56, 1 << 60, 1 << 62, (1 << 64) - 2, (1 << 64) - 1,
]


def policy_seeds():
    cases = {
        # The audit's cases (SPA): extent copy numbers, copies, columns,
        # a run of 2^36 slabs, sizes.
        "copy-max": [(10, 0, (1 << 64) - 1)],
        "copy-huge": [(10, 0, 1 << 36)],
        "copies-max": [(2, 0, (1 << 64) - 1)],
        "columns-u32": [(1, 0, 0xFFFFFFFF)],
        "columns-2-32": [(1, 0, 1 << 32)],
        "columns-1025": [(1, 0, 1025)],
        "slabs-2-36": [(7, 0, 1 << 36)],
        "size-2-62": [(0, 0, 1 << 62)],
        "size-0": [(0, 0, 0)],
        "groups-big": [(4, 0, 1 << 31)],
        "redundancy-3": [(3, 0, 3)],
        "interleave-255": [(5, 0, 255)],
        "unit-0": [(6, 0, 0)],
        "physical-2-56": [(11, 0, 1 << 56)],
        "virtual-max": [(8, 0, (1 << 64) - 2)],
        "stale-0": [(13, 0, 0)],
        "disk-0": [(12, 0, 0)],
        "column-1024": [(9, 0, 1024)],
        "two-fields": [(1, 0, 1024), (2, 0, 16)],
    }
    out = {}
    for which in range(8):
        for name, edits in cases.items():
            data = bytes([which]) + b"".join(bytes([f, r, VALUES.index(v)]) for f, r, v in edits)
            out[f"{which}-{name}"] = data
    return out


def le64(v):
    return "".join(f"\\x{b:02x}" for b in struct.pack("<Q", v))


def le32(v):
    return "".join(f"\\x{b:02x}" for b in struct.pack("<I", v))


def refs_dict():
    lines = ['# Boundary values for refs_volume: boot sector sectors, checkpoint clusters, container rows.']
    for name, v in [
        ("zero", 0), ("one", 1), ("units-0x40", 0x40), ("sectors-2-55", 1 << 55), ("sectors-2-58", 1 << 58),
        ("cluster-2-52", 1 << 52), ("cluster-2-63", 1 << 63), ("near-2-64", (1 << 64) - 0x11),
        ("ones", (1 << 64) - 1), ("start-2-32", 1 << 32),
    ]:
        lines.append(f'u64_{name.replace("-", "_")}="{le64(v)}"')
    for name, v in [("cpc-0x4000", 0x4000), ("cpc-0x400", 0x400), ("cpc-2-20", 1 << 20), ("cpc-2-31", 1 << 31),
                    ("u32-ones", 0xFFFFFFFF), ("rows-0x30", 0x30)]:
        lines.append(f'u32_{name.replace("-", "_")}="{le32(v)}"')
    for sig in ["MLog", "CHKP", "SUPB", "MSB+", "FSRS", "ReFS"]:
        lines.append(f'sig_{sig.replace("+", "plus")}="{sig}"')
    return "\n".join(lines) + "\n"


def guard_dict():
    lines = ["# Partition tables and boot sectors for guard_userdata."]
    lines.append('gpt="EFI PART"')
    lines.append('ntfs="NTFS    "')
    lines.append('refs="ReFS\\x00\\x00\\x00\\x00"')
    lines.append('fsrs="FSRS"')
    lines.append('mbr_sig="\\x55\\xaa"')
    lines.append('basic_data="' + "".join(f"\\x{b:02x}" for b in BASIC_DATA) + '"')
    for name, v in [("lba-2-63", 1 << 63), ("lba-2-56", 1 << 56), ("ones", (1 << 64) - 1), ("sectors-2-58", 1 << 58)]:
        lines.append(f'u64_{name.replace("-", "_")}="{le64(v)}"')
    for name, v in [("count-4096", 4096), ("size-4096", 4096), ("ones", 0xFFFFFFFF)]:
        lines.append(f'u32_{name.replace("-", "_")}="{le32(v)}"')
    return "\n".join(lines) + "\n"


def write_dir(target, seeds):
    d = os.path.join(CORPUS, target)
    os.makedirs(d, exist_ok=True)
    for name, data in sorted(seeds.items()):
        with open(os.path.join(d, "seed-" + name), "wb") as f:
            f.write(data)


write_dir("guard_userdata", guard_seeds())
write_dir("policy_fields", policy_seeds())
os.makedirs(DICTS, exist_ok=True)
with open(os.path.join(DICTS, "refs_volume.dict"), "w") as f:
    f.write(refs_dict())
with open(os.path.join(DICTS, "guard_userdata.dict"), "w") as f:
    f.write(guard_dict())
