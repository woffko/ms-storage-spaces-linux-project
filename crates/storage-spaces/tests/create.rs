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

/// New-StoragePool on blank disks, for pools of 1, 3, 4 and 8 disks of
/// 512-byte sectors (4 KiB physical), 4 KiB logical sectors on the same
/// disks, and 4Kn disks: the partition table, the SPACEDB header and the
/// pool database are predicted byte for byte, and nothing else is written.
#[test]
fn new_pools_are_predicted_byte_for_byte() {
    // (c9smoke's snapshots cover the first 64 MiB only.)
    for (name, label) in [
        ("c9one", "p0"),
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

/// The space a transition created, as `NewSpace` with Windows' choices
/// taken from the state after it (`after`), and the new pool database's
/// timestamp.
fn new_space_of(
    before: &storage_spaces::Pool<&SparseImage>,
    after: &storage_spaces::Pool<&SparseImage>,
    after_disks: &[SparseImage],
) -> (storage_spaces::create::NewSpace, u64, Vec<u64>) {
    use storage_spaces::create::{Hidden, NewHidden, NewSpace, SPACE_DATABASE_STRIDE};
    let m = after
        .members
        .iter()
        .find(|m| after.database_copy(m.device).unwrap().is_some())
        .unwrap();
    let (header, records) = after.database_copy(m.device).unwrap().unwrap();
    let bodies: Vec<SpaceBody> = records
        .iter()
        .filter(|r| {
            (r.kind == 3 || r.kind == 6)
                && !before
                    .spaces
                    .contains_key(&SpaceBody::decode(r.kind == 6, &r.body).unwrap().id)
        })
        .map(|r| SpaceBody::decode(r.kind == 6, &r.body).unwrap())
        .collect();
    let user = bodies.iter().find(|b| b.role == 2).unwrap();
    // A database's creation time, from the metadata space of `after`.
    let metadata = after
        .spaces
        .values()
        .find(|s| s.info.role == storage_spaces::format::SpaceRole::Metadata)
        .unwrap();
    // (From a copy that holds it: larger pools hold five.)
    let created = |number: u64| {
        metadata
            .extents
            .iter()
            .find_map(|e| {
                let (device, at) = after.slab_location(e.disk_id, e.physical_slab).unwrap().unwrap();
                let mut h = [0u8; 0x50];
                after_disks[device]
                    .read_exact_at(&mut h, at + number * SPACE_DATABASE_STRIDE)
                    .unwrap();
                (&h[..8] == b"SDBC    ").then(|| u64::from_be_bytes(h[0x48..0x50].try_into().unwrap()))
            })
            .unwrap()
    };
    let extents_of = |id: u64| after.spaces[&id].extents.clone();
    let hidden = bodies
        .iter()
        .filter(|b| !b.child && b.parent == user.id)
        .map(|c| {
            let child = bodies.iter().find(|b| b.child && b.parent == c.id).unwrap();
            NewHidden {
                kind: match c.role {
                    0x06 => Hidden::DirtyRegions,
                    0x0a => Hidden::Journal,
                    _ => Hidden::Cache,
                },
                container_id: c.id,
                container_guid: c.guid,
                number: c.number,
                child_id: child.id,
                child_guid: child.guid,
                size: c.size,
                redundancy: c.redundancy,
                copies: c.copies,
                interleave_log2: c.interleave_log2,
                slabs: extents_of(child.id)
                    .iter()
                    .map(|e| (e.virtual_slab, e.copy, e.disk_id, e.physical_slab))
                    .collect(),
                created: created(c.number),
            }
        })
        .collect();
    let space = NewSpace {
        id: user.id,
        guid: user.guid,
        name: user.name.clone(),
        number: user.number,
        size: user.size,
        provisioning: user.provisioning,
        allocation_unit: user.allocation_unit,
        resiliency: user.resiliency,
        redundancy: user.redundancy,
        copies: user.copies,
        columns: user.columns,
        interleave_log2: user.interleave_log2,
        write_cache: user.write_cache,
        extents: extents_of(user.id),
        hidden,
        created: created(user.number),
    };
    // The copies of the metadata space Windows wrote the new databases to.
    let written: Vec<u64> = metadata
        .extents
        .iter()
        .filter(|e| {
            let (device, at) = after.slab_location(e.disk_id, e.physical_slab).unwrap().unwrap();
            let mut sig = [0u8; 8];
            after_disks[device]
                .read_exact_at(&mut sig, at + space.number * SPACE_DATABASE_STRIDE)
                .unwrap();
            &sig == b"SDBC    "
        })
        .map(|e| e.disk_id)
        .collect();
    (space, header.timestamp, written)
}

/// New-VirtualDisk on the pool of c9new: a simple space, a mirror (with its
/// dirty region log), a parity space (with its journal and 1 GiB cache), a
/// thin simple and a thin mirror space (with their first row); a mirror of 4
/// columns on 8 disks, simple spaces with 4 KiB logical sectors and on 4Kn
/// disks: the records, the databases in the metadata space and the hidden
/// spaces' first pages are predicted byte for byte, and nothing else is
/// written.
#[test]
fn new_spaces_are_predicted_byte_for_byte() {
    for (name, before, after) in [
        ("c9new", "p0", "p1"),
        ("c9new", "p1", "p2"),
        ("c9new", "p2", "p3"),
        ("c9new", "p3", "p4"),
        ("c9new", "p4", "p5"),
        ("c9one", "p0", "p1"),
        ("c9eight", "p0", "p1"),
        ("c9l4k", "p0", "p1"),
        ("c94kn", "p0", "p1"),
    ] {
        let (_, old) = state(name, before);
        let (dir, new) = state(name, after);
        let old_pool = storage_spaces::Pool::open(old.iter().collect::<Vec<_>>()).unwrap();
        let new_pool = storage_spaces::Pool::open(new.iter().collect::<Vec<_>>()).unwrap();
        let (space, timestamp, metadata_disks) = new_space_of(&old_pool, &new_pool, &new);
        let db = storage_spaces::database::Database::read_formatted(
            &old[old_pool.members[0].device],
            old_pool.members[0].partition.offset + 0x1000,
        )
        .unwrap();
        let plan = space.plan(&old_pool, &db, timestamp, Some(&metadata_disks)).unwrap();
        for (i, disk) in new.iter().enumerate() {
            let mut writes: Vec<(u64, Vec<u8>)> = old[i]
                .ranges()
                .iter()
                .map(|&(o, l)| {
                    let mut b = vec![0u8; l];
                    old[i].read_exact_at(&mut b, o).unwrap();
                    (o, b)
                })
                .collect();
            writes.extend(plan.iter().filter(|(d, _, _)| *d == i).map(|(_, o, b)| (*o, b.clone())));
            assert_pages_equal(&format!("{} disk {i} ({})", dir.display(), space.name), disk, &writes);
        }
    }
}
