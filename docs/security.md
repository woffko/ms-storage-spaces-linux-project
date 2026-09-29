# Security review

Scope: the `spaces` command and the `storage-spaces` library as of the
write support (0.2.0). Reviewed 2026-09-29.

## Threat model

* **Untrusted disks.** Any disk with a Storage Spaces partition can be
  plugged in; the udev rule then runs `spaces attach` as root, which parses
  every piece of metadata on it (GPT, SPACEDB header, pool database,
  records, write-back cache and its checkpoints, parity journal, dirty region
  log). Hostile metadata must not corrupt memory, exhaust memory or CPU,
  make the tool write anywhere, or expose data to other users.
* **Local users.** Attached spaces and the processes serving them must not
  give users without access to the member disks access to their contents.
* **Writes.** Writing happens only when asked for (`--rw`); on a pool whose
  metadata was crafted to mislead, writes must still stay inside the pool
  partitions of its member disks.

## Findings and mitigations

| Area | Status |
|---|---|
| Memory safety | No `unsafe` code in either crate (`grep -rn unsafe crates/`); parsers index with checked slices or `get`. |
| Parsers on hostile input | cargo-fuzz targets: `partitions`, `pool_open`, `record_decode`, `database` (update model, page growth, extent record round trip), `cache_index` (slots, checkpoints, writer model with wraps and destaging), `parity_journal` (slots, checkpoints, writer model), `dirty_regions` (writer model), `pool_write` (every space of a patched pool opened for writing, written, discarded and flushed; see "Where writes go"). A 24 h run is part of the release checklist. |
| Memory exhaustion | Every size read from disk is bounded before allocating: database ≤ 64 MiB and ≤ 1024 formatted pages, cache and journal slot areas ≤ 64 MiB, checkpoints ≤ 16 MiB (found in this review: a crafted checkpoint could claim the 125 MiB of a journal area per copy; now capped), checkpoint area offsets ≤ 2^56 for the cache and the parity journal (found by fuzzing: a journal area offset near 2^64 overflowed), slab and extent numbers < 2^32. |
| Command execution | External programs (`dmsetup`, `systemd-run`, `nbd-client`, `losetup`, `udevadm`, `journalctl`) get separate arguments, never a shell; the one `sh -c` checks fixed program names. Device-mapper names are made of `[A-Za-z0-9_.+]` only, whatever the pool and space are called. |
| Where writes go | Every slab write resolves through `Pool::slab_location`, which refuses slabs beyond the member's pool partition; database copies go to the fixed offset in that partition. A misleading pool can at worst overwrite its own partitions, which `--rw` asked for. Checked by the fuzz target `pool_write` and the test `writes_on_corrupted_metadata_stay_in_the_pool_partitions`: whatever the metadata, no write lands outside a member's pool partition. Found by `pool_write`: a user space whose record claims a part of an address space, as only tiers do, made writes start below its layout (a panic); such spaces are now refused for writing. |
| What is written | `--rw` is refused unless the pool is clean and every structure the writes touch is understood (see the user guide); the udev rule and `storage-spaces-attach.service` never pass `--rw`. |
| Exposure to other users | Block devices (`/dev/mapper/ss-*`, `/dev/ublkb*`, `/dev/nbd*`, loop devices) are created by the kernel as root:disk 0660. NBD sockets are made owner-only (0600) right after they are bound, before connections are accepted (found in this review: they followed the caller's umask); the attach path keeps them in `/run/storage-spaces` (root, 0755). The FUSE mount is read-only and without `allow_other`, so only root can open it. State files are root's. |
| Denial of service by panics | A panic ends the attach or serving process for that pool only; the fuzz targets check the parsers do not panic. |

## Remaining risks

* A hostile pool attached read-only can make reads fail or return its own
  (hostile) data; that is inherent in reading it.
* Writing trusts the pool it was asked to write: with `--rw` on a crafted
  pool, the data of its spaces can end up anywhere inside its partitions.
