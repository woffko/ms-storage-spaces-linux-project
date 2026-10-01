//! Creating pools: what Windows writes when it makes a pool of blank disks
//! (`New-StoragePool`), reproduced byte for byte from the choices it leaves
//! open (GUIDs, times, which disks carry the pool database). See
//! docs/storage-spaces-format.md, "Creating a pool".

use crate::database::{Database, POOL_DATABASE_LIMIT};
use crate::error::{Result, format_err};
use crate::format::{DATA_AREA_OFFSET, DiskHeader, ExtentRecord, POOL_DB_OFFSET, SLAB_SIZE};
use crate::gpt::PoolDiskTable;
use crate::guid::Guid;
use crate::records::{DiskBody, PoolBody, SpaceBody};

/// Record kinds and the versions Windows 11 24H2 (pool version 28) writes.
const POOL_RECORD: (u8, u8) = (1, 15);
const DISK_RECORD: (u8, u8) = (2, 8);
pub(crate) const SPACE_RECORD: (u8, u8) = (3, 16);
pub(crate) const EXTENT_RECORD: (u8, u8) = (4, 6);
pub(crate) const CHILD_RECORD: (u8, u8) = (6, 4);
/// The record of a per-space database: the pool's disks.
const DISK_LIST_RECORD: (u8, u8) = (7, 1);

/// Pool version of Windows 11 24H2.
pub const POOL_VERSION: u16 = 28;

/// Most disks of a pool that carry a copy of the pool database.
pub const DATABASE_COPIES: usize = 5;

/// Interleave of the internal metadata space (16 MiB).
const METADATA_INTERLEAVE_LOG2: u8 = 24;

/// What to write to one disk: byte offsets and bytes.
pub type DiskWrites = Vec<(u64, Vec<u8>)>;

/// Database records: ((kind, version), body).
pub type Records = Vec<((u8, u8), Vec<u8>)>;

/// A blank disk that becomes a pool member.
#[derive(Debug, Clone)]
pub struct NewDisk {
    /// Size in bytes and logical sector size.
    pub size: u64,
    pub sector: u64,
    /// The member's GUID (its SPACEDB header, its disk record).
    pub guid: Guid,
    /// GUIDs of the partition table: the disk's, the Microsoft reserved
    /// partition's and the pool partition's.
    pub gpt_disk_guid: Guid,
    pub msr_guid: Guid,
    pub partition_guid: Guid,
    /// FILETIME of the moment the disk joined (SPACEDB header).
    pub joined: u64,
    /// As the disk reports them.
    pub manufacturer: String,
    pub model: String,
    /// Whether the disk carries a copy of the pool database.
    pub database_copy: bool,
}

/// A pool to be made of blank disks.
#[derive(Debug, Clone)]
pub struct NewPool {
    pub name: String,
    pub guid: Guid,
    pub logical_sector: u32,
    pub physical_sector: u32,
    /// GUID of the internal metadata space.
    pub metadata_guid: Guid,
    /// FILETIME of the pool database's first update.
    pub created: u64,
    pub disks: Vec<NewDisk>,
}

