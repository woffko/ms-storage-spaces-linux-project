# Changelog

## 1.1.1 (unreleased)

### Fixes

* Spaces with 4 KiB logical sectors and a write-back cache (Windows uses
  one for spaces on SSDs) read stale data where the cache held only part
  of a chunk: the cache counts its runs of valid sectors in the space's
  logical sectors, and 1.1.0 took them as 512-byte sectors. It showed as
  "Alternate GPT is invalid" when such a space was attached, and data
  written last in Windows could read as older data. Every byte of such a
  space now reads as Windows reads it (pool `wc4k`, checked against
  Windows' own reads). Writing to such spaces made the same mistake; a
  cache 1.1.0 wrote that way is now refused instead of misread.
* `mount -t ReFS`, fstab and udisks2 mounts of ReFS volumes did not show
  on systemd 254 and later (`mount.ReFS` gave up after 10 s, udisks2 said
  "Unknown error"): there, the private network of the unit `refs` runs in
  brought a private mount namespace along. The unit keeps the host's
  mounts now (`PrivateMounts=no`).
* `mount.ReFS` gave up after 10 s, without a word, on volumes that take
  longer to open (a 10.5 TiB volume on two hard disks after a restart).
  `refs mount` now reports its progress and the outcome in a status file,
  and `mount.ReFS` waits for that instead of a fixed time: it returns once
  the mount is in place, or says why it is not, and shows on a terminal
  how much has been read. `MOUNT_REFS_TIMEOUT` sets a limit.
* Mounting a ReFS volume on a device with 4 KiB logical sectors as root
  (`mount -t ReFS`, udisks2, `refs mount` of a block device; every ReFS
  volume in a space with 4 KiB sectors) failed with "Invalid argument":
  the mount's block size was FUSE's default of 512 bytes, smaller than
  the device's sectors. It is the device's sector size now.
* On Ubuntu releases that confine `fusermount3` with AppArmor (25.10 and
  later), root's mounts of ReFS volumes were refused ("Permission
  denied"): the profile knows neither the mount type `fuseblk.refs` nor
  udisks2's mount points under `/run/media`. As root, `refs` now makes the
  mount itself (mount(2), with the /dev/fuse connection handed to the FUSE
  session) and no longer runs `fusermount3`; the mount goes away when
  `refs` ends, also when it is killed (a watcher process unmounts it).
* Installing the package or running `contrib/install.sh` left the pools
  whose disks were present unattached until a restart: the udev rule only
  acts on disks that appear. Both start the attach once now, and the
  package loads `nbd` as well.
* Removing the package detached every attached space, also those another
  installation's `spaces` serves (`contrib/install.sh`'s); it detaches
  only those its own `spaces` serves now.
