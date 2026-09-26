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
| `0x1000` | Pool database (SDBC header + SDBB entries); in larger pools only some members carry a copy, the others are zero here (**verified**: `dual7`, 5 of 7) |
| `0x2000_0000` | Data area: physical slab `n` at `0x2000_0000 + n * 0x1000_0000` (**verified**) |

Slabs are 256 MiB.

## Disk header ("SPACEDB ", BE)

| Offset | Size | Field |
|---|---|---|
| 0x00 | 8 | "SPACEDB " |
| 0x08 | 2 | layout version, 3 on Windows 10/11 |
| 0x0a | 2 | 0x0200 |
| 0x0c | 4 | CRC-32 (zlib, stored BE) of bytes 0..0x200 with this field zeroed (**verified**) |
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
| 0x0c | 4 | CRC-32 like the disk header (**verified**) |
| 0x10 | 16 | owner GUID (pool or space) |
| 0x24 | 4 | entry size (0x40) |
| 0x28 | 4 | entry slots in use, including the 8 header slots |
| 0x40 | 8 | update sequence; the member copy with the highest value is current |
| 0x48 | 8 | FILETIME of the last update |

Entry slot `i` (i >= 8) is at `i * entry_size`:

| Offset | Size | Field |
|---|---|---|
| 0x00 | 4 | "SDBB" |
| 0x04 | 4 | own slot number (checked as an integrity test) |
| 0x08 | 4 | record id, 0 = free |
| 0x0c | 2 | fragment index |
| 0x0e | 2 | fragment count |
| 0x10 | 0x30 | payload fragment |

Concatenated fragments form a record: `type:u8, version:u8, 2 bytes, length:u32`,
then `length` bytes of body.

## Records

### Type 1: pool
`vint, vint, guid[16], name, description, vint, u16 version,
u8 log2(logical sector size), u8 log2(physical sector size), ...`
(**verified**: version 29, sectors 512/4096 and 4096/4096). Strings are `u16 BE`
length in UTF-16 code units (including the terminating NUL) followed by
UTF-16BE text.

Spaces expose the pool's logical sector size (**verified** against
`Get-VirtualDisk`); space records carry no sector size of their own. Whether a
later change of the pool default affects existing spaces is not tested yet.
Block devices and loop devices for a space must use this sector size: a 4 KiB
space keeps its GPT at byte 0x1000.

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

Storage tiers (**verified**: pools `tiered`, SSD mirror + HDD 2-column simple,
and `mapar`, SSD mirror + HDD 3-column parity): the user space has no extents
of its own; each tier is a type 6 child of the space with its own policy and
extents. After the parent id a type 6 record holds `u32`, then the start and
length of the child within the parent's address space (`u64` BE each: SSD
0/1 GiB, HDD 1 GiB/2 GiB; a cache child covers its own size from 0). Extent
virtual slab numbers are those of the parent space; rows and parity rotation
of a tier count from the tier's start. Tier templates created with
`New-StorageTier` are separate type 6 records without extents.

### Type 4: extent
```
vint, vint (record format: 1 metadata space, 2, 3 after a state change),
vint, u8 flags, vint slab_count, vint space_id, vint virtual_slab,
vint column, vint copy, vint stale marker, vint disk_id, vint physical_slab
```
`flags`: 0x04 on cache extents, 0x01 on a copy that is being regenerated.
The stale marker is `0xffffffff` for a current copy; when a disk drops out
and the space is written, Windows keeps that disk's copy in the database with
a different value (2 in the sample) and allocates a replacement copy with a
copy number beyond the policy's copy count and flag 0x01 (**verified**: pool
`stale3`, 2-way mirror on 3 disks; only the copy that stayed current holds the
data written while the disk was away). Readers must use current copies only;
a row with out-of-date copies only is lost.
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
  in column `(parity + 1 + i) % C`. Parity = XOR of the data units. The
  stripe number `s` counts from the first row of the extent run (type 4
  record) that holds the stripe (**verified**: `mapar`, whose parity tier has
  one run per row; pools with a single run cannot tell the difference).
* Dual parity (`redundancy 2`): `D = columns - 2`; the two parity units of
  stripe `s` are in columns `P = (C - 2 - 2s) mod C` and `P + 1`, data unit
  `i` in column `(P + 2 + i) % C` (**verified**: pool `dual7`, 7 columns).
  The general rule for `r` parity units is `P = (C - r - r*s) mod C`.
  P is the XOR of the data units (**verified**). Q is a Reed-Solomon code
  over GF(16) (polynomial x^4 + x + 1) in bit-matrix form: each 512-byte
  chunk of a unit is four 128-byte packets, packet `j` holding bit `j` of
  4-bit symbols, and Q = sum of `c_k * D_k` where multiplying by `c` XORs
  input packet `j` into output packet `i` when bit `i` of `c * x^j` is set.
  The coefficients depend on the number of data columns D and not on the
  physical column: D = 5: `9, 1, 8, 2, 11`; D = 6..8: the first D of
  `13, 9, 4, 1, 12, 8, 5, 2` (5 data columns use every second element of
  that sequence). Measured with impulse pools of 7-10 columns (`imp7b`,
  `imp8`-`imp10`)
  (**verified**: derived from single-byte impulse stripes written by Windows
  (pool `imp7b`) and checked over whole units of 12 stripes of `dual7`; any
  two failed disks of `dual7` read back its full pattern).
