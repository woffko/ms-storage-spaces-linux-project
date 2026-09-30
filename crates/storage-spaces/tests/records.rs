//! The complete record models (`storage_spaces::records`) reproduce every
//! pool, disk and space record of every pool Windows created, byte for byte:
//! the metadata fixtures of the corpus and every state of the scenarios.

use std::fs::{self, File};
use std::path::{Path, PathBuf};

use storage_spaces::Pool;
use storage_spaces::io::SparseImage;
use storage_spaces::records::{DiskBody, PoolBody, SpaceBody};

/// Directories holding `disk<N>.fixture` files.
fn pool_dirs() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut dirs = Vec::new();
    let mut todo = vec![root.join("fixtures"), root.join("scenarios")];
    while let Some(dir) = todo.pop() {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for e in entries.map(|e| e.unwrap().path()) {
            if e.is_dir() {
                todo.push(e);
            } else if e.file_name().is_some_and(|n| n == "disk0.fixture") {
                dirs.push(dir.clone());
            }
        }
    }
    dirs.sort();
    dirs
}

#[test]
fn every_record_windows_wrote_is_reproduced_byte_for_byte() {
    let mut counts = [0usize; 7];
    for dir in pool_dirs() {
        let disks: Vec<SparseImage> = (0..)
            .map_while(|i| File::open(dir.join(format!("disk{i}.fixture"))).ok())
            .map(|f| SparseImage::read_from(f).unwrap())
            .collect();
        let Ok(pool) = Pool::open(disks) else { continue };
        for m in &pool.members {
            let Some((_, records)) = pool.database_copy(m.device).unwrap() else {
                continue;
            };
            for r in records {
                let at = || {
                    format!(
                        "{} device {} record {} (type {} v{})",
                        dir.display(),
                        m.device,
                        r.id,
                        r.kind,
                        r.version
                    )
                };
                let encoded = match r.kind {
                    1 => PoolBody::decode(r.version, &r.body).map(|p| p.encode().unwrap()),
                    2 => DiskBody::decode(&r.body).map(|d| d.encode()),
                    3 | 6 => SpaceBody::decode(r.kind == 6, &r.body).map(|s| s.encode().unwrap()),
                    _ => continue,
                };
                let encoded = encoded.unwrap_or_else(|e| panic!("{}: {e}", at()));
                assert_eq!(encoded, r.body, "{}", at());
                counts[r.kind as usize] += 1;
            }
        }
    }
    // Every kind is covered many times over.
    assert!(
        counts[1] > 100 && counts[2] > 300 && counts[3] > 300 && counts[6] > 100,
        "{counts:?}"
    );
}

/// What the models decode matches what Windows reports and the reader
/// already knows, on a pool of each record layout.
#[test]
fn decoded_fields_are_the_known_ones() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mirror2_26100");
    let disks: Vec<SparseImage> = (0..2)
        .map(|i| SparseImage::read_from(File::open(dir.join(format!("disk{i}.fixture"))).unwrap()).unwrap())
        .collect();
    let pool = Pool::open(disks).unwrap();
    let (_, records) = pool.database_copy(0).unwrap().unwrap();
    for r in &records {
        match r.kind {
            1 => {
                let p = PoolBody::decode(r.version, &r.body).unwrap();
                assert_eq!(
                    (p.guid, p.name.as_str(), p.version),
                    (pool.guid, pool.name.as_str(), 28)
                );
                assert_eq!(
                    (1u32 << p.logical_sector_log2, 1u32 << p.physical_sector_log2),
                    (512, 4096)
                );
            }
            2 => {
                let d = DiskBody::decode(&r.body).unwrap();
                assert_eq!((d.manufacturer.as_str(), d.model.as_str()), ("Msft", "Virtual Disk"));
                assert!(d.database_copy);
                assert_eq!(d.size, 8 << 30);
                // The partition less the 512 MiB before physical slab 0.
                let m = pool.members.iter().find(|m| m.header.disk_guid == d.guid).unwrap();
                assert_eq!(d.data_size, m.partition.length - 0x2000_0000);
            }
            3 => {
                let s = SpaceBody::decode(false, &r.body).unwrap();
                let known = &pool.spaces[&s.id];
                assert_eq!((s.guid, s.size), (known.info.guid, known.info.size.unwrap()));
                let policy = known.info.policy.unwrap();
                assert_eq!(
                    (s.columns, s.copies, 1u64 << s.interleave_log2),
                    (policy.columns, policy.copies, policy.interleave)
                );
            }
            _ => {}
        }
    }
}

/// Every member's SPACEDB header re-encodes byte for byte (the rest of its
/// page is zero), and its byte 0x41 says whether the disk carries a copy of
/// the pool database, as its disk record does: pools of up to five disks
/// carry one on every disk, larger pools on five. A retired disk's copy is
/// no longer updated (c9disk d3); a repair clears its flags (d4).
#[test]
fn disk_headers_are_reproduced_and_mark_database_copies() {
    use storage_spaces::format::DiskHeader;
    use storage_spaces::io::ReadAt;
    let mut checked = 0;
    for dir in pool_dirs() {
        let disks: Vec<SparseImage> = (0..)
            .map_while(|i| File::open(dir.join(format!("disk{i}.fixture"))).ok())
            .map(|f| SparseImage::read_from(f).unwrap())
            .collect();
        let Ok(pool) = Pool::open(disks.iter().collect::<Vec<_>>()) else {
            continue;
        };
        let (_, records) = pool
            .members
            .iter()
            .find_map(|m| pool.database_copy(m.device).unwrap())
            .unwrap();
        let disk_records: Vec<DiskBody> = records
            .iter()
            .filter(|r| r.kind == 2)
            .map(|r| DiskBody::decode(&r.body).unwrap())
            .collect();
        let mut copies = 0;
        for m in &pool.members {
            let mut page = vec![0u8; 4096];
            disks[m.device].read_exact_at(&mut page, m.partition.offset).unwrap();
            let header = DiskHeader::parse(&page).unwrap();
            assert_eq!(
                header.encode()[..],
                page[..0x200],
                "{} device {}",
                dir.display(),
                m.device
            );
            assert!(page[0x200..].iter().all(|&b| b == 0));
            let Some(record) = disk_records.iter().find(|d| d.guid == header.disk_guid) else {
                continue;
            };
            if record.usage == 5 {
                // A retired disk keeps its copy, no longer updated, until a
                // repair moves its data off and clears both flags.
                assert_eq!(header.database_copy, record.database_copy);
                continue;
            }
            assert_eq!(
                header.database_copy,
                record.database_copy,
                "{} device {}",
                dir.display(),
                m.device
            );
            let has_copy = pool.database_copy(m.device).unwrap().is_some();
            assert_eq!(header.database_copy, has_copy, "{} device {}", dir.display(), m.device);
            copies += has_copy as usize;
            checked += 1;
        }
        let present = disk_records.iter().filter(|d| d.usage != 5).count();
        if present == pool.members.len() {
            assert_eq!(copies, present.min(5), "{}", dir.display());
        }
    }
    assert!(checked > 200, "{checked}");
}
