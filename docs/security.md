# Security review

Scope: the `spaces` command and the `storage-spaces` library as of the
write support (0.2.0), reviewed 2026-09-29; pool management (`spaces pool`,
`space`, `disk`; Stage 3), reviewed 2026-09-30; the ReFS reader (`refs`
and the `refs` library, Track B), reviewed 2026-10-03.

## Threat model

* **Untrusted disks.** Any disk with a Storage Spaces partition can be
  plugged in; the udev rule then runs `spaces attach` as root, which parses
  every piece of metadata on it (GPT, SPACEDB header, pool database,
  records, write-back cache and its checkpoints, parity journal, dirty region
  log). Hostile metadata must not corrupt memory, exhaust memory or CPU,
  make the tool write anywhere, or expose data to other users.
* **Local users.** Attached spaces and the processes serving them must not
  give users without access to the member disks access to their contents.
* **Writes.** Writing happens only when asked for (`--rw`); on a pool whose
  metadata was crafted to mislead, writes must still stay inside the pool
  partitions of its member disks.
* **Management.** `spaces pool|space|disk` change metadata and move data
  only when asked for (`--yes`), only on the disks named, never on disks in
  use, and leave a pool that opens after a crash at any point.
* **Untrusted ReFS volumes.** `refs` is run by hand on a volume (never by
  udev) and only reads: hostile metadata must not corrupt memory, exhaust
  memory or CPU, or show the volume to other users.

## Findings and mitigations

