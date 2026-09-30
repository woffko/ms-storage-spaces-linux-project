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
