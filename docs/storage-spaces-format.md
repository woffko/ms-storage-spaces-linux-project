# Storage Spaces on-disk format

Reverse-engineered from pools created by Windows 11 Pro Insider build 26340
(pool version 29, `spaceport.sys` 10.0.26100.8951). Every statement marked
**verified** is checked by the corpus tests (`crates/storage-spaces/tests/corpus.rs`)
against Windows' own `Get-PhysicalExtent` output and against a known data
pattern. Everything else is an observation or a hypothesis.

Conventions: offsets in hex; "BE"/"LE" = big/little-endian; a **vint** is a
length-prefixed big-endian integer: one length byte `n` (0..8) followed by `n`
bytes (`00` = 0, `01 05` = 5, `04 10 00 00 00` = 0x10000000).

## Member disk

GPT disk with a partition of type `e75caf8f-f680-4cee-afa3-b001e56efc2d`
("Storage pool"). It starts at 16 MiB on disks Windows initialised itself and at
LBA 264192 on the older VMware test pool. Offsets below are relative to the
partition start.

| Offset | Content |
|---|---|
| `0x0` | Disk header "SPACEDB " |
| `0x1000` | Pool database (SDBC header + SDBB entries), a copy on every member |
| `0x2000_0000` | Data area: physical slab `n` at `0x2000_0000 + n * 0x1000_0000` (**verified**) |

Slabs are 256 MiB.

## Disk header ("SPACEDB ", BE)

| Offset | Size | Field |
|---|---|---|
| 0x00 | 8 | "SPACEDB " |
| 0x08 | 2 | layout version, 3 on Windows 10/11 |
| 0x0a | 2 | 0x0200 |
| 0x0c | 4 | checksum (algorithm unknown) |
| 0x18 | 8 | FILETIME when the disk joined the pool |
| 0x20 | 16 | pool GUID |
| 0x30 | 16 | physical disk GUID (as in the `PD:{...}` part of the disk ObjectId) |

GUIDs in the database are stored in plain big-endian byte order.

## Database ("SDBC    ", BE)

The pool database lives at partition offset 0x1000. Per-space databases with
the same structure exist inside the internal metadata space (the first one at
physical slab 0).

Header (the first 8 entry slots = 0x200 bytes):

| Offset | Size | Field |
|---|---|---|
| 0x00 | 8 | "SDBC    " |
| 0x0c | 4 | checksum (unknown) |
| 0x10 | 16 | owner GUID (pool or space) |
| 0x24 | 4 | entry size (0x40) |
| 0x28 | 4 | entry slots in use, including the 8 header slots |
| 0x40 | 8 | update sequence; the member copy with the highest value is current |
| 0x48 | 8 | FILETIME of the last update |

Entry slot `i` (i >= 8) is at `i * entry_size`:

| Offset | Size | Field |
|---|---|---|
| 0x00 | 4 | "SDBB" |
| 0x04 | 4 | unknown (slot number) |
| 0x08 | 4 | record id, 0 = free |
| 0x0c | 2 | fragment index |
| 0x0e | 2 | fragment count |
| 0x10 | 0x30 | payload fragment |

Concatenated fragments form a record: `type:u8, version:u8, 2 bytes, length:u32`,
then `length` bytes of body.

## Records

### Type 1: pool
`vint, vint, guid[16], name`. Strings are `u16 BE` length in UTF-16 code units
(including the terminating NUL) followed by UTF-16BE text.

### Type 2: physical disk
`vint id, vint, guid[16], name, ...` (then description, manufacturer, model,
serial, sizes; not decoded).

