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
| 0x10 | 8 | how often the header was written: 1 on the disks of `New-StoragePool`; 2 on disks added later (written without, then with the database copy: `c9disk`), on a retired disk after the repair (copy cleared) and on a removed disk |
| 0x18 | 8 | FILETIME when the disk joined the pool |
| 0x20 | 16 | pool GUID |
| 0x30 | 16 | physical disk GUID (as in the `PD:{...}` part of the disk ObjectId) |
| 0x41 | 1 | 1 if the disk carries a copy of the pool database, else 0 |

The rest of the 4 KiB page is zero (one disk of the Insider build carries
a GUID at 0x52). Pools of up to five disks keep a copy of the pool
database on every disk, larger pools on five of them (not the first five;
which ones does not follow from the metadata), and byte 0x41 and the disk
record say which (**verified** by the test
`disk_headers_are_reproduced_and_mark_database_copies`, which also
re-encodes every header of the fixtures byte for byte). A retired disk
keeps its copy, no longer updated, and both flags until a repair has
moved its data off; then Windows writes the current database to it once
more with both flags cleared (`c9disk` d3, d4).

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

How Windows updates the database (**verified** byte for byte on every
member by the test `pool_database_updates_follow_the_model`, which replays
the scenario `m5db` of `tools/scenarios.sh` on `database::Database`:
rename, new space, extension, deletion):

* A record's id is the number of its first slot. The pool database starts
  with 64 formatted slots (4 KiB); a free slot keeps "SDBB" and its own
  number, the rest is zero. When no run of free slots is long enough for a
  record, Windows formats another page of 64 slots after the last one
  (**verified** by the test `the_pool_database_grows_by_a_page_of_slots`
  on the scenario `m7grow`: 47 allocations took the database from 28 to
  75 slots in use and 128 formatted, byte for byte as the model predicts;
  `lrc15` and `mapar_26100` also have 128).
* An update writes the new version of every changed record, and every new
  record, in order into the first free slots, whether they are adjacent or
  not, while the old versions still occupy theirs; only then are the old
  versions (and deleted records) freed. A record split over non-adjacent
  slots keeps its fragment numbers in order (**verified** by the test
  `resized_and_renamed_spaces_are_predicted_byte_for_byte`, scenario
  `c9resize`: a record of five slots took the free slots 30-31 and 71-73). A rename or an extension therefore
  moves the space record to new slots, and a later record may reuse the
  freed ones (`m5db`: the renamed space moved from 21 to 27, the next new
  space took 21).
* The header then gets the slots up to the last one in use (0x28), the new
  sequence at 0x38 and 0x40, the timestamp at 0x48 and its checksum.
  Setting the pool read-only or writable changes nothing in it.
* Records carry the sequence of the update that wrote them as their second
  integer. A space record changed through the management API (renamed,
  extended) also gains a security descriptor (`SPACE_SECURITY_DESCRIPTOR`:
  owner Administrators, group SYSTEM, read for Everyone, full access for
  SYSTEM and Administrators); new spaces are written without one.
* A new space also gets its own database in the internal metadata space,
  4 MiB after the previous one (sequence 1, one type 7 record); deleting
  the space leaves it in place.
* Not predicted yet: the object id of a new space (37 in `m5db`, where the
  existing ones were 1-5), and which disks and slabs its extents take (see
  slab allocation).