* Without root, `spaces status` said "no spaces attached" when spaces were
  attached (the attach unit's umask made `/run/storage-spaces` 0700), and
  `spaces scan` said "no Storage Spaces pool members found" when it could
  not open the disks. The state directory is readable by everyone now
  (0755, its files 0644; the NBD sockets stay 0600), an unreadable one is
  an error that says to run as root, and `scan` says how many devices it
  could not open.

## 1.1.0 (2026-10-05)

ReFS: `refs` reads, mounts, checks and (3.14) writes ReFS volumes, checked
against what Windows makes. Storage Spaces itself is unchanged apart from
the hardening below.

### ReFS

* `refs` (new command, `man refs`): `ls`, `cat`, `stat`, `mount` (FUSE,
  read-only or `--rw`), `check`, `info`, and the writing commands `set`,
  `overwrite`, `write`, `create`, `rename`, `move`, `link`, `clone`,
  `delete`, `mkdir` (each only with `--yes`). It works on volumes on disks,
  images and spaces of pools (`--space`).
* Reads: files (resident, in extents, sparse, block-cloned, deduplicated,
  compressed with LZ4 or ZSTD), directories of any size, named streams,
  stream snapshots, hard links, symbolic links and junctions, attributes
  and times; integrity streams checked on every read; 4 KiB and 64 KiB
  clusters, CRC-64 and SHA-256 metadata checksums.
* Versions, each verified against volumes the Windows that makes them
  created (every file as Windows lists it, `refs check` clean, fixtures in
  CI): 3.14 (Windows 11), 3.7 (Server 2022), 3.4 (Server 2019), 3.1
  (Server 2016), 1.2 (Server 2012 R2). Not verified, for lack of Windows
  to make them: 1.1, 2.x, 3.2, 3.3, 3.5, 3.6, 3.8 to 3.13.
* Writes, 3.14 only (every change checked by Windows: `refsutil leak` and
  `triage`, and Windows reading and changing the result): files up to 64
  GiB changed in place as Windows does, created, renamed, moved, linked,
  deleted (block-cloned and deduplicated files too), directories, times,
  attributes, named streams, integrity streams, files with stream
  snapshots, block clones (also whole-file copies through the mount),
  compressed files (written whole into ordinary clusters). Other versions
  are refused.
* `refs mount`, also for udisks2 and `mount -t ReFS` through
  `contrib/mount.refs`; `refs check` checks pages, allocators and shared
  clusters (not the allocators of 1.x).
* The format is described in `docs/refs-format.md`, as far as it was found.

### Hardening (code audit, `docs/security.md`)

* The attach unit, the servers it starts and `mount.ReFS` run with limits
  (no new privileges, read-only file system, no namespaces or network,
  fewer capabilities): `systemd-analyze security` 9.5 UNSAFE down to 5.1
  MEDIUM; on a systemd that does not know a property the servers start
  without limits, with a warning.
* Hostile volumes: bounds on ReFS 1.x run rows and cluster sizes, on the
  unit size of compressed containers, on what `refs write --append` holds
  in memory and on the chunk size of a pool's write-back cache; the FUSE
  mount forgets inodes and bounds its caches, and survives a panic in one
  request.
* Release builds check integer overflow (no measurable cost); the four
  crates forbid `unsafe` code; CI runs `cargo deny` and `cargo audit` on
  every push and every week.
* New tests: damaged volumes of every ReFS version, random bytes to the
  parsers. Fuzzing: 4-hour runs of the ReFS targets after each change, all
  clean; the 24 h run of every target was not repeated for this release
  (the Storage Spaces parsers changed in two lines).

### Installing

* `contrib/uninstall.sh`; the Installing section of the user guide says
  what is needed and what each way of installing gives.
* The Debian package and `contrib/install.sh` include `refs`,
  `mount.ReFS` and the man pages; the package recommends `fuse3`.
* Assets: static `spaces` and `refs` (x86_64, musl), the `.deb`, and
  `SHA256SUMS`.

## 1.0.0 (2026-10-02)

Pool management, the way Windows 11 24H2 does it: Windows takes pools
created and changed on Linux as healthy, repairs and optimizes them.

### Management

* `spaces pool create`: a pool (version 28) of blank disks (`--wipe` for
  others), 512-byte or 4 KiB logical sectors, 512e and 4Kn disks; the
  partition tables, disk headers and pool database Windows would write.
* `spaces space create`: simple, two- and three-way mirror and single
  parity spaces, fixed and thin, with Windows' defaults (columns,
  interleave, the parity space's write-back cache and journal, the mirror's
  dirty region log); `space delete`, `rename`, `resize` (growing).
* `spaces disk add` (also to a pool missing a disk), `disk set` (media
  type, usage), `disk retire` (everything moved off the disk), `disk
  remove` (a retired disk, or a missing one after a repair).
* `spaces pool rename`, `pool remove`, `pool repair` (copies on missing or
  out-of-date disks rebuilt elsewhere: mirror copies copied, parity columns
  recomputed), `pool optimize` (extents spread over the disks), `pool
  scrub` (mirror copies and parity compared, `--repair` makes them agree),
  `pool health` (HealthStatus and OperationalStatus as Windows would show
  them).
* Every command prints its plan and writes only with `--yes`, in steps
  that each end with a flush; data is copied before the metadata points at
  it, so a crash at any point leaves a pool that opens (replayed after
  every step in the tests; Windows took pools cut short this way as
  healthy).
* Member disks are opened exclusively for writing: O_EXCL on block devices
  (also through symlinks), an exclusive lock on image files.
* Checked byte for byte against what Windows writes for every operation it
  exposes, by Windows round trips of pools created and changed on Linux,
  and by fio and NTFS write checks on spaces created on Linux.
* `spaces tier create` (tier templates, `New-StorageTier`) and `spaces space
  create --tier`: tiered spaces of an SSD mirror over an HDD simple or
  parity tier (mirror-accelerated parity), with their cache, dirty region
  log and parity journal on the SSD disks.
* Not supported: creating dual parity spaces; writing tiered spaces; new
  spaces in pools of version 29 (Insider builds).

### Known issue in Windows

* Windows 11 24H2 bugchecks (0x50 in spaceport.sys) when a pool of four
  disks with simple, mirror and parity spaces arrives with one disk absent,
  whether Linux or Windows created the pool.

