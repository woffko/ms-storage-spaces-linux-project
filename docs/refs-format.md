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

Key: 16 bytes, the object id in the second u64. Value: the root of the
object's tree as a page reference at 0x20. Directories are objects; the
root directory is 0x600; user objects start at 0x701. (**verified**)

## Directories

A directory's tree holds, keyed by a u16 row type at key 0:

* 0x10: the directory's own attributes (its value is an attribute tree
  like a file record, see below; a junction's reparse point is there);
* 0x30: one row per name, the UTF-16LE name at key + 4; key flags at
  key + 2 decide the value:
  * **1, embedded record**: the value is the file's record (times at
    0x28 created, 0x30 modified, 0x38 changed, 0x40 accessed; attributes
    at 0x48; size at 0x58; allocated at 0x60; the attribute tree's node
    header at the offset in its first u32);
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

## File records

A file record (and a directory's own row) is a node whose rows are the
file's attributes. A row's key: 0x08 instance marker (0x80000001 single,
0x80000002 multiple), 0x0c descriptor (low half the attribute type);
for multi-instance $DATA a sub-stream id at 0x10 (0x1000 the live
stream). Attributes `refs` reads (**verified**: every file of the corpus
reads back with Windows' SHA-256):

* **$DATA inline** (0x80000001, 0x80): stream size at value 0x20, the
  bytes from value 0x3c;
* **$DATA in extents** (0x80000002, 0x000e0080): one row per *level* (see
  below), the live stream's level id 0x1000 at key 0x10; stream size at
  value 0x38; the value is a node whose leaf entries are raw extent
  records (no row header):

  | Offset | Size | Field |
  |---|---|---|
  | 0x00 | 8 | first virtual cluster |
  | 0x08 | 2 | flags: 0x10 written, 0x20 sparse hole, 0x80 checksums follow |
  | 0x0a | 2 | record size (24, more with checksums) |
  | 0x0c | 4 | first cluster in the file |
  | 0x14 | 4 | clusters |

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
snapshots read back with Windows' SHA-256.)

### Deduplication and block cloning

`refsutil dedup` and `Copy-Item` (block cloning) leave plain extent maps
whose runs name the clusters of other files, or the same cluster many
times: a 5 MiB file of zeros became 1280 one-cluster runs of one cluster.
Nothing else is needed to read them (**verified**: identical text and
random files deduplicated, and the zero file).

## Writing (Track B4)

`refs::write` commits the way described below (`refs set` changes times
and attributes of files whose record is in their directory entry; tests
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
* Allocator rows (roots 1, 2) are bitmaps of a cluster range with a count
  of free clusters; allocating a 16 KiB page sets four bits and lowers
  the count by four.
* Object table rows carry, before their page reference, a counter pair
  that the checkpoint (0x70) and MLog records carry as well; it grows with
  every transaction (the log's sequence number, presumably).
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

## Not read yet

Compression (LZ4/ZSTD: `refsutil compression` and the ReFS dedup jobs of
Windows 11 26340 deduplicate but do not compress, so there are no samples
yet; Microsoft documents compression for Windows Server 2025), extended attributes, EFS, the USN
journal, snapshots of named streams (read by the same rules, no sample),
volumes before ReFS 3.10 (104-byte references are parsed but untested)
and ReFS 1.x/2.x.
