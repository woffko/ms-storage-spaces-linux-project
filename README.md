# Linux Storage Spaces

Read Microsoft Storage Spaces pools (and later ReFS volumes) on Linux, in Rust.

Status: early development. Reading works for simple, mirror (2/3-way) and
single-parity spaces, fixed and thin provisioning, including data held in the
per-space write-back cache and degraded pools with missing disks. Writing,
dual parity and storage tiers are not supported yet.

## Usage

```sh
cargo build --release
# Pool members: whole disks, partitions or raw images
spaces info /dev/sdb /dev/sdc
spaces extents /dev/sdb /dev/sdc --space "My space"
spaces export /dev/sdb /dev/sdc --space "My space" --output space.img
```

The exported image can then be attached with `losetup -P` and mounted.
Everything is read-only; the tools never write to the pool members.

## Layout

* `crates/storage-spaces` - library: metadata parsing, space layout, reader.
* `crates/spaces-cli` - the `spaces` command.
* `docs/storage-spaces-format.md` - the on-disk format as understood so far.
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