## 0.2.0 (2026-09-30)

Writing, for spaces whose state is fully understood.

### Writing

* `spaces attach --rw` (and `serve-ublk --rw`, `serve-nbd --rw`): simple,
  two- and three-way mirror and single parity spaces, written the way
  Windows writes them, so that Windows reads the result back (healthy,
  chkdsk clean, every file intact in the round trips of the test suite).
  Refused, with the reason: pools that are not clean (missing disks, stale
  copies, diverging metadata), degraded spaces, dual parity, storage tiers,
  write-back caches whose copies disagree.
* Mirror spaces: the dirty region log lists an extent run, durably, before
  its first write.
* Single parity spaces: writes go through the write-back cache as Windows'
  own do (whole stripes the parity journal records as not consistent go to
  the space directly); destaging rewrites whole stripes under the journal,
  so a stripe always matches its parity or is held whole by the cache: the
  write hole stays closed.
* Thin spaces (256 MiB allocation units) allocate rows as Windows does:
  one pool database update per row, on every member before the data;
  parity rows when the cache destages them. The pool database grows by
  pages of 64 slots when full.
* Discards (TRIM) give rows of thin simple and mirror spaces that they
  cover whole back to the pool, as Windows does (ublk and NBD).
* A pool database copy left stale by an interrupted update is brought up
  to the newest copy when a space is opened for writing, as Windows does.
* The cache and journal logs wrap behind checkpoints as Windows expects;
  a cache holding data when a space is opened for writing is destaged first.
* Writes are durable after a flush (sync, FUA) or a clean detach; the
  serving processes flush when stopped.
* Checked by crash replays of every flush point and of unordered writes,
  fio with verification through ublk, NBD and device-mapper, and NTFS
  written on Linux and verified by Windows.
* The write path is fuzzed (`pool_write`): whatever the metadata, a space
  is written only if it is understood, and writes stay inside the pool
  partitions of its members.

### Reading

* The write-back cache and parity journal are read as Windows reads them:
  from the newest checkpoint on (a wrapped log is no longer read in full).
* `spaces dump` shows checkpoints.

## 0.1.0 (2026-09-28)

First release: read-only access to Microsoft Storage Spaces pools created by
Windows 11 (24H2, pool version 28, and Insider builds, pool version 29).

### Reading

* Simple, two- and three-way mirror, single parity, dual parity (Reed-Solomon
  over GF(16) for 7 to 10 columns, local reconstruction code for 11 and more
  columns), storage tiers and mirror-accelerated parity.
* Fixed and thin provisioning, several spaces per pool, fragmented and
  extended spaces, 512-byte and 4 KiB logical sectors, 512e and 4Kn member
  disks, interleave 16 KiB to 1 MiB.
* The per-space write-back cache, including data not yet moved to the space.
* Missing or failing disks as far as the redundancy allows (any two for dual
  parity); out-of-date mirror copies are never read; quorum check.
* Unclean shutdowns: parity stripes the journal does not mark consistent
  are checked, and so are the copies of mirror extent runs the dirty region
  log lists (those written since the space was last disconnected); stripes
  whose parity does not match and rows whose copies differ are refused
  unless `--unclean-parity data` is given.

### Block devices

* `spaces scan`, `attach`, `detach`, `status`, with device-mapper, ublk, NBD
  and FUSE backends; `/dev/mapper/ss-<pool>-<space>` and one device per
  partition.
* udev rule and systemd unit: pools are attached at boot and when their disks
  appear.
* `spaces info`, `extents`, `export`, `dm-table` for inspecting pools without
  attaching them; `spaces dump` and `diff` print the metadata one fact per
  line and compare two states of a pool.

### Packaging

* Debian package script, Arch Linux PKGBUILD, `contrib/install.sh`, static
  musl build in CI, manual page `spaces(8)`.

### Verification

* Every statement of `docs/storage-spaces-format.md` marked verified is
  checked by tests against pools created by Windows: metadata equal to
  `Get-PhysicalExtent`, a verification pattern read back through every
  backend on Linux, NTFS pools with file checksums recorded on Windows, and
  crash experiments compared with the content Windows shows after recovery.
* Metadata and data fixtures from those pools run in CI, with cargo-fuzz
  targets for the parsers.

### Not supported yet

* Writing, pool management (stages 2 and 3 of `docs/plan.md`).
* Pools created by Windows 8, 10 or Windows Server.
* ReFS.
