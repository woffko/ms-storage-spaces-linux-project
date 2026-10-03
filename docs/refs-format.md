# ReFS 3.x on-disk format (as read by `refs`)

What the `refs` crate reads, with the evidence for each part. **Verified**
marks statements backed by a test against volumes Windows created
(`crates/refs/tests/corpus.rs` on the corpus of `tools/vm/New-RefsVolume.ps1`:
ReFS 3.14 Dev Drives of Windows 11 Insider 26340, 4 KiB and 64 KiB
clusters, CRC64 and SHA-256 metadata checksums, integrity streams, stream
snapshots and deduplicated files, a volume inside a two-way mirror space
read through `storage-spaces`).
The most complete public description is
[forefst](https://github.com/xbqt/forefst) (GPL-3.0, documentation read,
no code taken); refsprogs (GPL-2.0+) and libfsrefs are the other prior art
(see `docs/research.md`).

All integers are little-endian. A *cluster* is 4 KiB or 64 KiB; a
*metadata page* is 16 KiB on 4 KiB clusters (four clusters, each named
separately) and one cluster on 64 KiB clusters.

## Bootstrap

```
boot sector -> superblock (cluster 0x1e) -> checkpoint -> container table
            -> object table -> directories and files
```

### Boot sector (sector 0 of the volume)

| Offset | Size | Field |
|---|---|---|
| 0x03 | 8 | `"ReFS\0\0\0\0"` |
| 0x10 | 4 | `"FSRS"` |
| 0x16 | 2 | checksum: over bytes 3..512 without these two, `c = ror16(c, 1) + byte` |
| 0x18 | 8 | sectors |
| 0x20 | 4 | bytes per sector |
| 0x24 | 4 | sectors per cluster |
| 0x28 | 1+1 | version major, minor (3.14) |
| 0x2a | 2 | checksum kind the volume was formatted with (2 CRC64, 4 SHA-256) |
| 0x2c | 4 | flags |
| 0x38 | 8 | serial number |
| 0x40 | 8 | bytes per container (64 MiB) |

(**verified**: every corpus volume; the checksum recomputed.)

### Metadata pages and page references

Every metadata page starts with a 0x50-byte header: signature (`SUPB`,
`CHKP`, `MSB+`), version 2, the volume signature (XOR of the four
32-bit words of the volume GUID), two clocks, the page's own clusters
(0x20..0x40) and, on B+-tree pages, the owning table at 0x48.

A *page reference* names a page and its checksum:

| Offset | Size | Field |
|---|---|---|
| 0x00 | 4 × 8 | the page's clusters (one used on 64 KiB clusters) |
| 0x22 | 1 | checksum kind: 1 CRC32-C, 2 CRC-64/NVME, 4 SHA-256 |
| 0x23 | 1 | offset of the checksum from 0x20 (8) |
| 0x24 | 4 | checksum length |
| 0x28 | | the checksum, over the whole page |

References are 48 bytes with CRC64, 72 with SHA-256 (104 on volumes
formatted before 3.10). CRC-64/NVME is the reflected polynomial
0x9A6C9329AC4BC9B5 with initial value and final XOR all ones, not
ECMA-182. (**verified**: every page `refs` reads is checked; a mismatch is
an error.)

### Superblock

At cluster 0x1e, copies at the last cluster but 2 and 3. 0x50: volume
GUID; 0x70/0x74: offset and count (2) of the checkpoint clusters; 0x78/
0x7c: offset and length of the page's own reference. That reference's
checksum covers the first cluster with the whole reference zeroed: CRC32-C
on 4 KiB clusters (**verified**), CRC64 on 64 KiB clusters and SHA-256 on
SHA-256 volumes (**verified**: those volumes open, which needs it).

### Checkpoint

