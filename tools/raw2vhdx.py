#!/usr/bin/env python3
"""Convert a raw image into a dynamic VHDX file (the reverse of vhdx2raw.py).

Blocks that hold only zeros are left out, so a sparse pool member becomes a
small file. Unlike qemu-img, the logical sector size can be 4096, so 4Kn
pool members can go back to Windows. Follows MS-VHDX: no parent, an empty
log, CRC-32C checksums over the headers and region tables.

Usage: tools/raw2vhdx.py [--sector 512|4096] [--block-mb N] IN.img OUT.vhdx
"""
import argparse
import os
import struct
import uuid

MB = 1 << 20
BAT_REGION = uuid.UUID("2DC27766-F623-4200-9D64-115E9BFD4A08")
METADATA_REGION = uuid.UUID("8B7CA206-4790-4B9A-B8FE-575F050F886E")
FILE_PARAMETERS = uuid.UUID("CAA16737-FA36-4D43-B3B6-33F0AA44E76B")
VIRTUAL_DISK_SIZE = uuid.UUID("2FA54224-CD1B-4876-B211-5DBED83BF4B8")
PAGE_83_DATA = uuid.UUID("BECA12AB-B2E6-4523-93EF-C309E000C746")
LOGICAL_SECTOR_SIZE = uuid.UUID("8141BF1D-A96F-4709-BA47-F233A8FAAB5F")
PHYSICAL_SECTOR_SIZE = uuid.UUID("CDA348C7-445D-4471-9CC9-E9885251C556")

PAYLOAD_NOT_PRESENT = 0
PAYLOAD_FULLY_PRESENT = 6

# Fixed layout: headers and region tables in the first MiB, then the log,
# the metadata region and the BAT, each starting on a MiB boundary.
LOG_OFFSET, LOG_LENGTH = 1 * MB, 1 * MB
METADATA_OFFSET, METADATA_LENGTH = 2 * MB, 1 * MB
BAT_OFFSET = 3 * MB


def crc32c(data):
    crc = 0xFFFFFFFF
    for b in data:
        crc = CRC32C_TABLE[(crc ^ b) & 0xFF] ^ (crc >> 8)
    return crc ^ 0xFFFFFFFF


def _table():
    table = []
    for i in range(256):
        c = i
        for _ in range(8):
            c = (c >> 1) ^ 0x82F63B78 if c & 1 else c >> 1
        table.append(c)
    return table


CRC32C_TABLE = _table()


def with_checksum(buf):
    """Stores the CRC-32C of `buf` (checksum field at 4..8 zeroed) in it."""
    buf[4:8] = bytes(4)
    buf[4:8] = struct.pack("<I", crc32c(bytes(buf)))
    return bytes(buf)


def header(sequence, file_write, data_write):
    h = bytearray(4096)
    struct.pack_into("<4sIQ16s16s16sHHIQ", h, 0, b"head", 0, sequence, file_write.bytes_le,
                     data_write.bytes_le, bytes(16), 0, 1, LOG_LENGTH, LOG_OFFSET)
    return with_checksum(h)


def region_table(bat_length):
    t = bytearray(64 * 1024)
    struct.pack_into("<4sIII", t, 0, b"regi", 0, 2, 0)
    struct.pack_into("<16sQII", t, 16, BAT_REGION.bytes_le, BAT_OFFSET, bat_length, 1)
    struct.pack_into("<16sQII", t, 48, METADATA_REGION.bytes_le, METADATA_OFFSET, METADATA_LENGTH, 1)
    return with_checksum(t)


def metadata(block_size, size, sector):
    m = bytearray(METADATA_LENGTH)
    virtual_disk, required = 2, 4
    items = [
        (FILE_PARAMETERS, struct.pack("<II", block_size, 0), required),
        (VIRTUAL_DISK_SIZE, struct.pack("<Q", size), virtual_disk | required),
        (PAGE_83_DATA, uuid.uuid4().bytes_le, virtual_disk | required),
        (LOGICAL_SECTOR_SIZE, struct.pack("<I", sector), virtual_disk | required),
        (PHYSICAL_SECTOR_SIZE, struct.pack("<I", 4096), virtual_disk | required),
    ]
    struct.pack_into("<8sHH", m, 0, b"metadata", 0, len(items))
    at = 64 * 1024  # item data starts after the table
    for i, (item, data, flags) in enumerate(items):
        struct.pack_into("<16sIIII", m, 32 + 32 * i, item.bytes_le, at, len(data), flags, 0)
        m[at:at + len(data)] = data
        at += 4096
    return bytes(m)


def has_data(fd, start, end):
    """Whether a sparse file has data (not a hole) in [start, end)."""
    try:
        return os.lseek(fd, start, os.SEEK_DATA) < end
    except OSError:  # no data after start
        return False


def main():
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--sector", type=int, choices=(512, 4096), default=512, help="logical sector size")
    p.add_argument("--block-mb", type=int, default=32, help="payload block size in MiB (power of 2)")
    p.add_argument("src")
    p.add_argument("dst")
    a = p.parse_args()
    block_size = a.block_mb * MB
    if a.block_mb & (a.block_mb - 1) or not 1 <= a.block_mb <= 256:
        raise SystemExit("the block size must be a power of 2 from 1 to 256 MiB")
    size = os.path.getsize(a.src)
    if size % a.sector:
        raise SystemExit(f"{a.src}: size is not a multiple of {a.sector}")

    blocks = (size + block_size - 1) // block_size
    # A sector bitmap entry follows every chunk_ratio payload entries.
    chunk_ratio = (1 << 23) * a.sector // block_size
    entries = blocks + (blocks - 1) // chunk_ratio
    bat_length = (8 * entries + MB - 1) // MB * MB
    bat = bytearray(bat_length)
    next_offset = BAT_OFFSET + bat_length
    present = 0
    with open(a.src, "rb") as src, open(a.dst, "xb") as out:
        for block in range(blocks):
            state = PAYLOAD_NOT_PRESENT
            data = b""
            if has_data(src.fileno(), block * block_size, (block + 1) * block_size):
                src.seek(block * block_size)
                data = src.read(block_size)
            if any(data):
                out.seek(next_offset)
                out.write(data.ljust(block_size, b"\0"))
                state = PAYLOAD_FULLY_PRESENT | next_offset
                next_offset += block_size
                present += 1
            struct.pack_into("<Q", bat, 8 * (block + block // chunk_ratio), state)
        identifier = bytearray(64 * 1024)
        identifier[:8] = b"vhdxfile"
        creator = "storage-spaces raw2vhdx.py".encode("utf-16-le")
        identifier[8:8 + len(creator)] = creator
        file_write, data_write = uuid.uuid4(), uuid.uuid4()
        for offset, data in [
            (0, bytes(identifier)),
            (0x10000, header(0, file_write, data_write)),
            (0x20000, header(1, file_write, data_write)),
            (0x30000, region_table(bat_length)),
            (0x40000, region_table(bat_length)),
            (LOG_OFFSET, bytes(LOG_LENGTH)),
            (METADATA_OFFSET, metadata(block_size, size, a.sector)),
            (BAT_OFFSET, bytes(bat)),
        ]:
            out.seek(offset)
            out.write(data)
        out.truncate(next_offset)
    print(f"{a.dst}: {size} bytes, {a.sector}-byte sectors, {present} of {blocks} blocks of {a.block_mb} MiB")


if __name__ == "__main__":
    main()
