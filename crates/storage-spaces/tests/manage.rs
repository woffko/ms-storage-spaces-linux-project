//! Changes to a pool predicted byte for byte (scenario c9ops of
//! tools/scenarios.sh: rename the pool, set media types and usages, set it
//! read-only and back, delete its space, remove it).

use std::fs::File;
use std::path::Path;

use storage_spaces::Pool;
use storage_spaces::database::Database;
use storage_spaces::format::DiskHeader;
use storage_spaces::gpt::PoolDiskTable;
use storage_spaces::io::{ReadAt, SparseImage};
use storage_spaces::manage;

fn state(label: &str) -> Vec<SparseImage> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/scenarios/c9ops")
        .join(label);
    (0..)
        .map_while(|i| File::open(dir.join(format!("disk{i}.fixture"))).ok())
        .map(|f| SparseImage::read_from(f).unwrap())
        .collect()
}

/// The pool database of `disks` (every copy must be the same).
fn database(disks: &[SparseImage]) -> Database {
    let copies: Vec<Database> = disks
        .iter()
        .map(|d| Database::read_formatted(d, (16 << 20) + 0x1000).unwrap())
        .collect();
    assert!(copies.iter().all(|c| c.bytes() == copies[0].bytes()));
    copies.into_iter().next().unwrap()
}

/// The disk id of image `i` (its SPACEDB header's GUID in the database).
fn disk_id(disks: &[SparseImage], i: usize) -> u64 {
    let mut page = [0u8; 4096];
    disks[i].read_exact_at(&mut page, 16 << 20).unwrap();
    let guid = DiskHeader::parse(&page).unwrap().disk_guid;
    let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
    pool.disks.values().find(|d| d.guid == guid).unwrap().id
}

#[test]
fn pool_changes_are_predicted_byte_for_byte() {
    let step = |before: &str, after: &str, change: &dyn Fn(&Database, &[SparseImage], u64) -> Database| {
        let old = state(before);
        let new = state(after);
        let db = database(&old);
        let windows = database(&new);
        let predicted = change(
            &db,
            &old,
            Database::read_formatted(&new[0], (16 << 20) + 0x1000)
                .unwrap()
                .timestamp(),
        );
        assert!(predicted.bytes() == windows.bytes(), "{before} -> {after}");
    };
    step("o0", "o1", &|db, _, t| manage::rename_pool(db, "ss-c9opsx", t).unwrap());
    step("o1", "o2", &|db, d, t| {
        manage::set_disk(db, disk_id(d, 0), Some(2), None, t).unwrap()
    });
    // Two changes, two updates (the first one's time is overwritten).
    step("o2", "o3", &|db, d, t| {
        let db = manage::set_disk(db, disk_id(d, 1), Some(1), None, 0).unwrap();
        manage::set_disk(&db, disk_id(d, 2), None, Some(2), t).unwrap()
    });
    step("o3", "o4", &|db, d, t| {
        manage::set_disk(db, disk_id(d, 2), None, Some(3), t).unwrap()
    });
    step("o4", "o5", &|db, d, t| {
        manage::set_disk(db, disk_id(d, 2), None, Some(1), t).unwrap()
    });
    // Set-StoragePool -IsReadOnly writes nothing.
    for i in 0..3 {
        assert!(
            Database::read_formatted(&state("o5")[i], (16 << 20) + 0x1000)
                .unwrap()
                .bytes()
                == Database::read_formatted(&state("o6")[i], (16 << 20) + 0x1000)
                    .unwrap()
                    .bytes()
        );
    }
    step("o6", "o7", &|db, d, t| {
        let pool = Pool::open(d.iter().collect::<Vec<_>>()).unwrap();
        manage::delete_space(db, pool.find_space("c9opsm").unwrap().id(), t).unwrap()
    });
}

/// Remove-StoragePool leaves the partition table without the pool
/// partition and the SPACEDB header and database behind it.
#[test]
fn a_removed_pool_leaves_the_reserved_partition() {
    let before = state("o7");
    let after = state("o8");
    for (old, new) in before.iter().zip(&after) {
        let mut gpt = vec![0u8; 1024 + 256];
        old.read_exact_at(&mut gpt, 0).unwrap();
        let mixed = |b: &[u8]| storage_spaces::Guid::from_mixed_endian(b.try_into().unwrap());
        let table = PoolDiskTable {
            disk_size: old.size,
            sector: 512,
            disk_guid: mixed(&gpt[512 + 56..512 + 72]),
            msr_guid: mixed(&gpt[1024 + 16..1024 + 32]),
            pool_partition_guid: mixed(&gpt[1024 + 128 + 16..1024 + 128 + 32]),
            pool_name: "ss-c9ops".into(),
        };
        for (offset, bytes) in table.regions_without_pool() {
            let mut windows = vec![0u8; bytes.len()];
            new.read_exact_at(&mut windows, offset).unwrap();
            assert!(windows == bytes, "{offset:#x}");
        }
        // Behind it, unchanged: the header and the database.
        let mut a = vec![0u8; 0x2000];
        let mut b = vec![0u8; 0x2000];
        old.read_exact_at(&mut a, 16 << 20).unwrap();
        new.read_exact_at(&mut b, 16 << 20).unwrap();
        assert!(a == b);
    }
}

/// Resize-VirtualDisk of a parity and a mirror space and a rename
/// (scenario c9resize), with Windows' slabs for the new rows.
#[test]
fn resized_and_renamed_spaces_are_predicted_byte_for_byte() {
    let load = |label: &str| -> Vec<SparseImage> {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/scenarios/c9resize")
            .join(label);
        (0..3)
            .map(|i| SparseImage::read_from(File::open(dir.join(format!("disk{i}.fixture"))).unwrap()).unwrap())
            .collect()
    };
    for (before, after, name) in [("z0", "z1", "c9rp"), ("z1", "z2", "c9rm")] {
        let (old, new) = (load(before), load(after));
        let db = database(&old);
        let windows = database(&new);
        let old_pool = Pool::open(old.iter().collect::<Vec<_>>()).unwrap();
        let new_pool = Pool::open(new.iter().collect::<Vec<_>>()).unwrap();
        let (was, now) = (old_pool.find_space(name).unwrap(), new_pool.find_space(name).unwrap());
        let added: Vec<_> = now
            .extents
            .iter()
            .filter(|e| !was.extents.contains(e))
            .cloned()
            .collect();
        let predicted =
            manage::resize_space(&db, was.id(), now.info.size.unwrap(), &added, windows.timestamp()).unwrap();
        assert!(predicted.bytes() == windows.bytes(), "{before} -> {after}");
    }
    let (old, new) = (load("z2"), load("z3"));
    let db = database(&old);
    let windows = database(&new);
    let pool = Pool::open(old.iter().collect::<Vec<_>>()).unwrap();
    let predicted =
        manage::rename_space(&db, pool.find_space("c9rs").unwrap().id(), "c9rs2", windows.timestamp()).unwrap();
    assert!(predicted.bytes() == windows.bytes());
}