Choosing a copy: members carry copies of the pool database, and the one
with the highest sequence is current (**verified**: stale copies of `stale3`).
SDBB entries carry no checksum, so a copy torn by an interrupted update can
only be recognised by comparison. `spaces` groups identical copies, uses the
newest version that decodes (the one most members hold when copies of one
sequence differ, the first member's on a tie) and reports the others:
copies of the same sequence with different records as torn, newer copies
that do not decode as unusable (unit tests on `mirror3` with one copy
altered). Windows chooses the same way (**verified** by the test
`windows_resolves_diverging_database_copies`: round trips of `m5db` with
device 1's copy altered): with equal sequences it used device 0's copy and
rewrote neither; a newer copy that does not decode made it treat that disk
as lost ("Lost Communication", the space degraded) and write the good copy
again with a sequence above every copy seen (5 after a broken 4).

## Creating a pool

Pools of version 28 (Windows 11 24H2) only. Insider build 26340 (version
29) writes other layouts in a new pool: pool records of version 16, space
records of version 17 (layout byte 1), child records of version 5, and a
write-back cache (role 11) under simple and mirror spaces too (pools
`simple1c`, `mirror2` against `simple1c_26100`, `mirror2_26100`). `spaces`
edits the records of such pools in their own layout (the test
`version_29_pools_are_edited_but_get_no_new_spaces`) but creates no spaces
there.

What `New-StoragePool` writes to blank disks (**verified** byte for byte
by the test `new_pools_are_predicted_byte_for_byte`: pools of 3, 4 and 8
disks with 512-byte sectors, 4 KiB logical sectors on the same disks, and
4Kn disks, scenarios `c9new`, `c9four`, `c9eight`, `c9l4k`, `c94kn`;
`storage_spaces::create` takes Windows' GUIDs and times as inputs and
reproduces every page, and nothing else is written):

* A GPT of 128 entries: a Microsoft reserved partition from the first
  usable sector to 16 MiB, then the pool partition, named after the pool,
  from 16 MiB to the last whole MiB before the backup table. The protective
  MBR has one partition of type 0xee from LBA 1 with the size field all
  ones and an end cylinder (255 heads, 63 sectors) truncated to 10 bits.
  All GUIDs are time-based (version 1) on Windows; any unique GUIDs do.
* On every disk the SPACEDB header (generation 1, the time it joined, the
  database copy flag); on five disks at most the pool database.
* The pool database, one update of sequence 1 at 0x1000 of the partition:
  a page of 64 slots (header 0x08 = 00 01 02 00, 0x20 = 0x7fff80,
  0x30 = 0x10000); the pool record; a disk record per disk (ids 1..n,
  usage Auto-Select, the manufacturer and model the disk reports); the
  internal metadata space (id n+1, role 1, an n-way mirror of one 256 MiB
  slab with 16 MiB interleave, redundancy n-1); its extents, copy k on
  disk k+1 at physical slab 0.
* Nothing in the metadata space: its first database is written with the
  first space. On a single disk the metadata space is simple (one copy).

## Creating a space

What `New-VirtualDisk` writes (**verified** byte for byte by the test
`new_spaces_are_predicted_byte_for_byte`: a simple, a mirror, a parity, a
thin simple and a thin mirror space on three disks (`c9new`), a mirror of
four columns on eight disks (`c9eight`), simple spaces on one disk, with
4 KiB logical sectors and on 4Kn disks; `storage_spaces::create::NewSpace`
takes Windows' ids, GUIDs, slabs and times as inputs and reproduces every
page, and nothing else is written):

* One pool database update (sequence + 1) with, in this order: the space
  record; for each hidden space (the dirty region log of a mirror, or the
  parity journal, then the write-back cache) its container (type 3,
  internal, parent the space), the extents of its child (flag 4, one slab
  per row and copy) and the child (type 6, range 0 to its size); then the
  space's extents. A thin space gets its first row at creation.
* Every type 3 space (the space and each container) has a `number`, the
  slot of its database in the metadata space: the smallest number no type
  3 space of the pool has (so the count of them as long as none was
  deleted; after deletions Windows reused number 0 and overwrote that
  slot's old database, `c9opts2`). Numbers go to the
  space first, then the cache container, then the journal container,
  although the journal's records come first in the update.
* A database per type 3 space in the metadata space, 4 MiB apart by number:
  a page of 64 slots (0x20 = 0x10000), sequence 1, one record of type 7
  (version 1): `vint 0, vint 1, vint n, n disk GUIDs`, the disks whose
  copy of the metadata space holds the database, in the order of their
  ids. In pools of up to five disks that is every disk; the 8-disk pool
  holds five, not the five that carry the pool database. They are written
  before the pool database update.
* Hidden spaces: mirror copies of 256 MiB (the cache: its size) with the
  space's interleave, 256 MiB allocation unit; the dirty region log of a
  three-way mirror has three copies. On Windows 11 24H2 with HDDs only,
  parity spaces always get a write-back cache (1 GiB by default and when
  none is asked for, 512 MiB when 64 MiB is asked for; two columns on five
  disks, one on three), simple and mirror spaces none (asking for one
  fails: `c9new`, `c9opts`). The dirty region log starts
  as an empty header (generation 0) at 0 and at its size less 8 KiB; the
  parity journal as its header alone (owner the space, 1024 slots of 4 KiB
  from 8 KiB, two checkpoint areas of 125 MiB at 6 MiB, then the size and
  number of the space's extent runs and its stripe width); the cache as its header and slot 0 (type
  1, sequence 1, one entry (8, 1)). The cache header: 1024 slots of 4 KiB
  from 8 KiB; two checkpoint areas after them, each 0x200 bytes plus
  16 + chunk_size / 4096 bytes per chunk rounded up to 4 KiB; chunks of
  the space's stripe width from the next chunk boundary to the end
  (**verified** for the caches of `c9new`, `paritythin_26100`,
  `parity3_26100` and `wc64` by the test `new_caches_have_windows_geometry`).
* The first logical sector of the new space is cleared, so that an old
  partition table cannot show through; nothing else of what the slabs held
  is (scenario `c9zero`: over slabs filled with the verification pattern,
  bytes 0-0x1ff of the space were zero afterwards and the pattern
  continued at 0x200; `c9opts2`: the header of a deleted space's journal
  at that place vanished, which the test checks). `NewSpace::plan` clears
  the sector on every copy of a simple or mirror space and the whole first
  stripe, parity included, of a parity space.
* The extents of a fixed space take, on each disk, the first run of free
  physical slabs as long as the extent (four slabs with 1 GiB units:
  `c9opts2` skipped a run of three).
* Object ids do not follow from the metadata: the first space of a pool
  got 11 in `c9new` (disks 1-3, metadata space 4) and 15 in `c9eight`, and
  the ids of later spaces grow in steps (44, 73/74, 76, 109/110, 115/116).
  Windows reads any unique ids (they are inputs of the model).


## Changing a pool

Each of these is one pool database update and writes nothing else
(**verified** byte for byte on every copy by the tests
`pool_changes_are_predicted_byte_for_byte` and
`resized_and_renamed_spaces_are_predicted_byte_for_byte`, scenarios
`c9ops` and `c9resize`; `storage_spaces::manage`):

* `Set-StoragePool -NewFriendlyName`: the pool record with the new name and
  the security descriptor space records get. The partitions keep the old
  name.
* `Set-PhysicalDisk -MediaType` / `-Usage`: the disk record rewritten; two
  changes are two updates.
* `Set-VirtualDisk -NewFriendlyName`: the space record with the new name and
  the security descriptor.
* `Resize-VirtualDisk` of a fixed space: the space record with the new size
  and the security descriptor, and an extent per column and copy for the
  new rows (virtual slabs continuing, allocation unit x data columns per
  row group). The parity journal's header keeps its first run count.
* `Set-StoragePool -IsReadOnly` writes nothing.
* `Add-PhysicalDisk` of a blank disk to a pool of three (**verified** byte
  for byte on every page by the test `an_added_disk_is_predicted_byte_for_byte`,
  scenario `c9disk`): the new disk gets the partition table and a header
  without a database copy; one update writes the disk's record (without
  the copy), the pool record (only its sequence changes), the metadata
  space with one more copy and redundancy, and that copy's extent
  (physical slab 0 of the new disk), freeing the old pool and metadata
  space records; the databases in the metadata space get their disk list
  rewritten (sequence 2, the new disk included) on every copy, the new
  disk's included; the new disk gets the pool database and its header
  again, with the copy (so generation 2: the generation counts how often
  the header was written); a second update rewrites the disk's record with
  the copy. Disk ids come from Windows' object id counter (44 here).
* Retiring a disk (`Set-PhysicalDisk -Usage Retired`) rewrites its record
  (usage 5, the database copy flag kept) and moves every extent off it:
  each moved copy gets the next copy number beyond the policy's (copy 2 of
  a two-way mirror) on another disk, the old extent goes; the dirty region
  log's copy moved the same way. The retired disk's database copy stops at
  that update. The databases in the metadata space drop the disk from
  their lists by moving the last entry into its place. `Repair-VirtualDisk`
  afterwards only rewrites the disk's record without the copy flag, and
  writes that database and a header without the copy (generation 2) to the
  retired disk. `Optimize-StoragePool` after adding a disk moved one copy
  to it the same way. Windows' intermediate updates are not visible
  (`c9disk`: five updates for the optimisation, five for the retirement).
  Linux moves a copy by writing its data into free slabs first and then
  recording, in one update, the extent there and the old place free; a
  crash in between leaves only unreferenced slabs written. (A first
  version recorded a copy flagged as being regenerated first, as Windows'
  own moves leave it: after a crash there, Windows found a regenerating
  copy of a dirty region log and marked the disks holding the log lost,
  the pool read-only.)
* `Remove-PhysicalDisk` of that disk (**verified** byte for byte by the
  test `a_removed_disk_is_predicted_byte_for_byte`): one update rewrites
  the disk's record; the next writes the metadata space with one copy
  fewer, its extents renumbered over the remaining disks, and the pool
  record, and frees the old records and the disk's. Only these two updates
  match Windows' slots. The removed disk keeps its header and database
  behind a partition table without the pool partition. Removing a disk
  that still holds data failed with "not enough available capacity" on a
  pool with plenty (`c9drain`); Windows moves data off retired disks.
* A failed disk is replaced by adding a disk to the pool that misses it,
  repairing (the lost copies rebuilt on the new disk) and removing the
  missing disk; adding writes the pool database and the databases in the
  metadata space to the disks at hand only (the test
  `a_failed_disk_is_replaced`, with a crash after every step of the
  addition).
* `Remove-StoragePool` (its spaces removed first) rewrites the partition
  table without the pool partition, keeping the Microsoft reserved
  partition; the SPACEDB header and the database stay behind it
  (**verified** by the test `a_removed_pool_leaves_the_reserved_partition`).

## Deleting a space

`Remove-VirtualDisk` is one pool database update that frees the records of
the space, of its hidden containers and children and of all their extents
(**verified** byte for byte on every copy by the test
`deleted_spaces_are_predicted_byte_for_byte`: a thin parity and a simple
space, `c9opts2`). Its databases in the metadata space stay. Before
deleting a parity space Windows closes it, writing a checkpoint into its
journal and one into its cache, inside the slabs the update frees.

## Health

What `Get-StoragePool`, `Get-VirtualDisk` and `Get-PhysicalDisk` show
(`HealthStatus` / `OperationalStatus`) follows from the database and the
disks at hand; `spaces pool health` predicts it (`storage_spaces::health`)
with these rules, from Microsoft's documentation of the states:

* A disk at hand is Healthy / OK, a missing one Warning / Lost
  Communication. Usage (Retired, Hot Spare, ...) is a separate property:
  retired, hot spare, manual-select and journal disks were Healthy / OK
  (**verified**: every fixture whose manifest records `Get-PhysicalDisk`).
* The pool is Healthy / OK with every disk. With disks missing it is
  Warning / Degraded while more than half of the pool database copies are
  at hand (one of three disks lost: Warning / Degraded), otherwise
  Unhealthy / Read-only, and every space is detached (two of four lost:
  Read-only, both spaces Detached) (**verified** on those two states, the
  first versions of disk add and retire cut by a crash, `mgmt-crash.json`).
  Stale pool database copies do not count: pools with them attached as
  Healthy.
* A space counts its own extents and those of its hidden spaces (tiers,
  write-back cache, dirty region log, parity journal). With every copy
  current and at hand it is Healthy / OK; so it is while a present disk is
  being retired: Windows marks that disk's copies out of date (stale marker
  2) and rebuilds them elsewhere, and showed the space Healthy / OK during
  the rebuild (**verified**: `repairint`). Copies on missing disks make the
  space Incomplete, other out-of-date copies Degraded; it is Warning while
  it still survives a disk failure, Unhealthy with No Redundancy when it
  survives none (a two-way mirror that lost a copy: Unhealthy / No
  Redundancy, `tornt2`), and Unhealthy / Detached when data is lost.
  Windows showed Degraded where it had written the pool database with the
  disk away (`tornt2`: No Redundancy Degraded; the disk add state: Degraded
  Incomplete); from the metadata alone a copy on a missing disk is
  Incomplete.
