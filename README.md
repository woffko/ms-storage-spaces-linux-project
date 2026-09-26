# Linux Storage Spaces

Read Microsoft Storage Spaces pools (and later ReFS volumes) on Linux, in Rust.

Status: early development. Reading works for simple, mirror (2/3-way),
single and dual parity spaces, fixed and thin provisioning, including data
held in the per-space write-back cache and degraded pools with missing or
failing disks. Spaces can be attached as read-only block devices. Writing
and storage tiers are not supported yet.

## Usage

```sh
cargo build --release
sudo contrib/install.sh            # binary, udev rule, systemd unit

sudo spaces scan                   # find pool members
sudo spaces attach                 # attach every complete pool
sudo spaces status
sudo mount -o ro /dev/mapper/ss-<pool>-<space>-p2 /mnt
sudo spaces detach
```

Each space appears as `/dev/mapper/ss-<pool>-<space>` with one `-p<N>`
device per partition. Backends (`--backend`):

| Backend | Needs | Used by `auto` for |
|---|---|---|
| `dm` | dmsetup | simple spaces whose sector size matches the disks |
| `ublk` | Linux 6.0+, `ublk_drv` | everything else |
| `nbd` | `nbd` module, nbd-client | fallback (e.g. WSL) |
| `fuse` | FUSE, losetup | last resort |

Pools with missing disks are attached only with `--degraded`. Without
installing anything, pools can be inspected and copied out:

```sh
spaces info /dev/sdb /dev/sdc
spaces extents /dev/sdb /dev/sdc --space "My space"
spaces export /dev/sdb /dev/sdc --space "My space" --output space.img
```

Everything is read-only; the tools never write to the pool members.

## Layout

* `crates/storage-spaces` - library: metadata parsing, space layout, reader.
* `crates/spaces-cli` - the `spaces` command.
* `docs/storage-spaces-format.md` - the on-disk format as understood so far.
* `docs/user-guide.md` - installing, attaching, degraded and crashed pools.
* `docs/plan.md` - project plan (stages, milestones, exit criteria).
* `docs/research.md` - prior art.
* `tools/` - scripts that create test pools on a Windows VM and fetch them.

## Tests

Unit tests run with `cargo test`. The corpus tests compare the parser with
Windows' view of pools created by `tools/gen-corpus.sh` and fetched into
`testdata/pools/` by `tools/fetch-corpus.sh` (not in git); they are skipped when
the corpus is absent.

## License

GPL-2.0-or-later.
