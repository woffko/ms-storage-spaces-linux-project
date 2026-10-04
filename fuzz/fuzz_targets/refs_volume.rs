//! A ReFS volume read end to end: the partition table, boot sector,
//! superblock, checkpoint, container and object tables, directories,
//! file records, extents, streams and reparse points, and compressed
//! (compacted) containers, and checked as `refs check` does. The base is
//! one of six fixtures of
//! crates/refs/tests (64 KiB clusters; 4 KiB clusters with integrity
//! streams; stream snapshots and deduplicated files; LZ4 and ZSTD
//! compression; ReFS 3.4 with its older file records), chosen by the
//! first byte; the rest of the
//! input patches their stored bytes: a u32 position (modulo the stored
//! bytes), a length byte (1 to 16) and the bytes. Fuzzing builds of the
//! `refs` crate accept every checksum, so patched pages reach the parsers.
#![no_main]

use std::sync::LazyLock;

use libfuzzer_sys::fuzz_target;
use refs::{Target, Volume};
use storage_spaces::io::{ReadAt, SparseImage};

static BASES: LazyLock<[SparseImage; 6]> = LazyLock::new(|| {
    [
        &include_bytes!("../../crates/refs/tests/fixtures/r314basic64k/disk.fixture")[..],
        &include_bytes!("../../crates/refs/tests/fixtures/r314integ/disk.fixture")[..],
        &include_bytes!("../../crates/refs/tests/fixtures/r314feat/disk.fixture")[..],
        &include_bytes!("../../crates/refs/tests/fixtures/r314compress/disk.fixture")[..],
        &include_bytes!("../../crates/refs/tests/fixtures/r314zstd/disk.fixture")[..],
        &include_bytes!("../../crates/refs/tests/fixtures/r34integ/disk.fixture")[..],
    ]
    .map(|b| SparseImage::read_from(b).unwrap())
});

/// Byte offsets of the ReFS volumes: the device itself, or its partitions.
fn volumes(dev: &SparseImage) -> Vec<u64> {
    let mut sector = [0u8; 512];
    let mut found = Vec::new();
    if dev.read_exact_at(&mut sector, 0).is_ok() && refs::boot::BootSector::is_refs(&sector) {
        found.push(0);
    }
    for p in storage_spaces::gpt::read_partitions(dev, 512).unwrap_or_default() {
        if dev.read_exact_at(&mut sector, p.offset).is_ok() && refs::boot::BootSector::is_refs(&sector) {
            found.push(p.offset);
        }
    }
    found.truncate(2);
    found
}

fuzz_target!(|data: &[u8]| {
    let Some((&which, mut patches)) = data.split_first() else {
        return;
    };
    let mut image = BASES[usize::from(which) % BASES.len()].clone();
    let ranges = image.ranges();
    let stored: u64 = ranges.iter().map(|r| r.1 as u64).sum();
    while patches.len() >= 6 {
        let pos = u64::from(u32::from_le_bytes(patches[..4].try_into().unwrap())) % stored;
        let len = usize::from(patches[4] % 16) + 1;
        let bytes = &patches[5..(5 + len).min(patches.len())];
        patches = &patches[(5 + len).min(patches.len())..];
        let (mut left, mut at) = (pos, 0);
        for &(offset, n) in &ranges {
            if left < n as u64 {
                at = offset + left;
                break;
            }
            left -= n as u64;
        }
        image.insert(at, bytes);
    }
    for offset in volumes(&image) {
        let Ok(vol) = Volume::open(&image, offset) else {
            continue;
        };
        // Breadth first, a bounded number of entries.
        let mut dirs = vec![refs::volume::ROOT_DIRECTORY];
        let mut seen = 0;
        let mut buf = vec![0u8; 4096];
        while let Some(oid) = dirs.pop() {
            let Ok(entries) = vol.read_dir(oid) else {
                continue;
            };
            for e in entries {
                seen += 1;
                if seen > 300 {
                    return;
                }
                if let Target::Directory(child) = e.target
                    && dirs.len() < 16
                {
                    dirs.insert(0, child);
                }
                let Ok(file) = vol.open_file(&e) else {
                    continue;
                };
                let named = file.streams.iter().chain(&file.snapshots).map(|(_, s)| s);
                for s in file.data.iter().chain(named) {
                    let _ = vol.read_stream(s, 0, &mut buf);
                    let _ = vol.read_stream(s, s.size.saturating_sub(100), &mut buf);
                    let _ = vol.read_stream(s, s.size / 2, &mut buf[..1]);
                }
                if let Some(r) = &file.reparse {
                    let _ = r.link_target();
                }
            }
        }
        for path in ["/deep/a/b/c", "/links/sym_dir", "/snap/file.txt", "/nothing/at/all"] {
            let _ = vol.lookup(path);
        }
        // What `refs check` reads: every page, allocator and reference
        // count, the older checkpoint's pages.
        let _ = vol.check(&[]);
    }
});
