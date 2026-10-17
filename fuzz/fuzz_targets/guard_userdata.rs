//! The guard on a space's contents, which whoever formats a space writes:
//! the input (after its first byte, which picks a pool of the committed
//! fixtures: 512-byte or 4 KiB sectors, simple, mirror or parity) becomes
//! the first 64 KiB of the pool's space and what follows its last 64 KiB,
//! written through the space writer; then the guard's report on the space
//! with the file system checks attach makes (the partition table, NTFS,
//! the ReFS quick checks of every partition it names, or of the space
//! itself). Nothing may panic, and the checks must end within seconds.
#![no_main]

use std::fs::File;
use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use libfuzzer_sys::fuzz_target;
use storage_spaces::gpt::Partition;
use storage_spaces::guard::{self, Options};
use storage_spaces::io::{Overlay, ReadAt, SparseImage};
use storage_spaces::report::Check;
use storage_spaces::{Pool, SpaceReader};

const BASES: [&str; 4] = ["simple1c", "sect4k", "mirror2", "parity3"];
const PART: usize = 64 << 10;

fn bases() -> &'static Vec<Vec<SparseImage>> {
    static POOLS: OnceLock<Vec<Vec<SparseImage>>> = OnceLock::new();
    POOLS.get_or_init(|| {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../crates/storage-spaces/tests/fixtures");
        BASES
            .iter()
            .map(|name| {
                (0..)
                    .map_while(|i| File::open(root.join(name).join(format!("disk{i}.fixture"))).ok())
                    .map(|f| SparseImage::read_from(f).unwrap())
                    .collect()
            })
            .collect()
    })
}

/// The file system checks of `spaces attach` (without --deep).
fn hook<D: ReadAt>(r: &SpaceReader<'_, D>, p: &Partition, names: &[String]) -> Vec<Check> {
    let label = format!("p{}", p.number);
    let locate = |o: u64| r.describe(o, names);
    if let Some(c) = guard::ntfs_check(r, p.offset, p.length, &label, &locate) {
        return vec![c];
    }
    refs::health::quick_checks_in(r, p.offset, Some(p.length), &label, &locate)
}

fuzz_target!(|data: &[u8]| {
    let Some((&which, rest)) = data.split_first() else {
        return;
    };
    let disks: Vec<Overlay<SparseImage>> = bases()[usize::from(which) % BASES.len()]
        .iter()
        .map(|d| Overlay::new(d.clone()))
        .collect();
    let pool = Pool::open(disks).expect("the fixture opens");
    let id = pool.user_spaces().next().expect("a user space").id();
    {
        let writer = pool.open_space_rw(id).expect("the fixture's space takes writes");
        let size = writer.size();
        let (first, last) = rest.split_at(rest.len().min(PART));
        writer.write_all_at(first, 0).expect("written");
        let last = &last[..last.len().min(PART)];
        if !last.is_empty() {
            writer.write_all_at(last, size - PART as u64).expect("written");
        }
        writer.flush().expect("flushed");
    }
    let options = Options {
        journal_budget: Some(1 << 20),
    };
    let started = Instant::now();
    let report = guard::space_report(&pool, &pool.spaces[&id], &[], &options, &mut hook);
    let _ = report.to_text();
    let took = started.elapsed();
    assert!(took < Duration::from_secs(10), "the checks took {took:?}");
});