| Area | Status |
|---|---|
| ReFS volumes on hostile input | No `unsafe` code in `refs` or `refs-cli`. Every metadata page `refs` reads must match the checksum (CRC-64 or SHA-256) its reference records, the superblock and checkpoint their own, the boot sector its sum; a mismatch is an error. The data of integrity streams must match its CRC32-C or CRC-64 on every read (test `integrity_streams_refuse_damaged_data`). B+-trees deeper than 16 levels and chains of more than 1024 data levels are refused, reads may not leave their container, page buffers are sized by the page size of the boot sector (4 KiB or 64 KiB clusters only). Fuzz targets `refs_parsers` (boot sector, page references and headers, nodes, rows and records) and `refs_volume` (two fixtures patched by the input and read end to end: partition table, superblock, checkpoint, tables, directories, records, extents, streams, links; fuzzing builds accept every checksum so that patched pages reach the parsers); a 2-minute run of each found nothing (2026-10-03). Tests damage a directory page, the boot sector and the superblock copies of the fixtures. The FUSE mount is read-only and only for the mounting user unless `--allow-other`; named streams over 64 KiB are not handed to the kernel as extended attributes. |
| Writing ReFS | Only `refs set|overwrite|create|rename|delete` write, only with `--yes`, only to a device or image given by path (never a pool's space). A commit writes changed pages only to clusters free in both the old and the new allocator state (clusters freed by the transaction stay reserved until it commits), flushes, and then writes one checkpoint cluster into the older slot and flushes: the old checkpoint stays intact until then (test `setting_times_and_attributes_commits_copy_on_write` checks the write order and that nothing the old checkpoint reaches is written). Records not in their directory entry are refused. Every page a commit writes is checked in the tests the way Windows checks pages (rows parse by their sizes, the key index names live rows, the free bytes add up); Windows refused a page with a zeroed hole before that rule was known. |
| Memory safety | No `unsafe` code in either crate (`grep -rn unsafe crates/`); parsers index with checked slices or `get`. |
| Parsers on hostile input | cargo-fuzz targets: `partitions`, `pool_open`, `record_decode`, `database` (update model, page growth, extent record round trip), `cache_index` (slots, checkpoints, writer model with wraps and destaging), `parity_journal` (slots, checkpoints, writer model), `dirty_regions` (writer model), `pool_write` (every space of a patched pool opened for writing, written, discarded and flushed; see "Where writes go"), and for pool management `records` (record models and disk headers encode back to what they decoded), `create` (pools and spaces from fuzzed parameters open clean and read back writes) and `manage` (every planner on patched pools; every write stays inside the members' pool partitions or partition tables). A 24 h run is part of the release checklist; for 0.2.0 it found nothing (2026-09-30); for 1.0.0 the runs of the management targets found the two problems below and an error in a harness, all fixed, and the final 24 h of every target found nothing (2026-10-02, see `docs/plan.md`). |
| Memory exhaustion | Every size read from disk is bounded before allocating: database ≤ 64 MiB and ≤ 1024 formatted pages, cache and journal slot areas ≤ 64 MiB, checkpoints ≤ 16 MiB (found in this review: a crafted checkpoint could claim the 125 MiB of a journal area per copy; now capped), checkpoint area offsets ≤ 2^56 for the cache and the parity journal (found by fuzzing: a journal area offset near 2^64 overflowed), slab and extent numbers < 2^32. |
| Command execution | External programs (`dmsetup`, `systemd-run`, `nbd-client`, `losetup`, `udevadm`, `journalctl`) get separate arguments, never a shell; the one `sh -c` checks fixed program names. Device-mapper names are made of `[A-Za-z0-9_.+]` only, whatever the pool and space are called. |
| Where writes go | Every slab write resolves through `Pool::slab_location`, which refuses slabs beyond the member's pool partition; database copies go to the fixed offset in that partition. A misleading pool can at worst overwrite its own partitions, which `--rw` asked for. Checked by the fuzz target `pool_write` and the test `writes_on_corrupted_metadata_stay_in_the_pool_partitions`: whatever the metadata, no write lands outside a member's pool partition. Found by `pool_write`: a user space whose record claims a part of an address space, as only tiers do, made writes start below its layout (a panic); such spaces are now refused for writing. |
| What is written | `--rw` is refused unless the pool is clean and every structure the writes touch is understood (see the user guide); the udev rule and `storage-spaces-attach.service` never pass `--rw`. |
| Exposure to other users | Block devices (`/dev/mapper/ss-*`, `/dev/ublkb*`, `/dev/nbd*`, loop devices) are created by the kernel as root:disk 0660. NBD sockets are made owner-only (0600) right after they are bound, before connections are accepted (found in this review: they followed the caller's umask); the attach path keeps them in `/run/storage-spaces` (root, 0755). The FUSE mount is read-only and without `allow_other`, so only root can open it. State files are root's. |
| Which disks management writes | Every command prints its plan and writes only with `--yes`. `pool create` and `disk add` refuse disks that are not blank (a partition table, file system or pool data in the first or last MiB) unless `--wipe`. Members are opened exclusively: block devices with O_EXCL, which fails while they are mounted, assembled or served, and image files with an exclusive lock that every `spaces` process opening them for writing takes (found in this review: only paths spelled under `/dev` got O_EXCL, so a block device named through a symlink did not, and image files had no exclusion at all: a management command could change a pool whose images `serve-ublk --rw` was writing). |
| Management on hostile metadata | Planning starts with `check_pool`: the pool must be clean (no missing disk, stale copy or torn record; repair and removing a missing disk accept missing disks and nothing else) and every record decode into a model that encodes back to the same bytes, so nothing unknown is rewritten. The fuzz target `manage` runs every planner on patched pools and checks every action of the plan: writes only inside the members' pool partitions (removing a pool: only its partition tables), or to the disk being added. Sizes are rounded to whole rows with checked arithmetic and bounded to 2^32 slabs (found in this review: a size near 2^64 wrapped to a space of 0 bytes, and a record's allocation unit of 0 would have divided by zero when growing its space); the fuzz target `create` now draws sizes from the whole 64-bit range. Found by the 24 h run of `manage`: records claiming more slabs of a disk than its size made the count of its free slabs overflow, and in a release build new extents could have been placed over data; extents beyond their disk or overlapping each other now stop the planners (every pool of the corpus and the scenarios passes the check). Found by the second 24 h run of `manage`: removing a pool or a disk rewrites the partition table where the table's own headers say, unchecked; a header claiming a sector near 2^64 overflowed, and a crafted table could have directed those writes into the disk's data. The headers must now sit at the disk's first and last sector and the entry arrays outside the partitions' area, with checked arithmetic (test `crafted_partition_tables_are_not_rewritten`). |
| Crashes during management | A plan's steps are made durable one after the other; pool database copies are written one per step; data is copied into free slabs before the one database update that records the move. Tests replay a crash after every step of creating a space, retiring a disk and repairing (`ops.rs`), and Windows 11 took pools cut after steps of creating a space, adding and retiring a disk (`mgmt-crash.json`). |
| Denial of service by panics | A panic ends the attach or serving process for that pool only; the fuzz targets check the parsers do not panic. |

## Remaining risks

* A hostile pool attached read-only can make reads fail or return its own
  (hostile) data; that is inherent in reading it.
* Writing trusts the pool it was asked to write: with `--rw` on a crafted
  pool, the data of its spaces can end up anywhere inside its partitions.
* Image files are locked with advisory locks: other programs (a hypervisor
  using the same images, `dd`) are not kept out.
* Windows 11 24H2 bugchecked when a pool of four disks arrived with one
  disk absent, whoever had created the pool (`docs/plan.md`, Stage 3
  progress). Handing Windows a pool with a disk missing is a risk to the
  Windows machine, not to the pool.
