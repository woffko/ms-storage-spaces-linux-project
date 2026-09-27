# User guide

`spaces` reads Microsoft Storage Spaces pools created by Windows 11 (24H2:
pool version 28; Insider builds: pool version 29) and exposes their virtual
disks ("spaces") as read-only Linux block devices. It never writes to the
pool disks.

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
| Pools after a crash or power loss | read; parity stripes and mirror copies whose outcome Windows decides on its next mount are refused (see below) |
| Several spaces per pool, fragmented and extended spaces | read |
| Member disks with 512-byte, 512e and 4Kn sectors | read |
| Pools after a disk was retired, replaced or removed | read |
| Pools created by Windows 8/10/Server | not supported |
| Writing, pool management | not supported |

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

In mirror runs with writes in flight the copies are compared on every
read; rows whose copies differ are refused the same way (`spaces info`
reports `dirty region tracking: ... not cleanly shut down`), because which
copy Windows keeps when it resynchronises them is not predictable. The
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
