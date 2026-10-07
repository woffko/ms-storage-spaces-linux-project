//! Assembling a pool from its member devices.

use std::collections::BTreeMap;

use crate::error::{Error, Result, format_err};
use crate::format::{
    DATA_AREA_OFFSET, DbHeader, DiskHeader, DiskRecord, DiskUsage, ExtentRecord, MediaType, POOL_DB_OFFSET, PoolRecord,
    RawRecord, Record, SLAB_SIZE, SpaceRecord, SpaceRole, read_database,
};
use crate::gpt::{PartitionLocation, find_spaces_partition};
use crate::guid::Guid;
use crate::io::{ReadAt, WriteAt, read_vec};
use crate::reader::{OpenOptions, SpaceReader};
use crate::writer::SpaceWriter;

/// Warning about a member whose pool database copy is older than the
/// newest one.
const STALE_COPY: &str = "stale pool database copy";

/// A problem found while assembling a pool; each is also a line of
/// [`Pool::warnings`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Issue {
    /// A device whose disk header does not parse: it is not used.
    IgnoredDevice { device: usize, error: String },
    /// A member whose copy of the pool database names another pool.
    ForeignDatabase { device: usize, owner: Guid },
    /// A member whose copy of the pool database cannot be read.
    UnreadableCopy { device: usize, error: String },
    /// A version of the pool database that does not decode; `newer` when
    /// it is newer than the version used (what Windows recorded last is
    /// then not what is read).
    UnusableCopy {
        devices: Vec<usize>,
        sequence: u64,
        error: String,
        newer: bool,
    },
    /// Copies of the sequence used that differ from it (a torn write).
    TornCopy {
        devices: Vec<usize>,
        sequence: u64,
        used: Vec<usize>,
    },
    /// A copy older than the one used, on a disk that is not retired.
    StaleCopy { device: usize, sequence: u64, current: u64 },
    /// An extent of a space the database does not list.
    UnknownSpaceExtent { space: u64 },
    /// A member the database does not list among the pool's disks.
    UnlistedDevice { device: usize },
    /// A disk of the pool that is not among the devices.
    MissingDisk { disk: u64, guid: Guid },
}

impl std::fmt::Display for Issue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Issue::IgnoredDevice { device, error } => write!(f, "device {device} ignored: {error}"),
            Issue::ForeignDatabase { device, owner } => write!(f, "device {device}: database belongs to {owner}"),
            Issue::UnreadableCopy { device, error } => {
                write!(f, "device {device}: cannot read pool database: {error}")
            }
            Issue::UnusableCopy {
                devices,
                sequence,
                error,
                ..
            } => write!(
                f,
                "pool database copy of sequence {sequence} on device(s) {devices:?} is unusable: {error}"
            ),
            Issue::TornCopy {
                devices,
                sequence,
                used,
            } => write!(
                f,
                "device(s) {devices:?}: pool database copy of sequence {sequence} differs from the one on {used:?} (torn write)"
            ),
            Issue::StaleCopy { device, .. } => write!(f, "device {device}: {STALE_COPY}"),
            Issue::UnknownSpaceExtent { space } => write!(f, "extent for unknown space {space}"),
            Issue::UnlistedDevice { device } => write!(f, "device {device} is not listed in the pool database"),
            Issue::MissingDisk { disk, guid } => write!(f, "disk {disk} ({guid}) is missing"),
        }
    }
}

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
    pub usage: DiskUsage,
    pub media: MediaType,
    pub guid: Guid,
    pub name: String,
    /// Whether the disk carries a copy of the pool database.
    pub database_copy: bool,
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

/// The records of one pool database copy.
struct Decoded {
    pool_record: PoolRecord,
    disks: Vec<DiskRecord>,
    spaces: Vec<SpaceRecord>,
    extents: Vec<ExtentRecord>,
}

fn decode_records(raw: &[RawRecord]) -> Result<Decoded> {
    let mut pool_record = None;
    let mut disks = Vec::new();
    let mut spaces = Vec::new();
    let mut extents = Vec::new();
    for r in raw {
        match Record::decode(r)? {
            Record::Pool(p) => pool_record = Some(p),
            Record::Disk(d) => disks.push(d),
            Record::Space(s) => spaces.push(s),
            Record::Extent(e) => extents.push(e),
            Record::Other { .. } => {}
        }
    }
    Ok(Decoded {
        pool_record: pool_record.ok_or_else(|| format_err!("pool database has no pool record"))?,
        disks,
        spaces,
        extents,
    })
}

