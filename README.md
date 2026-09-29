# Microsoft Storage Spaces on Linux

[![CI](https://github.com/woffko/ms-storage-spaces-linux-project/actions/workflows/ci.yml/badge.svg)](https://github.com/woffko/ms-storage-spaces-linux-project/actions/workflows/ci.yml)
[![License: GPL-2.0-or-later](https://img.shields.io/badge/license-GPL--2.0--or--later-blue.svg)](LICENSE)

Read and write Microsoft Storage Spaces pools on Linux: `spaces` assembles a
pool from its member disks and exposes every virtual disk ("space") as a
Linux block device, read-only unless asked otherwise, attached
automatically at boot or when the disks are plugged in, so the NTFS (or any
other) file system inside mounts like on an ordinary disk. Written in Rust, from a reverse-engineered description of the
on-disk format that is checked against pools created by Windows.

> **Status: pools created by Windows 11; reading, and writing to simple,
> mirror and single parity spaces (`attach --rw`, new).** Pool management is
> planned (see [the plan](docs/plan.md)); ReFS will follow. The pool disks
> are written only for spaces attached read-write.

## What works

| Configuration | Status |
|---|---|
| Simple (striped) spaces, any column count, interleave 16 KiB to 1 MiB | read |
| Two-way and three-way mirror | read; survives missing or failing disks |
| Single parity | read; one missing or failing disk rebuilt |
| Dual parity, 7-10 columns (Reed-Solomon over GF(16)) | read; any two missing disks rebuilt |
| Dual parity, 11+ columns (local reconstruction code) | read; any two missing disks rebuilt |
| Storage tiers, mirror-accelerated parity | read |
| Fixed and thin provisioning, several spaces per pool, extended spaces | read |
| The per-space write-back cache | read, including data not yet moved out of it |
| 512-byte and 4 KiB logical sectors; 512e and 4Kn member disks | read |
| Retired, replaced or missing disks, out-of-date mirror copies | read as far as the redundancy allows |
| Pools after a crash or power loss | read; the few parity stripes or mirror rows left inconsistent by writes in flight are refused |
| Pools created by Windows 11 24H2 (pool version 28) and Insider builds (version 29) | yes |
| Pools created by Windows 8, 10 or Windows Server | not supported |
| Writing simple, mirror and single parity spaces (fixed, or allocated rows of thin ones) | `attach --rw`; Windows reads the result back |
| Writing dual parity, tiers, new rows of thin spaces, degraded pools | refused |
| Pool management, ReFS | not yet |

## Quick start

```sh
cargo build --release
sudo contrib/install.sh            # /usr/local/sbin/spaces, udev rule, systemd unit

sudo spaces scan                   # pool members found among the block devices
sudo spaces attach                 # attach every complete pool
sudo spaces status
sudo mount -o ro /dev/mapper/ss-<pool>-<space>-p2 /mnt
sudo umount /mnt
sudo spaces detach
```

Each space appears as `/dev/mapper/ss-<pool>-<space>`, with one
`-p<N>` device per partition of the space. Pools with missing disks are
attached only with `--degraded`; `spaces info <disks...>` shows the pool, its
disks and whether each space is healthy, degraded or failed.

Packages: `contrib/deb/build-deb.sh` builds a Debian/Ubuntu package,
`contrib/arch/PKGBUILD` an Arch Linux one, and CI builds a static musl
binary. Without installing anything, pools can be inspected and copied out:

```sh
spaces info /dev/sdb /dev/sdc
spaces export /dev/sdb /dev/sdc --space "My space" --output space.img
```

See the [user guide](docs/user-guide.md) for details, writing, degraded
pools and pools after a crash, and `man contrib/man/spaces.8`.

### Backends

| Backend | Needs | Used by `--backend auto` for |
|---|---|---|
| `dm` | `dmsetup` | simple spaces whose sector size matches the disks (kernel speed) |
| `ublk` | Linux 6.0+, `ublk_drv` | everything else: parity, cache, tiers, degraded pools |
| `nbd` | `nbd` module, `nbd-client` | kernels without ublk |
| `fuse` | FUSE, `losetup` | last resort |

## How it is built and tested

* [`docs/storage-spaces-format.md`](docs/storage-spaces-format.md) describes
  the on-disk format as far as it is understood: pool and space databases,
  slab allocation, the striping of every resiliency type and their parity
  codes, the write-back cache log, the parity journal and dirty region
  tracking. Every statement marked **verified** is backed by a test.
* Test pools are created by scripts on Windows 11 VMs (`tools/vm/`): every
  configuration above, pools with NTFS and real files, pools whose disks were
  pulled while Windows was writing. Each records Windows' own view
  (`Get-PhysicalExtent`, file checksums, the content after Windows recovered
  a crashed pool).
* The tests compare the parser with that view, read back a verification
  pattern written by Windows, rebuild data with any tolerated number of
  failed disks, and check crashed pools MiB by MiB against Windows' recovery.
  Metadata and data fixtures captured from those pools run in CI; the full
  images (tens of GB) stay out of git.
* On a Linux VM every pool goes through every backend (sequential, random,
  O_DIRECT and fio reads), and the NTFS pools are attached by udev and
  systemd and mounted with ntfs3 to compare every file.
* The parsers are fuzzed with cargo-fuzz (`fuzz/`).

## Repository layout

| Path | Contents |
|---|---|
| `crates/storage-spaces` | library: metadata parsing, space layouts, reader |
| `crates/spaces-cli` | the `spaces` command and its block device backends |
| `docs/` | format description, user guide, project plan, prior art |
| `tools/` | test pool generators for the Windows VM, corpus and VM test scripts |
| `fuzz/` | cargo-fuzz targets (`cargo +nightly fuzz run <target>`) |
| `contrib/` | udev rule, systemd unit, man page, Debian and Arch packaging |

## Roadmap

1. **Read-only** access that behaves like a normal disk (done for Windows 11
   pools).
2. **Writes** to the exposed block devices, with Windows accepting the pool
   afterwards (simple, mirror and single parity spaces done; thin
   allocation and TRIM next).
3. **Pool management**: creating, extending and repairing pools and spaces.

ReFS support is planned as a separate track. Details in
[docs/plan.md](docs/plan.md).

## License

GPL-2.0-or-later. Not affiliated with or endorsed by Microsoft; "Storage
Spaces" and "Windows" are trademarks of Microsoft Corporation.
