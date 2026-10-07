//! Whole-pool open, reads and the guard's checks on the committed metadata
//! fixtures with byte patches from the input: (pool, disk, range,
//! position, value) records of 8 bytes each.
#![no_main]

use std::fs::File;
use std::path::Path;
use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;
use storage_spaces::Pool;
use storage_spaces::io::SparseImage;

fn fixtures() -> &'static Vec<Vec<SparseImage>> {
    static POOLS: OnceLock<Vec<Vec<SparseImage>>> = OnceLock::new();
    POOLS.get_or_init(|| {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../crates/storage-spaces/tests/fixtures");
        let mut dirs: Vec<_> = std::fs::read_dir(&root).unwrap().map(|e| e.unwrap().path()).collect();
        dirs.sort();
        dirs.iter()
            .map(|dir| {
                (0..)
                    .map_while(|i| File::open(dir.join(format!("disk{i}.fixture"))).ok())
                    .map(|f| SparseImage::read_from(f).unwrap())
                    .collect()
            })
            .collect()
    })
}

fuzz_target!(|data: &[u8]| {
    let pools = fixtures();
    let Some(&first) = data.first() else { return };
    let mut disks = pools[first as usize % pools.len()].clone();
    for p in data[1..].chunks_exact(8) {
        let disk = &mut disks[p[0] as usize % pools[first as usize % pools.len()].len()];
        let ranges = disk.ranges();
        let (start, len) = ranges[u16::from_le_bytes([p[1], p[2]]) as usize % ranges.len()];
        let at = start + u32::from_le_bytes([p[3], p[4], p[5], p[6]]) as u64 % len as u64;
        disk.insert(at, &[p[7]]);
    }
    let Ok(pool) = Pool::open(disks) else { return };
    let ids: Vec<u64> = pool.spaces.keys().copied().collect();
    let mut buf = vec![0u8; 64 << 10];
    for id in ids {
        let Ok(reader) = pool.open_space(id) else { continue };
        let size = reader.size();
        for offset in [0, size / 3, size.saturating_sub(4096)] {
            let _ = reader.read_exact_at(&mut buf[..4096], offset);
        }
        let _ = reader.read_exact_at(&mut buf, size / 2);
        let _ = reader.segments();
    }
    // The guard's checks of every space (the partition table and NTFS
    // included), reading at most 1 MiB of listed parity stripes.
    let options = storage_spaces::guard::Options {
        journal_budget: Some(1 << 20),
    };
    let _ = storage_spaces::guard::pool_reports(&pool, &[], &options, &mut storage_spaces::guard::ntfs_hook);
});