* Not in the metadata, so never predicted: a repair in service, a space set
  to manual attach and detached (`spstates`: Unknown / Detached), a pool
  or space made read-only by an administrator.

Windows could not be asked about more states with disks absent: a pool of
four disks with simple, mirror and parity spaces made Windows 11 24H2
bugcheck (0x50 in spaceport.sys) when it arrived without one disk, whether
Linux or Windows had created it (`windows-absent-disk-bugcheck.json`).

## Records

Every record body below is complete: `storage_spaces::records` decodes
each field and re-encodes every pool, disk and space record of every
fixture byte for byte (**verified** by the test
`every_record_windows_wrote_is_reproduced_byte_for_byte`, about 1500
records of 169 pool states in the corpus). Integers ("vint": a length
byte, then that many big-endian bytes) are always as short as possible.

### Type 1: pool
`vint 0, vint sequence, guid[16], name, description, vint 0, u16 version,
u8 log2(logical sector size), u8 log2(physical sector size), settings,
u8 n + n bytes security descriptor, resiliency defaults`
(**verified**: versions 28 and 29, sectors 512/4096 and 4096/4096). Strings are `u16 BE`
length in UTF-16 code units (including the terminating NUL) followed by
UTF-16BE text; an empty string is a length of 0.

`settings` is the same in every pool (37 bytes in record version 15,
Windows 11 24H2; 38 in version 16, Insider): pool-wide defaults, starting
with the thin provisioning alert threshold (0x46 = 70 %). The resiliency
defaults close the record: simple (no redundancy, one copy), mirror (one
failure, two copies), parity (one failure), each with automatic columns
(0xffffffff) and 256 KiB interleave. The security descriptor is empty on
a new pool; renaming the pool adds the one space records get
(`SPACE_SECURITY_DESCRIPTOR`, scenario `c9smoke`).