impl NewPool {
    /// The pool database: the pool, every disk (ids 1..n), the internal
    /// metadata space (id n+1, an n-way mirror of one slab, physical slab 0
    /// of every disk; simple on a single disk) and its extents, in one
    /// update of sequence 1.
    pub fn database(&self) -> Result<Database> {
        let n = self.disks.len() as u64;
        if n == 0 {
            return Err(format_err!("a pool needs at least one disk"));
        }
        let log2 = |s: u32| -> Result<u8> {
            if s.is_power_of_two() && (512..=4096).contains(&s) {
                Ok(s.trailing_zeros() as u8)
            } else {
                Err(format_err!("sector size {s}"))
            }
        };
        let pool = PoolBody {
            record_version: POOL_RECORD.1,
            sequence: 1,
            guid: self.guid,
            name: self.name.clone(),
            description: String::new(),
            version: POOL_VERSION,
            logical_sector_log2: log2(self.logical_sector)?,
            physical_sector_log2: log2(self.physical_sector)?,
            security_descriptor: Vec::new(),
        }
        .encode()?;
        let mut records = vec![(POOL_RECORD, pool)];
        for (i, d) in self.disks.iter().enumerate() {
            let table = self.table(d);
            let disk = DiskBody {
                id: i as u64 + 1,
                sequence: 1,
                guid: d.guid,
                name: String::new(),
                description: String::new(),
                database_copy: d.database_copy,
                usage: 1,
                manufacturer: d.manufacturer.clone(),
                model: d.model.clone(),
                extra: [String::new(), String::new()],
                media: 0,
                size: d.size,
                data_size: table.pool_partition().length - DATA_AREA_OFFSET,
            };
            records.push((DISK_RECORD, disk.encode()));
        }
        let metadata = SpaceBody {
            child: false,
            layout: 0,
            id: n + 1,
            sequence: 1,
            guid: self.metadata_guid,
            name: String::new(),
            description: String::new(),
            internal: 1,
            role: 1,
            size: SLAB_SIZE,
            number: 0xffff_ffff,
            provisioning: 2,
            allocation_unit: SLAB_SIZE,
            tiering: 0,
            // Simple on a single disk.
            resiliency: if n == 1 { 1 } else { 2 },
            redundancy: n - 1,
            copies: n,
            groups: 1,
            columns: 1,
            interleave_log2: METADATA_INTERLEAVE_LOG2,
            write_cache: 0,
            security_descriptor: Vec::new(),
            linked: 0,
            parent: 0,
            range: None,
        };
        records.push((SPACE_RECORD, metadata.encode()?));
        for copy in 0..n {
            let extent = ExtentRecord {
                flags: 0,
                stale_marker: 0xffff_ffff,
                space_id: n + 1,
                virtual_slab: 0,
                column: 0,
                copy,
                slab_count: 1,
                disk_id: copy + 1,
                physical_slab: 0,
            };
            records.push((EXTENT_RECORD, extent.encode(1)));
        }
        let writes: Vec<(u8, u8, &[u8])> = records.iter().map(|((k, v), b)| (*k, *v, b.as_slice())).collect();
        let (mut db, _) = Database::new(self.guid, POOL_DATABASE_LIMIT).updated(&writes, &[])?;
        db.commit(1, self.created);
        Ok(db)
    }

    fn table(&self, d: &NewDisk) -> PoolDiskTable {
        PoolDiskTable {
            disk_size: d.size,
            sector: d.sector,
            disk_guid: d.gpt_disk_guid,
            msr_guid: d.msr_guid,
            pool_partition_guid: d.partition_guid,
            pool_name: self.name.clone(),
        }
    }

    /// Everything to write to each disk (byte offset, bytes): the
    /// partition table, the SPACEDB header and, on disks that carry one, the
    /// pool database. Nothing else: the metadata space stays empty until
    /// the first space is created.
    pub fn writes(&self) -> Result<Vec<DiskWrites>> {
        let db = self.database()?;
        let mut out = Vec::new();
        for d in &self.disks {
            let table = self.table(d);
            let partition = table.pool_partition();
            if partition.length <= DATA_AREA_OFFSET + SLAB_SIZE {
                return Err(format_err!("a disk of {} bytes is too small for a pool", d.size));
            }
            let mut writes = table.regions();
            let header = DiskHeader {
                version: 3,
                generation: 1,
                format_time: d.joined,
                pool_guid: self.guid,
                disk_guid: d.guid,
                database_copy: d.database_copy,
                rest: Vec::new(),
            };
            let mut page = header.encode().to_vec();
            page.resize(POOL_DB_OFFSET as usize, 0);
            if d.database_copy {
                page.extend_from_slice(db.bytes());
            }
            writes.push((partition.offset, page));
            out.push(writes);
        }
        Ok(out)
    }
}

/// A hidden space of a new space: its container (type 3) and child (type 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hidden {
    /// Mirror spaces: the dirty region log.
    DirtyRegions,
    /// Parity spaces: the parity journal.
    Journal,
    /// The write-back cache.
    Cache,
}

impl Hidden {
    fn container_role(self) -> u8 {
        match self {
            Hidden::DirtyRegions => 0x06,
            Hidden::Journal => 0x0a,
            Hidden::Cache => 0x0b,
        }
    }

    fn child_role(self) -> u8 {
        match self {
            Hidden::DirtyRegions => 0x04,
            Hidden::Journal => 0x06,
            Hidden::Cache => 0x07,
        }
    }
}

