# Project plan

Status as of 2026-10-03: Stages 1, 2 and 3 have met their exit criteria
(v1.0.0; tags v0.1.0, v0.2.0 and v1.0.0); Track B (ReFS) reads ReFS 3.14
volumes and mounts them read-only. The plan is
split into three stages with checkable exit criteria. ReFS is a separate track (last section) that shares the test
infrastructure but not the milestones.

## Goal

Make Microsoft Storage Spaces pools fully usable from Linux:

1. **Stage 1 - read-only:** any pool created by Windows 11 appears on Linux
   as ordinary read-only block devices (one per virtual disk) that can be
   partition-scanned and mounted like any disk.

Scope decision (2026-09-26): only Windows 11 pools (pool version 28 from
24H2 and 29 from Insider builds) are targeted. Older Windows releases, Windows Server and pools
created by them are out of scope unless a user need appears; the parser
rejects unknown layout versions instead of guessing.
2. **Stage 2 - writes:** the same block devices are writable, and Windows
   accepts the pool afterwards as healthy, including after a crash of Linux
   mid-write.
3. **Stage 3 - management:** pools and spaces can be created, extended,
   repaired and deleted from Linux, producing pools Windows treats as native.

Guiding rules for all stages:

* Windows is the oracle. Every format claim is verified against pools created
  by Windows and every Linux-written pool is verified by Windows.
* Unknown on-disk state means refuse, never guess: an unrecognised record,
  resiliency, cache state or journal content makes the tool stop (read-only
  stage: refuse to expose the space, or expose it with an explicit override).
* Pool members are opened read-only until Stage 2 is explicitly enabled per
  command (`--rw`), and never written by read paths.
* Hostile or corrupt metadata must never cause a panic, unbounded allocation or
  out-of-range I/O (fuzzed).

## Done (M0, 2026-09-26)

* Workspace, GPL-2.0-or-later, `storage-spaces` library and `spaces` CLI
  (`info`, `extents`, `export`).
* Format documented in `docs/storage-spaces-format.md`: SPACEDB, SDBC/SDBB,
  record types 1-4 and 6, space hierarchy, slab mapping, left-symmetric single
  parity, SPCACHE write-back cache.
* Reads of simple / 2-way / 3-way mirror / single-parity, fixed and thin
  spaces, cache overlay, degraded reads (mirror copies, parity XOR rebuild).
* VM tooling to create pools on VHDX and a corpus of 8 pools; tests compare
  with `Get-PhysicalExtent` and a verification pattern.

## Progress log (Stages 1 and 2)

Evidence is recorded here as milestones advance; commands refer to the tools
in this repository.

### M1 (exit criteria met, 2026-09-27)

* Corpus of 16+ Windows-created pools (Windows 11 Insider 26340, pool version
  29): simple 1-3 columns, 2/3-way mirror, single parity 3-5 columns, dual
  parity 7 columns, thin and fixed, 512/4096 logical sectors, 1 GiB
  allocation unit, write-back cache 0 / 64 MiB / default, plus the real
  `test_ubuntu` pool on two VMware SATA disks.
* Every pool: parsed extents equal `Get-PhysicalExtent` exactly; pool version
  and sector sizes equal `Get-StoragePool`/`Get-VirtualDisk`; the whole
  verification pattern reads back (`cargo test`, corpus + fixture tests).
* Decoded since M0: pool version and sector sizes, SPACEDB/SDBC CRC-32,
  members without a database copy, write-back cache entry states with
  partially valid chunks and wrapped logs, dual parity layout (P = XOR),
  hidden children identified (SPACEDRT dirty region tracking, SPVDT parity
  journal).
* Degraded reads: any single member missing at open time or failing at run
  time for mirror, parity and dual parity pools (`reads_fail_over_...`).
* Storage tiers and mirror-accelerated parity read correctly (`tiered`,
  `mapar`); tiers are child spaces with their own start and layout.
