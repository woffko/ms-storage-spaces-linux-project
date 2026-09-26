# Project plan

Status as of 2026-09-26. The plan is split into three stages with checkable
exit criteria. ReFS is a separate track (last section) that shares the test
infrastructure but not the milestones.

## Goal

Make Microsoft Storage Spaces pools fully usable from Linux:

1. **Stage 1 - read-only:** any pool created by Windows 11 appears on Linux
   as ordinary read-only block devices (one per virtual disk) that can be
   partition-scanned and mounted like any disk.

Scope decision (2026-09-26): only Windows 11 pools (current layout, pool
version 29) are targeted. Older Windows releases, Windows Server and pools
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

## Progress log (Stage 1)

Evidence is recorded here as milestones advance; commands refer to the tools
in this repository.

### M1 (in progress)

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
* Open: the Q code of dual parity (two-column rebuild), extent/disk health
  states, quorum rules.

### M2 (backends done, corpus matrix pending)

* dm (`dm-table`), ublk (`serve-ublk`), NBD (`serve-nbd`) and FUSE
  (`serve-fuse`) all expose the real `test_ubuntu` pool: data equal to the
  exported image (full SHA-256 for dm, sampled ranges for the others), writes
  refused, NTFS mounted read-only through each. ublk and NBD expose the 4 KiB
  logical sector size; dm inherits the members' 512.
* Pending: `tools/backend-matrix.sh` over the whole corpus on the Linux VM.

### M3 (in progress)

* `spaces scan/attach/detach/status`: stable `/dev/mapper/ss-<pool>-<space>`
  and `-p<N>` partition devices from our own GPT parser (Windows omitted the
  protective MBR inside `test_ubuntu`, so the kernel would not see it).
* All backends attach and detach cleanly on the real pool; `auto` picks dm
  for simple spaces with matching sectors, ublk otherwise.
* udev rule + `storage-spaces-attach.service` attach the pool when its
  members appear (`udevadm trigger --action=add` test on the Linux VM).
* Pending: reboot test on the Linux VM, unplug-a-mirror-member test on the
  VM, performance targets.

## Test infrastructure (continuous, feeds every stage)

T1. **Windows 11 only.** All pools are created by the Windows 11 test VM
    (currently Insider build 26340, pool version 29). When that VM moves to
    a newer Windows 11 build, regenerate the corpus and diff the metadata.

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

T4. **Linux test VM.** Ubuntu 22.04 VM `codex@192.168.189.144` (kernel 6.8,
    shared with the LinuxReflect project; `tools/linux-vm.sh`). It has the
    `ublk_drv`, `nbd`, `dm-raid` and `ntfs3` modules, `dmsetup`, `nbd-client`,
    `ntfs-3g` and cargo; `fio` still has to be installed. The corpus lives on
    a dedicated 128 GB data disk mounted at `/srv/spaces`. WSL lacks ublk, so ublk tests run only on this VM. Next step:
    attach the `test_ubuntu` pool disks of the Windows VM to it (VMware
    configuration on the host) as the first real-disk case.

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
4. Tag `v0.1.0`, publish crates.

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