Two copies; the one with the higher clock (0x60) whose own checksum holds
is current. Its own reference is at the offset in 0x58 (same rule as the
superblock's). 0x5c: page reference size; 0x78: flags; 0x90: number of
roots (13); with flag 0x200 the u32 at 0x94 is the offset of an array of
root offsets, otherwise the array is at 0x94; each offset names a page
reference within the checkpoint. Roots used: 0 object table, 7 and 8 the
container table and its copy (physical clusters). (**verified**)

## B+-trees

Every table is a B+-tree of `MSB+` pages. A node starts with a
descriptor whose first u32 is the offset from the descriptor to the node
header (the descriptor is at page + 0x50; nodes inside values start at
the value). The header: level at 0x0c (0 = leaf), flags at 0x0d, the
key index from 0x10 to 0x20 (offsets relative to the header), the row
count at 0x14 (`(end - start) / 4 == count`, checked). A key index entry
is a u16 row offset and a u16 marker. A row: u32 size, u16 key offset,
u16 key length, u16 reserved, u16 value offset, u16 value length (both
relative to the row). An index node's rows hold page references to its
children in their values. (**verified**: all tables, including a
directory of 5000 names spanning several pages.)

### Virtual clusters and the container table

Clusters named anywhere but in roots 7, 8 and 12 are virtual. The
container table (key: container id at 0; value: clusters per container at
0x18, the container's first physical cluster at `len − 16`) maps them:
`physical = start[vlcn >> bits(cpc)] + (vlcn & (cpc − 1))`, where
`bits(cpc)` is one more than log2(cpc) (15 on 4 KiB clusters, 11 on 64
KiB): containers are twice their size apart in virtual numbers, so a run
never crosses one. (**verified**)

### Object table

Key: 16 bytes, the object id in the second u64. Value: a counter pair
at 0x18 (the log sequence of the object's last change), the root of the
object's tree as a page reference at 0x20, and for directories the last
file id given out at 0x50 (Windows raises it with every file it creates
and never lowers it; new files take the next one, **verified**). Directories are objects; the
root directory is 0x600; user objects start at 0x701. (**verified**)

## Directories

A directory's tree holds, keyed by a u16 row type at key 0:

* 0x10: the directory's own attributes (its value is an attribute tree
  like a file record, see below; a junction's reparse point is there);
* 0x30: one row per name, the UTF-16LE name at key + 4; key flags at
  key + 2 decide the value:
  * **1, embedded record**: the value is the file's record: the
    attribute tree's descriptor (0x00: offset of the node header, 0xa8;
    0x20: number of attribute rows), times at 0x28 created, 0x30
    modified, 0x38 changed, 0x40 accessed; attributes at 0x48; 0x4c 8
    when the data is inline; 0x50 a security descriptor reference (files
    with the same descriptor share it; directories have their own); size
    at 0x58; allocated at 0x60; the file id at 0x80 (the key of its type
    0x20 row); 0x98 the link count (u32); 0x9c the last stream set id
    given out (u32, 0xf000 after the first named stream in clusters;
    rewriting a record must keep it); the attribute tree's node at 0xa8.
    (**verified**: files `refs create` writes this way are read, changed
    and deleted by Windows, with the inherited permissions of their
    neighbours);
  * **2, index entry** (84 bytes): ordinal at 0, home directory at 8,
    times at 0x10..0x30, allocated at 0x30, size at 0x38, attributes at
    0x40. With ReFS's directory bit 0x10000000 in the attributes, the
    home field is the subdirectory's object id; otherwise the record is
    the type 0x40 row of the home directory whose key holds the ordinal
    at 8 and the home directory at 0x10 (files that were moved or have
    several names: **verified** on hard links);
* 0x40: records of files whose name rows are index entries.

Windows shows the times of the index entry for directories once the
volume was written back (**verified** after reattaching; a listing taken
while the volume is still attached can show older directory times,
because ReFS updates them lazily).

### Leaf pages and rows (as written)

A node's header (at the descriptor's offset): 0x00 start of the row area,
0x04 end of the rows, 0x08 free bytes (holes left by removed rows
included), 0x0c level, 0x0d flags (2 on leaves), 0x10 start of the key
index, 0x14 rows, 0x20 end of the key index. Rows grow from the start of
the row area, unordered; the key index (u32 entries: the row's offset |
0xffff0000) sits at the end of the page, in key order, and grows down. A
table's descriptor (page + 0x50, or a record's start) counts the table's
rows at 0x20. A row: u32 size (8-aligned), u16 key offset (0x10), u16
key length, u16 flags (1 on name rows whose value embeds a record, 4 on
removed rows), u16 value offset (8-aligned), u16 value length.

Node flag 8 (key deltas): the high half of every key index entry holds
the first u64 of its row's key less a base the node header keeps at
0x18 (u64); Windows makes the base the node's first key less 1 and keeps
it while the node changes (the medium allocator's index root: base
0x87ffe, entries 0x0001 for (0x87fff, 1) and 0xc001 for (0x93fff, 1);
the small allocator: base 0x23, 0x0001 for 0x24 and 0x3fdd for 0x4000;
the block reference count table: base 0x300bff, entries 0x0001, 0x0401,
0x0801, 0x0c01; extent maps: base 0, the first cluster in the stream).
Keyless rows carry 0xffff. When a key does not fit (below the base, or
0xffff or more above it), the node goes without the flag, base 0 and
0xffff in every entry (allocator leaves, flags 4; extent maps past
cluster 0xffff). Windows takes a node with the flag whose entries do not
match for an invalid page (**verified**: 0xffff for a key that fits
made the medium allocator invalid, the delta made it valid); without
the flag the high halves are not looked at (the container allocator's
leaf, flags 6, keeps stale deltas on three of its rows). `refs check`
checks the deltas, and the writer keeps them in every node it changes.
(**verified**: rows `refs` inserts this way are found by Windows.)

Removing a row leaves it in place with flag 4 and drops its key index
entry; the free bytes (0x08) are the row area (start to key index) less
the live rows. Windows walks a page's row area row by row by their sizes
and refuses a page where that breaks: a zeroed hole made it report "an
invalid metadata page" and refuse to mount the volume (**verified**; the
tests check every page this way, Windows' pages included).