* Crash experiments (`tools/vm/New-CrashPool.ps1`, disks pulled during
  writes, Windows' recovered content recorded): mirror and thin+cache read
  exactly like Windows after recovery; parity equals it except one MiB whose
  stripe the parity journal marks unknown and whose parity mismatches, which
  is refused by default (`--unclean-parity data` reads the disk as is)
  (`cargo test --test crash`).
* Out-of-date mirror copies: a disk pulled while the space was written keeps
  copies Windows marks stale, plus a replacement copy being regenerated; the
  reader uses current copies only and fails reads that have no current copy
  (`cargo test --test stale`, pool `stale3`).
* Quorum: pools with fewer than half of their disks present are refused by
  attach unless `--force` (the lone removed disk of `stale3` otherwise reads
  the old state).
* Dual parity Q decoded (GF(16) bit-matrix Reed-Solomon, found with
  single-byte impulse stripes written by Windows, `tools/vm/New-ImpulsePool.ps1`);
  any two failed disks are rebuilt for 7-10 column spaces (`cargo test`,
  also in CI on the impulse pool fixtures).
* Dual parity with 11+ columns (local reconstruction code, 2-3 groups)
  decoded on pattern pools `lrc11`/`lrc12` and impulse pool `lrc17i`: local
  XOR parity per group plus a global GF(16) parity; any two failed disks are
  rebuilt (corpus tests; CI on the `lrc17i` fixture). The Windows VM
  crashes seen while generating wide pools (bugcheck 0x124, parameter
  0x10 = WHEA error reported by a device driver, after stornvme reset the
  VM's virtual NVMe disk during multi-second host write stalls) stopped
  when the generators paced their writes (15 MB/s, write-through) and
  nothing else loaded the host disk; it recurred (17:14Z, vmware.log: WRITE
  commands of 4.6 s, stornvme reset, 0x124) while the Linux VM, whose disk
  shares the same host drive, ran a kernel test at the same time. Heavy I/O
  runs on one VM at a time.
  Kernel level on the Linux VM: `lrc11` through ublk, NBD and FUSE passes
  the backend matrix, and attached with `--degraded` and two of its eleven
  disks absent (ublk) reads its whole pattern, sequentially and with 500
  random reads.

### Stage 1 completion work (from 2026-09-27)

* Test VM: pools are generated on DESKTOP-BQ2J4NS (Windows 11 24H2, build
  26100, QEMU, SATA system disk; no crashes); `tools/vm.sh` reaches the old
  VM through `WIN_VM_HOST`. Fetched pools are deleted from both VMs
  (`fetch-corpus.sh --remove-remote`, `Remove-TestPool.ps1`).
* Housekeeping: `tools/gen-corpus.sh` lists every pool including the impulse
  and LRC pools (`@Script.ps1` lines, `SUFFIX`, `THROTTLE`); metadata
  fixtures for `lrc11`, `lrc12`; data fixture `lrc12i` (12 columns, 2
  groups, built by 24H2) next to `lrc17i`, both made with
  `tools/data-fixture.py --no-cache-slots` (it reproduces the `lrc17i`
  fixture byte for byte); CI rebuilds any two lost columns of both.
* Configuration matrix (T3) closed with pools created by Windows 11 24H2,
  each with metadata equal to `Get-PhysicalExtent`, the whole pattern read
  back and a metadata fixture: interleave 16 KiB (`il16k`) and 1 MiB
  (`il1m`, parity); simple spaces of 4 to 8 columns (`simple4c`-`simple8c`);
  several spaces of different resiliency in one pool (`multi`); a space
  allocated into the hole of a deleted one and behind another space (`frag`,
  `fragpar`); spaces extended after writing (`ressimple`, `respar`,
  `resthin`); 4Kn member disks (`m4kn`, `parity4kn`; every other pool has
  512e members); a pool filled completely (`full`); retired disks with the
  repair finished (`retired`); grouped dual parity of 11 to 17 columns
  (`lrc11`-`lrc16`, `lrc17i`: group sizes follow D split into g groups, the
  first D mod g one larger, and any two failed disks are rebuilt).
* M1 items 4, 5, 7: disk usage (Auto-Select, Manual-Select, Hot Spare,
  Journal, Retired) and media type decoded and shown by `spaces info`
  (`usages`, `retired`); missing disks and devices no longer in the pool
  reported; space health computed (healthy/degraded/failed); manual attach,
  detached and read-only states are not kept in the pool metadata
  (`spstates`). Torn and unusable pool database copies are detected and
  reported (unit tests). Cache slot type 1 (initialisation record) and the
  absence of a head/tail are documented. Crashes with dirty write-back
  cache data (`crashmirrorwc`, `crashparitywc`) led to reading dirty region
  tracking, merging the copies of cache and journal slot areas and ignoring
  uncommitted provisional cache entries: all five crash pools read exactly
  like Windows' recovery or refuse (at most 9 of 2048 MiB), never different
  data.
* M2 on the Linux VM (2026-09-27), `tools/backend-matrix.sh` with fio 3.28:
  every corpus pool with a pattern, 67 pools (the first corpus including
  `dual7`, `lrc11`, `lrc12`; the 24H2 regenerations; the configuration
  matrix, disk state and grouped dual parity pools), every space of each,
  through dm, ublk, NBD and FUSE: 277 pool/backend combinations pass, none
  fails, dm is skipped for the 27 spaces it cannot map (parity, cached data,
  tiers). The NTFS pools are checked by `tools/ntfs-check.sh` (M3).
  Each passing combination read the whole pattern sequentially, 300 random
  reads, 2000 random O_DIRECT reads from 4 threads and a 20 s fio random
  read load (4 KiB-1 MiB, queue depth 32, 4 jobs) without errors.
* Windows 11 24H2 pools (pool version 28, space record layout 16) read:
  the basic configurations regenerated as `*_26100` pass the metadata and
  pattern tests (see the format document).

### M2 (exit criteria met on the test VM, 2026-09-26)

* dm (`dm-table`), ublk (`serve-ublk`), NBD (`serve-nbd`) and FUSE
  (`serve-fuse`) all expose the real `test_ubuntu` pool: data equal to the
  exported image (full SHA-256 for dm, sampled ranges for the others), writes
  refused, NTFS mounted read-only through each. ublk and NBD expose the 4 KiB
  logical sector size; dm inherits the members' 512.
* `tools/backend-matrix.sh` on the Linux VM over eight corpus pools covering
  every layout (simple, mirror, single parity, thin with cached data, 4 KiB
  sectors, tiers, mirror-accelerated parity, thin parity): 27 of 27
  applicable pool/backend combinations pass the full sequential pattern check
  and 300 random reads (4 KiB-1 MiB) through `/dev/mapper`; dm is skipped
  where it cannot map the space (parity, cached data, tiers).
* The remaining corpus pools go through the same `SpaceReader` that every
  backend serves; `cargo test` checks it on all 21 pools, and the dm segment
  test covers every pool dm can map.

### M3 (exit criteria met on the test VM, 2026-09-26)

* `spaces scan/attach/detach/status`: stable `/dev/mapper/ss-<pool>-<space>`
  and `-p<N>` partition devices from our own GPT parser (Windows omitted the
  protective MBR inside `test_ubuntu`, so the kernel would not see it).
  attach and detach are serialized by a lock; serving processes open the
  members with O_EXCL (a second server got EBUSY in a race).
* Boot: after a reboot of the Linux VM `storage-spaces-attach.service`
  attached `test_ubuntu` through ublk and its NTFS partition mounted
  read-only (the first reboot exposed a systemd ordering deadlock, fixed by
  starting server units without default dependencies).
* Runtime failure: `mirror2` attached through ublk over device-mapper
  wrappers; switching one wrapper to the `error` target kept the whole
  pattern readable (sequential and 500 random reads); with both failed reads
  return I/O errors; detach took 0.09 s.
* Throughput on the VM (warm host cache, 1 GiB direct reads): dm 337-342
  MB/s, ublk 307-317 MB/s (91-93 % of dm; better than dm for 64 KiB reads),
  NBD 287 MB/s. dm maps the members directly, so it runs at member speed.
* NTFS pools (2026-09-27): five pools created by Windows 11 24H2 with NTFS
  and real files (System32 DLLs and random files of awkward sizes, 1252
  files, 593 MiB, SHA-256 recorded on Windows) with single parity, dual
  parity (7 columns), grouped dual parity (12 columns), tiers and
  mirror-accelerated parity. `tools/ntfs-check.sh` on the Linux VM attaches
  their images as partition-scanned loop devices: udev and
  `storage-spaces-attach.service` assembled every pool on their own (ublk),
  the NTFS partitions mounted read-only with ntfs3, and all 1252 files of
  each pool matched (5 of 5 pass). They also exposed and now cover two
  cache and journal decoding errors (entry lengths in bytes, 8-byte
  alignment; the parity pool carries its default 1 GiB write-back cache).
* Not applicable: a pool created on a physical Windows machine (the test
  setup has Windows 11 VMs only, see the scope decision).

### M4 (exit criteria met except the release, 2026-09-28)

* Release: prepared (a tag `v0.1.0` and a GitHub release with a static
  x86_64 musl build; the crates go to crates.io only once a stable release
  is confirmed, decision 2026-09-28, their metadata is ready). Decision
  2026-09-29: nothing more is pushed to GitHub, tagged or released until
  Stage 3 is complete; the work stays in local commits until then.
* Dirty region log of mirrors (settled 2026-09-28): every mirror pool of the
  corpus listed extent runs although its copies agreed. Batch 9 of
  `tools/gen-corpus.sh` ended small mirror pools in every way
  (`New-TestPool.ps1 -Finish/-IdleSeconds`, one restart of the VM): the log
  lists the runs written since the space was last disconnected; 1-15 min
  idle, a read-only pool, detaching the disks and a Windows restart keep
  them, only `Disconnect-VirtualDisk` empties it (fixtures and the test
  `dirty_region_log_after_each_ending`). Round trips of copies whose mirror
  copies were made to differ in 256 blocks (`tools/raw2vhdx.py`,
  `tools/vm/Test-RoundTrip.ps1`): Windows attached them as healthy, left
  both copies as they were, also after 120 s and `Repair-VirtualDisk`, and
  read copy 0 on one attach and copy 1 on another (`tests/roundtrip.rs`).
  Consequences: `spaces info` now says "dirty region log: N extent run(s)
  written since the space was last disconnected" instead of calling the
  pool unclean; reads keep comparing the copies of listed runs, since only
  there can a crash leave them different and Windows offers no answer (the
  comparison now reads the highest copy into the caller's buffer and costs
  about 20 % CPU when cached, one read per extra copy from disk); the
  generator keeps detaching the disks with the pool online, because a
  restart leaves the same state.
* Packaging: `cargo install --locked --path crates/spaces-cli` installs
  `spaces`; `contrib/install.sh`; Debian package script
  (`contrib/deb/build-deb.sh`); `contrib/arch/PKGBUILD` builds and tests the
  package with makepkg in an Arch Linux bootstrap root (bwrap; package with
  binary, udev rule, unit with `/usr/bin/spaces`, man page, modules-load
  file, docs); static musl build in CI; manual page `contrib/man/spaces.8`.
* User documentation: `docs/user-guide.md` (supported configurations,
  backends, pools with missing disks, pools after a crash, copying a space
  out, troubleshooting), README and CHANGELOG.
* crates.io metadata (repository github.com/woffko/ms-storage-spaces-linux-project,
  readme, keywords, categories; the library package leaves out the test
  fixtures; both crate names are free).
* Security review of the parsing code: every read of metadata-controlled
  sizes is bounded (database 64 MiB, cache and journal slot areas 64 MiB,
  NBD requests 32 MiB, options 64 KiB); integer overflows found and fixed
  with regression tests: GPT table and partition bounds, extent slab numbers
  (now at most 2^32), grouped parity group counts, slab offsets, cache data
  offsets. No `unsafe` code in either crate (the ublk and FUSE crates hold
  their own). A mutation test of the metadata parser (200 000 corrupted
  variants of the fixtures) runs without a panic.
* Fuzzing: seven cargo-fuzz targets in `fuzz/` (record decoding, SDBB
  assembly, cache header and slot log, parity journal, GPT/MBR, dirty region
  tracking headers, whole-pool open with reads on patched fixtures); CI runs
  each for 30 s.
* 24 h without findings: all seven targets ran for 24 h with 4 processes
  each (`cargo +nightly fuzz run <target> -- -fork=4 -max_total_time=86400
  -rss_limit_mb=4096`) from 2026-09-27T14:13Z to 2026-09-28T14:14Z on
  5f235fe: no crash, timeout or out-of-memory case, no artifacts.
  Executions (edge coverage): record_decode 22.7 G (428), database 14.6 G
  (656), dirty_regions 10.9 G (364), cache_index 5.9 G (531), parity_journal
  5.5 G (467), partitions 2.5 G (279), pool_open 16.4 M (4030). Two earlier
  attempts restarted the count: the first (4772df4, six targets) was stopped
  after 4.6 h without findings to add the dirty region target; in the
  second, pool_open hit a stack overflow after 1.5 h (hidden spaces whose
  dirty region containers form a cycle), fixed in 5f235fe with a regression
  test.
  The only later code change, 4088d3e, replaces `chunks_exact` with
  `as_chunks`; a 15 min run of all seven targets on 3a14cf5 with the 24 h
  corpora (2026-09-28T14:30Z) found nothing either.

### M5 (exit criteria met, 2026-09-28)

Method: `tools/scenarios.sh` creates a pool on the Windows VM
(`New-TestPool.ps1 -Finish Keep`) and `tools/vm/Invoke-Scenario.ps1` runs
scripted steps on it, snapshotting the member disks between them while the
pool stays attached (raw reads of the members; zero pages left out, pattern
blocks stored as offset and tag). `tools/fetch-snapshot.sh` turns the
snapshots into images, `spaces dump`/`diff` compare states, and fixtures of
every state plus the step times (`scenario.json`) are committed under
`crates/storage-spaces/tests/scenarios/`. Each model is checked by
`tests/scenarios.rs` against every state of its scenarios, byte for byte.
The format document has the specifications; its open questions list what
the experiments left open (the disk and object id Windows picks, how it
groups concurrent cache writes and when a flush starts, the order of copy
writes within one update, a generation jump of the dirty region log).

Round trip to Windows: `tools/raw2vhdx.py` turns raw images (512-byte or
4Kn) into dynamic VHDX files, `tools/vm.sh -put` uploads them, and
`tools/vm/Test-RoundTrip.ps1` attaches the pool and records pool, space and
disk health before and after `Repair-VirtualDisk`, `Get-PhysicalExtent`,
the verification pattern and, for NTFS pools, `chkdsk` and the file hashes
(`drtdism` and `ntfstier` unchanged: healthy, pattern and all 1252 files
intact; used for the mirror copy and diverging database experiments).

* Dirty region tracking (`m5drt`, `m5drt2`; `DrtWriter`): 14 snapshots
  predicted byte for byte, stale entries included: runs added in the next
  generation into the older copy, runs idle for about 30 s (29 s kept,
  35 s dropped) removed by moving the last entry into their place, a
  disconnect resetting both copies. Not modelled: the generation jumped by
  2 once while a mirror disk was missing.
* Write-back cache (`m5wbc`, `m5pj`; `CacheWriter`) and parity journal
  (`m5pj2`; `JournalWriter`): every slot of every state byte for byte. The
  first write into a chunk takes the next block (parity caches from 64)
  and the next slot; writes of whole stripes bypass the cache and get a
  journal slot listing the run's consistent stripes. 24H2 ignores a zero
  cache size for parity spaces. Destaging (`m5wbc2`, 1500 small writes):
  driven by the log, not the clock; the log wraps to slot 0, and before
  reusing the oldest slots Windows flushes every cached chunk (tombstone
  entries in batches, the data through the parity journal); a disconnect
  and five idle minutes destage nothing. All 1500 blocks read back through
  the reader, destaged or still cached.
* Database update protocol (`m5db`, `m5stale`; `database::Database`): every
  member's pool database in every state byte for byte (rename, new space,
  extension, deletion, a disk that missed writes and came back): new
  record versions first-fit into free slots while the old ones still hold
  theirs, then the old ones freed, then the header; the default security
  descriptor on the first change of a space. While a disk is away nothing
  is written; the returning disk gets the next update first, the others at
  the repair. Diverging copies (round trips of altered copies): on equal
  sequences Windows used device 0's copy, as `spaces` does; a newer copy
  that does not decode cost its disk, and the good copy was rewritten with
  a higher sequence.
* Extent and space health (`m5stale`, with `stale3` of the corpus): with a
  disk to reallocate to, a missing disk's copy gets a stale marker and a
  replacement copy; without one, missed writes are known only from the
  dirty region log and in memory ("Need Reallocation", "Stale Metadata")
  until the repair.
* Slab allocation (`m5thin`, `m5thinm`, `m5thin2`, `m5thinwbc`): one
  database update per row (an extent record per copy or column) at the
  first free slab of the chosen disks; the disks are chosen differently in
  two identical runs, so they are an input of the model. With a cache, a
  thin row is allocated only when the cache destages it; its data read back
  before and after. Object ids of new spaces (`m5ids`: 37, 70,
  99/100, 108) do not follow from the metadata either.

### M6 (exit criteria met, 2026-09-29)

Met for simple, mirror and single parity spaces: fio with verification
through every backend, NTFS written on Linux and verified by Windows
(healthy, nothing to repair, chkdsk clean, every file intact), crash states
accepted and handled by Windows, crash replays of every flush point and of
unordered unflushed writes. Refused rather than written: dual parity (M5
did not cover its write path), degraded pools and spaces, storage tiers,
rows of thin spaces not allocated yet (M7).

* Write infrastructure: `WriteAt`, `Overlay` (writes kept in memory, for
  tests), `Pool::open_space_rw` and `SpaceWriter`, which opens a space for
  writing only when its state is fully understood (clean pool, healthy
  space, no cached data) and refuses everything it cannot keep consistent
  yet with the reason; `serve-nbd --rw` and `serve-ublk --rw` (members
  opened read-write and exclusively; NBD with FLUSH and FUA, ublk with a
  volatile cache).
* Simple spaces: `tools/rw-kernel-check.sh` on the Linux VM wrote 512 MiB
  of random fio writes (4 KiB-1 MiB, crc32c verification) through ublk,
  NBD and dm (a read-write table on loop devices), verified them, and
  verified them again after exposing the space read-only; the pattern
  outside stayed intact. `tools/rw-roundtrip.sh`: four ranges written by
  `SpaceWriter` into a copy of `simple2c_26100` were read by Windows
  exactly, the space healthy before and after `Repair-VirtualDisk`.
  `tools/rw-ntfs-check.sh`: GPT and NTFS created on Linux, 3000 seeded
  file operations checked against a model, read back read-only; Windows
  (`tools/work-roundtrip.sh`) found the space healthy, chkdsk clean and all
  files intact, with ntfs-3g and with ntfs3 without truncation. ntfs3 of
  Linux 6.8 corrupts small files truncated to zero (chkdsk reports the
  same four records on a plain disk image); not a matter of the space.
  Evidence in `tests/evidence`, checked by `tests/roundtrip.rs`.
* Mirror spaces (`mirror2_26100`): writes follow the dirty region log model
  (the run listed and flushed before its first write, both copies
  written); fio through ublk and NBD, NTFS with ntfs-3g accepted by
  Windows, a write cut off between the copies attached as healthy; a crash
  replay test checks that copies differ only inside listed runs.
* Single parity (`parity3_26100`): in-place read-modify-write under the
  journal passed fio and NTFS, but a write cut off between data and parity
  was left inconsistent by Windows, even by `Repair-VirtualDisk`: the write
  hole stays open for Windows. Parity writes therefore go through the
  write-back cache as Windows' own do (whole stripes the journal records as
  not consistent go to the space directly); destaging makes partial
  chunks whole in the cache before rewriting whole stripes, so a stripe
  always matches its parity or is held whole by the cache (crash replay
  test over every prefix and random subsets of unflushed writes). The
  first NTFS round trip through the cache lost 7 files: Windows reads a
  wrapped cache log only behind a checkpoint (`SPCHECK`, see the format
  document), which Linux did not write; experiments on Windows pinned
  down the checkpoint format and rule (`rw-cache-log.json`), and the
  cache and journal writers now write checkpoints as Windows does. With
  them, NTFS written through the cache (the log wrapped, 830 chunks
  cached) came back healthy with nothing to repair, chkdsk clean and all
  files intact (`rw-parity-cache-ntfs3g.json`); a destage cut off between
  a stripe's data and parity was read correctly from the cache and
  finished by Windows (`rw-parity-cache-crash.json`). A repair Windows ran
  once turned out to be the test harness mounting the third disk 32 s
  late (the round trips now mount all disks at once and record the
  driver's events).
* `spaces attach --rw` (`tools/rw-attach-check.sh`): fio with verification
  through `/dev/mapper` passed for simple (device-mapper read-write),
  mirror and parity (ublk) copies, and again read-only; mirrors are
  refused by dm read-write and dual parity by `--rw`, with the reason.

### M7 (exit criteria met, 2026-09-29)

* Windows does not clear new slabs (`m7zero`), writes nothing but every
  member's pool database page and the data for an allocation (`m5thin`,
  page-level comparison), and grows the pool database by pages of 64
  formatted slots (`m7grow`, reproduced byte for byte).