/// A hidden container and its child of a new space.
#[derive(Debug, Clone)]
pub struct NewHidden {
    pub kind: Hidden,
    pub container_id: u64,
    pub container_guid: Guid,
    /// The container's number (its database's slot in the metadata space).
    pub number: u64,
    pub child_id: u64,
    pub child_guid: Guid,
    /// The child's size: 256 MiB, a cache its own size.
    pub size: u64,
    /// Mirror placement of the child: redundancy, copies, columns (a cache
    /// may have several), interleave.
    pub redundancy: u64,
    pub copies: u64,
    pub columns: u64,
    pub interleave_log2: u8,
    /// Its slabs: (row, column, copy, disk id, physical slab).
    pub slabs: Vec<(u64, u64, u64, u64, u64)>,
    /// FILETIME of the container's database in the metadata space.
    pub created: u64,
}

/// A space to create (`New-VirtualDisk`), with the choices Windows makes
/// (ids, GUIDs, numbers, slabs, times) as inputs.
#[derive(Debug, Clone)]
pub struct NewSpace {
    pub id: u64,
    pub guid: Guid,
    pub name: String,
    /// The space's number (its database's slot in the metadata space).
    pub number: u64,
    pub size: u64,
    /// 1 thin, 2 fixed.
    pub provisioning: u8,
    pub allocation_unit: u64,
    /// 1 simple, 2 mirror, 3 parity.
    pub resiliency: u8,
    pub redundancy: u64,
    pub copies: u64,
    pub columns: u64,
    pub interleave_log2: u8,
    /// Size of the write-back cache (0 without one).
    pub write_cache: u64,
    /// The data extents (a thin space has its first row).
    pub extents: Vec<ExtentRecord>,
    /// Hidden spaces in the order Windows writes them: the dirty region
    /// log, the parity journal, then the cache.
    pub hidden: Vec<NewHidden>,
    /// The tiers of a tiered space, fastest first (the space then has no
    /// extents of its own and takes the first tier's policy).
    pub tiers: Vec<NewTier>,
    /// FILETIME of the space's database in the metadata space.
    pub created: u64,
}

/// A tier of a new space: a type 6 child of it, named
/// `<space>-<template>`, covering `size` bytes of the space from `start`
/// with its own policy and extents.
#[derive(Debug, Clone)]
pub struct NewTier {
    pub id: u64,
    pub guid: Guid,
    pub name: String,
    pub ssd: bool,
    pub resiliency: u8,
    pub redundancy: u64,
    pub copies: u64,
    pub columns: u64,
    pub interleave_log2: u8,
    pub allocation_unit: u64,
    pub start: u64,
    pub size: u64,
    pub extents: Vec<ExtentRecord>,
}

impl NewTier {
    fn body(&self, space: u64, sequence: u64) -> SpaceBody {
        SpaceBody {
            child: true,
            layout: 0,
            id: self.id,
            sequence,
            guid: self.guid,
            name: self.name.clone(),
            description: String::new(),
            internal: 0,
            role: 1,
            size: 0,
            number: 0,
            provisioning: 2,
            allocation_unit: self.allocation_unit,
            tiering: if self.ssd { 2 } else { 1 },
            resiliency: self.resiliency,
            redundancy: self.redundancy,
            copies: self.copies,
            groups: 1,
            columns: self.columns,
            interleave_log2: self.interleave_log2,
            write_cache: 0,
            security_descriptor: Vec::new(),
            linked: 0,
            parent: space,
            range: Some((1, self.start, self.size)),
        }
    }

    fn data_columns(&self) -> u64 {
        self.columns - self.redundancy * (self.resiliency == 3) as u64
    }
}

/// Where the per-space databases lie in the metadata space: 4 MiB apart,
/// by number.
pub const SPACE_DATABASE_STRIDE: u64 = 4 << 20;

impl NewSpace {
    fn space_body(&self, sequence: u64) -> SpaceBody {
        SpaceBody {
            child: false,
            layout: 0,
            id: self.id,
            sequence,
            guid: self.guid,
            name: self.name.clone(),
            description: String::new(),
            internal: 0,
            role: 2,
            size: self.size,
            number: self.number,
            provisioning: self.provisioning,
            allocation_unit: self.allocation_unit,
            tiering: if self.tiers.is_empty() { 0 } else { 2 },
            resiliency: self.resiliency,
            redundancy: self.redundancy,
            copies: self.copies,
            groups: 1,
            columns: self.columns,
            interleave_log2: self.interleave_log2,
            write_cache: self.write_cache,
            security_descriptor: Vec::new(),
            linked: 1,
            parent: 0,
            range: None,
        }
    }

