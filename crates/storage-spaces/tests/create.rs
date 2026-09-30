//! Management operations predicted byte for byte: from a pool state and the
//! choices Windows leaves open (GUIDs, times, disks), the library produces
//! every page Windows wrote in the scenarios `c9*` of tools/scenarios.sh.

use std::fs::File;
use std::path::{Path, PathBuf};

use storage_spaces::Guid;
use storage_spaces::create::{NewDisk, NewPool};
use storage_spaces::format::{DiskHeader, Record, read_database};
use storage_spaces::io::{ReadAt, SparseImage};
use storage_spaces::records::{DiskBody, PoolBody, SpaceBody};

fn state(name: &str, label: &str) -> (PathBuf, Vec<SparseImage>) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/scenarios")
        .join(name)
        .join(label);
    let disks = (0..)
        .map_while(|i| File::open(dir.join(format!("disk{i}.fixture"))).ok())
        .map(|f| SparseImage::read_from(f).unwrap())
        .collect();
    (dir, disks)
}

fn mixed(b: &[u8]) -> Guid {
    Guid::from_mixed_endian(b.try_into().unwrap())
}

/// The choices Windows made for a pool it created, read back from what it
/// wrote.
fn new_pool_of(disks: &[SparseImage]) -> NewPool {
    let mut members = Vec::new();
    let mut database = None;
    for disk in disks {
        let sector = [512u64, 4096]
            .into_iter()
            .find(|&s| {
                let mut sig = [0u8; 8];
                disk.read_exact_at(&mut sig, s).unwrap();
                &sig == b"EFI PART"
            })
            .unwrap();
        let mut gpt = vec![0u8; sector as usize * 2 + 256];
        disk.read_exact_at(&mut gpt, sector).unwrap();
        let entries = &gpt[sector as usize..];
        let mut page = [0u8; 4096];
        disk.read_exact_at(&mut page, 16 << 20).unwrap();
        let header = DiskHeader::parse(&page).unwrap();
        if header.database_copy && database.is_none() {
            database = read_database(disk, (16 << 20) + 0x1000).unwrap();
        }
        members.push(NewDisk {
            size: disk.size,
            sector,
            guid: header.disk_guid,
            gpt_disk_guid: mixed(&gpt[56..72]),
            msr_guid: mixed(&entries[16..32]),
            partition_guid: mixed(&entries[128 + 16..128 + 32]),
            joined: header.format_time,
            manufacturer: String::new(),
            model: String::new(),
            database_copy: header.database_copy,
        });
    }
    let (db, records) = database.unwrap();
    let mut pool = None;
    let mut metadata_guid = None;
    for r in &records {
        match r.kind {
            1 => pool = Some(PoolBody::decode(r.version, &r.body).unwrap()),
            2 => {
                let d = DiskBody::decode(&r.body).unwrap();
                let m = members.iter_mut().find(|m| m.guid == d.guid).unwrap();
                (m.manufacturer, m.model) = (d.manufacturer, d.model);
            }
            3 => {
                let s = SpaceBody::decode(false, &r.body).unwrap();
                if s.role == 1 {
                    metadata_guid = Some(s.guid);
                }
            }
            _ => {}
        }
    }
    let pool = pool.unwrap();
    // Members in the order of their disk ids, as Windows numbered them.
    let order: Vec<Guid> = records
        .iter()
        .filter_map(|r| match Record::decode(r) {
            Ok(Record::Disk(d)) => Some((d.id, d.guid)),
            _ => None,
        })
        .collect::<std::collections::BTreeMap<_, _>>()
        .into_values()
        .collect();
    members.sort_by_key(|m| order.iter().position(|g| *g == m.guid));
    NewPool {
        name: pool.name,
        guid: pool.guid,
        logical_sector: 1 << pool.logical_sector_log2,
        physical_sector: 1 << pool.physical_sector_log2,
        metadata_guid: metadata_guid.unwrap(),
        created: db.timestamp,
        disks: members,
    }
}

/// Every non-zero page of `actual` equals the prediction, and the
/// prediction writes nothing else.
fn assert_pages_equal(what: &str, actual: &SparseImage, writes: &[(u64, Vec<u8>)]) {
    let mut predicted = SparseImage::new(actual.size);
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
        if a != p {
            let first = (0..4096).find(|&i| a[i] != p[i]).unwrap();
            panic!(
                "{what}: page {page:#x} differs from byte {first:#x}: windows {:02x?}, predicted {:02x?}",
                &a[first..(first + 16).min(4096)],
                &p[first..(first + 16).min(4096)]
            );
        }
    }
}

/// New-StoragePool on blank disks, for pools of 2, 3, 4 and 8 disks of
/// 512-byte sectors (4 KiB physical), 4 KiB logical sectors on the same
/// disks, and 4Kn disks: the partition table, the SPACEDB header and the
/// pool database are predicted byte for byte, and nothing else is written.
#[test]
fn new_pools_are_predicted_byte_for_byte() {
    // (c9smoke's snapshots cover the first 64 MiB only.)
    for (name, label) in [
        ("c9new", "p0"),
        ("c9four", "p0"),
        ("c9eight", "p0"),
        ("c9l4k", "p0"),
        ("c94kn", "p0"),
    ] {
        let (dir, disks) = state(name, label);
        let new = new_pool_of(&disks);
        let writes = new.writes().unwrap();
        for (i, (disk, writes)) in disks.iter().zip(&writes).enumerate() {
            assert_pages_equal(&format!("{} disk {i}", dir.display()), disk, writes);
        }
    }
}
