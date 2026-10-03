# User guide

`spaces` reads Microsoft Storage Spaces pools created by Windows 11 (24H2:
pool version 28; Insider builds: pool version 29) and exposes their virtual
disks ("spaces") as Linux block devices. They are read-only, and the pool
disks are never written, unless a space is attached read-write
(`attach --rw`, see [Writing](#writing)) or the pool is changed with
`spaces pool`, `spaces space` or `spaces disk` (see
[Managing pools](#managing-pools)).

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
| Writing: simple, mirror, single parity, fixed and thin (256 MiB allocation units) | `attach --rw` |
| Writing: dual parity, tiers, degraded pools | not supported (refused) |
| Creating pools (version 28) and simple, mirror and single parity spaces, fixed and thin; deleting, renaming, growing spaces | `spaces pool`, `spaces space` |
| Adding, retiring, removing and replacing disks; repair, optimize, scrub, health | `spaces disk`, `spaces pool` |
| Creating tiered spaces: an SSD mirror over an HDD simple or parity tier (mirror-accelerated parity) | `spaces tier`, `spaces space create --tier` |
| Creating dual parity spaces; new spaces in version 29 pools | not supported |

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

Thin spaces allocate a row (256 MiB per column and copy) when a write first
reaches it, as Windows does: the pool database on every member records the
new slabs before the data is written. A slab is not cleared when it is
allocated (Windows does not clear it either), so the parts of a new row not
written yet read whatever the disk held there. Thin spaces with other
allocation units can be written only where they are allocated; writes
elsewhere fail with an I/O error. When the pool has no free slab left, so do
writes into rows not allocated yet. In a new row of a thin parity space, the
stripes not written yet hold whatever the disks held and their parity does
not match it; the parity journal lists them as not consistent, as it does
for rows Windows allocates, so `spaces` refuses to read them (I/O error)
unless `--unclean-parity data` is given. File systems do not read what they
have not written, but a copy of the whole device does.

Discards (`fstrim`, the `discard` mount option, `blkdiscard`) give the rows
of thin simple and mirror spaces back to the pool once they are discarded
whole, as Windows does with TRIM; a row given back reads as zeros. Discards
of parts of rows are remembered while the space stays attached and count
towards the whole row until it is written again. Parity and fixed spaces
do not offer discards.

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

## Managing pools

`spaces pool`, `spaces space` and `spaces disk` change pools the way
Windows 11 24H2 does, so that Windows takes the result as its own: pools
created and changed on Linux attach in Windows as healthy, and its Repair
and Optimize work on them. Every command works on the disks given (block
devices or image files) and:

* computes a plan and prints it; nothing is written without `--yes`;
* opens the disks exclusively: a disk that is mounted, attached (also by
  `spaces attach`) or used by another program fails with "busy" or "in
  use"; detach the pool first;
* works on clean pools only: every disk present, no copy out of date,
  every record understood. Attach a pool that is not clean to Windows
  first, or see [Replacing a failed disk](#replacing-a-failed-disk);
* writes in steps that each end with a flush, so that after a crash or
  power loss at any point the pool opens in the state before or after a
  step; data being moved is copied before the metadata points at it.

### Creating a pool and spaces

```sh
sudo spaces pool create --name Data /dev/sdb /dev/sdc /dev/sdd            # prints the plan
sudo spaces pool create --name Data --yes /dev/sdb /dev/sdc /dev/sdd
sudo spaces space create --name Files --resiliency mirror --size 1T --yes /dev/sdb /dev/sdc /dev/sdd
sudo spaces attach
```

`pool create` takes blank disks only: a partition table, file system or
other data in the first or last MiB makes it refuse, unless `--wipe` (which
destroys everything on those disks). The disks get the layout
`New-StoragePool` gives them (a Microsoft reserved partition and the pool
partition); up to five of them carry a copy of the pool database.
`--logical-sector 4096` makes the spaces use 4 KiB sectors.

`space create` takes Windows' defaults for what is not given:

| Option | Default |
|---|---|
| `--resiliency simple` | one column per disk (up to 8) |
| `--resiliency mirror` | two copies (`--copies 3` needs five disks), disks / copies columns (up to 8) |
| `--resiliency parity` | single parity, three columns, a write-back cache of 1 GiB (`--write-cache`, at least 512 MiB) |
| `--thin` | fixed provisioning otherwise; thin spaces take 256 MiB allocation units |
| `--interleave` | 256K |

Sizes (`10G`, `500M`, `2T`) are rounded up to whole rows (fixed spaces:
1 GiB per data column). As on Windows, a new space's slabs are not cleared:
only its first sector (a parity space: its first stripe) is, so that no old
partition table shows up. Create a partition table and a file system as on
any disk, through `spaces attach --rw`.

```sh
sudo spaces space rename --space Files --name Archive --yes DISKS...
sudo spaces space resize --space Archive --size 2T --yes DISKS...   # grows; grow the file system yourself
sudo spaces space delete --space Archive --yes DISKS...
sudo spaces pool rename --name Data2 --yes DISKS...
sudo spaces pool remove --yes DISKS...                               # a pool without spaces
```

### Tiered spaces

A tiered space puts its first part on SSD disks (a two-way mirror) and the
rest on HDD disks (simple or single parity: mirror-accelerated parity), as
`New-StorageTier` and `New-VirtualDisk -StorageTiers` make it. Mark the
disks' media first, then create a template per tier and the space over
them:

```sh
sudo spaces disk set --disk 1 --media ssd --yes DISKS...      # each SSD disk
sudo spaces disk set --disk 3 --media hdd --yes DISKS...      # each HDD disk
sudo spaces tier create --name fast --media ssd --resiliency mirror --yes DISKS...
sudo spaces tier create --name big --media hdd --resiliency parity --columns 3 --yes DISKS...
sudo spaces space create --name Files --tier fast=100G --tier big=2T --yes DISKS...
```

The space gets, on the SSD disks, a write-back cache of 1 GiB and the dirty
region log and parity journal its tiers need. It needs at least two SSD
disks; tier sizes are rounded up to whole rows. Linux reads tiered spaces
but does not write them: format and fill them on Windows.

### Disks

`spaces info DISKS...` lists the disks of a pool with their ids.

```sh
sudo spaces disk add --new /dev/sde --yes /dev/sdb /dev/sdc /dev/sdd   # the pool's disks, then the new one
sudo spaces pool optimize --yes /dev/sdb /dev/sdc /dev/sdd /dev/sde    # spread the spaces over it
sudo spaces disk set --disk 4 --media ssd --yes DISKS...               # or --usage manual-select, hot-spare
sudo spaces disk retire --disk 2 --yes DISKS...                        # move everything off disk 2
sudo spaces disk remove --disk 2 --yes DISKS...                        # then take it out of the pool
```

`disk add` takes a blank disk (`--wipe` as above) and a pool of at most
four disks. `disk retire` moves every extent of the disk to the others
(pools of up to five disks); it needs room there, and the rows of every
space need enough other disks (a three-column parity space needs three
disks besides the retired one). `disk remove` removes a disk that holds
nothing any more; the disk keeps its data behind a partition table without
the pool partition.

### Replacing a failed disk

With a disk missing, the pool database and the spaces' redundancy still
allow a repair:

```sh
sudo spaces pool health /dev/sdb /dev/sdc                   # what is missing, what is degraded
sudo spaces disk add --new /dev/sdf --yes /dev/sdb /dev/sdc  # a new disk, if the others have no room
sudo spaces pool repair --yes /dev/sdb /dev/sdc /dev/sdf    # rebuild the lost copies
sudo spaces disk remove --disk 3 --yes /dev/sdb /dev/sdc /dev/sdf   # the missing disk
```

`pool repair` rebuilds every copy that is on a missing disk or out of date
on another disk: mirror copies from a current copy, single parity columns
from the other columns. Without the new disk it uses the disks at hand if
their rows allow it.

### Health, scrub

`spaces pool health DISKS...` shows what Windows would show
(`Get-StoragePool`, `Get-VirtualDisk`, `Get-PhysicalDisk`) for the pool with
the disks given, and how many more disk failures every space survives:

| Shown | Meaning |
|---|---|
| Healthy / OK | every disk of the pool at hand, every copy current |
| Warning / Degraded | a disk of the pool is missing (even with nothing of this space on it), or copies are out of date |
| Warning / Degraded Incomplete | copies of this space are on missing disks; its data is still complete |
| Unhealthy / No Redundancy Degraded | data of this space is lost; reads of it fail |
| Unhealthy / Detached | the pool lost its quorum |
| pool Warning / Degraded | disks missing, more than half of the database copies at hand |
| pool Unhealthy / Read-only | half of the database copies or fewer at hand |
| disk Warning / Lost Communication | the disk is missing |

These are the states Windows showed for the same pools, created on Linux
and by Windows, after disks were detached while in use.

`spaces pool scrub DISKS...` reads every copy of every mirror space and
every stripe of every single parity space and reports what disagrees
(read-only). Differences where writes were under way (extent runs the
dirty region log lists, stripes the parity journal does not list as
consistent) are what a crash leaves and are counted apart; Windows settles
them when it takes the pool. `--repair --yes` makes everything agree,
keeping the first mirror copy and recomputing parity from the data.

### Limits

* Dual parity spaces are not created, nor tiered spaces of other shapes
  than an SSD mirror over an HDD tier; existing ones are read, and their
  pools can be changed otherwise.
* Pools of version 29 (Insider builds) get no new spaces; their records
  differ there. Other changes keep each record's own layout.
* Windows 11 24H2 crashed (a bugcheck in its Storage Spaces driver) when a
  pool of four disks with simple, mirror and parity spaces arrived with one
  disk absent,
  whether Linux or Windows had created the pool. Replace or remove a failed
  disk on Linux before handing the pool to Windows.

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

## Reading ReFS volumes

`refs` reads ReFS 3.x volumes (Dev Drives, ReFS data volumes, ReFS inside a
space) without Windows and never writes to them. The source is an image, a
disk or a partition (on a disk the first ReFS partition of its GPT), or,
with `--space NAME`, the member disks of a pool:

```sh
refs info /dev/sdb2                       # version, clusters, checksums
refs ls /dev/sdb2 --path /src -l          # attributes, size, modified (UTC)
refs ls /dev/sdb2 -R                      # the whole tree
refs stat /dev/sdb2 --path /src/main.c    # times, extents, streams, snapshots, link target
refs cat /dev/sdb2 --path /notes.txt --stream summary
refs cat /dev/sdb2 --path /db.mdf --snapshot nightly > db-nightly.mdf
refs ls --space Data /dev/sdd /dev/sde    # ReFS inside a space
refs mount --space Data /dev/sdd /dev/sde mnt    # options before the disks
mkdir -p mnt && refs mount /dev/sdb2 mnt  # read-only FUSE mount, foreground
fusermount -u mnt
```

The mount shows named streams as extended attributes `user.<name>`
(`getfattr -d`; Linux limits them to 64 KiB, larger streams are read with
`refs cat --stream`), symbolic links and junctions as symbolic links (an
absolute target `C:\path` points into the mount) and hard links with one
inode. Every metadata page is checked against its checksum; a volume that
fails is reported, not guessed at. The data of integrity streams
(`Set-FileIntegrity`, or volumes formatted with integrity streams) is
checked on every read too: damaged data is an error (EIO in the mount),
never returned.

Read: files (resident, in extents, sparse, block-cloned, deduplicated),
directories of any size, named streams, stream snapshots (`refsutil
streamsnapshot`; `refs cat --snapshot`, not in the mount), hard links,
symbolic links and junctions, attributes and times, volumes with 4 KiB
and 64 KiB clusters, CRC-64 or SHA-256 metadata checksums and integrity
streams (verified on ReFS 3.14 volumes made by Windows 11). Not read yet:
compressed files, encrypted files, ReFS 1.x/2.x; writing ReFS is not
supported.

## Troubleshooting

* `no Storage Spaces pool members found`: the disks are not visible
  (`lsblk`), or they are not pool members (Windows 11 pools only).
* `device-mapper device ... already exists`: an interrupted run left a device
  behind; remove it with `dmsetup remove <name>`.
* `backend ublk is not available`: load the module (`modprobe ublk_drv`) or
  use `--backend nbd`.
* `cannot open /dev/...: Device or resource busy`: another process (a mount,
  mdadm, LVM, a second `spaces` instance) holds the disk.
* `... is in use by another process` (an image file): another `spaces`
  process has it open for writing (`serve-ublk --rw`, a management command).
* `the pool is not in a clean state`: a management command was given a pool
  with a missing disk or out-of-date copies; see
  [Replacing a failed disk](#replacing-a-failed-disk) or attach it to
  Windows first.
* Logs of serving processes: `journalctl -u 'storage-spaces-*'`.
