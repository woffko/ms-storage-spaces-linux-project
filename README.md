# Microsoft Storage Spaces on Linux

[![CI](https://github.com/woffko/ms-storage-spaces-linux-project/actions/workflows/ci.yml/badge.svg)](https://github.com/woffko/ms-storage-spaces-linux-project/actions/workflows/ci.yml)
[![License: GPL-2.0-or-later](https://img.shields.io/badge/license-GPL--2.0--or--later-blue.svg)](LICENSE)

Read, write and manage Microsoft Storage Spaces pools on Linux: `spaces`
assembles a pool from its member disks and exposes every virtual disk
("space") as a Linux block device, read-only unless asked otherwise,
attached automatically at boot or when the disks are plugged in, so the
NTFS (or any other) file system inside mounts like on an ordinary disk. It
also creates pools and spaces and adds, retires and replaces disks the way
Windows does, so that Windows takes the result as its own. Written in Rust,
from a reverse-engineered description of the on-disk format that is checked
against pools created by Windows.

> **Status: pools created by Windows 11; reading; writing to simple, mirror
> and single parity spaces, fixed and thin (`attach --rw`); pool management
> (`spaces pool|space|disk`); reading and (experimentally) writing ReFS 3.x
> volumes (`refs`, new).**
> The pool disks are written only for spaces attached read-write and by
> management commands given `--yes`; `refs` writes only with its writing
> commands and `--yes`, or mounted with `--rw`, and never into a pool.

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
| Writing simple, mirror and single parity spaces, fixed and thin | `attach --rw`; Windows reads the result back |
| Thin spaces: rows allocated as they are written, discards (TRIM) give them back | `attach --rw` (256 MiB allocation units) |
| Writing dual parity, tiers, degraded pools | refused |
| Creating pools and simple, mirror and single parity spaces (fixed and thin); deleting, renaming, growing spaces | `spaces pool create`, `spaces space ...`; Windows takes the pools as healthy |
| Adding, retiring, removing and replacing disks; repair, optimize, scrub; health in Windows' terms | `spaces disk ...`, `spaces pool ...` |
| Creating tiered spaces (SSD mirror over HDD simple or parity) | `spaces tier`, `spaces space create --tier` |
| Creating dual parity spaces; writing tiered spaces | not yet |
| ReFS 3.x volumes (Dev Drives, data volumes, inside a space): files, sparse, block-cloned and deduplicated files, named streams, stream snapshots, links, attributes; integrity streams checked on every read | `refs` reads and mounts read-only (FUSE); verified on ReFS 3.14 |
| Writing ReFS: files (up to 4 GiB each), directories, renames and moves, hard links, times and attributes, named streams, integrity streams, block clones (`refs clone`), deleting block-cloned files and files with snapshots; a read-write FUSE mount; `mount -t ReFS` and udisks2 through `mount.ReFS` | `refs set|overwrite|write|create|rename|move|link|clone|delete|mkdir --yes`, `refs mount --rw` (experimental); Windows takes the result as healthy (refsutil leak and triage) |
| Checking ReFS | `refs check` (pages, allocators, shared clusters) |
| ReFS compression, encryption; ReFS 1.x/2.x and 3.4-3.13 (no Windows at hand makes them); cloning files, stream snapshots by `refs` | not yet |

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

Managing a pool (every command prints its plan and writes only with
`--yes`):

```sh
sudo spaces pool create --name Data --yes /dev/sdb /dev/sdc /dev/sdd
sudo spaces space create --name Files --resiliency mirror --size 1T --yes /dev/sdb /dev/sdc /dev/sdd
sudo spaces pool health /dev/sdb /dev/sdc /dev/sdd
```

See the [user guide](docs/user-guide.md) for details, writing, managing
pools, degraded pools and pools after a crash, and
`man contrib/man/spaces.8`.

ReFS volumes (also on a disk image, or inside a space of a pool whose disks
are given) are read with `refs`, see `man contrib/man/refs.1`:

```sh
refs ls /dev/sdb2 -lR
refs cat /dev/sdb2 --path /src/main.c > main.c
refs mount /dev/sdb2 /mnt/devdrive             # read-only, FUSE
refs mount --rw devdrive.img /mnt/devdrive     # for writing (experimental)
refs ls --space Data /dev/sdd /dev/sde --path /
```

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
  tracking. Every statement marked **verified** is backed by a test;
  [`docs/refs-format.md`](docs/refs-format.md) does the same for ReFS 3.x.
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
* Writing is checked by replaying every crash state of the member writes
  (each flush point and unordered unflushed writes), by fio with
  verification through every backend, and by NTFS written on Linux that
  Windows then attaches as healthy, with chkdsk clean and every file
  intact, also after crashes and after TRIM.
* Management is checked by predicting byte for byte what Windows writes
  when it creates pools and spaces, changes and deletes them and adds and
  removes disks; by replaying a crash after every step of each operation;
  by Windows taking pools created and changed on Linux (also pools cut
  short by a crash) as healthy, repairing and optimizing them; and by the
  write checks above on spaces created on Linux.
* The parsers, the write path and the management planners are fuzzed with
  cargo-fuzz (`fuzz/`).

## Repository layout

| Path | Contents |
|---|---|
| `crates/storage-spaces` | library: metadata parsing, space layouts, reader, writer and management planners |
| `crates/spaces-cli` | the `spaces` command and its block device backends |
| `crates/refs` | library: reading, writing and checking ReFS 3.x volumes |
| `crates/refs-cli` | the `refs` command and its FUSE driver |
| `docs/` | format description, user guide, project plan, prior art |
| `tools/` | test pool generators for the Windows VM, corpus and VM test scripts |
| `fuzz/` | cargo-fuzz targets (`cargo +nightly fuzz run <target>`) |
| `contrib/` | udev rule, systemd unit, man page, Debian and Arch packaging |

## Roadmap

1. **Read-only** access that behaves like a normal disk (done for Windows 11
   pools).
2. **Writes** to the exposed block devices, with Windows accepting the pool
   afterwards (done: simple, mirror and single parity spaces, thin
   allocation and TRIM).
3. **Pool management**: creating, extending and repairing pools and spaces
   (done, except creating dual parity spaces).

ReFS is a separate track: reading ReFS 3.x and mounting it (done for ReFS
3.14, with snapshots and deduplicated files), writing (done for the
operations above, each checked on Windows), then compression and older
versions, which need Windows Server 2025 and older Windows to make
samples.
Details in [docs/plan.md](docs/plan.md).

## License

GPL-2.0-or-later. Not affiliated with or endorsed by Microsoft; "Storage
Spaces" and "Windows" are trademarks of Microsoft Corporation.