* `SpaceWriter` allocates rows of thin spaces with 256 MiB allocation
  units: simple and mirror rows on their first write, parity rows when the
  cache destages them; one database update per row, every member written
  and flushed in turn before the data. Unit tests: the update equals the
  verified model byte for byte, mirror copies land on different disks and
  the run is listed in the dirty region log, parity rows are allocated at
  destage only, and every crash state of an allocating write opens and
  reads the old or the new data.
* Thin simple, mirror and parity spaces filled with NTFS on Linux beyond
  their initial allocation (ntfs-3g, 3000 file operations) were attached
  by Windows as healthy, chkdsk clean, every file intact; fio beyond the
  allocated part passed through ublk and NBD.
* Metadata crash consistency: an allocation cut off between the members'
  database copies leaves a stale copy, which Windows brings up to the
  newest one (as `Pool::update_stale_copies` does when a space is opened
  for writing); replay tests open every crash state of an allocation.
* TRIM: Windows gives back the slabs a retrim covers whole (`m7trim`).
  Discards through ublk and NBD give back rows of thin simple and mirror
  spaces covered whole, in one discard or several since their last write
  (`tools/rw-trim-check.sh`: blkdiscard pieces and ext4 fstrim, e2fsck
  clean); Windows attached a thin simple and a thin mirror pool whose rows
  fstrim gave back from NTFS as healthy, with the same extents. Parity and
  fixed spaces ignore discards.
