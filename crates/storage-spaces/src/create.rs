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
const SPACE_RECORD: (u8, u8) = (3, 16);
const EXTENT_RECORD: (u8, u8) = (4, 6);

/// Pool version of Windows 11 24H2.
pub const POOL_VERSION: u16 = 28;

/// Most disks of a pool that carry a copy of the pool database.
pub const DATABASE_COPIES: usize = 5;

/// Interleave of the internal metadata space (16 MiB).
const METADATA_INTERLEAVE_LOG2: u8 = 24;

/// What to write to one disk: byte offsets and bytes.
pub type DiskWrites = Vec<(u64, Vec<u8>)>;

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
    /// of every disk) and its extents, in one update of sequence 1.
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
            resiliency: 2,
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
        let mut db = Database::new(self.guid, POOL_DATABASE_LIMIT);
        let writes: Vec<(u8, u8, &[u8])> = records.iter().map(|((k, v), b)| (*k, *v, b.as_slice())).collect();
        if db.update(&writes, &[]).is_none() {
            return Err(format_err!("the records of {n} disks do not fit a new pool database"));
        }
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
