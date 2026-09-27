# Changelog

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
* Unclean shutdowns: parity stripes and mirror rows with writes in flight are
  checked, and those whose outcome Windows decides on its next mount are
  refused unless `--unclean-parity data` is given.

### Block devices

* `spaces scan`, `attach`, `detach`, `status`, with device-mapper, ublk, NBD
  and FUSE backends; `/dev/mapper/ss-<pool>-<space>` and one device per
  partition.
* udev rule and systemd unit: pools are attached at boot and when their disks
  appear.
* `spaces info`, `extents`, `export`, `dm-table` for inspecting pools without
  attaching them.

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
