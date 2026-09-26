//! Assembling a pool from its member devices.

use std::collections::BTreeMap;

use crate::error::{Error, Result, format_err};
use crate::format::{
    DATA_AREA_OFFSET, DbHeader, DiskHeader, ExtentRecord, POOL_DB_OFFSET, Record, SLAB_SIZE, SpaceRecord, SpaceRole,
    read_database,
};
use crate::gpt::{PartitionLocation, find_spaces_partition};
use crate::guid::Guid;
use crate::io::{ReadAt, read_vec};
use crate::reader::SpaceReader;

/// A device that belongs to the pool.
#[derive(Debug, Clone)]
pub struct Member {
    /// Index into the device list passed to [`Pool::open`].
    pub device: usize,
    pub partition: PartitionLocation,
    pub header: DiskHeader,
    /// Sequence number of this device's copy of the pool database.
    pub db_sequence: Option<u64>,
}

/// A physical disk as recorded in the pool database.
#[derive(Debug, Clone)]
pub struct PhysicalDisk {
    pub id: u64,
    pub guid: Guid,
    pub name: String,
    /// Index into [`Pool::members`] if the disk was supplied.
    pub member: Option<usize>,
}

/// A space (virtual disk or internal space) and its slab allocation.
#[derive(Debug, Clone)]
pub struct Space {
    pub info: SpaceRecord,
    pub extents: Vec<ExtentRecord>,
}

impl Space {
    pub fn id(&self) -> u64 {
        self.info.id
    }

    pub fn name(&self) -> &str {
        &self.info.name
    }

    pub fn is_user(&self) -> bool {
        !self.info.is_child && self.info.role == SpaceRole::User
    }

    /// Bytes allocated to this space on the pool disks.
    pub fn allocated(&self) -> u64 {
        self.extents.iter().map(|e| e.slab_count * SLAB_SIZE).sum()
    }
}

/// An assembled pool.
pub struct Pool<D> {
    devices: Vec<D>,
    pub guid: Guid,
    pub name: String,
    /// Pool version ("Version 29" in `Get-StoragePool`).
    pub version: u16,
    /// Logical sector size exposed by the spaces of this pool.
    pub logical_sector_size: u32,
    pub physical_sector_size: u32,
    pub members: Vec<Member>,
    pub database: DbHeader,
    pub disks: BTreeMap<u64, PhysicalDisk>,
    pub spaces: BTreeMap<u64, Space>,
    /// Non-fatal problems found while assembling the pool.
    pub warnings: Vec<String>,
}

