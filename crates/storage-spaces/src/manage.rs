//! Changes to a pool as Windows makes them through its management API, each
//! one update of the pool database (see docs/storage-spaces-format.md,
//! "Changing a pool"). The functions return the updated database, committed
//! with the next sequence at `timestamp`; writing it to the members is the
//! caller's.

use crate::database::Database;
use crate::error::{Result, format_err};
use crate::format::{RawRecord, SPACE_SECURITY_DESCRIPTOR, assemble_records};
use crate::records::{DiskBody, PoolBody, SpaceBody};

fn records(db: &Database) -> Result<Vec<RawRecord>> {
    assemble_records(db.bytes(), 0x40)
}

/// Writes `body` as the new version of record `old` (kind and version kept)
/// and commits.
fn replace(db: &Database, old: &RawRecord, body: &[u8], timestamp: u64) -> Result<Database> {
    let (mut next, _) = db.updated(&[(old.kind, old.version, body)], &[old.id])?;
    next.commit(db.sequence() + 1, timestamp);
    Ok(next)
}

/// `Set-StoragePool -NewFriendlyName`: the pool record rewritten with the
/// new name and, the first time, the security descriptor space records get
/// when changed. The partitions keep the old name.
pub fn rename_pool(db: &Database, name: &str, timestamp: u64) -> Result<Database> {
    let old = records(db)?
        .into_iter()
        .find(|r| r.kind == 1)
        .ok_or_else(|| format_err!("no pool record"))?;
    let mut pool = PoolBody::decode(old.version, &old.body)?;
    pool.sequence = db.sequence() + 1;
    pool.name = name.to_string();
    if pool.security_descriptor.is_empty() {
        pool.security_descriptor = SPACE_SECURITY_DESCRIPTOR.to_vec();
    }
    replace(db, &old, &pool.encode()?, timestamp)
}

/// `Set-PhysicalDisk -MediaType` (0 unspecified, 1 HDD, 2 SSD) and
/// `-Usage` (as stored: 1 Auto-Select, 2 Manual-Select, 3 Hot Spare,
/// 4 Journal, 5 Retired): the disk record rewritten.
pub fn set_disk(db: &Database, disk_id: u64, media: Option<u8>, usage: Option<u8>, timestamp: u64) -> Result<Database> {
    let (old, mut disk) = records(db)?
        .into_iter()
        .filter(|r| r.kind == 2)
        .find_map(|r| {
            DiskBody::decode(&r.body)
                .ok()
                .filter(|d| d.id == disk_id)
                .map(|d| (r, d))
        })
        .ok_or_else(|| format_err!("no disk with id {disk_id}"))?;
    disk.sequence = db.sequence() + 1;
    if let Some(m) = media {
        disk.media = m;
    }
    if let Some(u) = usage {
        disk.usage = u;
    }
    replace(db, &old, &disk.encode(), timestamp)
}

/// `Set-VirtualDisk -NewFriendlyName`: the space record rewritten with the
/// new name and, the first time, the security descriptor.
pub fn rename_space(db: &Database, id: u64, name: &str, timestamp: u64) -> Result<Database> {
    let (old, mut space) = records(db)?
        .into_iter()
        .filter(|r| r.kind == 3)
        .find_map(|r| {
            SpaceBody::decode(false, &r.body)
                .ok()
                .filter(|s| s.id == id)
                .map(|s| (r, s))
        })
        .ok_or_else(|| format_err!("no space with id {id}"))?;
    space.sequence = db.sequence() + 1;
    space.name = name.to_string();
    if space.security_descriptor.is_empty() {
        space.security_descriptor = SPACE_SECURITY_DESCRIPTOR.to_vec();
    }
    replace(db, &old, &space.encode()?, timestamp)
}

/// `Remove-VirtualDisk`: the records of the space, its hidden containers and
/// children and all their extents freed.
pub fn delete_space(db: &Database, id: u64, timestamp: u64) -> Result<Database> {
    let ids = crate::create::space_family_records(db, id)?;
    let (mut next, _) = db.updated(&[], &ids)?;
    next.commit(db.sequence() + 1, timestamp);
    Ok(next)
}