* Cache destaging on Linux: a cache holding data is destaged when a space
  is opened for writing, and single parity writes destage as the cache
  fills (M6).

### Hardening of the write code (2026-09-29)

* Fuzz targets for every encoder (pool database updates and growth,
  cache and journal logs with checkpoints, dirty region log) check
  decode(encode(x)) against models; `pool_write` opens every space of a
  pool with patched metadata for writing, writes, discards and flushes,
  and checks that no write leaves a member's pool partition (assembly
  after torn or hostile metadata). It found one panic within minutes (a
  user space claiming a range, as tiers do), fixed by refusing such
  spaces; `tests/robustness.rs` does the same on stable in CI.
* Security review of the write paths: `docs/security.md` (threat model,
  findings: checkpoint sizes capped, NBD sockets owner-only, the range
  panic above).
* 24 h of fuzzing without findings (2026-09-29/30). The first hour of a
  first run found two failures (a parity journal checkpoint offset
  overflow, and a read-back check in the `database` harness itself), both
  fixed with tests. The clean run on the fixed code (commit b2e16f1; the
  later commits change no fuzzed code) went in two parts: 5 h 24 min with
  four workers per target until the machine crashed (nothing logged
  before it), then the remaining 18 h 36 min with one worker per target,
  continuing from the corpus. Every target exited cleanly, without
  crashes, timeouts or out-of-memory runs. Inputs run: `database`,
  `record_decode` and `dirty_regions` 2.3 billion each, `partitions` 1.3
  billion, `cache_index` 13 million, `pool_open` 8 million,
  `parity_journal` 4.2 million, `pool_write` 1.8 million.

