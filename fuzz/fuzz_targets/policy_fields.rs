//! Numbers in a pool's records at the edges of their types: the first byte
//! picks a pool of the committed fixtures, then up to four edits of three
//! bytes each pick a field (of a space record: size, columns, copies,
//! redundancy, local groups, interleave, allocation unit; of an extent
//! record: slabs, virtual slab, column, copy, physical slab, disk, stale
//! marker), a record of that kind and a value from a list of boundary
//! values. The records are rewritten in every copy of the pool database;
//! then the pool is opened, its health worked out, every space opened with
//! its parts (cache, parity journal, dirty region log), its listed parity
//! stripes checked, its device-mapper table made and a sector read, and the
//! guard reports on the pool. Nothing may panic, and all of it must end
//! within seconds (in 1.2.1 such numbers made it loop for minutes or
//! overflow).
#![no_main]

use std::fs::File;
use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use libfuzzer_sys::fuzz_target;
use storage_spaces::database::Database;
use storage_spaces::format::{RawRecord, Record, assemble_records, read_database};
use storage_spaces::gpt::find_spaces_partition;
use storage_spaces::guard::{self, Options};
use storage_spaces::io::SparseImage;
use storage_spaces::records::SpaceBody;
use storage_spaces::{OpenOptions, Pool, SpaceReader};

const BASES: [&str; 8] = [
    "simple2c", "mirror2", "mirror3", "parity3", "dual7", "lrc11", "tiered", "wc64",
];

/// Boundary values: small counts, powers of two and their neighbours, the
/// ends of u8, u16, u32 and u64, the limits of `storage_spaces::limits`.
const VALUES: [u64; 32] = [
    0,
    1,
    2,
    3,
    4,
    7,
    8,
    15,
    16,
    17,
    31,
    32,
    33,
    63,
    64,
    255,
    256,
    1023,
    1024,
    1025,
    0xffff,
    1 << 16,
    (1 << 31) - 1,
    1 << 31,
    0xffff_ffff,
    1 << 32,
    1 << 36,
    1 << 56,
    1 << 60,
    1 << 62,
    u64::MAX - 1,
    u64::MAX,
];

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

/// Where the copy of the pool database of `disk` is, if it has one.
fn copy_at(disk: &SparseImage) -> Option<u64> {
    let at = find_spaces_partition(disk).ok()??.offset + 0x1000;
    matches!(read_database(disk, at), Ok(Some(_))).then_some(at)
}

/// Record `r` with field `field` (0-6 of a space record, 7-13 of an extent
/// record) set to `value`: its new body, if the field is of its kind.
fn edit(r: &RawRecord, field: u8, value: u64, sequence: u64) -> Option<Vec<u8>> {
    if field < 7 {
        if r.kind != 3 && r.kind != 6 {
            return None;
        }
        let mut s = SpaceBody::decode(r.kind == 6, &r.body).ok()?;
        match field {
            0 => s.size = value,
            1 => s.columns = value,
            2 => s.copies = value,
            3 => s.redundancy = value,
            4 => s.groups = value,
            5 => s.interleave_log2 = value as u8,
            _ => s.allocation_unit = value,
        }
        s.encode().ok()
    } else {
        let Ok(Record::Extent(mut e)) = Record::decode(r) else {
            return None;
        };
        match field {
            7 => e.slab_count = value,
            8 => e.virtual_slab = value,
            9 => e.column = value,
            10 => e.copy = value,
            11 => e.physical_slab = value,
            12 => e.disk_id = value,
            _ => e.stale_marker = value,
        }
        Some(e.encode(sequence))
    }
}

fuzz_target!(|data: &[u8]| {
    let Some((&which, rest)) = data.split_first() else {
        return;
    };
    let mut disks = bases()[usize::from(which) % BASES.len()].clone();
    let Some(db) = disks
        .iter()
        .filter_map(|d| Database::read_formatted(d, copy_at(d)?).ok())
        .max_by_key(Database::sequence)
    else {
        return;
    };
    let records = assemble_records(db.bytes(), 0x40).expect("the fixture's records");
    let sequence = db.sequence() + 1;
    let mut writes: Vec<(u8, u8, Vec<u8>)> = Vec::new();
    let mut frees = Vec::new();
    for e in rest.chunks_exact(3).take(4) {
        let field = e[0] % 14;
        let value = VALUES[usize::from(e[2]) % VALUES.len()];
        let fits: Vec<&RawRecord> = records
            .iter()
            .filter(|r| !frees.contains(&r.id) && edit(r, field, 0, sequence).is_some())
            .collect();
        let Some(r) = fits.get(usize::from(e[1]) % fits.len().max(1)) else {
            continue;
        };
        if let Some(body) = edit(r, field, value, sequence) {
            writes.push((r.kind, r.version, body));
            frees.push(r.id);
        }
    }
    let w: Vec<(u8, u8, &[u8])> = writes.iter().map(|(k, v, b)| (*k, *v, b.as_slice())).collect();
    let Ok((mut next, _)) = db.updated(&w, &frees) else {
        return;
    };
    next.commit(sequence, db.timestamp());
    for d in disks.iter_mut() {
        if let Some(at) = copy_at(d) {
            d.insert(at, next.bytes());
        }
    }
    let started = Instant::now();
    let Ok(pool) = Pool::open(disks) else {
        return;
    };
    let _ = storage_spaces::health::health(&pool);
    let mut sector = [0u8; 4096];
    for id in pool.spaces.keys().copied().collect::<Vec<_>>() {
        let Ok(reader) = SpaceReader::open_parts(&pool, id, OpenOptions::default()) else {
            continue;
        };
        let _ = reader.check_unclean_parity(Some(1 << 20));
        let _ = reader.segments();
        let _ = reader.read_exact_at(&mut sector, reader.size() / 2 & !4095);
    }
    let options = Options {
        journal_budget: Some(1 << 20),
    };
    let _ = guard::pool_reports(&pool, &[], &options, &mut guard::ntfs_hook);
    let took = started.elapsed();
    assert!(took < Duration::from_secs(10), "the checks took {took:?}");
});
