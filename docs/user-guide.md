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

### What it needs

* Linux with device-mapper (`dmsetup`), systemd and udev for the attach at
  boot. The backends beyond device-mapper: `ublk` (kernel 6.0 or later,
  module `ublk_drv`) for parity, caches, tiers and degraded pools; `nbd`
  (`nbd-client`, module `nbd`) where there is no ublk; FUSE (`fuse3`,
  `losetup`) as the last resort and for `refs mount`.
* To use what is inside a space: a file system driver (`ntfs3`, `ntfs-3g`,
  ...); ReFS volumes need none, `refs` reads them.
* To build: Rust 1.89 or later and `libclang` (the ublk bindings; on
  Debian/Ubuntu `libclang-dev`, or just the library, `libclang1-21` with
  `LIBCLANG_PATH=/usr/lib/llvm-21/lib`). `--no-default-features` builds
  without the ublk and FUSE backends and without `libclang`.

### Which way

| Way | Gives | For |
|---|---|---|
| From a checkout: `cargo build --release && sudo contrib/install.sh` | `spaces`, `refs`, `mount.ReFS`, man pages, udev rule, systemd unit, module list | any distribution |
| Debian/Ubuntu package: `contrib/deb/build-deb.sh && sudo apt install ./target/deb/storage-spaces_*.deb` | the same, as a package | Debian, Ubuntu |
| Arch Linux: `(cd contrib/arch && makepkg -si)` | the same, as a package | Arch |
| The releases page of the repository | static `spaces` and `refs` binaries (x86_64, musl) and a `.deb`, with `SHA256SUMS` (from 1.1.0; 1.0.0 had no `refs`) | trying it out; `sha256sum -c SHA256SUMS` first |
| `cargo install --locked --path crates/spaces-cli` (and `crates/refs-cli`) | the binary only, no udev rule or unit | development |

`contrib/install.sh` (as root; it takes the path of `spaces` if it was
built elsewhere) puts `spaces` in `/usr/local/sbin`, `refs` and its man
page in `/usr/local/bin` when it was built next to `spaces`,
`mount.ReFS` (and `mount.refs`) in `/sbin`, the udev rule and the
unit, lists `ublk_drv` and `nbd` in `/etc/modules-load.d`, and enables
`storage-spaces-attach.service`. `contrib/uninstall.sh` (`-n` lists what it
would do) removes it again; detach spaces first (`spaces detach`).

