//! The ReFS writer on hostile metadata: the small fixture of
//! crates/refs/tests (a clean log, no shared clusters), its stored bytes
//! patched by the start of the input (as in refs_volume), then the
//! operations the rest of the input picks (times, attributes, overwriting,
//! creating, writing, renaming, moving, linking and deleting files and
//! directories, also through the names those operations give) on a
//! writable overlay. Fuzzing builds accept every checksum. Whatever
//! the metadata says, nothing may panic and no byte may be written outside
//! the volume.
#![no_main]

use std::sync::LazyLock;

use libfuzzer_sys::fuzz_target;
use refs::{Times, Volume};
use storage_spaces::io::{Overlay, SparseImage};

static BASE: LazyLock<SparseImage> = LazyLock::new(|| {
    SparseImage::read_from(&include_bytes!("../../crates/refs/tests/fixtures/r314small/disk.fixture")[..]).unwrap()
});
const OFFSET: u64 = 16 << 20;
const PATHS: [&str; 11] = [
    "/small.txt",
    "/mid.bin",
    "/last.txt",
    "/dir/inner.txt",
    "/dir",
    "/new.txt",
    "/dir/new",
    "/d2",
    "/renamed",
    "/dir/moved",
    "/linked",
];

fuzz_target!(|data: &[u8]| {
    let Some((&patches, rest)) = data.split_first() else {
        return;
    };
    let mut image = BASE.clone();
    let ranges = image.ranges();
    let stored: u64 = ranges.iter().map(|r| r.1 as u64).sum();
    let (patch_bytes, ops) = rest.split_at((usize::from(patches % 8) * 8).min(rest.len()));
    for p in patch_bytes.chunks_exact(8) {
        let pos = u64::from(u32::from_le_bytes(p[..4].try_into().unwrap())) % stored;
        let (mut left, mut at) = (pos, 0);
        for &(offset, n) in &ranges {
            if left < n as u64 {
                at = offset + left;
                break;
            }
            left -= n as u64;
        }
        image.insert(at, &p[4..]);
    }
    let overlay = Overlay::new(&image);
    let Ok(mut vol) = Volume::open(&overlay, OFFSET) else {
        return;
    };
    let end = OFFSET.saturating_add(vol.boot.volume_size());
    for op in ops.chunks(4).take(12) {
        let path = PATHS[usize::from(op[0]) % PATHS.len()];
        let arg = op.get(1..).unwrap_or(&[]);
        let n = arg.iter().fold(0usize, |a, &b| a * 256 + usize::from(b));
        let now = 133_000_000_000_000_000 + n as u64;
        let _ = match op[0] / 8 % 8 {
            0 => vol.set_times(
                path,
                &Times {
                    created: now,
                    modified: now,
                    changed: now,
                    accessed: now,
                },
            ),
            1 => vol.set_attributes(path, n as u32),
            2 => vol.overwrite(path, (n % 4096) as u64, &arg[..arg.len().min(3)], now),
            3 => vol.create_file(path, &vec![0x5a; n % 6000], now),
            4 => vol.write_file(path, &vec![0xa5; n % 9000], now),
            5 if n % 2 == 0 => vol.rename(path, "renamed", now),
            5 => vol.move_file(path, "/dir/moved", now),
            6 if n % 3 == 0 => vol.link_file(path, "/linked", now),
            6 => vol.delete_file(path, now),
            _ => vol.create_directory(path, now),
        };
    }
    for at in overlay.written_pages() {
        assert!(at >= OFFSET && at < end, "a write at {at:#x}, outside the volume");
    }
});
