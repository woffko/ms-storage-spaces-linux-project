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
        // Editing the table on the disk gives the same.
        let edits = storage_spaces::gpt::remove_partitions(old, storage_spaces::gpt::STORAGE_SPACES_PARTITION_TYPE)
            .unwrap()
            .unwrap();
        for (offset, bytes) in edits {
            let mut windows = vec![0u8; bytes.len()];
            new.read_exact_at(&mut windows, offset).unwrap();
            assert!(windows == bytes, "edited {offset:#x}");
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

fn c9disk(label: &str) -> Vec<SparseImage> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/scenarios/c9disk")
        .join(label);
    (0..)
        .map_while(|i| File::open(dir.join(format!("disk{i}.fixture"))).ok())
        .map(|f| SparseImage::read_from(f).unwrap())
        .collect()
}

/// Every page of `actual` equals `base` with `writes` applied.
fn assert_writes(what: &str, actual: &SparseImage, base: Option<&SparseImage>, writes: &[(u64, Vec<u8>)]) {
    let mut predicted = SparseImage::new(actual.size);
    if let Some(base) = base {
        for (offset, len) in base.ranges() {
            let mut b = vec![0u8; len];
            base.read_exact_at(&mut b, offset).unwrap();
            predicted.insert(offset, &b);
        }
    }
    for (offset, bytes) in writes {
        predicted.insert(*offset, bytes);
    }
    let mut pages = std::collections::BTreeSet::new();
    for image in [actual, &predicted] {
        for (offset, len) in image.ranges() {
            pages.extend((offset / 4096..(offset + len as u64).div_ceil(4096)).map(|p| p * 4096));
        }
    }
    for page in pages {
        let (mut a, mut p) = (vec![0u8; 4096], vec![0u8; 4096]);
        actual.read_exact_at(&mut a, page).unwrap();
        predicted.read_exact_at(&mut p, page).unwrap();
        assert!(a == p, "{what}: page {page:#x} differs");
    }
}

/// Add-PhysicalDisk of a blank disk to a pool of three (c9disk d0 -> d1):
/// the pool database (two updates), the databases of the metadata space on
/// every copy, and every page of the new disk are predicted byte for byte.
#[test]
fn an_added_disk_is_predicted_byte_for_byte() {
    use storage_spaces::create::{NewDisk, SPACE_DATABASE_STRIDE};
    use storage_spaces::records::DiskBody;
    let (old, new) = (c9disk("d0"), c9disk("d1"));
    let old_pool = Pool::open(old.iter().collect::<Vec<_>>()).unwrap();
    let new_pool = Pool::open(new.iter().collect::<Vec<_>>()).unwrap();
    let db = database(&old);
    let windows = database(&new);
    // The new disk, as Windows set it up.
    let added = &new[3];
    let mut gpt = vec![0u8; 1024 + 256];
    added.read_exact_at(&mut gpt, 0).unwrap();
    let mixed = |b: &[u8]| storage_spaces::Guid::from_mixed_endian(b.try_into().unwrap());
    let mut page = [0u8; 4096];
    added.read_exact_at(&mut page, 16 << 20).unwrap();
    let header = DiskHeader::parse(&page).unwrap();
    let record = storage_spaces::format::assemble_records(windows.bytes(), 0x40)
        .unwrap()
        .into_iter()
        .filter(|r| r.kind == 2)
        .map(|r| DiskBody::decode(&r.body).unwrap())
        .find(|d| d.guid == header.disk_guid)
        .unwrap();
    let disk = NewDisk {
        size: added.size,
        sector: 512,
        guid: header.disk_guid,
        gpt_disk_guid: mixed(&gpt[512 + 56..512 + 72]),
        msr_guid: mixed(&gpt[1024 + 16..1024 + 32]),
        partition_guid: mixed(&gpt[1024 + 128 + 16..1024 + 128 + 32]),
        joined: header.format_time,
        manufacturer: record.manufacturer.clone(),
        model: record.model.clone(),
        database_copy: true,
    };
    // The times of the databases in the metadata space.
    let meta = new_pool
        .spaces
        .values()
        .find(|s| s.info.role == storage_spaces::format::SpaceRole::Metadata)
        .unwrap();
    let e = &meta.extents[0];
    let (device, at) = new_pool.slab_location(e.disk_id, e.physical_slab).unwrap().unwrap();
    let times = |number: u64| {
        let mut h = [0u8; 0x50];
        new[device]
            .read_exact_at(&mut h, at + number * SPACE_DATABASE_STRIDE)
            .unwrap();
        u64::from_be_bytes(h[0x48..0x50].try_into().unwrap())
    };
    let plan = storage_spaces::manage::add_disk(&old_pool, &db, &disk, record.id, windows.timestamp(), times).unwrap();
    assert!(plan.database.bytes() == windows.bytes());
    for (i, disk) in old.iter().enumerate() {
        let writes: Vec<(u64, Vec<u8>)> = plan
            .members
            .iter()
            .filter(|(d, _, _)| *d == i)
            .map(|(_, o, b)| (*o, b.clone()))
            .collect();
        assert_writes(&format!("disk {i}"), &new[i], Some(disk), &writes);
    }
    assert_writes("the new disk", added, None, &plan.new_disk);
    // The plan's steps give the same.
    let members: Vec<storage_spaces::io::Overlay<&SparseImage>> =
        old.iter().map(storage_spaces::io::Overlay::new).collect();
    let blank = SparseImage::new(added.size);
    let fresh = [storage_spaces::io::Overlay::new(&blank)];
    plan.plan.apply(&members, &fresh).unwrap();
    for (i, m) in members.iter().enumerate() {
        for (offset, len) in new[i].ranges() {
            let (mut a, mut b) = (vec![0u8; len], vec![0u8; len]);
            new[i].read_exact_at(&mut a, offset).unwrap();
            m.read_exact_at(&mut b, offset).unwrap();
            assert!(a == b, "disk {i} at {offset:#x}");
        }
    }
    for (offset, len) in added.ranges() {
        let (mut a, mut b) = (vec![0u8; len], vec![0u8; len]);
        added.read_exact_at(&mut a, offset).unwrap();
        fresh[0].read_exact_at(&mut b, offset).unwrap();
        assert!(a == b, "the new disk at {offset:#x}");
    }
}

