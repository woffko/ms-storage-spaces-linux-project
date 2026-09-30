# Changelog

## 1.0.0 (unreleased)

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
* Not supported: creating storage tiers, mirror-accelerated parity and
  dual parity spaces; new spaces in pools of version 29 (Insider builds).

### Known issue in Windows

* Windows 11 24H2 bugchecks (0x50 in spaceport.sys) when a pool of four
  disks with simple, mirror and parity spaces arrives with one disk absent,
  whether Linux or Windows created the pool.

## 0.2.0 (unreleased)

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

## 0.1.0 (unreleased)

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
