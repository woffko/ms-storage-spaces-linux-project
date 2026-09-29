//! Opening the spaces of a pool for writing, writing, discarding and
//! flushing, on the committed metadata fixtures with byte patches from the
//! input (records as in `pool_open`). Hostile metadata must not make the
//! writer panic, and every write must stay inside the pool partition of a
//! member.
#![no_main]

use std::fs::File;
use std::path::Path;
use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;
use storage_spaces::Pool;
use storage_spaces::io::{Overlay, SparseImage};

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
    let count = disks.len();
    for p in data[1..].chunks_exact(8) {
        let disk = &mut disks[p[0] as usize % count];
        let ranges = disk.ranges();
        let (start, len) = ranges[u16::from_le_bytes([p[1], p[2]]) as usize % ranges.len()];
        let at = start + u32::from_le_bytes([p[3], p[4], p[5], p[6]]) as u64 % len as u64;
        disk.insert(at, &[p[7]]);
    }
    let disks: Vec<Overlay<SparseImage>> = disks.into_iter().map(Overlay::new).collect();
    let Ok(pool) = Pool::open(disks.iter().collect::<Vec<_>>()) else {
        return;
    };
    // Where each device may be written: its pool partition.
    let mut allowed = vec![None; count];
    for m in &pool.members {
        allowed[m.device] = Some((m.partition.offset, m.partition.offset + m.partition.length));
    }
    let ids: Vec<u64> = pool.spaces.keys().copied().collect();
    let block = vec![0x5a; 64 << 10];
    for id in ids {
        let Ok(writer) = pool.open_space_rw(id) else { continue };
        let size = writer.size();
        for offset in [0, size / 3, size.saturating_sub(block.len() as u64)] {
            let _ = writer.write_all_at(&block, offset & !4095);
        }
        let _ = writer.discard(0, size.min(1 << 30));
        let _ = writer.flush();
    }
    drop(pool);
    for (disk, allowed) in disks.iter().zip(allowed) {
        for page in disk.written_pages() {
            assert!(
                allowed.is_some_and(|(start, end)| start <= page && page < end),
                "write at {page:#x} outside the pool partition {allowed:x?}"
            );
        }
    }
});