/// Remove-PhysicalDisk of the retired, repaired disk (c9disk d4 -> d5): the
/// pool database of the remaining disks and the removed disk's partition
/// table are predicted byte for byte; nothing else changes.
#[test]
fn a_removed_disk_is_predicted_byte_for_byte() {
    let old = c9disk("d4");
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/scenarios/c9disk/d5");
    let new: Vec<SparseImage> = (0..3)
        .map(|i| SparseImage::read_from(File::open(dir.join(format!("disk{i}.fixture"))).unwrap()).unwrap())
        .collect();
    let db = Database::read_formatted(&old[1], (16 << 20) + 0x1000).unwrap();
    let windows = database(&new);
    let predicted = manage::remove_disk(&db, disk_id(&old, 0), windows.timestamp()).unwrap();
    assert!(predicted.bytes() == windows.bytes());
    for (i, disk) in new.iter().enumerate() {
        let writes = vec![((16u64 << 20) + 0x1000, predicted.bytes().to_vec())];
        assert_writes(&format!("disk {}", i + 1), disk, Some(&old[i + 1]), &writes);
    }
    // A disk that still holds data is refused.
    assert!(manage::remove_disk(&db, disk_id(&old, 1), 0).is_err());
}

/// Taking the pool partition out of a partition table writes where the
/// table's own headers say; a crafted table must not direct those writes
/// into the disk's data (found by the fuzz target `manage`: a backup header
/// claimed near 2^64 overflowed, and any header or entry array placed in the
/// middle of the disk would have been rewritten there). Windows' tables are
/// rewritten; crafted ones are refused.
#[test]
fn crafted_partition_tables_are_not_rewritten() {
    use storage_spaces::Guid;
    use storage_spaces::gpt::{STORAGE_SPACES_PARTITION_TYPE, remove_partitions};
    let table = PoolDiskTable {
        disk_size: 8 << 30,
        sector: 512,
        disk_guid: Guid([1; 16]),
        msr_guid: Guid([2; 16]),
        pool_partition_guid: Guid([3; 16]),
        pool_name: "crafted".into(),
    };
    let disk = || {
        let mut d = SparseImage::new(8 << 30);
        for (offset, bytes) in table.regions() {
            d.insert(offset, &bytes);
        }
        d
    };
    let writes = remove_partitions(&disk(), STORAGE_SPACES_PARTITION_TYPE)
        .unwrap()
        .unwrap();
    let last = (8u64 << 30) - 512;
    assert!(
        writes
            .iter()
            .all(|(at, b)| *at + b.len() as u64 <= 1 << 20 || *at >= last - (1 << 20))
    );
    // (field offset in the primary header at sector 1, value)
    let middle = (4u64 << 30) / 512;
    for (field, value) in [(32usize, u64::MAX), (32, middle), (24, middle), (72, middle), (72, 0)] {
        let mut d = disk();
        d.insert(512 + field as u64, &value.to_le_bytes());
        assert!(
            remove_partitions(&d, STORAGE_SPACES_PARTITION_TYPE).is_err(),
            "field {field:#x} = {value:#x}"
        );
    }
    // The backup header placed elsewhere than the last sector.
    let mut d = disk();
    d.insert(last + 24, &middle.to_le_bytes());
    assert!(remove_partitions(&d, STORAGE_SPACES_PARTITION_TYPE).is_err());
}
