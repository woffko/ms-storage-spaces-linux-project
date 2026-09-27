# Storage Spaces on-disk format

Reverse-engineered from pools created by Windows 11 Pro Insider build 26340
(pool version 29, `spaceport.sys` 10.0.26100.8951) and Windows 11 Pro 24H2
build 26100 (pool version 28; pools named `*_26100`). Every statement marked
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

Choosing a copy: members carry copies of the pool database, and the one
with the highest sequence is current (**verified**: stale copies of `stale3`).
SDBB entries carry no checksum, so a copy torn by an interrupted update can
only be recognised by comparison. `spaces` groups identical copies, uses the
newest version that decodes (the one most members hold when copies of one
sequence differ) and reports the others: copies of the same sequence with
different records as torn, newer copies that do not decode as unusable
(unit tests on `mirror3` with one copy altered).

## Records

### Type 1: pool
`vint, vint, guid[16], name, description, vint, u16 version,
u8 log2(logical sector size), u8 log2(physical sector size), ...`
(**verified**: versions 28 and 29, sectors 512/4096 and 4096/4096). Strings are `u16 BE`
length in UTF-16 code units (including the terminating NUL) followed by
UTF-16BE text.

Spaces expose the pool's logical sector size (**verified** against
`Get-VirtualDisk`); space records carry no sector size of their own. Whether a
later change of the pool default affects existing spaces is not tested yet.
Block devices and loop devices for a space must use this sector size: a 4 KiB
space keeps its GPT at byte 0x1000.

### Type 2: physical disk
```
vint id, vint sequence, guid[16], name, description,
u8, u8 (2; 0 on a retired disk), u8 usage,
manufacturer, model, string, string, u8 (0x0f), u8 media, ...
```
`sequence` is the database sequence at which the record was last written
(disks whose media type was set one after the other carry consecutive
numbers). `usage` counts differently from `Get-PhysicalDisk`: 1
Auto-Select, 2 Manual-Select, 3 Hot Spare, 4 Journal, 5 Retired. `media`:
0 unspecified, 1 HDD, 2 SSD. (**verified**: pools `usages`, one disk of
each usage and media type, and `retired`; the metadata tests compare both
with `Get-PhysicalDisk`.) Windows stops updating the pool database copy on a
retired disk, so its copy is older than the others; `spaces` does not
report that as stale. A disk removed from the pool (`Remove-PhysicalDisk`)
disappears from the database while its own header still names the pool;
`spaces` reports such a device as not listed in the pool database. A disk
of the database without a device at hand is missing.

### Types 3 (space) and 6 (child space)
```
vint id, vint sequence, guid[16], name, description,
u8, u8, u8 role,
type 3: vint size, vint number (0xffffffff on the metadata space)
u8 provisioning    1 thin, 2 fixed
vint allocation unit (bytes; all ones on tier templates)
u8                 (2 on tiered spaces and hidden containers, else 0)
prefix             01 00 01 00 00 (record version 17, type 6 version 5)
                   01 01 00 00    (record version 16: Windows 11 24H2)
u8 resiliency      1 simple, 2 mirror, 3 parity
vint redundancy    (disk failures tolerated)
vint copies
vint groups
vint columns       (0xffffffff on tier templates: chosen by Windows)
u8 log2(interleave)
type 3: vint x4, u8 n + n bytes (security descriptor, usually n = 0),
        vint (=1), vint parent, ...
type 6: vint, vint parent, ...
```
(**verified**: every corpus pool; `ressimple`, extended with
`Resize-VirtualDisk`, carries a 120-byte self-relative security descriptor
in its tail; provisioning and allocation unit equal
`Get-VirtualDisk` in the metadata tests.) The record version (byte 1 of the
record) selects the prefix: Insider build 26340 writes pool version 29 with
space records of version 17, Windows 11 24H2 (build 26100) pool version 28
with version 16 (**verified**: pools `*_26100`). A record with another prefix
is reported as an unsupported layout instead of being guessed at.
`sequence` is the database sequence at which the record was last written,
as for disks. `number` counts the spaces a pool gets, hidden containers
included (`spstates`: 0, 2, 4 for its three spaces, 1, 3, 5 for their dirty
region tracking containers).

Space states: a space set to manual attach and detached
(`Set-VirtualDisk -IsManualAttach`, `Disconnect-VirtualDisk`) and a space
whose disk is read-only (`Set-Disk -IsReadOnly`) have records identical to
those of a normal space apart from ids, names and numbers, and setting
these states wrote no record (pool `spstates`): they are not kept in the
pool database. `spaces` exposes every space read-only and attaches detached
ones too. Whether a space is degraded follows from its extents and the
disks at hand (`spaces info`: healthy, degraded or failed).

The internal metadata space (role 1) holds one database per space and
hidden container, 4 MiB apart, each an SDBC database (same header and
entries as the pool database) whose owner is the space GUID and whose only
record, type 7, lists the member disks.

`role`: 1 = internal metadata space, 2 = user virtual disk, 0x0b =
write-back cache container, 6 and 0x0a = unknown 256 MiB mirrored children
of mirror and parity spaces (dirty region tracking and parity journal).

Windows 11 24H2 shows pool version 28 as "Windows Server vNext" in
`Get-StoragePool`; the CIM property holds 28. It creates no write-back cache
for simple spaces by default (26340 creates 1 GiB).

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
at 0x20 (0 = mapping, 1 = initialisation record, see below), `u64 sequence` at
0x28, `u32 count` at 0x30, then `count` variable-length entries at 0x38:

| Offset | Size | Field |
|---|---|---|
| 0x00 | 8 | owner offset (multiple of the chunk size); bit 63 marks a provisional entry, see below |
| 0x08 | 4 | cache chunk index, `0xffffffff` = chunk removed from the cache |
| 0x0c | 2 | state: 0 = block assigned, nothing valid; 2 = partially valid; 3 = whole chunk valid |
| 0x0e | 2 | length in bytes of the data that follows the entry; the next entry starts at the next multiple of 8 |
| 0x10 | n | for state 2: runs from the chunk start, u16 LE each, bit 15 = valid, bits 0-14 = length in 512-byte sectors, 0 or the end of the data ends the list |

(**verified**: pool `parity4`, whose 1 MiB-split writes leave chunks of three
256 KiB units valid for 256 KiB or 512 KiB; the log had wrapped around its
1024 slots. The data length is in bytes and entries are 8-byte aligned
(**verified**: NTFS pool `ntfsparity`, whose entries carry 4 to 14 bytes of
runs summing to the chunk; with only 4-byte run lists, as in `parity4`,
bytes plus padding and 16-bit words cannot be told apart.) Entries are ordered by slot sequence and then by position within
the slot.

Provisional entries (bit 63 of the offset) are logged before their data is
written to the cache and committed by a later entry for the chunk without
the flag. A provisional entry that was never committed describes data that
may not exist and is ignored (**verified**: crash pool `crashparitywc`,
where the newest entry of a chunk was provisional, its cache block held
unrelated bytes, and Windows showed the chunk as the previous committed
entry describes; in the other pools every provisional entry was committed).

The cache and parity journal spaces are two-way mirrors. After an unclean
shutdown their copies can differ: a slot written last may have reached only
one copy (`crashparitywc`: slot 359 held sequence 1385 in one copy and 360
in the other; `crashparity`: a journal slot existed in one copy only).
Windows kept the newer slot in one experiment and the older in the other,
so `spaces` reads the slot areas of all copies, uses the newest valid slot
per position, and treats what the versions disagree about as unknown: cache
chunks mapped differently are refused, and parity stripes that any version
leaves inconsistent are checked against their parity (`--unclean-parity
data` reads the newest version instead).

Log order (**verified** as far as reads go: the newest-entry rule reproduces
the pattern of every corpus pool, including fully wrapped logs in `parity4`
and `lrc12`, and the content Windows showed after the crash experiments):
nothing on disk marks a head or a tail. The header is written once (its
sequence stays 1 while mapping slots reach thousands), so the current state
is the newest entry per chunk and per cache block over all valid slots.
Slot placement is not a plain ring: in `wc64` slots 0-61 hold sequences
1-62 and slots 62-97 sequences 127-162, so the log restarted at slot 62 and
the slots after 97 were cleared. Caches of mirror and parity spaces start
with a type 1 slot in slot 0 (sequence 1, one 8-byte entry
`08 00 00 00 01 00 00 00` in every pool seen: `mirrorthin`, `paritythin`,
`crashmirror`, `crashparity`), written when the cache is initialised; their
mapping slots start at sequence 2. It carries no mappings and is skipped.

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

## Dirty region tracking (SPACEDRT, LE)

Mirror spaces have a hidden role 6 child holding "SPACEDRT". Its space
starts with a header, and a second copy of it sits 8 KiB before the end:

| Offset | Size | Field |
|---|---|---|
| 0x00 | 8 | "SPACEDRT" |
| 0x08 | 8 | generation (the copy with the higher one is current) |
| 0x10 | 4 | number of entries |
| 0x14 | 4 | CRC-32 (zlib) of the first `0x18 + 8 * count` bytes with this field zeroed |
| 0x18 | 8 each | virtual slab where an extent run with writes in flight starts |

(**verified**: clean pools have no entries; `crashmirrorwc`, whose disks were
pulled during small writes, lists both of its extent runs, 0 and 4, in
generation 2 and run 0 in generation 1.) The copies of a mirror may differ
inside a listed run, and Windows resynchronises them when it mounts the
pool. Which copy it keeps is not recorded and not predictable: in
`crashmirrorwc` it kept copy 1 in 8 of the 9 differing MiB and copy 0 in
the other, in `crashmirror` copy 0 in both. `spaces` reads every copy in a
listed run and refuses rows whose copies differ (`--unclean-parity data`
returns the highest copy).

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

Entries start 8-byte aligned (**verified**: `ntfsparity`, whose slots hold
several entries with 34-byte run lists and bitmaps; the earlier pools had
one entry per slot).

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

* SDBB entries carry no checksum of their own; how Windows itself detects
  torn entries is unknown (whether it compares copies like `spaces`).
* Remaining record fields (provisioning type, sizes, disk attributes, tiers).
* How Windows chooses between differing mirror copies after an unclean
  shutdown; the meaning of the constant entry
  of the cache's type 1 slot; why the cache log restarts mid-area.
* Which side Windows trusts for an inconsistent parity stripe.
* Group sizes of grouped dual parity for 13-16 columns (not generated yet;
  the rule above predicts them).
* Tier movement by the tiering optimizer (not exercised yet), enclosure
  awareness, older pool versions
  (Windows 8/Server 2012 layout differs, see StorageSpaceReconstructor).