The udev rule starts `storage-spaces-attach.service` whenever a pool member
appears, so pools are attached at boot and when their disks are plugged in
(read-only; see Hardening below); installing starts it once too, for the
pools whose disks are there already. To attach by hand only, remove
`/etc/udev/rules.d/69-storage-spaces.rules`. Removing the package
detaches the spaces its `spaces` serves; the spaces another installation's
`spaces` serves (such as `contrib/install.sh`'s in `/usr/local`) stay
attached.

Check the installation: `spaces --version`, `refs --version`,
`systemctl status storage-spaces-attach.service`, and `spaces scan` with a
pool's disks plugged in. Man pages: `man spaces`, `man refs`. Upgrading is
installing again; attached spaces keep running the old servers until they
are detached and attached again.

## Attaching and mounting

```sh
sudo spaces scan                     # pools and their members
sudo spaces attach                   # every complete pool
spaces status                        # what is attached (no root needed)
lsblk /dev/mapper/ss-*
sudo mount -o ro /dev/mapper/ss-<pool>-<space>-p2 /mnt
sudo umount /mnt
sudo spaces detach                   # or: spaces detach <space>
```

`spaces scan` needs root to open the disks; without it, it says how many
it could not open. `spaces status` works for every user (the state in
`/run/storage-spaces` is readable).

`attach` checks every space first, as `spaces check` does, and attaches
only healthy spaces without being asked; a space that is not healthy is
refused with its report (see "Checking a pool"), which `spaces status`
lists too.

Device names: `ss-<pool>-<space>` for the whole virtual disk and
`ss-<pool>-<space>-p<N>` for partition `N` (characters other than letters,
digits, `_ . +` become `_`). Pools can share a name (Windows calls every new
pool "Storage pool"): when a space of another pool has the name already,
the first eight hex digits of the pool's GUID follow,
`ss-<pool>-<space>-<guid>` (and `-p<N>` after that); `spaces detach
<space>` finds both. Partition devices are created from the space's
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

With a disk of its pool missing a space is degraded, and `attach` refuses
it. `spaces info <disks...>` shows which disk is missing and whether each
space is degraded (still complete) or failed (data lost with the disks at
hand). If the redundancy of every space still covers the loss (mirror: at
least one copy of every slab; single parity: at most one missing disk;
dual parity: at most two), attach it anyway, read-only:

```sh
sudo spaces attach --degraded
```

Reads that need a missing disk fail with an I/O error instead of returning
wrong data. Copies that missed writes while their disk was away are never
used. A failed space (data lost) is not attached at all.

If no more than half of the copies of the pool database are at hand, the
pool lacks its quorum, as Windows counts it (it takes such a pool
read-only and detaches its spaces): the disks at hand may all be disks
that dropped out earlier, and their metadata would describe an old state
of the pool. Its spaces are suspect, and only `--force` attaches them,
read-only; use it only when you know these disks were the last ones
written.

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

`spaces check` checks the spaces of every pool found (or of the pool whose
devices are given: `spaces check DEVICES...`) the way `spaces attach` does
before it reads them, and prints a report per space: every check with its
status and evidence, and the verdict of the space (healthy, degraded,
suspect or failed). `--json` gives the same for programs, `--deep` adds the
checks that read whole spaces, and `--bundle FILE.tar.gz` packs the reports
and the pool's metadata (no file data) for a bug report. It exits with 1
unless every space is healthy.

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
refs check /dev/sdb2                      # consistency, read-only (exit 1 on a problem)
refs mount --space Data /dev/sdd /dev/sde mnt    # options before the disks
mkdir -p mnt && refs mount /dev/sdb2 mnt  # read-only FUSE mount, foreground
refs mount --rw image.img mnt              # for writing (experimental)
fusermount -u mnt                         # (as root: umount mnt)
```

Mounted by root, a block device (a disk, a partition, a space's
`/dev/mapper/ss-...-pN`) becomes a mount of type `fuseblk.refs` on that
device, as ntfs-3g's are, so that udisks2 and `findmnt` count it as the
device's mount. `refs` makes that mount itself, with the device's sector
size as block size (devices with 4 KiB sectors included), without
`fusermount3`; the mount goes away when `refs` ends, also when it is
killed (a small watcher process unmounts it).

The mount shows named streams as extended attributes `user.<name>`
(`getfattr -d`; Linux limits them to 64 KiB, larger streams are read with
`refs cat --stream`), symbolic links and junctions as symbolic links (an
absolute target `C:\path` points into the mount) and hard links as one
inode with its link count.

With `--rw` (one image, disk or partition; not `--space`) the mount
writes through the same code as the writing commands below: creating,
writing, truncating and deleting files (a file opened for writing is
kept in an unlinked temporary file, in `$REFS_TMPDIR` or the system's
temporary directory; when it is closed or synced, what was written goes
to the volume as Windows writes it: changed bytes where they are,
appended data in new runs, a shortened file's tail freed, the touched
clusters of an integrity stream copied on write; a file in its record,
a new or truncated file and a file Windows compressed are written
whole), copies (a whole file copied
into an empty one with copy_file_range, as coreutils 9 `cp` and file
managers copy, becomes a block clone that shares the clusters, as
`Copy-Item` makes on a Dev Drive), directories, renames and moves, hard
links, times (`touch`), the read-only attribute (`chmod a-w`) and named
streams (`setfattr -n user.NAME`). Every change is one transaction with a new checkpoint, so a
crash or a pulled cable leaves the volume as it was after the last
completed change. Symbolic links cannot be made. The mount is served by
one thread; it suits copying files to or from a Dev Drive, not heavy
use.

`contrib/mount.refs`, installed as `/sbin/mount.ReFS` (the type blkid
reports) and `/sbin/mount.refs`, lets `mount -t ReFS /dev/sdb2 /mnt`,
`/etc/fstab` entries (`/dev/sdb2 /mnt ReFS ro,nofail 0 0`) and udisks2
(`udisksctl mount -b /dev/sdb2`, desktop file managers) mount ReFS
volumes through `refs mount`, in the background. Mounted by root, every
user may enter the mount; it is read-only unless the options hold both
`rw` and `refs.rw`. Mounted by root, a block device is mounted as type
`fuseblk` (as ntfs-3g mounts are), so udisks2 counts it as the device's
mount: `udisksctl unmount` and the file managers unmount it. Every metadata page is checked against its checksum; a volume that
fails is reported, not guessed at. The data of integrity streams
(`Set-FileIntegrity`, or volumes formatted with integrity streams) is
checked on every read too: damaged data is an error (EIO in the mount),
never returned.

`refs` reads the volume's last checkpoint. A volume that was detached
without being dismounted (the disk image detached, the cable pulled, or
Windows' fast startup) can have newer changes in its log, which Windows
replays when it attaches the volume again; `refs info` says so ("records
past the checkpoint"), and the writing commands refuse such a volume:
attach it to Windows once and take it offline or safely remove it before
writing with `refs`.

Before `refs mount` (and so `mount -t ReFS` and udisks2) mounts a volume,
it runs the quick checks `refs check --quick` prints: the boot sector, the
superblock and its two copies, both checkpoints, and that log. A volume
that is not healthy (such a log makes it suspect) is not mounted unless
asked, read-only:

```sh
refs check --quick /dev/sdb2         # the report, with the evidence
sudo mount -t ReFS -o force /dev/sdb2 /mnt
```

A refused mount says the first failing check and where its report is
(`/run/storage-spaces/reports/refs-<serial>.txt`); udisks2 shows that line.
A volume on a space `spaces attach` attached past its verdict (with
`--degraded` or `--force`) needs `force` too. `refs ls`, `cat`, `stat` and
`info` read a volume that is not healthy and say so.

Changing a file's times or attributes is the first write `refs` does
(experimental; a ReFS volume that is not attached anywhere else; only
with `--yes`, otherwise it prints what would change):

```sh
refs set devdrive.img --path /notes.txt --modified "2024-02-29 12:00:00" --yes
refs set /dev/sdb2 --path /notes.txt --attributes 0x21 --yes   # read-only, archive
refs overwrite devdrive.img --path /data.bin --at 4096 --from patch.bin --yes
refs create devdrive.img --path /notes/todo.txt --from todo.txt --yes
refs rename devdrive.img --path /notes/todo.txt --to done.txt --yes
refs delete devdrive.img --path /notes/done.txt --yes
refs mkdir devdrive.img --path /notes/archive --yes
refs write devdrive.img --path /notes/log.txt --from more.txt --append --yes
refs move devdrive.img --path /notes/log.txt --to /archive/log.txt --yes
refs link devdrive.img --path /archive/log.txt --to /notes/log.txt --yes
refs clone devdrive.img --path /archive/big.iso --to /notes/big.iso --yes
```

`refs overwrite` replaces bytes inside a file (not beyond its end, not in
sparse ranges; the clusters of integrity streams it touches are copied
on write, a compressed file of up to 64 MiB is copied whole) where they
are, as Windows does,
and sets the modification and change times to now. `refs create` makes a
file (up to 1 KiB kept in its record; larger ones in clusters near the
data of the files around it, read from the local file piece by piece,
up to 64 GiB) with a printable ASCII name, in a directory
of any size (its pages split as it grows and merge as it shrinks); it
takes the permissions of the files beside it. `refs rename` renames a
file or directory within its directory and `refs delete` deletes a file
(for a hard-linked file: the one name; also symbolic links, junctions
and files with stream snapshots) or an empty directory; deleting a
file whose clusters other files share (block clones, deduplication)
lowers their reference counts and frees only clusters no other file
has. A file Windows compressed is changed by writing it whole into
ordinary clusters (Windows copies only the clusters it touches); its
compressed clusters lose a reference each, and those no file references
any more are left for Windows' dedup job to reclaim, as Windows leaves
them. `refs mkdir` creates a directory;
`refs write` replaces a file's content (or appends to it with
`--append`), keeping its creation time, attributes and permissions.
New data goes into the volume's data containers; when they are full
`refs` hands out the next free container for data, as Windows does,
and only then uses room in the metadata containers.
`refs move` moves a file or directory into another directory and `refs
link` gives a file another name (a hard link); changing a hard-linked
file through one name changes it for all of them. `refs clone` copies a
file as a block clone, as `Copy-Item` does on a Dev Drive: the copy
shares the file's clusters (counted in the block reference count table)
and takes no room until one of them changes; writing to either later
leaves the other as it was. `refs write --stream
NAME` writes a named stream (an alternate data stream; up to 1 KiB stays
in the file's record) and `refs delete --stream NAME`
deletes one. `refs set --integrity
on` turns integrity streams on for an empty file (as Set-FileIntegrity
does); its data is then written with checksums (CRC32-C per 4 KiB
cluster, CRC-64 per 16 KiB on 64 KiB clusters; up to 2 GiB), and files
created in a directory with
integrity streams have them too.

It writes the way Windows does (copy on write, then a new checkpoint), so
an interruption leaves the volume as it was before.

Read: files (resident, in extents, sparse, block-cloned, deduplicated),
directories of any size, named streams, stream snapshots (`refsutil
streamsnapshot`; `refs cat --snapshot`, not in the mount), hard links,
symbolic links and junctions, attributes and times, volumes with 4 KiB
and 64 KiB clusters, CRC-64 or SHA-256 metadata checksums and integrity
streams, and files Windows compressed with LZ4 or ZSTD (`Enable-ReFSDedup
-Type DedupAndCompress`, `refsutil compression`; checked unit by unit)
(verified on ReFS 3.14 volumes made by Windows 11 and on ReFS 3.7, 3.4,
3.1 and 1.2 volumes made by Windows Server 2022, 2019, 2016 and 2012 R2).
Not verified: ReFS 1.1, 3.2, 3.3, 3.5, 3.6 and 3.8 to 3.13. Not read yet:
encrypted files, ReFS 2.x. Only ReFS 3.14 volumes are
written; the writing commands and `refs mount --rw` refuse other
versions. Compressed files are
changed by writing them whole, see above; their named streams, if
compressed, are not changed.

## Hardening

Everything that parses a disk runs as root, and a disk can be anyone's: a
USB stick plugged in, a pool handed over. So the parts that do it are
limited to what they need (checked on Ubuntu 22.04 with `systemd-analyze
security`, scores 9.5 UNSAFE before, 5.1 MEDIUM after):

* `storage-spaces-attach.service` (installed by `contrib/install.sh`):
  no new privileges, the file system read-only except `/run`, a private
  `/tmp`, no namespaces, no network but local sockets and netlink, only
  the capabilities device-mapper and the module loader need. Home
  directories stay readable, as pool disks may be image files.
* The servers `spaces attach` starts (`serve-ublk`, `serve-nbd`,
  `serve-fuse`, as transient `storage-spaces-<guid>.service` units) get
  the same limits from the program itself, but for the FUSE server a
  private view of the file system, whose mount the host could not see.
* `mount.ReFS` (what `mount -t ReFS` and udisks2 call, as root) runs
  `refs mount` as a transient `refs-mount-<pid>.service` without network,
  new privileges, namespaces or most capabilities. It keeps the host's
  mounts (`PrivateMounts=no`: since systemd 254 a private network would
  otherwise bring a private mount namespace, where the host never sees
  the mount). Without systemd it falls back to a plain process. `refs`
  mounts the block device itself (mount(2)) rather than through the
  setuid helper `fusermount3`, which only users' mounts need.

The limits need systemd 231 or later for the unit file and 247 for the
servers' (older ones refuse the unknown property): then `spaces attach`
says so and starts the server without limits, rather than not at all.
The udev rule starts parsing a disk with the Storage Spaces partition type
without anyone asking (see Installing for turning it off). The code
itself has no `unsafe` (`#![forbid(unsafe_code)]`), is built with integer
overflow checks, and its parsers are fuzzed (`docs/security.md`).

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
* A ReFS volume takes long to mount: the first mount of a large volume on
  hard disks reads its tables (minutes on very large volumes; again after
  a restart, when nothing is cached). `mount -t ReFS` waits for it and
  shows every 10 s how much has been read; udisks2 waits too. Set
  `MOUNT_REFS_TIMEOUT=SECONDS` for a limit.
* AppArmor (Ubuntu) logs `apparmor="DENIED" operation="capable"
  profile="fusermount3" capname="dac_override"` (or `setuid`): other
  programs' FUSE mounts (gvfs, portals) cause these, and they are harmless.
  `refs` does not use `fusermount3` when root mounts a block device.
* `mount.refs: refs mounted the volume, but the mount is not visible here`:
  `refs` ran in another mount namespace (a sandbox around `mount`, an old
  `mount.ReFS` on systemd 254 or later); mount from the host.