* Dual parity with 11 or more columns uses `g` groups (`groups` in the
  policy: 11-12 columns 2 groups, 17 columns 3 groups), a local
  reconstruction code with `r = g + 1` parity units per stripe and
  `D = C - g - 1` data units. The parity units rotate like above
  (`P = (C - r - r*s) mod C`, data following them). The data units of a
  stripe are split into `g` consecutive groups, the first `D mod g` groups
  one unit larger (12 columns: 5 + 4, 17 columns: 5 + 4 + 4). Parity unit
  `i < g` is the XOR of group `i` (local parity); the last one is a global
  GF(16) parity in the bit-matrix form above with the coefficients
  `1, 2, 3, ...` restarting in every group (12 columns: `1 2 3 4 5 1 2 3 4`).
  (**verified**: the unit map and all three parity units of the first 8
  stripes of pattern pool `lrc12`, the global code solved bit by bit over
  GF(2) and matching GF(16) multiplication; any two failed disks of `lrc11`
  and `lrc12` read back their pattern, and of impulse pool `lrc17i` its
  impulses.) Two lost units of one group are solved from the local and the
  global parity, lost units of different groups from their local parities.
  Windows gives these spaces a write-back cache even with
  `-WriteCacheSize 0` (`lrc11`: 1.5 GiB, `lrc12`: 1 GiB); its chunk size is
  one data stripe.
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
0x28, `u32 count` at 0x30, then `count` variable-length entries at 0x38:

| Offset | Size | Field |
|---|---|---|
| 0x00 | 8 | owner offset (multiple of the chunk size); bit 63 is a flag of unknown meaning, seen only on entries superseded by the next slot |
| 0x08 | 4 | cache chunk index, `0xffffffff` = chunk removed from the cache |
| 0x0c | 2 | state: 0 = block assigned, nothing valid; 2 = partially valid; 3 = whole chunk valid |
| 0x0e | 2 | number of extra 16-bit words that follow the entry |
| 0x10 | 2*n | for state 2: runs from the chunk start, u16 LE each, bit 15 = valid, bits 0-14 = length in 512-byte sectors, 0 ends the list |

(**verified**: pool `parity4`, whose 1 MiB-split writes leave chunks of three
256 KiB units valid for 256 KiB or 512 KiB; the log had wrapped around its
1024 slots.) Entries are ordered by slot sequence and then by position within
the slot.

Data of owner offset `X` held in the cache is at cache offset
`data_offset + chunk_index * chunk_size + (X % chunk_size)`.

An entry with chunk index `0xffffffff` removes the owner chunk from the cache;
Windows writes it when it destages the chunk (**verified**: pools `au1g`,
`wc64`). A mapping is current only if it is the newest entry for its owner
chunk and the newest assignment of its cache block.

The cache is the space's write-back cache (`WriteCacheSize` of
`New-VirtualDisk`): with `-WriteCacheSize 0` no cache children exist, and the
smallest cache Windows creates is 512 MiB (**verified**: pools `nocache`,
`wc64`). Writes into unallocated rows of a thin space can stay in the cache
even after the pool is cleanly detached, so the cache must be consulted on
every read.

Extent `slab_count` is always in 256 MiB units, also for spaces with a 1 GiB
allocation unit (**verified**: pool `au1g`).

## Parity journal (SPVDT, LE)

Parity spaces have a hidden role 0x0a child holding "SPVDT\0\0\0", with the
same header and slot geometry as SPCACHE (CRC-32 fields, owner GUID,
`slot_offset`, `slot_size`, `slot_count`). Mapping slots (type 0) hold
entries keyed by the owner offset where an extent run starts:

| Offset | Size | Field |
|---|---|---|
| 0x00 | 8 | owner offset of the extent run |
| 0x08 | 2 | state: 1 = bitmap, 2 = run list, 3 = whole run consistent |
| 0x0a | 2 | states 1 and 2: byte length of what follows |
| 0x0c | … | state 1: bitmap, one bit per stripe; state 2: u16 LE runs (bit 15 = consistent, bits 0-14 = stripes); state 3: 4 bytes |

The newest entry per run wins. After a clean shutdown the runs are state 3
(or all-set bitmaps). In the crash experiment `crashparity` (disks pulled
while writing) the entry was state 2 `consistent 1164, unknown 2932` over
4096 stripes, and exactly stripe 1165 held new parity with a stale (zero) data
unit; Windows recovered that unit from parity, while in stripe 1164 it kept
the data and rewrote the stale parity. Because the rule Windows applies is
not known, the reader checks every stripe the journal does not mark
consistent (P = XOR of the data) and refuses reads of mismatching stripes
unless told to prefer the data (**verified**: every other MiB of the crashed
pool equals what Windows shows after recovery).

The mirror dirty region log (SPACEDRT, role 6) was unchanged in the mirror
crash experiment; Windows' recovered content equalled copy 0, which the reader
prefers. The thin space crash (cache in use) read exactly like Windows'
recovered space.

## Open questions

* SDBB entries carry no checksum of their own; how Windows detects torn
  entries is unknown.
* Remaining record fields (provisioning type, sizes, disk attributes, tiers).
* Per-space databases (type 7 record lists member disks).
* SPACEDRT contents when regions are dirty; cache slot type 1; cache head/tail.
* Which side Windows trusts for an inconsistent parity stripe.
* Group sizes of grouped dual parity for 13-16 columns (not generated yet;
  the rule above predicts them).
* Tier movement by the tiering optimizer (not exercised yet), enclosure
  awareness, older pool versions
  (Windows 8/Server 2012 layout differs, see StorageSpaceReconstructor).