    /// The records of the database update that creates the space, in
    /// Windows' order: the space, then for each hidden space its container,
    /// its child's extents and its child, then the space's extents.
    pub fn records(&self, sequence: u64) -> Result<Records> {
        let mut out = vec![(SPACE_RECORD, self.space_body(sequence).encode()?)];
        for h in &self.hidden {
            let container = SpaceBody {
                id: h.container_id,
                guid: h.container_guid,
                name: String::new(),
                internal: 1,
                role: h.kind.container_role(),
                size: h.size,
                number: h.number,
                provisioning: 2,
                allocation_unit: SLAB_SIZE,
                resiliency: 2,
                redundancy: h.redundancy,
                copies: h.copies,
                columns: h.columns,
                interleave_log2: h.interleave_log2,
                write_cache: if h.kind == Hidden::Cache { h.size } else { 0 },
                parent: self.id,
                ..self.space_body(sequence)
            };
            out.push((SPACE_RECORD, container.encode()?));
            for &(row, column, copy, disk_id, physical_slab) in &h.slabs {
                let extent = ExtentRecord {
                    flags: 4,
                    stale_marker: 0xffff_ffff,
                    space_id: h.child_id,
                    virtual_slab: row * h.columns,
                    column,
                    copy,
                    slab_count: 1,
                    disk_id,
                    physical_slab,
                };
                out.push((EXTENT_RECORD, extent.encode(sequence)));
            }
            let child = SpaceBody {
                child: true,
                id: h.child_id,
                guid: h.child_guid,
                internal: 0,
                role: h.kind.child_role(),
                write_cache: 0,
                linked: 0,
                parent: h.container_id,
                range: Some((1, 0, h.size)),
                ..container
            };
            out.push((CHILD_RECORD, child.encode()?));
        }
        for e in &self.extents {
            out.push((EXTENT_RECORD, e.encode(sequence)));
        }
        // Tiers: every tier's extents, then the tiers' records.
        for e in self.tiers.iter().flat_map(|t| &t.extents) {
            out.push((EXTENT_RECORD, e.encode(sequence)));
        }
        for t in &self.tiers {
            out.push((CHILD_RECORD, t.body(self.id, sequence).encode()?));
        }
        Ok(out)
    }

    /// What the cache and the parity journal follow: the last (capacity)
    /// tier of a tiered space, the space itself otherwise: (allocation
    /// unit, data columns, data stripe in bytes).
    fn capacity(&self) -> (u64, u64, u64) {
        match self.tiers.last() {
            Some(t) => (
                t.allocation_unit,
                t.data_columns(),
                t.data_columns() << t.interleave_log2,
            ),
            None => {
                let data = self.columns - self.redundancy * (self.resiliency == 3) as u64;
                (self.allocation_unit, data, data << self.interleave_log2)
            }
        }
    }

    /// The databases the space family gets in the metadata space: (number,
    /// database), each with one record listing the disks that carry a copy
    /// of it (`disks`, in the order of their ids).
    pub fn databases(&self, disks: &[Guid]) -> Vec<(u64, Database)> {
        let mut record = crate::format::encode_varint(0);
        record.extend(crate::format::encode_varint(1));
        record.extend(crate::format::encode_varint(disks.len() as u64));
        for g in disks {
            record.extend_from_slice(&g.0);
        }
        let family = std::iter::once((self.number, self.guid, self.created))
            .chain(self.hidden.iter().map(|h| (h.number, h.container_guid, h.created)));
        family
            .map(|(number, owner, created)| {
                let mut db = Database::new(owner, crate::database::SPACE_DATABASE_LIMIT);
                db.update(&[(DISK_LIST_RECORD.0, DISK_LIST_RECORD.1, &record)], &[])
                    .expect("one record fits a new database");
                db.commit(1, created);
                (number, db)
            })
            .collect()
    }