### M8 (exit criteria met, 2026-09-30: v0.2.0-ready)

* `spaces attach --rw` is documented in the user guide (Writing: what
  each space type does, what is refused and why, guarantees and risks,
  thin allocation and TRIM), the man page, the README and the CHANGELOG
  (0.2.0).
* Backends: device-mapper read-write tables for simple spaces that are
  fully allocated with an empty write-back cache; everything else through
  ublk (or NBD). Mirror spaces never qualify for dm: Windows lists every
  run in the dirty region log before its first write, so each first write
  to a run needs a metadata update.
* Version 0.2.0 in `Cargo.toml` and the Arch package; the Debian package
  builds. Nothing is pushed, tagged or released before Stage 3 is complete
  (decision 2026-09-29).
* With the 24 h fuzzing clean, the state is v0.2.0-ready: Stage 2 (M6-M8)
  is complete.

### Stage 3 (exit criteria met, 2026-10-02: v1.0.0)

* What Windows writes (Goal C, item 1): complete record models checked
  byte for byte on every record of the corpus; `New-StoragePool` (1, 3, 4
  and 8 disks, 4 KiB sectors, 4Kn), `New-VirtualDisk` (simple, mirror,
  three-way mirror, parity with caches of one and two columns, thin simple,
  mirror and parity, over deleted spaces), deletion, renaming, resizing,
  media and usage, adding a disk and removing a retired one are predicted
  byte for byte from Windows' choices (tests `create.rs`, `manage.rs`,
  scenarios `c9*`). Retirement, repair and optimisation move copies; their
  intermediate updates are not visible. Linux moves a copy by writing its
  data into free slabs first and then recording the move in one update.
* Management infrastructure (item 2): `storage_spaces::plan` (steps made
  durable one after the other, printed before anything is written),
  `storage_spaces::ops` (checks, defaults, slab placement), and the
  commands `spaces pool create|rename|repair|optimize|scrub|remove`,
  `spaces space create|delete|rename|resize` and `spaces disk
  add|set|retire|remove` (write only with `--yes`, disks not blank only
  with `--wipe`, members opened exclusively).
* M9 (pool and disk operations): pool create, rename and remove; disk add
  (also to a pool missing a disk, to replace it), media and usage
  settings, retire (data moved off), remove (also of a missing disk after
  a repair). Pools are created with version 28 (Windows 11 24H2, the
  current release); version 29 (Insider 26340) writes other record
  layouts and defaults, so spaces are not created in such pools (their
  records are edited in their own layout).
* M10.1-2 (space operations): create every kind of space Windows creates
  on a pool of that size, delete, rename, resize.
* M10.3 (tiers): tier templates and tiered spaces (an SSD mirror over an HDD
  simple or parity tier) predicted byte for byte (`c10tier`, `c10mapar`,
  `c10tier4`) and planned by `spaces tier create` and `spaces space create
  --tier`; Windows took both kinds created on Linux as healthy, repaired
  and optimized them (`mgmt-tiers.json`).
* M11.1-3 (maintenance): repair rebuilds copies on missing or out-of-date
  disks (mirror copies copied, single parity columns rebuilt by XOR);
  optimize spreads the extents over the disks; scrub compares mirror
  copies and single parity stripes, telling mismatches apart from
  differences where writes were under way (extent runs the dirty region
  log lists, stripes the parity journal does not list as consistent) and
  makes them agree on request.
* Crash replays (`ops.rs`): creating a space, retiring a disk and
  repairing, cut at every step, leave a pool whose data reads back.
* Windows round trips (`roundtrip.rs`, evidence `mgmt-*.json`):
  * `c9lnx`, created on Linux: healthy, nothing to repair, every pattern
    read back, the extents Windows lists equal to the database written on
    Linux;
  * `lifecycle`, every operation on one pool: healthy, Repair and
    Optimize complete, patterns read back after both;
  * corpus pools `parity3_26100` and `mirror2_26100` changed on Linux (disk
    added, space created and grown, first disk retired and removed):
    healthy, Optimize completes, patterns read back;
  * a failed disk replaced and a damaged mirror copy scrubbed on Linux
    (`mgmt-replace.json`): healthy, Repair and Optimize complete, patterns
    read back;
  * pools cut after a step of creating a space and of adding a disk:
    healthy after Repair, patterns read back. Cutting a disk addition
    after its first database update, before the new disk had its copy of
    the metadata space, left that disk lost on Windows; the copy is now
    written first.
* New spaces pass the Stage 2 write checks (tools/linux-created-checks.sh,
  evidence `linux-created-checks.json`): fio with crc32c verification
  through ublk, nbd and dm on simple, mirror, parity and thin spaces
  created on Linux, and NTFS through ntfs-3g on the mirror and parity
  ones; Windows then attached both NTFS pools as healthy, chkdsk clean,
  every file intact.
* Security review of management (docs/security.md): members are now locked
  exclusively also as image files and through symlinks; sizes are rounded
  with checked arithmetic and bounded to 2^32 slabs.
