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
    0x20 row); 0x98 1 (links); the attribute tree's node at 0xa8.
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
snapshots read back with Windows' SHA-256.)

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
* Allocator rows (roots 1, 2) are bitmaps of a cluster range with a count
  of free clusters; allocating a 16 KiB page sets four bits and lowers
  the count by four.
* Object table rows carry, before their page reference, a counter pair
  that the checkpoint (0x70) and MLog records carry as well; it grows with
  every transaction (the log's sequence number, presumably).
* Overwriting data (`Invoke-RefsSteps.ps1` step `write`): Windows
  writes a stream without integrity checksums where it is (the extents
  stay) and commits the new times with copy on write.
* Block reference counts (root 6): rows keyed by a range of virtual
  clusters (first, count) whose values count references per cluster
  (u16 each, 2 for a cluster two files share); empty on volumes without
  block clones or deduplication.
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
  `refs` writes and deletes streams kept in the record (**verified**:
  Windows' rows but for the order of rewritten rows).
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