impl<D: ReadAt> Pool<D> {
    /// Assembles a pool from member devices (whole disks, partitions or images).
    pub fn open(devices: Vec<D>) -> Result<Self> {
        if devices.is_empty() {
            return Err(Error::Pool("no devices given".into()));
        }
        let mut warnings = Vec::new();
        let mut members = Vec::new();
        for (index, dev) in devices.iter().enumerate() {
            let partition = find_spaces_partition(dev)?
                .ok_or_else(|| Error::Pool(format!("device {index} has no Storage Spaces partition")))?;
            let header = DiskHeader::parse(&read_vec(dev, partition.offset, DiskHeader::SIZE)?)?;
            members.push(Member {
                device: index,
                partition,
                header,
                db_sequence: None,
            });
        }
        let guid = members[0].header.pool_guid;
        if let Some(m) = members.iter().find(|m| m.header.pool_guid != guid) {
            return Err(Error::Pool(format!(
                "device {} belongs to pool {}, device 0 to pool {guid}",
                m.device, m.header.pool_guid
            )));
        }

        // Every member carries a copy of the pool database; use the newest.
        let mut newest: Option<(usize, DbHeader, Vec<crate::format::RawRecord>)> = None;
        for (i, member) in members.iter_mut().enumerate() {
            let dev = &devices[member.device];
            match read_database(dev, member.partition.offset + POOL_DB_OFFSET) {
                Ok((header, records)) => {
                    if header.owner_guid != guid {
                        warnings.push(format!(
                            "device {}: database belongs to {}",
                            member.device, header.owner_guid
                        ));
                        continue;
                    }
                    member.db_sequence = Some(header.sequence);
                    if newest.as_ref().is_none_or(|(_, h, _)| header.sequence > h.sequence) {
                        newest = Some((i, header, records));
                    }
                }
                Err(e) => warnings.push(format!("device {}: cannot read pool database: {e}", member.device)),
            }
        }
        let (_, database, raw_records) =
            newest.ok_or_else(|| Error::Pool("no readable copy of the pool database".into()))?;
        for m in &members {
            if m.db_sequence.is_some_and(|s| s < database.sequence) {
                warnings.push(format!("device {}: stale pool database copy", m.device));
            }
        }

        let mut pool_record = None;
        let mut disks = BTreeMap::new();
        let mut spaces = BTreeMap::new();
        let mut extents = Vec::new();
        for raw in &raw_records {
            match Record::decode(raw)? {
                Record::Pool(p) => pool_record = Some(p),
                Record::Disk(d) => {
                    let member = members.iter().position(|m| m.header.disk_guid == d.guid);
                    disks.insert(
                        d.id,
                        PhysicalDisk {
                            id: d.id,
                            guid: d.guid,
                            name: d.name,
                            member,
                        },
                    );
                }
                Record::Space(s) => {
                    spaces.insert(
                        s.id,
                        Space {
                            info: s,
                            extents: Vec::new(),
                        },
                    );
                }
                Record::Extent(e) => extents.push(e),
                Record::Other { .. } => {}
            }
        }
        for e in extents {
            match spaces.get_mut(&e.space_id) {
                Some(space) => space.extents.push(e),
                None => warnings.push(format!("extent for unknown space {}", e.space_id)),
            }
        }
        for m in &members {
            if !disks.values().any(|d: &PhysicalDisk| d.guid == m.header.disk_guid) {
                warnings.push(format!("device {} is not listed in the pool database", m.device));
            }
        }
        for d in disks.values().filter(|d| d.member.is_none()) {
            warnings.push(format!("disk {} ({}) is missing", d.id, d.guid));
        }

        let pool_record = pool_record.ok_or_else(|| format_err!("pool database has no pool record"))?;
        Ok(Pool {
            devices,
            guid,
            name: pool_record.name,
            version: pool_record.version,
            logical_sector_size: pool_record.logical_sector_size,
            physical_sector_size: pool_record.physical_sector_size,
            members,
            database,
            disks,
            spaces,
            warnings,
        })
    }

    /// Virtual disks visible to the user.
    pub fn user_spaces(&self) -> impl Iterator<Item = &Space> {
        self.spaces.values().filter(|s| s.is_user())
    }

    /// Finds a user space by name or GUID.
    pub fn find_space(&self, key: &str) -> Option<&Space> {
        self.user_spaces()
            .find(|s| s.name() == key || s.info.guid.to_string().eq_ignore_ascii_case(key))
    }

    /// Direct children of a space.
    pub fn children(&self, id: u64) -> impl Iterator<Item = &Space> {
        self.spaces
            .values()
            .filter(move |s| s.info.parent == Some(id) && s.id() != id)
    }

    /// Opens a space for reading.
    pub fn open_space(&self, id: u64) -> Result<SpaceReader<'_, D>> {
        SpaceReader::new(self, id)
    }

    /// Reads from a physical slab. Returns `false` if the disk is not present.
    pub(crate) fn read_slab(&self, disk_id: u64, slab: u64, offset: u64, buf: &mut [u8]) -> Result<bool> {
        let disk = self
            .disks
            .get(&disk_id)
            .ok_or_else(|| format_err!("extent refers to unknown disk {disk_id}"))?;
        let Some(member) = disk.member.map(|m| &self.members[m]) else {
            return Ok(false);
        };
        debug_assert!(offset + buf.len() as u64 <= SLAB_SIZE);
        let pos = DATA_AREA_OFFSET + slab * SLAB_SIZE + offset;
        if pos + buf.len() as u64 > member.partition.length {
            return Err(format_err!(
                "slab {slab} of disk {disk_id} lies beyond the partition end"
            ));
        }
        self.devices[member.device].read_exact_at(buf, member.partition.offset + pos)?;
        Ok(true)
    }
}