* Fuzz targets for the management code: `records` (the complete record
  models and the disk header encode back to what they decoded), `create`
  (pools and spaces from fuzzed parameters open clean, pass the checks and
  read back writes at both ends of every new space) and `manage` (every
  planner on patched fixture pools; every action writes only inside the
  members' pool partitions or their partition tables).
* M11.4: `spaces pool health` shows the pool, its disks and spaces in
  Windows' terms (rules in docs/storage-spaces-format.md, "Health"),
  checked against every Windows state on record and against Windows' view
  of a four-disk pool, created on Linux and by Windows, losing each disk
  and two of them while in use (`health-drop.json`: 12 cases, every state
  predicted; the two pools behaved alike). Asking Windows about more
  states with disks absent failed: the experiment (tools/health-states.sh,
  a Linux-created pool of four disks with simple, mirror and parity spaces)
  bugchecked Windows (0x50 in spaceport.sys) when the pool arrived without
  one disk; the same layout created by Windows (scenario c11ctl) crashed
  it the same way, at the same address (evidence
  `windows-absent-disk-bugcheck.json`). With the user's consent the
  experiments then went on with disks detached while in use, which did
  not crash Windows (`health-drop.json`).
* 24 h of fuzzing per target with one worker each at nice 19
  (`tools/fuzz-long.sh`): the nine parser and writer targets from
  2026-09-30 23:28 on e5abb35 (1 to 5 billion runs each; later changes did
  not touch their code); `create` from 2026-10-01 08:13 on 6f188b2 (1.55
  million runs) and `manage` from 2026-10-01 20:59 on b9ac212 (7.37 million
  runs, its code since unchanged): no findings. The earlier runs of these
  two found a size overflow when counting free slabs (extents beyond their
  disk) and an unchecked partition table rewrite, and an error in the
  `create` harness; all fixed, with tests (docs/security.md).
* Release `v1.0.0` (2026-10-02): version 1.0.0, user guide, man page,
  README, CHANGELOG, Debian and Arch packages, a static x86_64 musl build;
  pushed to GitHub with the tags v0.1.0 (the last read-only commit, before
  write support), v0.2.0 (the end of Stage 2) and v1.0.0, and a GitHub
  release for v1.0.0. The crates stay off crates.io until decided
  otherwise.

### Track B (from 2026-10-02)

* Corpus: Windows 11 Pro formats ReFS only as a Dev Drive (at least 50 GB),
  so `tools/vm/New-RefsVolume.ps1` makes a 52 GiB dynamic VHDX (a few
  hundred MB on disk) with a Dev Drive of ReFS 3.14, writes a known tree
  (names, sizes around cluster and page boundaries, fragmented, sparse and
  64 MiB files, a directory of 5000 files, named streams, hard and symbolic
  links, a junction, attributes, set times, block-cloned copies, deleted
  and renamed files), flushes, reattaches and records Windows' listing
  (`manifest.json`); `tools/fetch-refs.sh` copies it as a sparse raw image.
  Eight volumes: 4 KiB and 64 KiB clusters, SHA-256 metadata checksums,
  integrity streams on 4 KiB and 64 KiB clusters, an empty volume, a
  volume inside a mirror space, and the scenario `features` (stream
  snapshots, deduplicated files, compressible files).
* B1 (read-only library, `crates/refs`): boot sector, superblock and
  checkpoint (with their own checksums), page references with CRC-64 and
  SHA-256 checked on every page read, container table and virtual cluster
  translation, object table, B+-trees of any depth, directories (embedded
  records and index entries, hard links), file records, inline and extent
  data, sparse runs, named streams (inline and in stream sets), reparse
  points (symbolic links, junctions). Every file, directory and stream of
  the corpus reads back as Windows listed it: kind, attributes, times,
  sizes, link targets and the SHA-256 of every data and stream
  (`tests/corpus.rs`). Fixtures with the metadata and small files of each
  volume run in CI (`tests/fixtures.rs`, made by `refs fixture`); the
  parsers are fuzzed (`refs_parsers`, `refs_volume`; fuzzing builds accept
  every checksum so that patched pages reach the parsers). The format is in
  `docs/refs-format.md`. Open: ReFS 3.4 to 3.13 (no images yet: they need
  Windows 10 or Server 2016 to 2022).
* B2 (in progress): `refs mount` (FUSE, read-only; streams as extended
  attributes, links and junctions as symbolic links, hard links as one
  inode, directory listings cached), checked against Windows' listing of
  every corpus volume by `tools/refs-mount-check.sh`; `refs --space` reads
  ReFS inside a space of a pool. Block-cloned and deduplicated files
  (`refsutil dedup`) read correctly: their extents name shared clusters.
  Stream snapshots (`refsutil streamsnapshot`): a stream is a chain of
  data levels, each mapping what was written while it was live; the live
  data and every snapshot read back with Windows' hashes (scenario
  `features`, volume `r314feat`); `refs cat --snapshot`. Compression:
  `refsutil compression` and `Start-ReFSDedupJob -CompressionFormat` on
  Windows 11 26340 deduplicate but compress nothing (0 compressible
  clusters, also with 300 MB of text), so compressed samples need Windows
  Server 2025. Integrity streams: the data checksums (CRC32-C per 4 KiB
  cluster, CRC-64 per 16 KiB of 64 KiB clusters; volume `r314integ64k`)
  are checked on every read, damaged data is refused. ReFS inside a space:
  a Dev Drive in a two-way mirror space of two disks (`-PoolDisks 2
  -Resiliency Mirror`, volume `r314mirror`) reads back as Windows listed
  it, through the library and the mount (`refs --space`). Open:
  compression.
* B4 (research): write experiments on a small volume (scenario `small`,
  `tools/vm/Invoke-RefsSteps.ps1`, `tools/refs-diff.py`, `refs map` and
  `refs tree`) show what one change costs: copy-on-write pages up to both
  object tables, both allocators, internal objects, MLog records and both
  checkpoints (docs/refs-format.md, "Writing"). Next: the meaning of the
  allocator, object table and checkpoint fields, the MLog layout and
  whether Windows replays it over a checkpoint written without records.
  First write accepted: a file's time changed in place under the current
  checkpoint, every checksum up to the checkpoint fixed; Windows attached
  the volume as healthy, showed the time and kept it. Then the same with
  copy on write, as Windows commits (pages from the medium or container
  allocator, old clusters freed, a new checkpoint in the older slot):
  healthy, `refsutil leak` and `triage` as on the untouched volume, and
  Windows committed on top. Design of the writer (next): a transaction
  over the current checkpoint's trees in `refs::write` that copies every
  changed page (rows changed in place, inserted, removed; nodes split and
  merged), allocates and frees clusters in the allocators until nothing
  changes any more, and commits by writing the checkpoint cluster; first
  operations: times and attributes, data overwritten in allocated
  clusters (integrity checksums updated), then creating, renaming and
  deleting files and directories and growing files. Every operation is
  checked as above on Windows, and crash states (every subset of the
  page writes before the checkpoint) must read as the old volume.
  Done: `refs::write` (transactions, allocation to a fixed point with the
  clusters freed by the transaction kept until the commit, references
  child first, the checkpoint last between flushes) and `refs set` (times
  and attributes of files whose record is in their directory entry,
  `--yes`). Tests on the fixtures (4 KiB and 64 KiB clusters): only the
  rows asked for change, every page the new checkpoint reaches is used in
  its allocator (as on every Windows volume of the fixtures), nothing the
  old checkpoint reaches is written before the new checkpoint, the
  checkpoints alternate. On Windows: a volume changed by `refs set`
  attached healthy with the new times and attributes, `refsutil leak` and
  `triage /g` as on the untouched volume, no ReFS events. `refs
  overwrite`: data overwritten where it is (as Windows does for streams
  without integrity), inline data inside the record, then the times
  committed; Windows read the expected bytes. `refs create`: a file with
  up to 1 KiB of inline data (a file id row and a name row with its
  record inserted in key order, the directory's own times; the security
  descriptor reference of a neighbouring file); Windows read the files,
  showed the inherited permissions, appended to one, deleted another.
  `refs delete` and `refs rename` (within a directory) for files whose
  data is in their record; pages are compacted when the end of the row
  area is full. Windows refused the first deletes: it walks a page's rows
  by their sizes, so removed rows stay as tombstones (flag 4), as Windows
  leaves them; the tests now check every page that way. Limits for now:
  directories of one page, ASCII names, data in the record. `refs create`
  of files in extents (up to 64 MiB): data clusters from the medium
  allocator's row that holds nearby file data, after the data already
  there, in runs split at the file's clusters 1, 64 and multiples of 256
  as Windows splits them; Windows read the files, appended to one and
  deleted another. The first version wrote one run; Windows' append then
  bug-checked the VM (0x149, the run not found; analysed with the Windows
  debugger on the test VM): a run must not cross the file's cluster 64.
  Deleting and renaming files in extents: the record moves as it is; a
  deleted file's data clusters are freed (refused while the block
  reference count table, root 6, has rows: clusters may be shared by
  clones or deduplication). On Windows: healthy, `refsutil leak` as on
  the untouched volume, and Windows then appended to and deleted our
  files. Next: growing files, directories, splitting pages.