/// An assembled pool.
pub struct Pool<D> {
    pub(crate) devices: Vec<D>,
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
    /// Non-fatal problems found while assembling the pool, as text (one
    /// line per [`Issue`]).
    pub warnings: Vec<String>,
    /// The same problems, for programs.
    pub issues: Vec<Issue>,
}

impl<D: ReadAt> Pool<D> {
    /// Assembles a pool from member devices (whole disks, partitions or images).
    pub fn open(devices: Vec<D>) -> Result<Self> {
        if devices.is_empty() {
            return Err(Error::Pool("no devices given".into()));
        }
        let mut issues = Vec::new();
        let mut members = Vec::new();
        for (index, dev) in devices.iter().enumerate() {
            let partition = find_spaces_partition(dev)?
                .ok_or_else(|| Error::Pool(format!("device {index} has no Storage Spaces partition")))?;
            let header = match DiskHeader::parse(&read_vec(dev, partition.offset, DiskHeader::SIZE)?) {
                Ok(header) => header,
                Err(Error::Format(e)) => {
                    issues.push(Issue::IgnoredDevice {
                        device: index,
                        error: e,
                    });
                    continue;
                }
                Err(e) => return Err(e),
            };
            members.push(Member {
                device: index,
                partition,
                header,
                db_sequence: None,
            });
        }
        let Some(first) = members.first() else {
            let why: Vec<String> = issues.iter().map(Issue::to_string).collect();
            return Err(Error::Pool(format!("no usable pool member: {}", why.join("; "))));
        };
        let guid = first.header.pool_guid;
        if let Some(m) = members.iter().find(|m| m.header.pool_guid != guid) {
            return Err(Error::Pool(format!(
                "device {} belongs to pool {}, device 0 to pool {guid}",
                m.device, m.header.pool_guid
            )));
        }

        // Members carry copies of the pool database. Identical copies are
        // grouped; the newest version that decodes is used, preferring the
        // one most members agree on. A copy that differs from others of the
        // same sequence was torn by an interrupted write (SDBB entries carry
        // no checksum of their own).
        let mut versions: Vec<(DbHeader, Vec<RawRecord>, Vec<usize>)> = Vec::new();
        for member in members.iter_mut() {
            let dev = &devices[member.device];
            match read_database(dev, member.partition.offset + POOL_DB_OFFSET) {
                Ok(None) => {} // this member carries no copy
                Ok(Some((header, records))) => {
                    if header.owner_guid != guid {
                        issues.push(Issue::ForeignDatabase {
                            device: member.device,
                            owner: header.owner_guid,
                        });
                        continue;
                    }
                    member.db_sequence = Some(header.sequence);
                    match versions
                        .iter_mut()
                        .find(|(h, r, _)| h.sequence == header.sequence && *r == records)
                    {
                        Some((_, _, devs)) => devs.push(member.device),
                        None => versions.push((header, records, vec![member.device])),
                    }
                }
                Err(e) => issues.push(Issue::UnreadableCopy {
                    device: member.device,
                    error: e.to_string(),
                }),
            }
        }
        versions.sort_by_key(|v| std::cmp::Reverse((v.0.sequence, v.2.len())));
        let mut chosen = None;
        let mut failures = Vec::new();
        for (header, records, devs) in &versions {
            match decode_records(records) {
                Ok(decoded) => {
                    chosen = Some((header.clone(), decoded, devs.clone()));
                    break;
                }
                Err(e) => failures.push((devs.clone(), header.sequence, e.to_string())),
            }
        }
        let Some((database, decoded, used)) = chosen else {
            return Err(match failures.first() {
                Some(_) => Error::Pool(
                    failures
                        .iter()
                        .map(|(devices, sequence, error)| {
                            Issue::UnusableCopy {
                                devices: devices.clone(),
                                sequence: *sequence,
                                error: error.clone(),
                                newer: false,
                            }
                            .to_string()
                        })
                        .collect::<Vec<_>>()
                        .join("; "),
                ),
                None => Error::Pool("no readable copy of the pool database".into()),
            });
        };
        issues.extend(
            failures
                .into_iter()
                .map(|(devices, sequence, error)| Issue::UnusableCopy {
                    devices,
                    sequence,
                    error,
                    newer: sequence > database.sequence,
                }),
        );
        for (header, _, devs) in &versions {
            if header.sequence == database.sequence && *devs != used {
                issues.push(Issue::TornCopy {
                    devices: devs.clone(),
                    sequence: header.sequence,
                    used: used.clone(),
                });
            }
        }
        // Windows stops updating the copy on a retired disk.
        let retired = |m: &Member| {
            decoded
                .disks
                .iter()
                .any(|d| d.guid == m.header.disk_guid && d.usage == DiskUsage::Retired)
        };
        for m in &members {
            if let Some(sequence) = m.db_sequence.filter(|&s| s < database.sequence && !retired(m)) {
                issues.push(Issue::StaleCopy {
                    device: m.device,
                    sequence,
                    current: database.sequence,
                });
            }
        }

        let Decoded {
            pool_record,
            disks: disk_records,
            spaces: space_records,
            extents,
        } = decoded;
        let mut disks = BTreeMap::new();
        for d in disk_records {
            let member = members.iter().position(|m| m.header.disk_guid == d.guid);
            disks.insert(
                d.id,
                PhysicalDisk {
                    id: d.id,
                    usage: d.usage,
                    media: d.media,
                    guid: d.guid,
                    name: d.name,
                    database_copy: d.database_copy,
                    member,
                },
            );
        }
        let mut spaces = BTreeMap::new();
        for s in space_records {
            spaces.insert(
                s.id,
                Space {
                    info: s,
                    extents: Vec::new(),
                },
            );
        }
        for e in extents {
            match spaces.get_mut(&e.space_id) {
                Some(space) => space.extents.push(e),
                None => issues.push(Issue::UnknownSpaceExtent { space: e.space_id }),
            }
        }
        for m in &members {
            if !disks.values().any(|d: &PhysicalDisk| d.guid == m.header.disk_guid) {
                issues.push(Issue::UnlistedDevice { device: m.device });
            }
        }
        for d in disks.values().filter(|d| d.member.is_none()) {
            issues.push(Issue::MissingDisk {
                disk: d.id,
                guid: d.guid,
            });
        }

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
            warnings: issues.iter().map(Issue::to_string).collect(),
            issues,
        })
    }

    /// Whether at least half of the pool's disks are present. Without that,
    /// the members at hand may all be disks that dropped out earlier, and
    /// their database would describe an old state of the pool.
    pub fn has_quorum(&self) -> bool {
        let present = self.disks.values().filter(|d| d.member.is_some()).count();
        present * 2 >= self.disks.len()
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
        SpaceReader::new(self, id, OpenOptions::default())
    }

    /// Opens a space with explicit options.
    pub fn open_space_with(&self, id: u64, options: OpenOptions) -> Result<SpaceReader<'_, D>> {
        SpaceReader::new(self, id, options)
    }

    /// Device index and byte offset of a physical slab, or `None` if its
    /// disk is not present.
    /// Reads the copy of the pool database on member device `device` again
    /// (the pool keeps only the version it uses); `None` if it has none.
    pub fn database_copy(&self, device: usize) -> Result<Option<(DbHeader, Vec<RawRecord>)>> {
        let m = self
            .members
            .iter()
            .find(|m| m.device == device)
            .ok_or_else(|| Error::Pool(format!("device {device} is no pool member")))?;
        read_database(&self.devices[device], m.partition.offset + POOL_DB_OFFSET)
    }

    pub fn slab_location(&self, disk_id: u64, slab: u64) -> Result<Option<(usize, u64)>> {
        let disk = self
            .disks
            .get(&disk_id)
            .ok_or_else(|| format_err!("extent refers to unknown disk {disk_id}"))?;
        let Some(member) = disk.member.map(|m| &self.members[m]) else {
            return Ok(None);
        };
        let pos = slab
            .checked_mul(SLAB_SIZE)
            .and_then(|p| p.checked_add(DATA_AREA_OFFSET))
            .filter(|p| {
                p.checked_add(SLAB_SIZE)
                    .is_some_and(|end| end <= member.partition.length)
            });
        let Some(pos) = pos else {
            return Err(format_err!(
                "slab {slab} of disk {disk_id} lies beyond the partition end"
            ));
        };
        Ok(Some((member.device, member.partition.offset + pos)))
    }

    /// Why space `id` cannot be opened for writing, or `None` if it can
    /// (the checks [`Pool::open_space_rw`] makes, without writing).
    pub fn write_refusal(&self, id: u64) -> Result<Option<String>> {
        // Stale database copies are brought up to date when a space is
        // opened for writing ([`Pool::update_stale_copies`]).
        let unclean: Vec<&String> = self.warnings.iter().filter(|w| !w.ends_with(STALE_COPY)).collect();
        if !unclean.is_empty() {
            return Ok(Some(format!(
                "the pool is not in a clean state ({})",
                unclean.iter().map(|w| w.as_str()).collect::<Vec<_>>().join("; ")
            )));
        }
        let reader = self.open_space(id)?;
        Ok(crate::writer::refusal(self, &reader))
    }

    /// Opens a space for writing; see [`SpaceWriter`] for what it accepts.
    pub fn open_space_rw(&self, id: u64) -> Result<SpaceWriter<'_, D>>
    where
        D: WriteAt,
    {
        SpaceWriter::new(self, id)
    }

    /// Writes into a physical slab. Returns `false` if the disk is not present.
    pub(crate) fn write_slab(&self, disk_id: u64, slab: u64, offset: u64, buf: &[u8]) -> Result<bool>
    where
        D: WriteAt,
    {
        debug_assert!(offset + buf.len() as u64 <= SLAB_SIZE);
        let Some((device, start)) = self.slab_location(disk_id, slab)? else {
            return Ok(false);
        };
        self.devices[device].write_all_at(buf, start + offset)?;
        Ok(true)
    }

    /// Writes `db` as the pool database of every member that carries a
    /// copy, one member after the other, each flushed before the next is
    /// written, so that a crash leaves every copy whole, either old or new
    /// (Windows reads the copy with the newest sequence).
    pub(crate) fn write_database(&self, db: &crate::database::Database) -> Result<()>
    where
        D: WriteAt,
    {
        let mut members: Vec<&Member> = self.members.iter().filter(|m| m.db_sequence.is_some()).collect();
        members.sort_by_key(|m| m.device);
        for m in members {
            let dev = &self.devices[m.device];
            dev.write_all_at(db.bytes(), m.partition.offset + POOL_DB_OFFSET)?;
            dev.flush()?;
        }
        Ok(())
    }

    /// Writes the current pool database to the members whose copy is older
    /// (a crash between the copies of an update leaves them so): each
    /// whole, one after the other. Windows reads the newest copy anyway and
    /// rewrites all of them when it attaches the pool; afterwards every
    /// member carries the same copy.
    pub(crate) fn update_stale_copies(&self) -> Result<()>
    where
        D: WriteAt,
    {
        let retired = |index: usize| {
            self.disks
                .values()
                .any(|d| d.member == Some(index) && d.usage == DiskUsage::Retired)
        };
        let stale: Vec<&Member> = self
            .members
            .iter()
            .enumerate()
            .filter(|(i, m)| m.db_sequence.is_some_and(|s| s < self.database.sequence) && !retired(*i))
            .map(|(_, m)| m)
            .collect();
        if stale.is_empty() {
            return Ok(());
        }
        let db = self.database_model()?;
        for m in stale {
            let dev = &self.devices[m.device];
            dev.write_all_at(db.bytes(), m.partition.offset + POOL_DB_OFFSET)?;
            dev.flush()?;
        }
        Ok(())
    }

    /// The pool database as the model of its updates, read from a member
    /// that carries the current copy.
    pub(crate) fn database_model(&self) -> Result<crate::database::Database> {
        let m = self
            .members
            .iter()
            .find(|m| m.db_sequence == Some(self.database.sequence))
            .ok_or_else(|| Error::Pool("no member carries the current pool database".into()))?;
        crate::database::Database::read_formatted(&self.devices[m.device], m.partition.offset + POOL_DB_OFFSET)
    }

    /// Makes the writes to every member durable.
    pub(crate) fn flush_members(&self) -> Result<()>
    where
        D: WriteAt,
    {
        for m in &self.members {
            self.devices[m.device].flush()?;
        }
        Ok(())
    }

    /// Reads from a physical slab. Returns `false` if the disk is not present.
    pub(crate) fn read_slab(&self, disk_id: u64, slab: u64, offset: u64, buf: &mut [u8]) -> Result<bool> {
        debug_assert!(offset + buf.len() as u64 <= SLAB_SIZE);
        let Some((device, start)) = self.slab_location(disk_id, slab)? else {
            return Ok(false);
        };
        self.devices[device].read_exact_at(buf, start + offset)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::path::Path;

    use super::*;
    use crate::crc::crc32_excluding;
    use crate::io::SparseImage;

    fn fixture(name: &str) -> Vec<SparseImage> {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
        (0..)
            .map_while(|i| File::open(dir.join(format!("disk{i}.fixture"))).ok())
            .map(|f| SparseImage::read_from(f).unwrap())
            .collect()
    }

    /// Byte offset of the pool database of a member.
    fn database_offset(disk: &SparseImage) -> u64 {
        find_spaces_partition(disk).unwrap().unwrap().offset + POOL_DB_OFFSET
    }

    /// Offset of the first payload byte of the SDBB entry holding `(id,
    /// fragment 0)`.
    fn entry_of(disk: &SparseImage, id: u32) -> u64 {
        let db = database_offset(disk);
        (8..4096)
            .map(|slot| db + slot * 0x40)
            .find(|&at| {
                let e = read_vec(disk, at, 0x10).unwrap();
                &e[0..4] == b"SDBB" && e[8..12] == id.to_be_bytes() && e[12..14] == [0, 0]
            })
            .unwrap()
            + 0x10
    }

    fn set_sequence(disk: &mut SparseImage, sequence: u64) {
        let db = database_offset(disk);
        let mut h = read_vec(disk, db, 0x200).unwrap();
        h[0x40..0x48].copy_from_slice(&sequence.to_be_bytes());
        let crc = crc32_excluding(&h, 0x0c);
        h[0x0c..0x10].copy_from_slice(&crc.to_be_bytes());
        disk.insert(db, &h);
    }

    fn pool_record_id(disks: &[SparseImage]) -> u32 {
        let (_, records) = read_database(&disks[0], database_offset(&disks[0])).unwrap().unwrap();
        records.iter().find(|r| r.kind == 1).unwrap().id
    }

    #[test]
    fn reports_a_torn_copy_and_uses_the_majority() {
        let mut disks = fixture("mirror3");
        let pool = Pool::open(disks.clone()).unwrap();
        let copies: Vec<usize> = pool
            .members
            .iter()
            .filter(|m| m.db_sequence.is_some())
            .map(|m| m.device)
            .collect();
        assert!(copies.len() >= 3, "{copies:?}");
        let name = pool.name.clone();
        // An interrupted update left another name in one copy of the pool
        // record (same sequence number).
        let victim = copies[0];
        let at = entry_of(&disks[victim], pool_record_id(&disks)) + 0x1f;
        let mut b = read_vec(&disks[victim], at, 1).unwrap();
        b[0] ^= 0x20;
        disks[victim].insert(at, &b);
        let pool = Pool::open(disks).unwrap();
        assert_eq!(pool.name, name);
        assert!(
            pool.warnings
                .iter()
                .any(|w| w.contains(&format!("[{victim}]")) && w.contains("torn")),
            "{:?}",
            pool.warnings
        );
    }

    #[test]
    fn falls_back_from_an_unusable_newer_copy() {
        let mut disks = fixture("mirror3");
        let pool = Pool::open(disks.clone()).unwrap();
        let sequence = pool.database.sequence;
        let victim = pool.members.iter().find(|m| m.db_sequence.is_some()).unwrap().device;
        // A newer copy whose pool record lost its type: it cannot be used.
        let at = entry_of(&disks[victim], pool_record_id(&disks));
        disks[victim].insert(at, &[9]);
        set_sequence(&mut disks[victim], sequence + 1);
        let pool = Pool::open(disks).unwrap();
        assert_eq!(pool.database.sequence, sequence);
        assert!(
            pool.warnings.iter().any(|w| w.contains("unusable")),
            "{:?}",
            pool.warnings
        );
    }
}