/// `Resize-VirtualDisk` to `size` of a fixed space: its record rewritten
/// with the new size (and, the first time, the security descriptor) and
/// `extents` added for the new rows. Nothing else changes (the parity
/// journal header keeps its first run count: `c9resize`).
pub fn resize_space(
    db: &Database,
    id: u64,
    size: u64,
    extents: &[crate::format::ExtentRecord],
    timestamp: u64,
) -> Result<Database> {
    let (old, mut space) = records(db)?
        .into_iter()
        .filter(|r| r.kind == 3)
        .find_map(|r| {
            SpaceBody::decode(false, &r.body)
                .ok()
                .filter(|s| s.id == id)
                .map(|s| (r, s))
        })
        .ok_or_else(|| format_err!("no space with id {id}"))?;
    let sequence = db.sequence() + 1;
    space.sequence = sequence;
    space.size = size;
    if space.security_descriptor.is_empty() {
        space.security_descriptor = SPACE_SECURITY_DESCRIPTOR.to_vec();
    }
    let body = space.encode()?;
    let bodies: Vec<Vec<u8>> = extents.iter().map(|e| e.encode(sequence)).collect();
    let mut writes: Vec<(u8, u8, &[u8])> = vec![(old.kind, old.version, &body)];
    writes.extend(bodies.iter().map(|b| (4, 6, b.as_slice())));
    let (mut next, _) = db.updated(&writes, &[old.id])?;
    next.commit(sequence, timestamp);
    Ok(next)
}

/// What adding a disk writes: to the new disk, to the members (device,
/// offset, bytes), and the resulting pool database.
#[derive(Debug, Clone)]
pub struct AddDiskPlan {
    pub new_disk: crate::create::DiskWrites,
    pub members: Vec<(usize, u64, Vec<u8>)>,
    pub database: Database,
}