    /// What the hidden spaces hold when they are created: (index into
    /// `hidden`, offset within the child, bytes). The dirty region log: an
    /// empty header at both ends; the parity journal: its header; the
    /// cache: its header and slot 0.
    pub fn hidden_contents(&self) -> Vec<(usize, u64, Vec<u8>)> {
        let (unit, data_columns, stripe) = self.capacity();
        let stripe = stripe as u32;
        let mut out = Vec::new();
        for (i, h) in self.hidden.iter().enumerate() {
            match h.kind {
                Hidden::DirtyRegions => {
                    let page = crate::drt::DrtHeader {
                        generation: 0,
                        runs: Vec::new(),
                    }
                    .encode();
                    out.push((i, 0, page.clone()));
                    out.push((i, h.size - 0x2000, page));
                }
                Hidden::Journal => {
                    let run = unit * data_columns;
                    let header =
                        crate::journal::new_journal_header(self.guid, run, stripe, self.size.div_ceil(run) as u32);
                    out.push((i, 0, header));
                }
                Hidden::Cache => {
                    let header = crate::cache::CacheHeader::new(self.guid, h.size, stripe);
                    let slot = crate::cache::CacheWriter::new(header.clone(), 0).init_slot();
                    out.push((i, 0, header.encode(crate::cache::SPCACHE_SIGNATURE)));
                    out.push((i, header.slot_offset, slot));
                }
            }
        }
        out
    }

    /// Every write that creates the space in `pool`, whose database model
    /// is `db`, by device: the per-space databases (on the copies of the
    /// metadata space on `metadata_disks`, all copies if `None`), the
    /// contents of the hidden spaces, zeros over the space's first logical
    /// sector (its whole first stripe on parity), then the pool database update
    /// (sequence + 1 at `timestamp`) on every member that carries it.
    /// Windows writes in that order, so the space exists only once
    /// everything it needs is in place. (In pools of more than five disks
    /// Windows writes five copies of the metadata space, not always those
    /// of the disks carrying the pool database: `c9eight`.)
    pub fn plan<D: crate::io::ReadAt>(
        &self,
        pool: &crate::Pool<D>,
        db: &Database,
        timestamp: u64,
        metadata_disks: Option<&[u64]>,
    ) -> Result<Vec<(usize, u64, Vec<u8>)>> {
        let mut out = Vec::new();
        let metadata = pool
            .spaces
            .values()
            .find(|s| s.info.role == crate::format::SpaceRole::Metadata)
            .ok_or_else(|| format_err!("the pool has no metadata space"))?;
        // The copies of the metadata space that hold the databases, and the
        // disks they are on, in the order of the disk ids.
        let copies: Vec<&ExtentRecord> = metadata
            .extents
            .iter()
            .filter(|e| metadata_disks.is_none_or(|d| d.contains(&e.disk_id)))
            .collect();
        let mut disk_ids: Vec<u64> = copies.iter().map(|e| e.disk_id).collect();
        disk_ids.sort_unstable();
        let disks: Vec<Guid> = disk_ids
            .iter()
            .map(|id| pool.disks.get(id).map(|d| d.guid))
            .collect::<Option<_>>()
            .ok_or_else(|| format_err!("the metadata space lies on an unknown disk"))?;
        for (number, space_db) in self.databases(&disks) {
            let offset = number * SPACE_DATABASE_STRIDE;
            if offset + space_db.bytes().len() as u64 > SLAB_SIZE {
                return Err(format_err!("the metadata space has no room for space number {number}"));
            }
            for e in &copies {
                if let Some((device, at)) = pool.slab_location(e.disk_id, e.physical_slab)? {
                    out.push((device, at + offset, space_db.bytes().to_vec()));
                }
            }
        }
        for (i, offset, bytes) in self.hidden_contents() {
            // Striped over the child's columns (the pieces written here stay
            // within one interleave unit).
            let h = &self.hidden[i];
            let interleave = 1u64 << h.interleave_log2;
            let unit = offset / interleave;
            let column = unit % h.columns;
            let column_offset = unit / h.columns * interleave + offset % interleave;
            let row = column_offset / SLAB_SIZE;
            if offset % interleave + bytes.len() as u64 > interleave {
                return Err(format_err!("hidden space contents across interleave units"));
            }
            for &(r, c, _, disk_id, physical_slab) in &h.slabs {
                if (r, c) == (row, column)
                    && let Some((device, at)) = pool.slab_location(disk_id, physical_slab)?
                {
                    out.push((device, at + column_offset % SLAB_SIZE, bytes.clone()));
                }
            }
        }
        // The first logical sector of the new disk is cleared (an old
        // partition table must not show through): on every copy of column 0;
        // on a parity space the whole first stripe, parity included, so that
        // it stays consistent.
        // (A tiered space starts in its first tier.)
        let first = self
            .extents
            .iter()
            .chain(self.tiers.first().into_iter().flat_map(|t| &t.extents))
            .filter(|e| e.virtual_slab == 0 && (self.resiliency == 3 || e.column == 0));
        let clear = if self.resiliency == 3 {
            1u64 << self.interleave_log2
        } else {
            pool.logical_sector_size as u64
        };
        for e in first {
            if let Some((device, at)) = pool.slab_location(e.disk_id, e.physical_slab)? {
                out.push((device, at, vec![0; clear as usize]));
            }
        }
        let sequence = db.sequence() + 1;
        let records = self.records(sequence)?;
        let writes: Vec<(u8, u8, &[u8])> = records.iter().map(|((k, v), b)| (*k, *v, b.as_slice())).collect();
        let (mut db, _) = db.updated(&writes, &[])?;
        db.commit(sequence, timestamp);
        for m in pool.members.iter().filter(|m| m.header.database_copy) {
            out.push((m.device, m.partition.offset + POOL_DB_OFFSET, db.bytes().to_vec()));
        }
        Ok(out)
    }
}