Spaces expose the pool's logical sector size (**verified** against
`Get-VirtualDisk`); space records carry no sector size of their own. Whether a
later change of the pool default affects existing spaces is not tested yet.
Block devices and loop devices for a space must use this sector size: a 4 KiB
space keeps its GPT at byte 0x1000.

### Type 2: physical disk
```
vint id, vint sequence, guid[16], name, description,
u8 0, u8 (2 if the disk carries a copy of the pool database, else 0),
u8 usage, manufacturer, model, string, string, u8 0x0f, u8 media,
32 zero bytes, vint disk size, vint data area size (the partition less the
512 MiB before physical slab 0), vint 0xffffffffffffffff
```
`sequence` is the database sequence at which the record was last written
(disks whose media type was set one after the other carry consecutive
numbers). `usage` counts differently from `Get-PhysicalDisk`: 1
Auto-Select, 2 Manual-Select, 3 Hot Spare, 4 Journal, 5 Retired. `media`:
0 unspecified, 1 HDD, 2 SSD. (**verified**: pools `usages`, one disk of
each usage and media type, and `retired`; the metadata tests compare both
with `Get-PhysicalDisk`.) Windows stops updating the pool database copy on a
retired disk, so its copy is older than the others; `spaces` does not
report that as stale. `Remove-PhysicalDisk` deletes the disk's record and
its Storage Spaces partition entry; the SPACEDB header and the old database
stay on the disk unreferenced, so a removed disk is no member any more
(**verified**: pool `removed`, image `removed0.img`). A disk of the
database without a device at hand is missing.

