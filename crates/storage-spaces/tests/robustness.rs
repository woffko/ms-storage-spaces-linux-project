//! Mutation testing on the metadata fixtures: random corruption of the
//! stored metadata must never make the parser panic or read out of range;
//! errors are fine. Set FUZZ_ITERATIONS for longer runs.

use std::fs::{self, File};
use std::path::Path;

use storage_spaces::Pool;
use storage_spaces::io::{ReadAt, SparseImage};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

fn exercise(disks: Vec<SparseImage>, rng: &mut Rng) {
    let Ok(pool) = Pool::open(disks) else { return };
    let ids: Vec<u64> = pool.spaces.keys().copied().collect();
    for id in ids {
        let Ok(reader) = pool.open_space(id) else { continue };
        let mut buf = vec![0u8; 64 << 10];
        for _ in 0..4 {
            let len = (rng.below(buf.len() as u64) + 1) as usize;
            let offset = rng.below(reader.size().max(1));
            let _ = reader.read_exact_at(&mut buf[..len], offset);
        }
        let _ = reader.segments();
    }
}

#[test]
fn corrupted_metadata_never_panics() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut pools = Vec::new();
    for dir in fs::read_dir(&root).unwrap().map(|e| e.unwrap().path()) {
        let mut disks = Vec::new();
        let mut i = 0;
        while let Ok(f) = File::open(dir.join(format!("disk{i}.fixture"))) {
            disks.push(SparseImage::read_from(f).unwrap());
            i += 1;
        }
        pools.push((dir, disks));
    }
    pools.sort_by(|a, b| a.0.cmp(&b.0));
    let iterations: u64 = std::env::var("FUZZ_ITERATIONS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300);
    let mut rng = Rng(0x5eed_1234_abcd_ef01);
    for iteration in 0..iterations {
        let (dir, original) = &pools[rng.below(pools.len() as u64) as usize];
        let mut disks = original.clone();
        let flips = 1 + rng.below(8);
        for _ in 0..flips {
            let disk = &mut disks[rng.below(original.len() as u64) as usize];
            let ranges = disk.ranges();
            let (start, len) = ranges[rng.below(ranges.len() as u64) as usize];
            let at = start + rng.below(len as u64);
            let mut byte = [0u8];
            disk.read_exact_at(&mut byte, at).unwrap();
            let value = match rng.below(3) {
                0 => byte[0] ^ (1 << rng.below(8)),
                1 => 0xff,
                _ => rng.below(256) as u8,
            };
            disk.insert(at, &[value]);
        }
        let seed = rng.next();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| exercise(disks, &mut Rng(seed | 1))));
        assert!(result.is_ok(), "panic in iteration {iteration} on {}", dir.display());
    }
}

/// Hidden containers whose parents form a cycle (found by fuzzing
/// pool_open) must not make opening a space recurse without end.
#[test]
fn circular_hidden_spaces_do_not_recurse() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mirror2");
    let disks: Vec<SparseImage> = (0..)
        .map_while(|i| File::open(dir.join(format!("disk{i}.fixture"))).ok())
        .map(|f| SparseImage::read_from(f).unwrap())
        .collect();
    let mut pool = Pool::open(disks).unwrap();
    // The dirty region tracking container (role 6) and its child.
    let container = pool
        .spaces
        .values()
        .find(|s| s.info.role == storage_spaces::format::SpaceRole::Other(6))
        .unwrap()
        .id();
    let child = pool.children(container).next().unwrap().id();
    pool.spaces.get_mut(&container).unwrap().info.parent = Some(child);
    let _ = pool.open_space(child);
    let _ = pool.open_space(container);
}