Tables of more than one page: index nodes (level 1 and up; node flags:
1 index, 2 root) hold a row per child whose key is the child's last key
and whose value is the child's page reference. The last row of every
index node has row flag 2 and takes everything above: in the last node
of a level it has no key, in the others it keeps its child's last key
(Windows reports a node whose last row lacks the flag as an invalid
metadata page and drops the directory's contents; **verified**). Pages
below the root have no table descriptor (the u32 8 at 0x50, the node
header at 0x58); the root's descriptor counts the table's rows (0x20)
and the pages below the root (0x18) (**verified**: directories `refs`
grew this way, to three levels with index pages split, read on
Windows).
`refs` shrinks tables too: a page below the root left with less than a
quarter of its room filled merges with a sibling when both fit in three
quarters of a page (the later page takes the rows, the earlier one
leaves its parent), a page whose last row goes leaves its parent (when it
was the parent's last child, the row before loses its key and becomes
the last), and a root left with one child takes that child's rows when
they fit (the table loses a level). Windows' own policy is not known.

A directory's rows sort by type, then: file id rows (0x20) by id, name
rows (0x30) by name compared without case (NTFS-like: upcased UTF-16
units; the key flags at 2 do not count). (**verified** on the names of
the corpus; new names are ASCII for now.)

## File records

A file record (and a directory's own row) is a node whose rows are the
file's attributes. A row's key: 0x08 instance marker (0x80000001 single,
0x80000002 multiple), 0x0c descriptor (low half the attribute type);
for multi-instance $DATA a sub-stream id at 0x10 (0x1000 the live
stream). Attributes `refs` reads (**verified**: every file of the corpus
reads back with Windows' SHA-256):

* **$DATA inline** (0x80000001, 0x80; the key starts with the value's
  length as u64): 0x04 0x30 plus the allocated size, 0x08 0x0c, 0x0c
  0x30, allocated size at 0x18 and 0x30 (the size rounded up to 8 when
  created), stream size at 0x20, valid length at 0x28, 2 at 0x38, the
  bytes from 0x3c;
* **$DATA in extents** (0x80000002, 0x000e0080): one row per *level* (see
  below), the live stream's level id 0x1000 at key 0x10, row flag 1 (the
  value embeds a node). The value: 0x00 offset of the node (0x88), 0x14
  2, 0x16 the integrity checksum kind, 0x20 number of extent records,
  0x2c 0x28, allocated size at 0x30 and 0x48, stream size at 0x38, valid
  length at 0x40, 0x50 1; the node (flags 0x0e) holds raw extent records
  (no row header):

  | Offset | Size | Field |
  |---|---|---|
  | 0x00 | 8 | first virtual cluster |
  | 0x08 | 2 | flags: 0x10 written, 0x20 sparse hole, 0x80 checksums follow (Windows writes 0x50) |
  | 0x0a | 2 | record size (24, more with checksums) |
  | 0x0c | 4 | first cluster in the file |
  | 0x14 | 4 | clusters |

  The node's key index entries carry the record's first cluster in the
  file in their upper half (directory rows carry 0xffff there), and the
  node ends 8-aligned (4 free bytes before an odd number of entries; the
  same holds for every embedded node). Windows splits runs at the file's
  clusters 1, 64, 256 and multiples of 256 even where the clusters are
  contiguous, and its write path depends on it: appending to a file
  whose single run crossed cluster 64 made ReFS fail to find the run
  (bug check 0x149 in `TmsTableSet<CmsStreamSetCallbacks>::PinRow`, a row
  not found), while the same data in runs split at 64 worked
  (**verified**, `refs create` splits the same way).
  Runs without the written bit or with the hole bit read as zeros
  (**verified**: a sparse file of 1 GiB with three written MiB); index
  nodes point at pages of the same. **Integrity streams**: with flag 0x80
  the record is followed by checksums of the run's data, of the kind in
  the u16 at value 0x16 of the $DATA row (1 CRC32-C, 2 CRC-64/NVME, 0
  without integrity; the codes of page references), each over an equal
  part of a cluster: one CRC32-C per 4 KiB cluster, four CRC-64 per 64 KiB
  cluster (one per 16 KiB), as many as the record size leaves room for.
  `refs` checks every part it reads (**verified**: every file of both
  integrity-stream volumes; damaged data is refused). Block-cloned copies (`Copy-Item` on a Dev Drive)
  name the same virtual clusters as the original (**verified**);
* **named streams** (0x80000002, 0x000500b0, the UTF-16LE name from key
  0x10): ADS when value 0x10 is 0 (2 = a snapshot); size at 0x20; inline
  content from 0x3c, or, with bit 0x1000 in the u16 at value 2, in
  extents: the $DATA record of the stream-set row (key u32 at 8 = 3)
  whose key holds the set id (value 0x3c) at 0x30 and the sub-stream id
  (value 0x44) at 0x38 (**verified**: a 200000-byte stream; forefst
  matched these by size);
* **reparse point** (0x80000001, 0xc0): tag at value 0x0c, data length at
  0x10, the REPARSE_DATA_BUFFER's data from 0x14. Symbolic links (tag
  0xa000000c): substitute name offset/length, print name offset/length,
  flags (1 = relative), names from data + 12; junctions (0xa0000003) the
  same without the flags, names from data + 8 (**verified**: Windows'
  targets of an absolute and a relative file link, a directory link and
  a junction).

### Data levels and stream snapshots

A stream in extents is a chain of *levels*. Each level is a $DATA row
(key: level id at 0x10, parent level id at 0x18) or, for named streams, a
stream set row (key u32 at 8 = 3: set id at 0x30, level id at 0x38,
parent at 0x40). The set's header row has id 8 (value: the next free
level id at 0, the number of levels at 8); live streams have level id
0x1000, and on a file without snapshots its parent is the header.