### Types 3 (space) and 6 (child space)
```
vint id, vint sequence, guid[16], name, description,
u8 0, u8 internal (1 on the metadata space and hidden containers), u8 role,
type 3: vint size, vint number (0xffffffff on the metadata space)
u8 provisioning    1 thin, 2 fixed
vint allocation unit (bytes; all ones on tier templates)
u8                 (1-2 on tiered spaces, tiers and some containers, else 0)
prefix             01 00 01 00 00 (record version 17, type 6 version 5)
                   01 01 00 00    (record version 16: Windows 11 24H2)
u8 resiliency      1 simple, 2 mirror, 3 parity
vint redundancy    (disk failures tolerated)
vint copies
vint groups
vint columns       (0xffffffff on tier templates: chosen by Windows)
u8 log2(interleave)
type 3: vint write-back cache size (0 without), vint 0 x3,
        u8 n + n bytes (security descriptor, usually n = 0),
        vint (1; 0 on the metadata space), vint parent, u32 0
type 6: vint 0, vint parent, then u32 1, u64 start, u64 length (its range
        in the parent's address space) or, on tier templates, u32 0
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
vint 0, vint sequence (of the database update that wrote the record),
vint 0, u8 flags, vint slab_count, vint space_id, vint virtual_slab,
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
A disk that misses writes without a replacement (**verified** byte for byte
by the test `a_returning_disk_is_updated_first_and_the_others_at_repair`
on the scenario `m5stale`: two-way mirror on two disks, one detached while
the space was written in two runs, attached again, repaired):

* While the disk is away Windows writes nothing to the pool database. It
  lists the copy 0 of a written row on the missing disk as "Need
  Reallocation" and other copies as "Stale Metadata" (`Get-PhysicalExtent`),
  in memory only; the dirty region log (on the remaining disk) lists the
  written runs.
* When the disk returns, Windows resynchronises the dirty region log's own
  copies, brings the "Stale Metadata" copies up to date, and writes a
  database update to the returning disk only: the extent records of the
  "Need Reallocation" copies rewritten unchanged with the new sequence. The
  other members keep the older database, and the data of those copies stays
  out of date, until `Repair-VirtualDisk` regenerates it and writes the same
  database to the other members.
* Between the return and the repair nothing on disk says which copy of a
  listed run is current (the log's copies are equal again), so `spaces`
  refuses rows whose copies differ.

A run of `slab_count` consecutive physical slabs backs column `column`, copy
`copy`, rows `virtual_slab / data_columns ...` (**verified**).

## Slab allocation

How Windows allocates slabs (**verified** by the tests
`thin_slabs_are_allocated_per_update_at_the_first_free_slab` and
`thin_rows_are_allocated_when_the_cache_destages` on the scenarios
`m5thin`, `m5thin2`, `m5thinm` and `m5thinwbc`, and by
`pool_database_updates_follow_the_model` for fixed spaces):

* Each slab goes to the first slab of its disk that no extent uses; the
  copies of a mirror row and the columns of a parity row go to different
  disks. Which disks Windows picks does not follow from the metadata (not
  the emptiest) and differs between two identical runs (`m5thin`,
  `m5thin2`).
* A thin space allocates a row when a write first reaches it, as one
  database update with one extent record per copy (or column), in the
  order of the writes; a fixed space allocates all rows at creation and an
  extension adds the new rows in one update.
* With a write-back cache, a write into an unallocated row of a thin space
  stays in the cache without allocating; the row is allocated when the
  cache destages it (`m5thinwbc`: the row written first was allocated
  first).
* An extent record covers one allocation unit: 4 slabs with 1 GiB units
  (`m5db`'s second space), 1 slab with 256 MiB units (its extension, the
  thin spaces).
* One allocation writes nothing but the pool database page of every
  member and the data: a page-level comparison of the member disks before
  and after the first allocation of `m5thin` found only the database page
  (0x1000 into the partition) changed on each disk, and the written block.
* A new slab is not cleared (**verified** by the test
  `new_slabs_are_not_cleared` on the scenario `m7zero`): deleting a space
  leaves its data on the disks, and a thin row allocated at one of its
  slabs reads that data wherever it has not been written since.
* TRIM gives slabs back (**verified** by the test
  `trimmed_whole_slabs_are_given_back` on the scenario `m7trim`): after a
  768 MiB file on a thin simple space was deleted and the free space
  retrimmed (`Optimize-Volume -ReTrim`), the extent records of the two
  slabs the file had covered whole were gone, and the slab it covered in
  part stayed. The database sequence went from 7 to 14; how those updates
  divide between the deletion and the two slabs is not known.

`SpaceWriter` allocates rows of thin spaces with 256 MiB allocation units
the same way: one database update per row, written to every member in turn
(each flushed before the next, so a crash leaves each copy whole, old or
new), before the row's data; simple and mirror rows when a write first
reaches them, parity rows when the cache destages them. Each slab goes to
the present disk with the most free slabs that the row does not use yet,
so the disks fill evenly.

Discards (TRIM) give back the rows of thin simple and mirror spaces that
they cover whole, in one discard or in several since the row was last
written (file systems trim in pieces: ext4 by block groups of 128 MiB): one
database update per row, removing its extent records, written the same
way. The row then reads as zeros until a write allocates it again. Parity
and fixed spaces ignore discards: giving back a parity row would also have
to reset what the parity journal says about its stripes, which has not been
studied.
Windows attached a thin simple space and a thin mirror whose rows Linux
had given back this way (NTFS, a file deleted, fstrim) as healthy with
nothing to repair, and listed the same extents as the database written on
Linux (the tests `windows_reads_a_thin_space_after_linux_gave_rows_back`
and `windows_reads_a_thin_mirror_after_linux_gave_rows_back`).
`Get-PhysicalExtent` of a mirror space also lists the slab of its dirty
region log.

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
  policy: 11-15 columns 2 groups, 16 and 17 columns 3 groups), a local
  reconstruction code with `r = g + 1` parity units per stripe and
  `D = C - g - 1` data units. The parity units rotate like above
  (`P = (C - r - r*s) mod C`, data following them). The data units of a
  stripe are split into `g` consecutive groups, the first `D mod g` groups
  one unit larger (11: 4 + 4, 12: 5 + 4, 13: 5 + 5, 14: 6 + 5, 15: 6 + 6,
  16: 4 + 4 + 4, 17: 5 + 4 + 4). Parity unit
  `i < g` is the XOR of group `i` (local parity); the last one is a global
  GF(16) parity in the bit-matrix form above with the coefficients
  `1, 2, 3, ...` restarting in every group (12 columns: `1 2 3 4 5 1 2 3 4`).
  (**verified**: the unit map and all three parity units of the first 8
  stripes of pattern pool `lrc12`, the global code solved bit by bit over
  GF(2) and matching GF(16) multiplication; any two failed disks of `lrc11`
  to `lrc16` read back their pattern, and of impulse pools `lrc12i` and
  `lrc17i` their impulses.) Two lost units of one group are solved from the local and the
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
| 0x40 | 8 | offset of the first checkpoint area (just after the slot area) |
| 0x48 | 4 | size of a checkpoint area (0x46000-0x60000 seen) |
| 0x4c | 4 | number of checkpoint areas (2) |
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

Checkpoints ("SPCHECK\0", LE) hold the whole chunk map as of a sequence, in
one of the checkpoint areas after the slot area (**verified**: `parity4`,
`lrc12`, `m5wbc2`; pools whose log never wrapped have none):

| Offset | Size | Field |
|---|---|---|
| 0x00 | 8 | "SPCHECK\0" |
| 0x08 | 16 | owner space GUID (mixed-endian) |
| 0x18 | 4 | 1 |
| 0x1c | 4 | header size (0x50) |
| 0x20 | 4 | 0 when written at an attach, 1 otherwise |
| 0x24 | 4 | CRC-32 of the header (0x50 bytes) with this field zeroed |
| 0x28 | 8 | sequence (from the counter the slots use) |
| 0x30 | 4 | CRC-32 of the entries |
| 0x34 | 4 | bytes used: 0x200 + entries |
| 0x38 | 4 | the slot the log continues at after the checkpoint |
| 0x40 | 4 | offset of the entries (0x200) |
| 0x48 | 4 | bytes of entries |
| 0x4c | 4 | number of entries |
| 0x200 | … | entries, encoded as in mapping slots |

Log order (**verified** with logs written from Linux and read back by
Windows 11 24H2, see `rw-cache-log.json`; the rule also reproduces every
corpus pool): Windows takes the newest valid checkpoint and replays the
slots from the one the checkpoint names at 0x38, in slot order (wrapping
after the last), as long as their sequences keep increasing; without a
checkpoint it replays from slot 0. Entries are then ordered by (sequence,
position). A log that wrapped without a checkpoint loses everything before
slot 0: Windows ignored 921 chunks mapped by such slots and read 30 mapped
after the wrap. A checkpoint that named the wrong slot (2 instead of 0)
lost the chunk mapped by slot 1. So before the log comes round to the slot
the newest checkpoint names, Windows writes a new one into the other area,
with a sequence of its own (the "skipped" sequences): at the first wrap
(`parity4`, `lrc12`: before slot 0) or before the slot that followed the
checkpoint written at an attach (`m5wbc2`: sequence 3 at the attach,
continuing at slot 2; after the wrap slots 0 and 1 took 1026 and 1027, the
checkpoint 1028, slot 2 1029). Its map includes the write that made the log
wrap, which the next slot logs once more. The header is written once (its
sequence stays 1). In `wc64` slots 0-61 hold sequences 1-62 and slots 62-97
sequences 127-162 and the slots after 97 were cleared, without a
checkpoint. Caches of mirror and parity spaces start with a type 1 slot in
slot 0 (sequence 1, one 8-byte entry `08 00 00 00 01 00 00 00` in every pool
seen: `mirrorthin`, `paritythin`, `crashmirror`, `crashparity`), written
when the cache is initialised; their mapping slots start at sequence 2. It
carries no mappings and is skipped.

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
every read. Windows 11 24H2 ignores `-WriteCacheSize 0` for parity spaces
and creates the default 1 GiB cache (`m5pj`, `lrc13`: the manifest shows
1 GiB).

How Windows writes the cache log (**verified** slot by slot, byte for byte,
by the test `cache_log_follows_the_writes` on the scenarios `m5wbc` and
`m5pj`, 3-column parity with the default cache; `CacheWriter` implements
it):

* A slot is "SPSLOT\0\0", the owner GUID (mixed-endian), u32 1, u32 slot
  size, u32 type, u32 CRC-32 of the whole slot with this field zeroed, u64
  sequence, u32 entry count, u32 0, then the entries. A new cache holds only
  slot 0: type 1, sequence 1, one entry `u32 8, u32 1`.
* The first write into a chunk takes the next cache block (parity caches
  start at block 64, the mirror cache of `mirrorthin` at 0) and writes the
  next slot with the next sequence and one mapping entry per chunk it
  changes: state 2 with the runs of valid sectors (`u16` each, bit 15 =
  valid), or state 3 once the whole chunk is valid.
* The run words are always written, but counted in the entry length only
  for state 2; the next entry starts 8-byte aligned after the counted part
  and overwrites the rest, so the last full entry of a slot is followed by
  one stale run word covering the chunk (`0084` for 512 KiB chunks, `0082`
  for 256 KiB; also in the corpus pools `paritythin` and `mirrorthin`).
* A write into sectors already cached writes no slot; its data goes into the
  block.
* Destaging follows the log, not the clock (**verified** by the tests
  `cache_destages_to_reuse_its_log` and `destaged_and_cached_blocks_read_back`
  on the scenario `m5wbc2`: 1500 writes of 4 KiB into distinct chunks, then
  five minutes idle): after the last slot the log continues at slot 0, over
  the type 1 slot. Before the oldest slots are reused, Windows flushes every
  chunk cached so far: each is written to the space (a parity space logs it
  in its journal like a direct write) and logged as an entry of state 0 with
  block `0xffffffff`, in batches of up to 244 entries per slot. Blocks go
  onwards; a block freed right after it was handed out goes to the next
  chunk. Neither a disconnect nor two and a half or five minutes of idle
  time destaged anything; attaching wrote a checkpoint and a new type 1
  slot at the next position. The sequence numbers the slots skip are those
  of the checkpoints (at the attach, and once in the second lap).
* Checkpoints as described above (**verified** byte for byte against
  `m5wbc2` by the test `the_log_writes_a_checkpoint_where_the_last_one_continues`:
  the sequences of slots 0-490 of the second lap and the checkpoint before
  slot 2).
* Not modelled: how Windows groups the entries of concurrent writes into
  slots (the corpus shows 244 and 248 entries per slot), and when exactly a
  flush starts.

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
| 0x18 | 8 each | virtual slab where a listed extent run starts |

The log lists the extent runs written since the space was last
disconnected (**verified** with batch 9 of `tools/gen-corpus.sh`, whose
fixtures the test `dirty_region_log_after_each_ending` reads):

* A new space has both copies at generation 0 with no entries
  (`drtnowrite`).
* The first write into an extent run rewrites the older copy with the next
  generation and the run added: `drtdism` has generation 1 listing run 0
  at the end and generation 0 at the start; `m4kn`, written through four
  runs of 256 MiB, has generation 4 listing runs 0-3 at the start and
  generation 3 listing 0-2 at the end.
* The runs stay listed after 1, 5 and 15 minutes without writes
  (`drtidle*`), after setting the pool read-only (`drtro`), when the disks
  are detached with the pool online (`drtdism`, and every other pool of the
  generator) and through a Windows restart with the pool attached
  (`drtkeep`).
* Only `Disconnect-VirtualDisk` empties the log: `drtdisc` has both copies
  at generation 0 with no entries again.

So a listed run is no sign of a crash: pools moved from a Windows machine
that was shut down normally list every run written in their last sessions.
The copies of a mirror can only differ inside a listed run, where a crash
may have left a write on some copies only (`crashmirrorwc`, whose disks
were pulled during small writes, lists both of its extent runs, 0 and 4, in
generation 2 and run 0 in generation 1).

How Windows writes the log (**verified** byte for byte, stale entries
included, by the test `mirror_dirty_region_log_follows_the_writes`, which
replays the scenarios `m5drt` and `m5drt2` of `tools/scenarios.sh` with
their recorded step times on `DrtWriter` and compares every snapshot):

* Windows keeps the log in memory as a generation and an array of 509
  entries of which the first `count` are listed, and writes the whole array
  into the page. Attaching a space loads the listed runs of the newest copy
  into a zeroed array.
* When a write reaches an extent run that is not listed, Windows first
  removes the runs no write has reached for about 30 s (runs idle for 29 s
  stayed, runs idle for 35 s went), scanning the array from the front and
  moving the last listed entry into the place of each removed one, then
  appends the run, increments the generation and writes the page into the
  copy that does not hold the current generation (the one at the end when
  both hold the same), so the copies alternate. Once, with a disk of the
  mirror missing (`m5stale`), the generation went from 1 to 3 into the copy
  at the start; not modelled yet. Removed entries stay behind the listed ones,
  outside the count and checksum (`m5drt2`: listing run 2 after runs 0 and
  1 went clean leaves a stale 1 behind it).
* A write into a listed run writes no header, so the log keeps listing
  runs that went clean until the next run is added.
* `Disconnect-VirtualDisk` removes every run and writes generation 0 into
  both copies (after `[0, 1, 3]` the page holds the stale entries 1, 1, 3).
* Whether the header reaches the disks before the data is not observed yet.

Windows does not use the log to reconcile the copies, and it reads either
copy (**verified** by the round trips recorded in `tests/evidence`
and the test `roundtrip.rs`: copy 1 of `drtdism` and `drtdisc` was changed
in the first 4 KiB of every MiB; Windows attached both pools as healthy,
left both copies as they were, also after 120 s and `Repair-VirtualDisk`,
and read copy 0 in four passes over `drtdism` but copy 1 in three of four
passes over `drtdisc`). After the crash experiments its reads accordingly
returned copy 1 in 8 of the 9 differing MiB of `crashmirrorwc` and copy 0
in `crashmirror`. Differing copies have no right answer, so `spaces` reads
every copy in a listed run and refuses rows whose copies differ
(`--unclean-parity data` returns the highest copy).

## Parity journal (SPVDT, LE)

Parity spaces have a hidden role 0x0a child holding "SPVDT\0\0\0", with the
same header and slot geometry as SPCACHE (CRC-32 fields, owner GUID,
`slot_offset`, `slot_size`, `slot_count`), then at 0x40 the checkpoint
area offset (0x600000), at 0x48 the size of each area (125 MiB) and at
0x4c their number (2), at 0x50 the size of the owner's extent runs
(allocation unit x data columns), at 0x58 its stripe size (data columns x
interleave) and at 0x5c the number of runs (a fixed space of 2 GiB with
1 GiB units on two data columns: 2 GiB, 1; a thin space of 4 GiB: 512 MiB,
8; scenarios `c9new`, `c9opts2`). The log wraps behind
checkpoints exactly like the cache's (**verified**: `parity4` and `lrc12`
hold journal checkpoints of sequences 1025 and 2050, continuing at slot 0,
with one entry per run; pools whose journal never wrapped have none), and
`spaces` reads and writes it by the same rule. Mapping slots (type 0) hold
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

How Windows writes the journal (**verified** slot by slot, byte for byte,
by the test `parity_journal_logs_whole_stripe_writes` on the scenario
`m5pj2`: 3-column parity, writes of 4 MiB, 512 KiB and 4 MiB in requests
of 1 MiB, then 4 KiB; `JournalWriter` implements it):

* A new journal has no slots. Slots have the layout of cache slots, with
  the space GUID as owner.
* A write request of whole stripes (here every 1 MiB request: two stripes
  of 512 KiB of data) goes to the space directly, bypassing the write-back
  cache, and gets the next slot with the next sequence and one state 2
  entry for its extent run: runs over all stripes of the run (4096 here),
  in which the stripes written so far are consistent and those never
  written count as not consistent. Smaller writes go to the cache (the
  4 KiB write took cache block 64), as do writes into chunks the cache
  holds (`m5pj`: 512 KiB at 0).
* A minute without writes changed nothing.
* Stripes the journal records as consistent are never rewritten in place:
  writing the same 4 MiB again (`m5pj3`, test
  `consistent_stripes_are_rewritten_through_the_cache`) put eight full
  chunks into the write-back cache and left the journal and the stripes
  unchanged.

* When the last stripes of a run become consistent, the entry is state 3
  with length 0, followed by the run words that are written but not counted
  (`c000`: 16384 consistent stripes), as in the cache (**verified**:
  `parity3_26100`, filled front to back in whole stripes; test
  `whole_run_consistent_is_state_3_as_windows_writes_it`).

Rewriting a consistent stripe in place is not safe even when the journal
records it as not consistent first: a write cut off between the data and
the parity of stripe 0 (journal slot saying not consistent, new data, stale
parity) was attached by Windows as healthy, but Windows repaired nothing,
on attach or with `Repair-VirtualDisk` (evidence `rw-parity-crash.json`).
So `spaces` writes single parity spaces as Windows does (`SpaceWriter`):
whole stripes the journal records as not consistent and the cache does not
hold go to the space directly and are recorded as consistent afterwards;
everything else goes to the write-back cache (data into the block, flushed,
then the slot). When the log or the blocks run out, and when a space whose
cache holds data is opened for writing, every cached chunk is destaged:
partly valid chunks are made whole in the cache first (the rest of the
stripe copied from the space and logged as state 3), then the stripes are
recorded as not consistent (flushed), written with their parity (flushed),
recorded as consistent and tombstoned (flushed before their blocks are
handed out again). At every point a stripe either matches its parity or the
cache holds all of its data (test
`parity_writes_close_the_write_hole_in_every_crash_state`, which replays
every prefix of the recorded writes and random subsets of the unflushed
ones). A slot that reached only one copy of the cache makes the copies
disagree about its chunk; both versions are acceptable there (the older is
the state without the slot), but readers refuse the chunk unless told to
take the newer. Entries whose run list does not fit into a slot are written
as bitmaps (state 1); where neither fits, the whole run is recorded as not
consistent.

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
crash experiment, and Windows' reads returned copy 0 there. The thin space crash (cache in use) read exactly like Windows'
recovered space.

## Open questions

* SDBB entries carry no checksum of their own: a torn copy of equal
  sequence goes unnoticed by Windows (it used device 0's copy and rewrote
  neither), one that does not decode costs its disk.
* Remaining record fields (provisioning type, sizes, disk attributes, tiers).
* Which copy of a mirror Windows reads when (it varies between attaches);
  the meaning of the constant entry `8, 1` of the cache's type 1 slot; how
  Windows groups concurrent writes into cache slots and when a flush starts.
* The object id of a new space and the disk a new slab goes to (both vary
  between identical runs or follow no metadata); the order in which the
  members' database copies are written within one update.
* The dirty region log's generation jumping by 2 while a mirror disk was
  missing.
* Which side Windows trusts for an inconsistent parity stripe.
* Tier movement by the tiering optimizer (not exercised yet), enclosure
  awareness, older pool versions
  (Windows 8/Server 2012 layout differs, see StorageSpaceReconstructor).