/// `Add-PhysicalDisk` of a blank disk to a pool of at most four disks (every
/// disk then carries the pool database and a copy of the metadata space).
/// Windows writes the disk's partition table and header (generation 1,
/// without a database copy), then one update with the disk's record, the
/// pool record, the metadata space with one more copy and that copy's
/// extent (physical slab 0 of the new disk), rewrites the databases in the
/// metadata space to list the new disk and writes them to every copy, then
/// writes the pool database and a header of generation 2 to the new disk
/// and records its copy in a second update (`c9disk`). The databases in the
/// metadata space get `space_timestamps(number)`.
pub fn add_disk<D: crate::io::ReadAt>(
    pool: &crate::Pool<D>,
    db: &Database,
    disk: &crate::create::NewDisk,
    id: u64,
    timestamp: u64,
    space_timestamps: impl Fn(u64) -> u64,
) -> Result<AddDiskPlan> {
    use crate::create::SPACE_DATABASE_STRIDE;
    use crate::format::{DATA_AREA_OFFSET, DiskHeader, ExtentRecord, POOL_DB_OFFSET};
    use crate::gpt::PoolDiskTable;
    if pool.members.len() >= crate::create::DATABASE_COPIES {
        return Err(format_err!(
            "adding a disk to a pool of five or more disks is not supported yet"
        ));
    }
    let all = records(db)?;
    let pool_record = all
        .iter()
        .find(|r| r.kind == 1)
        .ok_or_else(|| format_err!("no pool record"))?;
    let mut pool_body = PoolBody::decode(pool_record.version, &pool_record.body)?;
    let (metadata_record, mut metadata) = all
        .iter()
        .filter(|r| r.kind == 3)
        .find_map(|r| {
            SpaceBody::decode(false, &r.body)
                .ok()
                .filter(|s| s.role == 1)
                .map(|s| (r, s))
        })
        .ok_or_else(|| format_err!("no metadata space"))?;
    let table = PoolDiskTable {
        disk_size: disk.size,
        sector: disk.sector,
        disk_guid: disk.gpt_disk_guid,
        msr_guid: disk.msr_guid,
        pool_partition_guid: disk.partition_guid,
        pool_name: pool.name.clone(),
    };
    let partition = table.pool_partition();
    let mut record = DiskBody {
        id,
        sequence: db.sequence() + 1,
        guid: disk.guid,
        name: String::new(),
        description: String::new(),
        database_copy: false,
        usage: 1,
        manufacturer: disk.manufacturer.clone(),
        model: disk.model.clone(),
        extra: [String::new(), String::new()],
        media: 0,
        size: disk.size,
        data_size: partition.length - DATA_AREA_OFFSET,
    };
    // The first update.
    let sequence = db.sequence() + 1;
    pool_body.sequence = sequence;
    metadata.sequence = sequence;
    metadata.copies += 1;
    metadata.redundancy += 1;
    let copy = ExtentRecord {
        flags: 0,
        stale_marker: 0xffff_ffff,
        space_id: metadata.id,
        virtual_slab: 0,
        column: 0,
        copy: metadata.copies - 1,
        slab_count: 1,
        disk_id: id,
        physical_slab: 0,
    };
    let bodies = [
        record.encode(),
        pool_body.encode()?,
        metadata.encode()?,
        copy.encode(sequence),
    ];
    let writes = [
        (2u8, 8u8, bodies[0].as_slice()),
        (pool_record.kind, pool_record.version, bodies[1].as_slice()),
        (metadata_record.kind, metadata_record.version, bodies[2].as_slice()),
        (4, 6, bodies[3].as_slice()),
    ];
    let (mut first, ids) = db.updated(&writes, &[pool_record.id, metadata_record.id])?;
    first.commit(sequence, timestamp);
    // The second: the disk's record with its database copy.
    record.sequence = sequence + 1;
    record.database_copy = true;
    let body = record.encode();
    let (mut second, _) = first.updated(&[(2, 8, &body)], &[ids[0]])?;
    second.commit(sequence + 1, timestamp);

    // The databases in the metadata space: every type 3 space's, listing
    // the disks of every copy, the new one included.
    let meta = pool
        .spaces
        .get(&metadata.id)
        .ok_or_else(|| format_err!("no metadata space"))?;
    let mut disk_ids: Vec<u64> = meta.extents.iter().map(|e| e.disk_id).chain([id]).collect();
    disk_ids.sort_unstable();
    let mut list = crate::format::encode_varint(0);
    list.extend(crate::format::encode_varint(2));
    list.extend(crate::format::encode_varint(disk_ids.len() as u64));
    for d in &disk_ids {
        let guid = if *d == id { disk.guid } else { pool.disks[d].guid };
        list.extend_from_slice(&guid.0);
    }
    let mut members = Vec::new();
    let mut new_disk = table.regions();
    let numbers: Vec<u64> = all
        .iter()
        .filter(|r| r.kind == 3)
        .filter_map(|r| SpaceBody::decode(false, &r.body).ok())
        .filter(|s| s.role != 1)
        .map(|s| s.number)
        .collect();
    for number in numbers {
        let offset = number * SPACE_DATABASE_STRIDE;
        let (device, at) = meta
            .extents
            .iter()
            .find_map(|e| pool.slab_location(e.disk_id, e.physical_slab).ok().flatten())
            .ok_or_else(|| format_err!("no copy of the metadata space at hand"))?;
        let old = Database::read_formatted(&pool.devices[device], at + offset)?;
        let list_record = assemble_records(old.bytes(), 0x40)?
            .into_iter()
            .find(|r| r.kind == 7)
            .ok_or_else(|| format_err!("a database of the metadata space without its disk list"))?;
        let (mut updated, _) = old.updated(&[(7, list_record.version, &list)], &[list_record.id])?;
        updated.commit(old.sequence() + 1, space_timestamps(number));
        for e in &meta.extents {
            if let Some((device, at)) = pool.slab_location(e.disk_id, e.physical_slab)? {
                members.push((device, at + offset, updated.bytes().to_vec()));
            }
        }
        new_disk.push((partition.offset + DATA_AREA_OFFSET + offset, updated.bytes().to_vec()));
    }
    for m in pool.members.iter().filter(|m| m.header.database_copy) {
        members.push((m.device, m.partition.offset + POOL_DB_OFFSET, second.bytes().to_vec()));
    }
    let header = DiskHeader {
        version: 3,
        generation: 2,
        format_time: disk.joined,
        pool_guid: pool.guid,
        disk_guid: disk.guid,
        database_copy: true,
        rest: Vec::new(),
    };
    let mut page = header.encode().to_vec();
    page.resize(POOL_DB_OFFSET as usize, 0);
    page.extend_from_slice(second.bytes());
    new_disk.push((partition.offset, page));
    Ok(AddDiskPlan {
        new_disk,
        members,
        database: second,
    })
}