/// The ids of the records of space `id` and everything that belongs to it:
/// its hidden containers and their children, and every extent of them.
pub fn space_family_records(db: &Database, id: u64) -> Result<Vec<u32>> {
    use crate::format::{Record, assemble_records};
    let records = assemble_records(db.bytes(), 0x40)?;
    let decoded: Vec<(u32, Record)> = records
        .iter()
        .filter_map(|r| Record::decode(r).ok().map(|d| (r.id, d)))
        .collect();
    let mut family = vec![id];
    // Containers (parent: the space), then their children.
    for _ in 0..2 {
        for (_, r) in &decoded {
            if let Record::Space(s) = r
                && s.parent.is_some_and(|p| family.contains(&p))
                && !family.contains(&s.id)
            {
                family.push(s.id);
            }
        }
    }
    let ids: Vec<u32> = decoded
        .iter()
        .filter(|(_, r)| match r {
            Record::Space(s) => family.contains(&s.id),
            Record::Extent(e) => family.contains(&e.space_id),
            _ => false,
        })
        .map(|(rid, _)| *rid)
        .collect();
    if !decoded.iter().any(|(_, r)| matches!(r, Record::Space(s) if s.id == id)) {
        return Err(format_err!("no space with id {id}"));
    }
    Ok(ids)
}

/// The number a new type 3 space gets: the smallest no space of `db` has
/// (Windows reuses the numbers, and so the metadata space slots, of deleted
/// spaces: `c9opts2`).
pub fn next_space_number(db: &Database) -> Result<u64> {
    use crate::format::assemble_records;
    let used: std::collections::BTreeSet<u64> = assemble_records(db.bytes(), 0x40)?
        .iter()
        .filter(|r| r.kind == 3)
        .filter_map(|r| SpaceBody::decode(false, &r.body).ok())
        .map(|s| s.number)
        .collect();
    Ok((0..).find(|n| !used.contains(n)).unwrap())
}

/// A storage tier template (`New-StorageTier`): a type 6 record without a
/// parent, a range or extents, naming the media and the placement policy
/// of the tiers made from it. Columns left to Windows (`None`) are stored
/// as 0xffffffff, as are the groups of a parity template; the allocation
/// unit is all ones.
#[derive(Debug, Clone)]
pub struct TierTemplate {
    pub id: u64,
    pub guid: Guid,
    pub name: String,
    /// SSD (`tiering` 2) or HDD (1).
    pub ssd: bool,
    /// 1 simple, 2 mirror, 3 parity.
    pub resiliency: u8,
    pub columns: Option<u64>,
    pub interleave_log2: u8,
}

impl TierTemplate {
    pub fn body(&self, sequence: u64) -> SpaceBody {
        let (redundancy, copies, groups) = match self.resiliency {
            2 => (1, 2, 1),
            3 => (1, 1, 0xffff_ffff),
            _ => (0, 1, 1),
        };
        SpaceBody {
            child: true,
            layout: 0,
            id: self.id,
            sequence,
            guid: self.guid,
            name: self.name.clone(),
            description: String::new(),
            internal: 0,
            role: 1,
            size: 0,
            number: 0,
            provisioning: 2,
            allocation_unit: u64::MAX,
            tiering: if self.ssd { 2 } else { 1 },
            resiliency: self.resiliency,
            redundancy,
            copies,
            groups,
            columns: self.columns.unwrap_or(0xffff_ffff),
            interleave_log2: self.interleave_log2,
            write_cache: 0,
            security_descriptor: Vec::new(),
            linked: 0,
            parent: 0,
            range: None,
        }
    }
}
