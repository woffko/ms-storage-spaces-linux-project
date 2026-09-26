# Research: Storage Spaces and ReFS on Linux

Status: initial survey, 2026-09-26. Read support for simple, mirror and
single-parity spaces implemented the same day.

## Goal

Read (and eventually write) Microsoft Storage Spaces pools and ReFS volumes on
Linux, implemented in Rust. ReFS support must also work without Storage Spaces
(plain partitions, Dev Drive, VHDX images).

## Prior art

### Storage Spaces

| Project | Language / license | Scope | Notes |
|---|---|---|---|
| [KimVegetable/StorageSpaceReconstructor](https://github.com/KimVegetable/StorageSpaceReconstructor) | Python, **no license** | Forensic: rebuilds a virtual disk image from member disks | Hard-coded per Windows version and layout (simple, 2/3-way mirror, parity, dual parity). Parses SPACEDB, SDBC, SDBB record types 1-4. Cannot be reused as code; useful as a description of the format. |
| Kim et al., *Digital forensic investigation methodology for Storage Space*, J. Forensic Sci. 2022 | paper | Format description + reconstruction | Same authors as SSR. |
| *A Research on Virtual Disk Reconstruction Method on Windows Storage Space*, J. Digital Forensics 2018 | paper (Korean) | Layout and metadata | |
| [Kyle Song blog series](https://kyl3song.github.io/windows%20storage%20spaces/Windows-Storage-Spaces-Forensics-(aka.-SPACEDB)-part-2/) | blog | Overview, 256 MiB slabs | Little byte-level detail. |
| R-Studio, UFS Explorer, ReclaiMe, DiskInternals | commercial, closed | Recovery / read-only reconstruction | Prove the format is fully tractable for reading. |

There is **no open-source Linux driver or tool that assembles a Storage Spaces
pool as a live block device**, and nothing supports writes.

### ReFS

| Project | Language / license | Versions | Notes |
|---|---|---|---|
| [unsound/refsprogs](https://github.com/unsound/refsprogs) | C, GPL-2.0+ | 1.x and 3.x | Erik Larsson (ntfs-3g/Tuxera). Read-only FUSE driver + tools (`refsls`, `refscat`, `redb`, ...). Active (Sep 2026). ~29 kLoC. |
| [libyal/libfsrefs](https://github.com/libyal/libfsrefs) | C, LGPL-3.0+ | 1, 2, 3 | Joachim Metz. Read-only, experimental. Has an AsciiDoc format description. |
| [xbqt/forefst](https://github.com/xbqt/forefst) | Python, GPL-3.0 | 3.4 - 3.14 | Forensic tool plus the most complete public **byte-level format reference** (25 structure pages, 34 concept pages, 489 verified claims; site: xbpt.gitlab.io/forefst). Covers checkpoints, Minstore B+-trees, container table / virtual addressing, allocators, refcounts, checksums, compression, dedup, snapshots, MLog. |
| Paragon ReFS for Linux | closed kernel module | 1.x, 3.x (up to Win10/2016-era) | Read/write, proprietary. |

No open-source implementation writes ReFS 3.x.

## Observations on the test VM (Windows 11 Pro Insider 26340)

- `refs.sys` / `spaceport.sys` 10.0.26100.8951. Storage pool format **version 29**.
- `Format-Volume -DevDrive` is available, so ReFS volumes can be created on
  Windows 11 Pro through Dev Drive.
- Test pool "Storage pool": two 64 GiB SATA disks, one space `test_ubuntu`
  (Simple, 1 column, thin, 125 GiB, interleave 256 KiB) containing GPT + NTFS.

The on-disk format as reverse-engineered so far is documented in
[storage-spaces-format.md](storage-spaces-format.md). Notable findings beyond
prior work: per-space metadata databases, hidden child spaces, and the
per-space write-back cache (SPCACHE) that keeps data of thin spaces even after
a clean detach.

## What has to be built

### Storage Spaces (`spaces` crates)

1. Metadata parser: SPACEDB, SDBC, SDBB reassembly, record decoding for all
   record types, versioning across pool versions (Win8 .. v29+), choosing the
   newest consistent database copy across members.
2. Extent map: space -> columns -> copies -> (disk, slab) mapping; simple,
   2/3-way mirror, single parity, dual parity; interleave and column count;
   thin provisioning (holes); storage tiers; enclosure awareness is irrelevant.
3. Degraded-mode reads (missing member, stale copies, parity reconstruction).
4. Exposure on Linux, in order:
   - `spaces dump/info` CLI and image export (like SSR, but generic);
   - device-mapper table generation (dm-linear/dm-stripe/dm-mirror) for simple
     and mirror spaces - kernel speed, zero-copy;
   - userspace block device via **ublk** (`libublk` crate) for everything else
     (parity, thin allocation on write, degraded modes), NBD as fallback.
5. Writes: in-place writes to already allocated slabs are just RAID writes
   (mirror: all copies; parity: read-modify-write, write hole). Allocation of new
   slabs on thin spaces, dirty region tracking (DRT) and metadata updates are the
   risky part and come last.

### ReFS (`refs` crates)

1. Read-only core for 3.x (3.4 - 3.14): VBR, SUPB, CHKP, container table
   (virtual -> physical cluster translation), object table, Minstore B+-tree
   rows, directories, file records, attributes, data runs, sparse files, ADS,
   reparse points, security, checksums (CRC32C / CRC64) verification.
2. Compression (Windows 11 24H2+ ReFS supports LZ4 / ZSTD), dedup, block clone
   refcounts, integrity streams, snapshots: read support.
3. FUSE mount via `fuser`; library usable standalone for image tools.
4. ReFS 1.x/2.x: separate, lower priority.
5. Writes: copy-on-write B+-tree updates, allocators, refcount tree,
   checkpoint rotation, MLog consistency, checksums. Start with "overwrite
   existing allocated data in place" (only safe when integrity streams are off),
   then CoW metadata updates. Every write feature must be validated by Windows
   (`chkdsk`, `refsutil`, `Repair-Volume -Scan`) on the VM.

## Test strategy

- Generate pools and ReFS volumes on the Windows VM using VHDX files
  (`diskpart create vdisk` works without Hyper-V) so many layouts (columns,
  interleave, parity, tiers, pool versions) can be produced by script and copied
  to Linux as images.
- Golden data: known file trees with hashes written by Windows, verified after
  Linux reads; for writes, Linux-written images are attached back to Windows and
  checked with Windows tools.
- Keep a small corpus of images in a separate storage location (not in git).
