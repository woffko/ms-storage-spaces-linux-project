#!/usr/bin/env python3
"""Convert a dynamic or fixed VHDX file to a sparse raw image.

qemu-img 8.2 refuses VHDX files with 4096-byte logical sectors ("Operation
not supported"); this converter follows the VHDX specification (MS-VHDX)
for the cases the test VMs produce: no parent (not differencing), an empty
log, any block and logical sector size.

Usage: tools/vhdx2raw.py IN.vhdx OUT.img
"""
import struct
import sys
import uuid

BAT_REGION = uuid.UUID("2DC27766-F623-4200-9D64-115E9BFD4A08")
METADATA_REGION = uuid.UUID("8B7CA206-4790-4B9A-B8FE-575F050F886E")
FILE_PARAMETERS = uuid.UUID("CAA16737-FA36-4D43-B3B6-33F0AA44E76B")
VIRTUAL_DISK_SIZE = uuid.UUID("2FA54224-CD1B-4876-B211-5DBED83BF4B8")
LOGICAL_SECTOR_SIZE = uuid.UUID("8141BF1D-A96F-4709-BA47-F233A8FAAB5F")

PAYLOAD_FULLY_PRESENT = 6
PAYLOAD_ABSENT = {0, 1, 2, 3}  # not present, undefined, zero, unmapped


def read_at(f, offset, length):
    f.seek(offset)
    data = f.read(length)
    if len(data) != length:
        raise SystemExit(f"short read at {offset:#x}")
    return data


def main():
    src, dst = sys.argv[1:3]
    with open(src, "rb") as f:
        if read_at(f, 0, 8) != b"vhdxfile":
            raise SystemExit("not a VHDX file")
        # The current header is the valid one with the higher sequence number.
        headers = []
        for off in (0x10000, 0x20000):
            h = read_at(f, off, 4096)
            if h[:4] == b"head":
                headers.append((struct.unpack_from("<Q", h, 8)[0], h))
        if not headers:
            raise SystemExit("no VHDX header")
        header = max(headers)[1]
        if header[48:64] != bytes(16):
            raise SystemExit("the VHDX log is not empty (the file was not closed cleanly)")

        regions = {}
        table = read_at(f, 0x30000, 0x10000)
        if table[:4] != b"regi":
            raise SystemExit("no VHDX region table")
        for i in range(struct.unpack_from("<I", table, 8)[0]):
            e = table[16 + 32 * i:48 + 32 * i]
            regions[uuid.UUID(bytes_le=e[:16])] = struct.unpack_from("<QI", e, 16)

        meta_off, meta_len = regions[METADATA_REGION]
        meta = read_at(f, meta_off, meta_len)
        if meta[:8] != b"metadata":
            raise SystemExit("no VHDX metadata table")
        items = {}
        for i in range(struct.unpack_from("<H", meta, 10)[0]):
            e = meta[32 + 32 * i:64 + 32 * i]
            off, length = struct.unpack_from("<II", e, 16)
            items[uuid.UUID(bytes_le=e[:16])] = meta[off:off + length]
        block_size, flags = struct.unpack("<II", items[FILE_PARAMETERS][:8])
        if flags & 2:
            raise SystemExit("differencing VHDX files are not supported")
        size = struct.unpack("<Q", items[VIRTUAL_DISK_SIZE][:8])[0]
        sector = struct.unpack("<I", items[LOGICAL_SECTOR_SIZE][:4])[0]
        # A sector bitmap entry follows every chunk_ratio payload entries.
        chunk_ratio = (1 << 23) * sector // block_size

        bat_off, bat_len = regions[BAT_REGION]
        bat = read_at(f, bat_off, bat_len)
        with open(dst, "wb") as out:
            out.truncate(size)
            for block in range((size + block_size - 1) // block_size):
                entry = struct.unpack_from("<Q", bat, 8 * (block + block // chunk_ratio))[0]
                state = entry & 7
                if state in PAYLOAD_ABSENT:
                    continue
                if state != PAYLOAD_FULLY_PRESENT:
                    raise SystemExit(f"block {block}: unsupported payload state {state}")
                length = min(block_size, size - block * block_size)
                data = read_at(f, (entry >> 20) << 20, length)
                if any(data):
                    out.seek(block * block_size)
                    out.write(data)
    print(f"{dst}: {size} bytes, {sector}-byte sectors, {block_size >> 20} MiB blocks")


if __name__ == "__main__":
    main()