### Types 3 (space) and 6 (child space)
```
vint id, vint, guid[16], name, description,
u8, u8, u8 role, [type 3: vint size], ...,
01 00 01 00 00                      <- constant prefix, located by search
u8 resiliency   1 simple, 2 mirror, 3 parity
vint redundancy (disk failures tolerated)
vint copies
vint groups
vint columns
u8 log2(interleave)
type 3: vint, vint x4, vint (=1), vint parent, ...
type 6: vint, vint parent, ...
```
The bytes between size and the policy differ between fixed and thin
provisioning and are not understood yet. `role`: 1 = internal metadata space,
2 = user virtual disk, 0x0b = write-back cache container, 6 and 0x0a = unknown
256 MiB mirrored children of mirror and parity spaces (probably dirty region
tracking and parity journal).

Hierarchy of a user virtual disk:
```
user space (type 3, role 2)            data extents
 ├─ cache container (type 3, role 0x0b)
 │   └─ cache space (type 6)          extents; holds SPCACHE
 └─ role 6 / 0x0a container (mirror/parity only)
     └─ child (type 6)                extents
```

### Type 4: extent
```
vint, vint (format), vint, u8 flags (0x04 for cache extents),
vint slab_count, vint space_id, vint virtual_slab, vint column, vint copy,
vint (0xffffffff), vint disk_id, vint physical_slab
```
A run of `slab_count` consecutive physical slabs backs column `column`, copy
`copy`, rows `virtual_slab / data_columns ...` (**verified**).

## Data layout (**verified**)

Data is cut into `interleave`-sized units. Unit `u` belongs to stripe
`s = u / D` (D = data columns) and lives at column offset `s * interleave`.
Column offset `o` is in row `o / 256 MiB` of the column.

* Simple: `D = columns`, unit `u` in column `u % columns`.
* Mirror: as simple; every copy holds the same data.
* Single parity (`redundancy 1`): `D = columns - 1`, left-symmetric RAID-5:
  parity of stripe `s` in column `C - 1 - s % C`, data unit `i` of the stripe
  in column `(parity + 1 + i) % C`. Parity = XOR of the data units.
* Dual parity: not analysed yet.
* Thin spaces: rows without an extent are unallocated and read as zeros.

## Write-back cache (SPCACHE, LE) (**verified** for thin spaces)

The cache space starts with a header:

| Offset | Size | Field |
|---|---|---|
| 0x00 | 8 | "SPCACHE\0" |
| 0x08 | 16 | owner space GUID (mixed-endian, like GPT) |
| 0x18 | 4 | 1 |
| 0x1c | 4 | structure size (0x60) |
| 0x24 | 4 | CRC-32 (zlib) of the structure with this field zeroed |
| 0x28 | 8 | sequence |
| 0x30 | 8 | slot area offset (0x2000) |
| 0x38 | 4 | slot size (0x1000) |
| 0x3c | 4 | slot count (0x400) |
| 0x40 | 8 | unknown (end of slot area) |
| 0x50 | 8 | data area offset |
| 0x58 | 4 | chunk size = owner full data stripe (data columns x interleave) |
| 0x5c | 4 | number of chunks in the data area |

Slots ("SPSLOT\0\0"): same GUID, size and CRC fields as the header, `u32 type`
at 0x20 (0 = mapping; 1 seen on mirror caches, not decoded), `u64 sequence` at
0x28, `u32 count` at 0x30, then `count` 16-byte entries at 0x38:
`u64 owner offset, u32 chunk index, u32 validity bitmap`. The bitmap has one bit
per interleave unit of the chunk (hypothesis; only full chunks observed).

Data of owner offset `X` held in the cache is at cache offset
`data_offset + chunk_index * chunk_size + (X % chunk_size)`. The newest slot
wins per owner chunk and per cache block.

Writes into unallocated rows of a thin space stay in the cache even after the
pool is cleanly detached, so the cache must be consulted on every read.

## Open questions

* Checksums of SPACEDB and SDBC/SDBB.
* Remaining record fields (provisioning type, sizes, disk attributes, tiers).
* Per-space databases (type 7 record lists member disks).
* Role 6 / 0x0a children; slot type 1; cache head/tail and destaging.
* Dual parity, storage tiers, enclosure awareness, older pool versions
  (Windows 8/Server 2012 layout differs, see StorageSpaceReconstructor).