## Test infrastructure (continuous, feeds every stage)

T1. **Windows 11 only.** All pools are created by Windows 11 test VMs: the
    first corpus by Insider build 26340 (pool version 29, DESKTOP-ELS4LDK),
    since 2026-09-27 by Windows 11 24H2 build 26100 (pool version 28,
    DESKTOP-BQ2J4NS, `*_26100` pools regenerate the basic configurations).
    When a VM moves to another Windows 11 build, regenerate the corpus and
    diff the metadata.

T2. **Corpus.** Large images stay out of git (`testdata/pools/`). Add a
    `spaces fixture` command that extracts only metadata regions (headers,
    databases, cache headers and slots, a few data blocks) into small sparse
    files committed under `tests/fixtures/`, so CI runs the metadata tests
    without the 19 GB corpus.

T3. **Configuration matrix** to cover: columns 1-8, interleave 16 KiB-1 MiB,
    2/3-way mirror, single/dual parity, fixed/thin, tiered (SSD/HDD media type
    set on VHDX), several spaces per pool, deleted and re-created spaces
    (fragmented allocation), extended spaces, pools with retired / removed /
    replaced disks, 512e and 4Kn member disks, pools upgraded from older
    versions, full and nearly-full pools.

T4. **Linux test VM.** Ubuntu 22.04 VM `codex@192.168.189.142` (kernel 6.8,
    shared with the LinuxReflect project; `tools/linux-vm.sh`). It has the
    `ublk_drv`, `nbd`, `dm-raid` and `ntfs3` modules, `dmsetup`, `nbd-client`,
    `ntfs-3g`, fio and cargo. The corpus lives on a dedicated 128 GB data
    disk mounted at `/srv/spaces`. WSL lacks ublk, so ublk tests run only on
    this VM.

T5. **CI.** GitHub Actions: fmt, clippy, unit tests, fixture tests, fuzz
    smoke run, MSRV build. Corpus and VM tests run locally or on a self-hosted
    runner.

T6. **Fuzzing.** `cargo fuzz` targets for record decoding, database assembly,
    cache index loading and whole-pool open on mutated fixtures.

## Stage 1: read-only mounting as a normal disk

### M1: format coverage for reading

1. Dual parity: generate 5-8 column dual-parity spaces, derive the layout
   (expected Reed-Solomon / LRC variant), implement read and 1-2 disk rebuild.
2. Storage tiers and mirror-accelerated parity: tiered spaces (several tier
   children per space), per-tier layouts, virtual-to-tier mapping.
3. Layout versions: reject SPACEDB/record layouts other than the Windows 11
   one with a clear message (older Windows is out of scope).
4. Remaining record fields needed for correctness: provisioning type, extent
   state (active / stale / needs regeneration), disk state (retired, missing,
   removed), space state (detached, read-only, degraded), usage flags.
5. Checksums of SPACEDB, SDBC and SDBB; pick the newest *valid* database copy
   and report torn copies.
6. Hidden children of mirror/parity spaces (roles 6 and 0x0a): identify dirty
   region tracking and the parity journal. For reads: detect unapplied journal
   entries or dirty regions after a crash and either replay them in memory or
   refuse the space with a clear message.
7. SPCACHE completeness: slot type 1, head/tail/sequence rules, partial
   validity bitmaps, caches of mirror/parity spaces with dirty data, wrapped
   logs. Construct cases on Windows (fill cache, then detach / hard power-off
   the VM).
8. Multi-pool handling: devices of several pools passed together, disks with
   the same pool GUID but stale databases, pools with missing disks
   (degraded) and quorum rules (refuse when metadata quorum is lost unless
   forced).

Exit: every configuration of T3 created by Windows 11 passes metadata and
pattern tests; crash-state cases are either read correctly or refused.

### M2: block device exposure

The library already offers `SpaceReader`. Exposure backends, all read-only:

1. **device-mapper** (kernel speed, no daemon): generate tables with
   `linear`/`striped` for simple, `raid1` or `mirror` for mirror and
   `raid5_ls` via `dm-raid` for single parity; created read-only
   (`dmsetup create --readonly`). Only used when the space is representable:
   cache empty for the whole space, no pending journal, known layout.
2. **ublk** (`libublk` crate, Linux >= 6.0): userspace block device serving
   `SpaceReader` directly; covers every case including the cache overlay,
   degraded parity and dual parity. Multi-queue, io_uring based.
3. **NBD** fallback (in-process server on a unix socket plus `nbd-client`
   or netlink setup) for kernels without ublk, including WSL.
4. **FUSE file + loop** last-resort fallback (single file exported by FUSE,
   attached with `losetup -r`).
5. Backend selection: `auto` prefers dm, then ublk, then NBD, then FUSE;
   `--backend` overrides.

Exit: for every corpus pool each backend passes the pattern check through
the block device (`spaces check-pattern` equivalent run with plain reads),
and `fio --verify` style random reads succeed.

### M3: assembly and lifecycle (the "normal disk" experience)

1. `spaces scan`: find pool members among block devices (GPT type +
   SPACEDB), group them by pool, report completeness.
2. `spaces attach <pool|space>` / `spaces detach`: assemble and expose spaces;
   stable names `/dev/mapper/spaces-<pool>-<space>` or ublk equivalents plus
   `/dev/disk/by-id/spaces-<space-guid>` symlinks; partitions inside are
   scanned by the kernel so `mount -o ro /dev/...p2` works.