`refsutil streamsnapshot /c NAME FILE` freezes the live level under a
new id (0x1001, 0x1002, ...) and starts an empty live level on top of
it: from then on each level maps only the clusters written while it was
live (value 0x48: its bytes; `refsutil streamsnapshot /l` shows them as
"snapshot size"). A level reads as the extents of its chain from the
header up, each level's runs replacing what the older ones map there.
Snapshots are named stream rows with value 0x10 = 2 (an alternate stream
has 0): size at 0x20, the level id at 0x44 and the set at 0x3c (0: the
file's own $DATA levels), bit 0x1000 of the u16 at 2 set. Windows lists
them as streams `NAME:$SNAPSHOT`. (**verified**: a file with two
snapshots between overwrites and an append; the live data and both
snapshots read back with Windows' SHA-256.) A level maps only what was
written while it was live, so a file's clusters are what its levels map;
deleting a file frees them all (**verified**: Windows found no leak
after `refs` deleted the file with two snapshots). The live level's value
lists only its own records; 0x30 is the size in whole clusters, 0x48
the bytes the level maps (4096 for one cluster written since the last
snapshot), 0x50 1. `refs` changes such a file in its live level: bytes
in clusters the live level maps are written where they are, clusters
only older levels map are copied on write into it (their old bytes with
the new ones), clusters it grows by go to it, and its clusters past a
new end are freed; the snapshots keep theirs (**verified**: Windows read
the file `refs` patched and appended to, listed both snapshots with
their sizes, found no leak, wrote into it, and `refs` read the result
and both snapshots with their hashes).

### Compression

ReFS compresses containers, not files (2026-10-04, Windows 11 26340:
`Enable-ReFSDedup -Type DedupAndCompress`, then `Start-ReFSDedupJob`
over 40 text files whose times were set three days back; `-Type
Compress` and `refsutil compression /c` alone compressed nothing). The
job *compacts* a data container: the clusters in use, in order, become
one stream, cut into units of 64 KiB, each compressed (LZ4: a block,
no frame; ZSTD: a frame with its content size, level 3 here) or kept
as it is when that does not make it shorter, and the
units are stored one after the other in clusters of another data
container. Files keep their extents; a cluster's data is in the unit
that holds its place in the stream.

* The compacted container's row in the container table: class 0xa (u32
  at 0x14), the kept clusters at 0x24, the format at 0x30 (1 LZ4, 2
  ZSTD; 3 LZ4 on QuickAssist hardware by forefst's notes), the unit size
  at 0x34 (0x10000), and in place of the physical start and clusters
  the *virtual* cluster of the compressed bytes (0x130000: container
  0x26) and their clusters (0x1628).
* Root 10 (empty before) has rows keyed (container id u64, sequence u32,
  type u32), the key the first 16 bytes of the value. Type 3: from 0x30
  a bitmap of the container's 0x4000 clusters, those kept; a cluster's
  place in the stream is the number of kept clusters before it (15 224
  of 16 384 here). Type 7, one per range of the stream (two here, 476
  units each): 0x10 the range's first byte in the stream and 0x18 its
  bytes, 0x20 the first of its compressed bytes (from the start of the
  container's compressed bytes) and 0x28 their bytes, 0x30 flags (2:
  checksums; 1 on the last range), 0x34 the units, 0x38 the offset
  (0x40) of a u32 per unit where its compressed bytes end, 0x3c the
  checksum kind (1: a CRC32-C of each unit's compressed bytes, a u32 per
  unit after the ends). Type 5: a summary not needed to read (0x3ff,
  0x400, ...).
* Windows reported 376 leaked clusters on the compacted volume by itself
  (`refsutil leak`), the same after `refs` wrote a file on it.
* The dedup engine also deduplicated the files (their text was the same
  but for the last bytes) and marks the block reference counts: the low
  14 bits of a count are the references beyond the first (what the
  row's total sums), 0x8000 marks clusters it deduplicated (0x8027: 40
  files), 0x4000 others it went through. Changing compressed files
  (steps `delete`, `write`, `append` on that volume): deleting one lowered
  the counts of its shared clusters (0x8027 to 0x8026; the compacted
  container's rows in root 10 stayed as they were), and writing into one
  or appending to one copied the touched clusters on write into an
  ordinary data container (the overwritten cluster's run split around
  it, the last partly used cluster moved), the old ones losing a
  reference.

ZSTD (`refsutil compression E: /c /f ZSTD` on a volume with
`Enable-ReFSDedup -Type DedupAndCompress`: 30.3 MB of text into 4.92 MB)
lays out the same, format 2 in the row, a ZSTD frame per unit.

`refs` reads compacted containers (Volume::read_virtual decompresses the
units a read needs, checking their CRC32-C, and keeps the last few; LZ4
with its own decoder, ZSTD with the `ruzstd` crate after checking the
frame's window and content size), and
`refs check` looks for their compressed clusters in the medium
allocator instead of the files' runs (**verified**: all 40 LZ4 and all
20 ZSTD files read with Windows' SHA-256, a damaged unit is refused). Changing a file with
compressed data is refused; new data never goes into a compacted
container (its class is neither 0 nor 1).

### Deduplication and block cloning

`refsutil dedup` and `Copy-Item` (block cloning) leave plain extent maps
whose runs name the clusters of other files, or the same cluster many
times: a 5 MiB file of zeros became 1280 one-cluster runs of one cluster.
Nothing else is needed to read them (**verified**: identical text and
random files deduplicated, and the zero file).

## Writing (Track B4)

`refs::write` commits the way described below (`refs set`, `overwrite`,
`write`, `create`, `delete`, `rename`, `move`, `link`, `mkdir`; tests
`tests/write.rs`). What Windows writes, from the write
experiments (`tools/vm/Invoke-RefsSteps.ps1` changes a volume one step at
a time and keeps an image after each; `tools/refs-diff.py` lists the
changed clusters with what `refs map` says they are; `refs tree` prints a
table's rows for a text diff):

* The checkpoint roots, in the order of the literature (Prade, Gross and
  Dewald, "Forensic analysis of the resilient file system (ReFS) version
  3.4", DFRWS 2020) and consistent with what the experiments show: 0
  object table, 1 medium allocator, 2 container allocator, 3 schema
  table, 4 parent-child table, 5 object table copy, 6 block reference
  counts, 7 and 8 container table and copy, 9 schema copy, 10 container
  index, 11 integrity state, 12 small allocator (roots 7, 8 and 12 name
  physical clusters; root 6 is empty on a volume without clones; roots 3
  and 9 hold 29 rows each on a new volume).
* Every transaction, also attaching a volume and detaching it unchanged,
  writes copies of the changed B+-tree pages to free clusters (copy on
  write, up to the object table and its copy), MLog pages (a control area
  near cluster 0x30 and records further on, both with the signature
  `MLog`) and both checkpoints, each with a higher clock (0x60). Changing
  a file's time rewrites the page of its directory, the object table and
  its copy, both allocators and the internal objects 0x500, 0x501, 0x701
  and 0x705.
* Containers have classes (u32 at 0x14 of their row in the container
  table): 1 metadata (the B+-tree pages), 0 data, 0x4000 data and full,
  0x2000 not handed out yet, 0x4001, 0x4200, 0x2040 and 0x2440 for the
  log and reserved areas (0x4200: the allocators' own pages). Windows
  hands out the next data container when its data containers fill (150
  MB on a small volume): the class becomes 0 in both container tables
  (roots 7 and 8, whose pages it copies on write through the small
  allocator, root 12), the container's uniform row in the medium
  allocator a bitmap row, and a container it filled becomes class 0x4000
  with a uniform row (kind 2, no free clusters, 0 at 0x16) in place of
  its bitmap, which keeps the allocator's tree small. Data written into
  a container still at 0x2000 makes Windows report the volume as needing
  repair. `refs` does the same: data into data containers, then a
  container it hands out, then metadata containers (leaving 1024
  clusters of each for pages); pages into metadata containers
  (**verified**: Windows read 150 MB written so on a small volume and
  100 MB on an empty one, wrote 100 MB more, refsutil leak and triage as
  for the untouched volumes).
* The allocator tables (roots 1, 2) index their pages by the last
  cluster a page covers: index row keys (last cluster, 1), not the last
  row's key, in an index root with flags 0xf (key deltas) over leaves
  with flags 4. Windows balances rows between the two leaves of the
  medium allocator before it adds a page (after freeing in a 52 GB
  volume: (0x87fff, 1) became (0x93fff, 1) and three rows moved left).
  `refs` splits a full leaf in two with such a key and its delta
  (**verified**: Windows read a medium allocator of three leaves made so
  when freeing 60 MB unpacked uniform rows, wrote 150 MB and deleted
  30 MB into it, leak and triage as for the untouched volumes, and
  merged the leaves back into two; with 0xffff in the new entry it took
  the allocator for an invalid page). The container tables and the small
  allocator (at physical clusters) are not split.
* The small allocator (root 12) has a row of 12 clusters (bitmap padded
  with set bits to whole bytes) beside one of a container.
* Allocator rows (roots 1, 2) are bitmaps of a cluster range with a count
  of free clusters; allocating a 16 KiB page sets four bits and lowers
  the count by four. Value: start, count, free (u16 at 0x10), kind (u16
  at 0x12: 1 a bitmap of 0x4000 clusters from 0x18, 0x218 at 0x14; 2 a
  uniform range of any multiple of 0x4000, 24 bytes, 0x200 at 0x14, free
  0 when all used and 0xffff when all free; 5 and 9 bitmaps Windows keeps
  aside). To free a cluster in a used uniform row `refs` splits it: the
  block of 0x4000 clusters holding it becomes a bitmap row with every bit
  set, the rest stays uniform (**verified**: Windows reported no leak and
  wrote on).
* Object table rows carry, before their page reference, a counter pair
  that the checkpoint (0x70) and MLog records carry as well; it grows with
  every transaction (the log's sequence number, presumably).
* Overwriting data (`Invoke-RefsSteps.ps1` step `write`): Windows
  writes a stream without integrity checksums where it is (the extents
  stay) and commits the new times with copy on write. Appending adds
  extent records after the file's own (its record count and sizes
  grow); `refs` appends and truncates the same way (Volume::update_file).
* Block reference counts (root 6): rows keyed by a range of virtual
  clusters (first, count), the key being the first 16 bytes of the value
  (row size 0x830 for a value of 0x820); empty on volumes without block
  clones or deduplication. The value: the key, a u32 at 0x10 that
  Windows changes with every transaction touching the row (its values
  end in 01: 0x4b205801, 0x465d7001), the row's kind (u32 at 0x14), the
  sum of the counts (u32 at 0x18). Kind 1: 0x400 clusters, from 0x1c a
  u16 per cluster counting its references beyond the first (three files
  sharing a cluster: 2; 0 for a cluster one file has alone). Kind 0: a
  value of 0x20 bytes, one count (u16 at 0x1c, again at 0x1e, the total
  0) for every cluster of a range of any number of blocks: Windows packs
  rows whose clusters have one count so and merges neighbours (after it
  deleted one of three copies: rows of 0x800 and 0x1000 clusters, count
  2). A row with 0 at 0x14 and an array of counts is read as kind 0 with
  the first count: Windows freed the clusters of a clone whose rows
  `refs` had written with 0 there as unshared (**verified**). Leaf
  nodes have key deltas (flags 0xe for a root, 0xc below an index of
  flags 7, whose rows are keyed (last cluster a page covers, 1) as in the
  allocators). Copying a file (Windows 11 clones its blocks with
  `Copy-Item` on a Dev Drive) adds a row for each 0x400-aligned range of
  its whole clusters with count 1 (its last, partly used cluster is
  copied); deleting one of the files sharing a cluster lowers its count
  and the total (the row stays at 0, or goes), and only a cluster of
  count 0 is freed. `refs` deletes and rewrites such files the same way,
  turning a row of kind 0 into rows of counts where counts change
  (**verified**: after refs deleted one or all three files sharing
  clusters, Windows reported no leak, read the others, cloned and deleted
  the rest), refuses to overwrite shared clusters in place (that would
  change the other files), and clones files (Volume::clone_file, `refs
  clone`): the copy gets the source's extent records (with their
  checksums), each cluster a reference more, new rows of kind 1 taking
  the other rows' u32 at 0x10 (else 1) (**verified**: Windows read clones
  of a 30 MB file and of a 64 MB integrity stream, wrote into one,
  deleted the sources, appended to the other clone, and `refs check` and
  `refsutil leak` found nothing; and after Windows packed rows to kind
  0, `refs` deleted a copy and cloned again through them, and Windows
  deleted the rest).
* Moving a file to another directory, or giving it a second name (steps
  `move`, `link`): its record leaves its name row and becomes a row of
  type 0x40 of the directory it was made in (its home; key 0x40, 0x8000,
  the file id at 8, the home at 0x10; row flag 1), with a link row per
  name (descriptor 0x000d0039: the directory at key 0x10, the name from
  0x20; the value is the key without its length, overlapping it) before
  its other rows, and its link count at record 0x98; the file id row
  becomes 2, the id, the home; each name is an index entry (key flags 2:
  the id, the home, the record's times, sizes and attributes). `refs`
  writes these rows as Windows does (**verified**: equal rows; on
  Windows both names have one file id, writes through one show through
  the other).
* Renaming or moving one name of such a file (steps `rename`, `move`)
  replaces its index entry (in the new directory for a move) and its
  link row; the record stays in its home, also when the last name moves
  back there, and its change time is updated. Deleting one name removes
  its index entry and its link row and lowers the link count; with the
  last name the record row and the file id row leave the home. Windows
  removes a link row by moving the record's later rows down and puts a
  new one at the end of the row area (the key index stays sorted); `refs`
  writes the rows in key order (**verified**: otherwise equal rows).
  Changing times, attributes or data through one name (steps `touch`,
  `attrib`, `write`, `append`) changes the record in the home and copies
  its times, sizes and attributes into the index entry of that name only;
  the other names' entries keep the old values (**verified**: `refs set`
  and `refs overwrite` give Windows' rows).
* Renaming a file within its directory (step `rename`): its name row is
  replaced by one with the new name and the same record (change time
  updated), its file id row gets the new name, and the directory gets new
  times in its own row and in its entry in the parent. Deleting (step
  `delete`): both rows are removed, the directory gets new times.
* Renaming or moving a directory (steps `rename`, `move`): its entry
  (key flags 2, value unchanged) leaves the old parent and goes to the
  new one under the new name, the link row of its own record (row 0x10)
  names the new parent and name, its parent-child row (root 4) names the
  new parent, and both parents get new times (their own rows, and their
  entries in their parents). Deleting an empty directory (step `delete`)
  removes its entry, its rows in both object tables and its parent-child
  row and frees its pages; the parent gets new times. `refs` writes
  these rows as Windows does (**verified**: equal rows but for the order
  of the record's rows).
* Named streams (steps `stream`, `unstream`): a small stream (200
  bytes) is a row of the file's record, key: the value's length,
  0x80000002, descriptor 0x000500b0, the name; value laid out like inline
  $DATA (0x04 = 0x30 + size, 0x08 = 0x0c, 0x0c = 0x30, the size at 0x18,
  0x20, 0x28 and 0x30, exact rather than rounded, 2 at 0x38, the bytes
  from 0x3c). The record's rows sort by marker (0x80000001, 0x80000002,
  3: the low bits), attribute type, then name. Adding one bumps the
  record's row count and its modification, change and access times;
  deleting one lowers the count and sets the change time. A larger stream
  (5000 bytes) is kept in clusters: value flag 0x1000 at 2, the stream
  set id (0xf000) at 0x3c and its level at 0x44, and rows of type 3 in
  the record (the set's header row and its live level with the extents).
  A stream set's rows: key the value's length, 3, 0, 32 constant bytes
  (00 00 0c 00 02 00 20 00 00 00 01 00 08 00 28 00 00 00 0e 00 18 00 30
  00 00 02 and zeros), the set id at 0x30, the level at 0x38 (8 for the
  header, 0x1000 live), the parent at 0x40 (8) and 1 at 0x48 for the
  header; values as for the levels of $DATA (the header: next level id
  0x1001, one level; the live level: the extent map, row flag 1). A new
  set takes 0xf000, or the record's counter at 0x9c plus one, and the
  counter follows; rewriting a stream keeps its set id. Deleting one in
  clusters removes its row and its set's rows and frees the set's
  clusters. `refs` writes and deletes streams both ways (**verified**:
  Windows' rows but for the order of rewritten rows and the split of
  runs, and Windows read and rewrote them).
* Integrity streams (steps `integrity`, `append`, `write`):
  Set-FileIntegrity on an empty file sets attribute 0x8000 and checksum
  kind 1 at 0x3a of its inline $DATA value. Data then goes to clusters
  with a CRC32-C of every whole cluster (zeros past the end) after each
  extent record (flags 0xd0, the record's length at 0x0a: 24 + 4 per
  cluster; records padded to 8 bytes in the node) and kind 1 at 0x16 of
  the $DATA value. Overwriting copies on write: the changed cluster goes
  to a new place with its own record, the old one is freed. `refs`
  writes the same records and checksums (**verified**: Windows read the
  data, checking it, and went on writing), and changes integrity streams
  as Windows does (overwriting, appending, the mount's write-back): the
  clusters a change touches go to new clusters with new checksums, their
  record split around them, the old ones freed; clusters it grows by
  follow (**verified**: Windows read a 64 MB stream patched in the
  middle and appended to so, patched it itself, and `refs` read that). On volumes of 64 KiB clusters the
  checksums are CRC-64 per 16 KiB (kind 2, four per cluster), and
  `refs` writes them so (**verified**: Windows read 6 MB written so). A larger
  map (6 MB of integrity stream: three records of up to 768 clusters'
  checksums) goes to a page of its own: the level value's node becomes
  an index node (level 1) with one row, no key, row flag 2, whose value
  is the page's reference (0x18 of the value: 1). The page: the header of
  its table's pages (0x48: the directory holding the record), the u32 8
  at 0x50, a leaf node (flags 0x0c) of the records, keyed as in the
  value. Node flag 8 (0x0e in values, 0x0c in pages; key deltas, base 0)
  means the key index entries carry each record's first cluster in the
  stream in their high half; a map with a record past cluster 0xffff
  leaves it out (0x06,
  0x04) and has 0xffff in every entry (Windows' sparse file of 1 GiB;
  Windows takes a file whose entries are cut to 16 bits for damaged and
  drops it, **verified**). Deleting or rewriting the data frees those pages too
  (**verified**: no leak after deleting Windows' 6 MB file). `refs` puts
  maps of more than 2 KiB in such a page (**verified**: Windows read 6
  and 8 MB integrity streams written so and wrote into them).
* Maps of several pages (Windows' 64 MB integrity stream: 27 records in
  7 pages, 3 to 5 records each): the value's index node (level 1, flags
  0xf) has a row per page keyed (last cluster in the stream the page
  maps, 1), the last one keyless with row flag 2, and key deltas against
  the first key less 1 (base 0x6fe: 0x0001, 0x0701, 0x0e01, ...); 0x18
  of the value counts the pages, 0x20 the records. Each page is a leaf
  (flags 0x0c) with key deltas against its first cluster in the stream
  less 1 (the first page: base 0), records in order. `refs` fills pages
  with whole records in order (about 580 records, 580 MiB of data, on 4
  KiB clusters; 15 MiB of integrity stream), a page whose records span
  0xffff clusters or more without deltas (flags 0x04), the index without
  them (flags 7) when its keys span that much; the index must fit the
  2 KiB a record keeps for it (about 22 pages). **Verified**: Windows read
  a 1.2 GB file (3 pages, the index without deltas) and a 64 MB
  integrity stream (5 pages) written so, appended to both (records added
  to the last page, with deltas against base 0x472ff, the rest kept), and
  `refs` read its result.
* Maps of more pages than the record's index holds (Windows' sparse file
  of 25 000 blocks of 4 KiB with holes between: 25 000 records): the
  value's node is at level 2 (flags 7) with one row, no key, row flag 2,
  naming an index page (level 1, flags 0x0d: key deltas against its
  first key less 1) whose rows name the leaves, keyed (last cluster the
  leaf maps, 1), the last one keyless; 0x18 of the value counts all the
  pages (101: the index page and 100 leaves of 248 records, 450 in the
  last). A leaf's key deltas are against the last cluster the leaf
  before maps (its key in the index), 0 for the first. `refs` writes
  maps that do not fit the record so (one index page: about 190 leaves).
  A sparse file's value has 0x30 the size in whole clusters, 0x48 the
  clusters allocated, bit 31 of 0x50 set, and its record attribute 0x200
  and the allocated size; a clone of one is sparse too (**verified**:
  Windows read a clone `refs` made of that file, with a map of an index
  page and 44 leaves, wrote into it and deleted the source; without the
  sparse fields it took the clone for corrupt and removed it).
* Creating a file (step `create`) adds two rows to its directory: the
  name row (type 0x30) with the embedded record, and a row of type 0x20
  (key: 0x20, flags 0x8000, the file id as u64 at 8; value: the name's
  offset 0x0c and length at 8 and 0x0a, the UTF-16 name) that maps the
  file id to its name; the directory's own row gets new times.
* **First write Windows accepted** (2026-10-03): on a cleanly detached
  volume, a file's modification time changed in place in its directory
  page, with the page's checksum stored again in the object table rows
  of roots 0 and 5, those pages' checksums in the checkpoint's root
  references and the checkpoint's own checksum recomputed (no copy on
  write, no allocation, no log record). Windows attached it as healthy
  without replaying the log, showed the new time, read every file, and
  carried the change on through its own later transactions. In-place
  changes are not crash safe; a real writer copies on write and switches
  checkpoints.
* **A copy-on-write commit Windows accepts** (2026-10-03,
  `tools/research/refs-touch-cow.py`): the same change committed the way
  Windows commits. Where pages go: the medium allocator (root 1) counts
  physical clusters below the container band and serves the object
  tables, directories and the other tables; the container allocator
  (root 2) serves the allocator tables themselves (roots 1 and 2, also 6
  and 11), its own page included. Each changed page gets four free
  clusters (one bit per cluster; the u16 at 0x10 counts the free ones),
  its old clusters are freed in the same commit, its header names its new
  virtual clusters (0x20) and the new clock (0x10); references are
  updated child first with their checksums, through the object table
  rows of roots 0 and 5. The checkpoint is a single cluster: the new one
  goes to the older slot with the next clock (0x10 and 0x60), the counter
  at 0x68 incremented, the changed roots' references, its own cluster in
  its self reference, and its own checksum; writing it is the commit
  point. No log record and the same log sequence number (0x70). Windows
  attached it as healthy, `refsutil leak` found exactly the leaks it finds
  on the untouched volume, `refsutil triage /g` passed, and Windows
  committed its own transactions on top (`tools/vm/Test-RefsVolume.ps1`).
* **Pages awaiting free.** Windows keeps some pages its last commit
  replaced allocated, because the older checkpoint still references
  them, and frees them with its next checkpoint, which takes the older
  one's slot: after Windows wrote and detached a volume, the old pages
  of the object tables (roots 0 and 5; 0x3400 and 0x3604, one page
  each) were allocated in the medium allocator, referenced only by the
  older checkpoint (the pages it replaced in the allocator tables were
  free already). A commit by `refs` takes that slot too, so it frees
  them first (Volume::deferred_pages: the pages of the tables whose root
  differs between the two checkpoints that only the older one references,
  still allocated, still matching the older checkpoint's checksums);
  without that they leak: `refsutil leak` counted 8 more leaked clusters
  after `refs` wrote such a volume, none once it freed them. The
  4 clusters `refsutil leak` reports on every untouched volume of the
  corpus are of that kind but referenced by neither checkpoint. `refs
  check` counts the pages awaiting free.

## The log (MLog)

Windows logs every transaction before it reaches a checkpoint, and when it
attaches a volume it replays the records from the checkpoint's log
sequence number (0x70: low u32, then high u32) to the end of the log
("Log Restart Start/Last LSN" in the ReFS operational event log).
Detaching an image without dismounting the volume first leaves records
past the checkpoint (every volume `New-RefsVolume.ps1` made before it
took the disk offline first had some); taking the disk offline makes
ReFS write a checkpoint that covers the log (**verified**).

* The control page ("MLog", the volume signature at 4, zero at 0x28)
  sits among the volume's first clusters on plain volumes (cluster 0x30
  on 4 KiB, 0x27 on 64 KiB clusters) and right after the log region on a
  volume inside a space (4 KiB page 0x44000); nothing found so far points
  at it. It gives the log's epoch (0x20) and its region, from 0xb8 to
  0xc0 in 4 KiB pages (0x24000..0x44000 on every corpus volume).
* Record pages (4 KiB): "MLog", the volume signature, the epoch at 0x20,
  the record's LSN at 0x28 and the previous one at 0x30.

The checkpoint misses changes when a record of the current epoch is not
older than its LSN (**verified** against Windows' restart ranges). `refs
info` reports it; `refs` reads only the checkpoint, and refuses to write
such a volume: Windows would replay the logged changes over its own (a
directory created on such a volume collided with objects the replay
created, which lost the directory's contents).

## Not read yet

Compression (LZ4/ZSTD: `refsutil compression` and the ReFS dedup jobs of
Windows 11 26340 deduplicate but do not compress, so there are no samples
yet; Microsoft documents compression for Windows Server 2025), extended attributes, EFS, the USN
journal, snapshots of named streams (read by the same rules, no sample),
volumes before ReFS 3.10 (104-byte references are parsed but untested)
and ReFS 1.x/2.x.
