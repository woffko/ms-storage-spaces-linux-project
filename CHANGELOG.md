# Changelog

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
* The cache and journal logs wrap behind checkpoints as Windows expects;
  a cache holding data when a space is opened for writing is destaged first.
* Writes are durable after a flush (sync, FUA) or a clean detach; the
  serving processes flush when stopped.
* Checked by crash replays of every flush point and of unordered writes,
  fio with verification through ublk, NBD and device-mapper, and NTFS
  written on Linux and verified by Windows.

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