3. Opening members with `O_EXCL` so mdadm, LVM or a second instance cannot
   use them concurrently; refuse members that are in use.
4. udev rules + systemd template unit for incremental auto-assembly when all
   members (or enough for degraded mode, opt-in) appear; clean detach on
   removal; `spaces status`.
5. Degraded assembly policy: explicit flag, clear warnings, never silently
   hide a missing column.
6. Performance: large aligned reads, per-device read-ahead, parallel column
   reads; target >= 90 % of member throughput for dm, >= 70 % for ublk on
   sequential reads.

Exit: the `test_ubuntu` pool disks moved to the Linux VM auto-assemble at
boot and its NTFS volume mounts read-only with `ntfs3`; the same works for
every corpus pool containing NTFS, and for a 3+ disk NTFS data pool
created on a physical Windows machine. Unplugging a member of a mirror keeps
the mount working.

### M4: stage 1 release

1. Packaging: `cargo install`, Debian/Ubuntu and Arch packages, static
   musl binary; man pages for `spaces(8)`.
2. User documentation: supported configurations table, recovery guide
   (degraded pools, stale disks), troubleshooting.
3. Security review of the parsing code, fuzzing run of at least 24 h without
   findings, no `unsafe` outside the ublk/NBD glue.
4. Tag `v0.1.0` and a GitHub release. The crates are published on
   crates.io only once a stable release is confirmed.

## Stage 2: write support

Writes are enabled only with an explicit `--rw` and only for spaces whose
complete state is understood. Order of work goes from "no metadata change" to
"metadata change".

### M5: understanding what Windows expects

Before writing anything, reverse-engineer from Windows behaviour (VM
experiments with I/O tracing and before/after diffs of metadata):

1. Dirty region tracking: format, when regions are marked and cleared.
2. Parity journal / write-back cache write path: how Windows logs a write,
   commits it and destages it; log head/tail advancement.
3. Database update protocol: which copies are written in which order, how
   sequence numbers and checksums change, how Windows chooses between
   diverging copies after a crash.
4. Extent and space health fields: how a copy is marked stale when a write
   misses a disk, how regeneration clears it.
5. Slab allocation policy for thin spaces and cache destaging.

Exit: a written specification for each item, validated by predicting the
metadata Windows writes in scripted scenarios.

### M6: in-place writes to allocated regions

1. Simple spaces: direct writes (dm `linear`/`striped` read-write, or ublk).
2. Mirror: write all copies, DRT marking before the write, clearing after
   flush; degraded writes mark missing copies stale in metadata.
3. Single parity: full-stripe writes and read-modify-write, protected by the
   journal (or DRT) exactly as Windows does, so the write hole is closed.
4. Writes into regions currently held in SPCACHE: go through the cache log
   (append SPSLOT + data) or destage first; never leave a stale cache entry
   pointing over newer base data.
5. Flush/FUA semantics mapped to member device flushes in the right order.

Exit: randomized write workloads (fio with verify, filesystem stress on NTFS
inside) followed by attaching to Windows: space Healthy, `Repair-VirtualDisk`
has nothing to do, `chkdsk` clean, data verified. Fault injection with
`dm-flakey`/killed ublk daemon/VM power-off at random points: Windows repairs
or accepts the pool and no acknowledged write is lost.

### M7: writes that change metadata

1. Thin provisioning: allocate slabs on first write to an unallocated row
   (directly or via the cache, matching Windows), update the database on all
   members with correct sequence and checksums.
2. TRIM/discard: optional slab reclamation on thin spaces, matching Windows'
   own behaviour.
3. Cache destaging on Linux (so a pool can be handed back to Windows with an
   empty cache, and so dm fast path becomes possible after destage).
4. Crash consistency of the metadata engine: write-ahead ordering, recovery
   on next assembly, tests with power cuts between every metadata write.

Exit: thin spaces filled from Linux beyond their initial allocation pass the
M6 verification in Windows, including crash tests.

### M8: stage 2 release

Read-write mode in `spaces attach --rw`, dm read-write tables for simple and
mirror where no DRT update is needed (otherwise ublk), documentation of
guarantees and risks, `v0.2.0`.

## Stage 3: full pool management

### M9: pool and disk operations

1. `spaces pool create` on blank disks: GPT, partition, SPACEDB, database,
   metadata space, pool version chosen to match a target Windows release.
2. Add disk, retire disk, remove disk (with evacuation), replace disk, rename
   pool, set read-only, delete pool (wipe metadata).
3. Media type and usage settings, enclosure awareness off.

### M10: space operations

1. Create space: simple / mirror / parity, fixed / thin, columns, interleave,
   size; hidden children (cache, DRT, journal) created as Windows would.
2. Extend space (add rows), delete space (free slabs), rename.
3. Tiered spaces and mirror-accelerated parity (after M1.2).

### M11: maintenance

1. Repair / regeneration of stale or missing copies onto other disks.
2. Rebalance / optimize (move slabs to spread load after adding disks).
3. Scrub: verify mirror copies and parity, report and optionally repair.
4. Pool health report compatible with Windows states.

Exit for stage 3: pools created and modified only on Linux are imported by
Windows 11 without warnings, show the expected properties
in `Get-StoragePool` / `Get-VirtualDisk`, and survive Windows' own repair and
optimize operations; and the reverse (Windows-created pools managed on Linux)
passes the same checks. Release `v1.0.0`.

### Optional later work

* udisks2 / libblockdev plugin so desktop tools see pools.
* A native kernel dm target or kernel driver, only if ublk performance is
  insufficient.
* Storage Spaces Direct (clustered) pools are out of scope.

## Track B: ReFS

Independent of Storage Spaces; works on plain partitions, Dev Drives, images
and on spaces exposed by stage 1.

1. B1 read-only library for ReFS 3.x (3.4 - 3.14): boot sector, superblock,
   checkpoints, container table, object table, Minstore B+-trees, directories,
   files, attributes, sparse data, ADS, reparse points, checksums. Corpus
   generated on the VM (Dev Drive / VHDX formatted ReFS) with known trees.
2. B2 FUSE mount (`fuser`), read-only; compression (LZ4/ZSTD), block clones,
   integrity streams, snapshots.
3. B3 ReFS 1.x/2.x read support.
4. B4 write support: copy-on-write B+-tree updates, allocators, refcounts,
   checkpoints, logging; validated by Windows `chkdsk` / `refsutil`.

## Risks

* Undocumented formats can change between Windows builds: keep the Windows
  matrix current and make every parser reject unknown versions.
* Write support risks data loss: gated behind `--rw`, extensive fault
  injection, and a conservative "refuse on unknown state" policy.
* Some configurations need hardware or Windows editions that are hard to
  reproduce (large column counts, Server-only features): use VHDX and
  evaluation media, document what was not verified.
* Legal: clean-room reverse engineering from Windows behaviour and public
  documentation only; no Microsoft code or symbols beyond public APIs.
