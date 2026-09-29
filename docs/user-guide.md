# User guide

`spaces` reads Microsoft Storage Spaces pools created by Windows 11 (24H2:
pool version 28; Insider builds: pool version 29) and exposes their virtual
disks ("spaces") as Linux block devices. They are read-only, and the pool
disks are never written, unless a space is attached read-write
(`attach --rw`, see [Writing](#writing)).

## Supported configurations

| Configuration | Status |
|---|---|
| Simple (striped), any column count and interleave | read |
| Two-way and three-way mirror | read, survives missing or failing disks |
| Single parity | read, one missing or failing disk rebuilt from parity |
| Dual parity, 7-10 columns | read; any two missing disks rebuilt |
| Dual parity, 11+ columns (local reconstruction code) | read; any two missing disks rebuilt |
| Fixed and thin provisioning | read |
| Write-back cache (default for new spaces) | read, including data not yet moved out of the cache |
| Storage tiers, mirror-accelerated parity | read |
| 512-byte and 4 KiB logical sectors, interleave 16 KiB to 1 MiB | read |
| Pools after a crash or power loss | read; parity stripes and mirror rows left inconsistent by writes in flight are refused (see below) |
| Several spaces per pool, fragmented and extended spaces | read |
| Member disks with 512-byte, 512e and 4Kn sectors | read |
| Pools after a disk was retired, replaced or removed | read |
| Pools created by Windows 8/10/Server | not supported |
| Writing: simple, mirror, single parity (fixed or allocated rows) | `attach --rw` |
| Writing: dual parity, tiers, new rows of thin spaces, degraded pools | not supported (refused) |
| Pool management (creating pools and spaces, adding disks) | not supported |

## Installing

From a checkout:

```sh
cargo build --release
sudo contrib/install.sh              # /usr/local/sbin/spaces, udev rule, systemd unit
# or build a package
contrib/deb/build-deb.sh && sudo apt install ./target/deb/storage-spaces_*.deb
(cd contrib/arch && makepkg -si)     # Arch Linux
```

Only the `spaces` binary: `cargo install --locked --path crates/spaces-cli`.

The udev rule starts `storage-spaces-attach.service` whenever a pool member
appears, so pools are attached at boot and when their disks are plugged in.

## Attaching and mounting

```sh
sudo spaces scan                     # pools and their members
sudo spaces attach                   # every complete pool
sudo spaces status
lsblk /dev/mapper/ss-*
sudo mount -o ro /dev/mapper/ss-<pool>-<space>-p2 /mnt
sudo umount /mnt
sudo spaces detach                   # or: spaces detach <space>
```

Device names: `ss-<pool>-<space>` for the whole virtual disk and
`ss-<pool>-<space>-p<N>` for partition `N` (characters other than letters,
digits, `_ . +` become `_`). Partition devices are created from the space's
own GPT or MBR, so they exist even when Windows left out the protective MBR.

For `/etc/fstab`, use the partition device and `nofail`:

```
/dev/mapper/ss-Storage_pool-Data-p2  /mnt/data  ntfs3  ro,nofail,x-systemd.device-timeout=60  0 0
```

### Backends

`--backend auto` (default) picks device-mapper for simple spaces whose
logical sector size matches the disks, and ublk otherwise. ublk keeps
mirror and parity spaces readable when a disk fails while mounted; the
device-mapper mapping reads one copy only.

| Backend | Requirement |
|---|---|
| `dm` | `dmsetup` |
| `ublk` | Linux 6.0 or newer, module `ublk_drv` |
| `nbd` | module `nbd`, `nbd-client` |
| `fuse` | FUSE, `losetup` |

## Writing

```sh
sudo spaces attach --rw --space Data          # one space, read-write
sudo mount /dev/mapper/ss-<pool>-Data-p2 /mnt
...
sudo umount /mnt && sudo spaces detach Data   # flushes everything
```

A space opens for writing only when everything about its state is
understood, and the writes keep it consistent the way Windows would, so
that Windows reads the result back. `attach --rw` refuses, with the reason:

* pools that are not clean: missing disks, copies that missed writes,
  diverging metadata (attach them to Windows first);
* degraded spaces, dual parity spaces, storage tiers and mirror-accelerated
  parity;
* write-back caches whose mirrored copies disagree after a crash.

Writes into rows a thin space has not allocated yet fail with an I/O error
(allocating them is not supported yet); fixed spaces and the allocated part
of thin spaces are written in place.

How the space types are written:

* Simple: in place. `--backend auto` uses device-mapper when the space is
  fully allocated and its write-back cache holds nothing, ublk otherwise.
* Mirror: every copy; the extent run is listed in the dirty region log,
  durably, before its first write (Windows compares those runs' copies).
  Served by ublk (or NBD).
* Single parity: through the write-back cache, like Windows' own writes;
  the cache is destaged in whole stripes under the parity journal when it
  fills up, so a crash never leaves a stripe whose parity is stale while
  its data exists nowhere else. Served by ublk (or NBD).

Guarantees and risks:

* Writes are durable once flushed (`sync`, `fsync`, FUA, unmounting) or
  after a clean `spaces detach`. A crash, power loss or `kill -9` of the
  serving process loses what was not flushed, as with a disk's volatile
  cache; the pool stays consistent and Windows attaches it as healthy.
* A write-back cache holding data when a space is attached read-write is
  destaged first.
* Do not attach the same pool to Windows (or another Linux system) while it
  is attached read-write here.
* NTFS: `ntfs3` of Linux 6.8 corrupts small files that are truncated to zero
  (on any disk, not only here); ntfs-3g is not affected.
* Writing is new: keep a backup of data you care about.

## Pools with missing disks

`attach` skips pools with missing disks. `spaces info <disks...>` shows which
disk is missing and whether each space is degraded (still complete) or
failed (data lost with the disks at hand). If the redundancy of every space still covers the loss
(mirror: at least one copy of every slab; single parity: at most one
missing disk; dual parity: at most two),
attach anyway:

```sh
sudo spaces attach --degraded
```

Reads that need a missing disk fail with an I/O error instead of returning
wrong data. Copies that missed writes while their disk was away are never
used.

If fewer than half of the pool's disks are present, `attach` refuses even
with `--degraded`: the disks at hand may all be disks that dropped out
earlier, and their metadata would describe an old state of the pool
(`spaces info` prints a warning). Use `--force` only when you know these
disks were the last ones written.

## After a crash or power loss

Windows records which mirror extent runs (dirty region tracking) and which
parity stripes (parity journal) had writes in flight. For such stripes whose parity does not match the data, it is not
known which side Windows would keep, so reads of them fail by default and
`spaces info` reports:

```
parity journal: 1 extent run(s) not cleanly shut down; mismatching stripes are refused
```

Options:

* Attach the pool to Windows 11 once; it repairs the stripes. Then read it on
  Linux again.
* Read the on-disk data as it is, accepting that at most the stripes being
  written at the crash may differ from what Windows would show:

  ```sh
  sudo spaces --unclean-parity data attach
  ```

Mirror spaces are handled differently: Windows lists the extent runs
written since the space was last disconnected (a restart does not clear
the list, so nearly every mirror shows some, and `spaces info` reports
`dirty region log: N extent run(s) written since ...`). The copies of such
runs are compared on every read, and rows whose copies differ, which only a
crash with writes in flight leaves behind, are refused the same way:
Windows itself never reconciles them and reads either copy, so there is no
right answer. Comparing costs reading every copy of those runs. The
write-back cache and the parity journal are mirrored too; what their copies
disagree about is treated the same way. `--unclean-parity data` reads the
highest mirror copy and the newest cache and journal slots. Simple spaces
need nothing special after a crash.

## Copying a space out

```sh
spaces export /dev/sdb /dev/sdc --space Data --output data.img
sudo losetup -r -P -b <sector size> -f --show data.img
```

`spaces info` prints the sector size; 4 KiB spaces need `-b 4096`, or the
partition table will not be found.

## Checking a pool

`spaces info <disks...>` prints the pool, its disks and every space with its
state:

* `healthy`: every copy and column is on a disk at hand.
* `degraded`: some copies or columns are on missing or out-of-date disks,
  but all data can still be read or rebuilt; attach with `--degraded`.
* `failed`: some data is only on missing disks; reads of it fail.

Warnings about the pool database: a `stale` copy belongs to a disk that was
away while the pool changed (normal); a `torn` copy was cut short by an
interrupted update and is ignored in favour of the copy most disks hold; an
`unusable` newer copy could not be decoded, so an older one is used.

`spaces dump DEVICES...` prints the whole metadata one fact per line: every
copy of the pool database with its records, and the dirty region log,
parity journal and write-back cache of each space. `spaces diff --old
DEVICES... --new DEVICES...` lists what changed between two states of a
pool (for example copies of the disks before and after Windows used it),
which helps with bug reports.

## Troubleshooting

* `no Storage Spaces pool members found`: the disks are not visible
  (`lsblk`), or they are not pool members (Windows 11 pools only).
* `device-mapper device ... already exists`: an interrupted run left a device
  behind; remove it with `dmsetup remove <name>`.
* `backend ublk is not available`: load the module (`modprobe ublk_drv`) or
  use `--backend nbd`.
* `cannot open /dev/...: Device or resource busy`: another process (a mount,
  mdadm, LVM, a second `spaces` instance) holds the disk.
* Logs of serving processes: `journalctl -u 'storage-spaces-*'`.
