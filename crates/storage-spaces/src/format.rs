//! On-disk structures of the pool metadata.
//!
//! Layout of the Storage Spaces partition (offsets relative to its start):
//!
//! * `0x0000` [`DiskHeader`] ("SPACEDB "), one per member disk.
//! * `0x1000` pool database ("SDBC    " header followed by "SDBB" entries).
//! * `0x2000_0000` data area; physical slab `n` starts at `DATA_AREA + n * SLAB_SIZE`.
//!
//! Integers in the database are big-endian. Record bodies mostly use
//! length-prefixed integers (see [`Cursor::varint`]). The layout was
//! reverse-engineered from pools created by Windows 11 (pool version 29);
//! fields that are not understood yet are skipped and named `unknown_*`.

use std::collections::BTreeMap;

use crate::crc::crc32_excluding;
use crate::error::{Result, format_err};
use crate::guid::Guid;
use crate::io::{ReadAt, read_vec};

pub const SPACEDB_SIGNATURE: &[u8; 8] = b"SPACEDB ";
pub const SDBC_SIGNATURE: &[u8; 8] = b"SDBC    ";
pub const SDBB_SIGNATURE: &[u8; 4] = b"SDBB";

/// Offset of the pool database relative to the partition start.
pub const POOL_DB_OFFSET: u64 = 0x1000;
/// Allocation unit of the pool.
pub const SLAB_SIZE: u64 = 0x1000_0000;
/// Offset of physical slab 0 relative to the partition start.
pub const DATA_AREA_OFFSET: u64 = 2 * SLAB_SIZE;

/// Per-disk header at the start of the Storage Spaces partition.
#[derive(Debug, Clone)]
pub struct DiskHeader {
    pub version: u16,
    /// FILETIME of the moment the disk was added to the pool.
    pub format_time: u64,
    pub pool_guid: Guid,
    pub disk_guid: Guid,
}

impl DiskHeader {
    pub const SIZE: usize = HEADER_CRC_SPAN;

    pub fn parse(b: &[u8]) -> Result<Self> {
        if b.len() < Self::SIZE || &b[0..8] != SPACEDB_SIGNATURE {
            return Err(format_err!("missing SPACEDB signature"));
        }
        if !header_crc_ok(b) {
            return Err(format_err!("SPACEDB checksum mismatch"));
        }
        let version = u16::from_be_bytes([b[8], b[9]]);
        if version != 3 {
            return Err(crate::Error::Unsupported(format!(
                "SPACEDB header version {version} (only Windows 10/11 layout 3 is known)"
            )));
        }
        Ok(DiskHeader {
            version,
            format_time: be_u64(&b[0x18..]),
            pool_guid: Guid::from_slice(&b[0x20..0x30]).unwrap(),
            disk_guid: Guid::from_slice(&b[0x30..0x40]).unwrap(),
        })
    }
}

/// SPACEDB and SDBC headers carry a big-endian CRC-32 (zlib) at 0x0c over
/// their first 0x200 bytes, computed with the checksum field zeroed.
const HEADER_CRC_SPAN: usize = 0x200;

fn header_crc_ok(b: &[u8]) -> bool {
    crc32_excluding(&b[..HEADER_CRC_SPAN], 0x0c) == be_u32(&b[0x0c..])
}

/// Header of a metadata database ("SDBC").
#[derive(Debug, Clone)]
pub struct DbHeader {
    /// Pool GUID for the pool database, space GUID for a per-space database.
    pub owner_guid: Guid,
    pub entry_size: u32,
    /// Number of entry slots in use (including the 8 slots taken by the header).
    pub entry_count: u32,
    /// Update counter; the copy with the highest value is the newest.
    pub sequence: u64,
    /// FILETIME of the last update.
    pub timestamp: u64,
}

/// A database record reassembled from its SDBB entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawRecord {
    pub id: u32,
    pub kind: u8,
    pub version: u8,
    pub body: Vec<u8>,
}

/// Reads a database whose SDBC header starts at `offset`. Returns `None`
/// if the location is empty: in larger pools only some members carry a copy
/// of the pool database.
pub fn read_database<D: ReadAt + ?Sized>(dev: &D, offset: u64) -> Result<Option<(DbHeader, Vec<RawRecord>)>> {
    let h = read_vec(dev, offset, 0x200)?;
    if h.iter().all(|&b| b == 0) {
        return Ok(None);
    }
    if &h[0..8] != SDBC_SIGNATURE {
        return Err(format_err!("missing SDBC signature at {offset:#x}"));
    }
    if !header_crc_ok(&h) {
        return Err(format_err!("SDBC checksum mismatch at {offset:#x}"));
    }
    let header = DbHeader {
        owner_guid: Guid::from_slice(&h[0x10..0x20]).unwrap(),
        entry_size: be_u32(&h[0x24..]),
        entry_count: be_u32(&h[0x28..]),
        sequence: be_u64(&h[0x40..]),
        timestamp: be_u64(&h[0x48..]),
    };
    let entry_size = header.entry_size as usize;
    if !(0x20..=0x1000).contains(&entry_size)
        || header.entry_count > 1 << 20
        || entry_size * header.entry_count as usize > 64 << 20
    {
        return Err(format_err!(
            "implausible database geometry: entry size {entry_size:#x}, {} entries",
            header.entry_count
        ));
    }
    let raw = read_vec(dev, offset, header.entry_count as usize * entry_size)?;
    let records = assemble_records(&raw, entry_size)?;
    Ok(Some((header, records)))
}

/// Groups SDBB entries by record id and concatenates their fragments.
/// `raw` is the whole database (SDBC header entries included).
pub fn assemble_records(raw: &[u8], entry_size: usize) -> Result<Vec<RawRecord>> {
    // record id -> fragment index -> payload
    let mut fragments: BTreeMap<u32, (u16, BTreeMap<u16, &[u8]>)> = BTreeMap::new();
    for (slot, entry) in raw.chunks_exact(entry_size).enumerate().skip(8) {
        if &entry[0..4] != SDBB_SIGNATURE {
            continue;
        }
        if be_u32(&entry[4..]) as usize != slot {
            return Err(format_err!(
                "SDBB entry {slot} claims to be slot {}",
                be_u32(&entry[4..])
            ));
        }
        let id = be_u32(&entry[8..]);
        let index = u16::from_be_bytes([entry[12], entry[13]]);
        let count = u16::from_be_bytes([entry[14], entry[15]]);
        if id == 0 || count == 0 {
            continue; // free entry
        }
        let slot = fragments.entry(id).or_insert((count, BTreeMap::new()));
        if slot.0 != count || index >= count || slot.1.insert(index, &entry[16..]).is_some() {
            return Err(format_err!("inconsistent fragments of record {id}"));
        }
    }
    let mut records = Vec::with_capacity(fragments.len());
    for (id, (count, parts)) in fragments {
        if parts.len() != count as usize {
            return Err(format_err!("record {id}: {} of {count} fragments present", parts.len()));
        }
        let data: Vec<u8> = parts.into_values().flatten().copied().collect();
        if data.len() < 8 {
            return Err(format_err!("record {id} too short"));
        }
        let len = be_u32(&data[4..]) as usize;
        let body = data
            .get(8..8 + len)
            .ok_or_else(|| format_err!("record {id}: length {len} exceeds its fragments"))?
            .to_vec();
        records.push(RawRecord {
            id,
            kind: data[0],
            version: data[1],
            body,
        });
    }
    Ok(records)
}

/// Record type 1: the pool.
#[derive(Debug, Clone)]
pub struct PoolRecord {
    pub guid: Guid,
    pub name: String,
    /// Pool version as shown by `Get-StoragePool` ("Version 29").
    pub version: u16,
    /// Logical sector size of the pool; spaces inherit it.
    pub logical_sector_size: u32,
    pub physical_sector_size: u32,
}

/// Record type 2: a physical disk.
#[derive(Debug, Clone)]
pub struct DiskRecord {
    pub id: u64,
    pub guid: Guid,
    pub name: String,
}

/// Role of a space, from the byte preceding its size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpaceRole {
    /// Internal space holding per-space metadata databases.
    Metadata,
    /// A virtual disk visible to the user.
    User,
    /// Container of a space's write-back cache (its child holds "SPCACHE").
    Cache,
    Other(u8),
}

impl SpaceRole {
    fn from_byte(b: u8) -> Self {
        match b {
            0x01 => SpaceRole::Metadata,
            0x02 => SpaceRole::User,
            0x0b => SpaceRole::Cache,
            other => SpaceRole::Other(other),
        }
    }
}

/// Resiliency type of a space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resiliency {
    Simple,
    Mirror,
    Parity,
    Other(u8),
}

/// Data placement parameters of a space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    pub resiliency: Resiliency,
    /// Number of disk failures tolerated.
    pub redundancy: u64,
    pub copies: u64,
    pub groups: u64,
    pub columns: u64,
    pub interleave: u64,
}

/// Record types 3 (space) and 6 (child space).
#[derive(Debug, Clone)]
pub struct SpaceRecord {
    pub id: u64,
    pub guid: Guid,
    pub name: String,
    /// True for record type 6.
    pub is_child: bool,
    pub role: SpaceRole,
    /// Layout version of the record.
    pub record_version: u8,
    /// Provisioned size in bytes (type 3 records only).
    pub size: Option<u64>,
    pub provisioning: Provisioning,
    /// Allocation unit in bytes (all ones on tier definitions).
    pub allocation_unit: u64,
    pub policy: Option<Policy>,
    /// Id of the parent space, 0 for top-level spaces.
    pub parent: Option<u64>,
    /// Child spaces: byte range of the parent's address space they cover
    /// (a storage tier, or the whole cache).
    pub range: Option<(u64, u64)>,
}

/// Record type 4: one run of physical slabs backing a column copy of a space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtentRecord {
    pub space_id: u64,
    /// Virtual slab number where the run starts (in data-slab units).
    pub virtual_slab: u64,
    pub column: u64,
    pub copy: u64,
    /// Number of consecutive slabs in the run.
    pub slab_count: u64,
    pub disk_id: u64,
    /// Physical slab number on the disk.
    pub physical_slab: u64,
    /// 0x04 on cache extents; 0x01 on a copy being regenerated.
    pub flags: u8,
    /// `0xffffffff` for a current copy; otherwise the copy is out of date
    /// (it missed writes while its disk was away).
    pub stale_marker: u64,
}

impl ExtentRecord {
    /// Flag of a copy that Windows is still rebuilding.
    pub const FLAG_REGENERATING: u8 = 0x01;
    const CURRENT: u64 = 0xffff_ffff;

    /// Whether the copy holds current data.
    pub fn is_current(&self) -> bool {
        self.stale_marker == Self::CURRENT && self.flags & Self::FLAG_REGENERATING == 0
    }
}

/// A decoded database record.
#[derive(Debug, Clone)]
pub enum Record {
    Pool(PoolRecord),
    Disk(DiskRecord),
    Space(SpaceRecord),
    Extent(ExtentRecord),
    Other { kind: u8 },
}

impl Record {
    pub fn decode(raw: &RawRecord) -> Result<Record> {
        let ctx = |e: crate::Error| format_err!("record {} (type {}): {e}", raw.id, raw.kind);
        let mut c = Cursor::new(&raw.body);
        match raw.kind {
            1 => decode_pool(&mut c).map(Record::Pool).map_err(ctx),
            2 => {
                let id = c.varint().map_err(ctx)?;
                c.varint().map_err(ctx)?;
                let guid = c.guid().map_err(ctx)?;
                let name = c.string().map_err(ctx)?;
                Ok(Record::Disk(DiskRecord { id, guid, name }))
            }
            3 | 6 => decode_space(&mut c, raw.kind == 6, raw.version)
                .map(Record::Space)
                .map_err(ctx),
            4 => decode_extent(&mut c).map(Record::Extent).map_err(ctx),
            kind => Ok(Record::Other { kind }),
        }
    }
}

fn decode_pool(c: &mut Cursor) -> Result<PoolRecord> {
    c.varint()?;
    c.varint()?;
    let guid = c.guid()?;
    let name = c.string()?;
    let _description = c.string()?;
    c.varint()?;
    let version = c.u16()?;
    let mut sector = || -> Result<u32> {
        match c.u8()? {
            log2 @ 9..=16 => Ok(1 << log2),
            other => Err(format_err!("sector size 2^{other}")),
        }
    };
    let logical_sector_size = sector()?;
    let physical_sector_size = sector()?;
    Ok(PoolRecord {
        guid,
        name,
        version,
        logical_sector_size,
        physical_sector_size,
    })
}

/// Bytes between the provisioning fields and the placement policy of a
/// space record, by record layout: Windows 11 24H2 (pool version 28, space
/// records version 16) and Insider build 26340 (pool version 29, version 17;
/// its child records, type 6, are version 5).
const POLICY_PREFIXES: [&[u8]; 2] = [&[0x01, 0x00, 0x01, 0x00, 0x00], &[0x01, 0x01, 0x00, 0x00]];

/// Thin or fixed provisioning of a space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provisioning {
    Thin,
    Fixed,
    Other(u8),
}

fn decode_space(c: &mut Cursor, is_child: bool, record_version: u8) -> Result<SpaceRecord> {
    let id = c.varint()?;
    c.varint()?;
    let guid = c.guid()?;
    let name = c.string()?;
    let _description = c.string()?;
    c.u8()?;
    c.u8()?;
    let role = SpaceRole::from_byte(c.u8()?);
    let size = if is_child { None } else { Some(c.varint()?) };

    // Type 3: size, an unknown number (0 on user spaces), then like type 6:
    // provisioning (1 thin, 2 fixed), allocation unit (all ones on tier
    // definitions), an unknown byte (2 on tiered spaces), a constant prefix
    // that depends on the record layout, and the policy.
    if !is_child {
        c.varint()?;
    }
    let provisioning = match c.u8()? {
        1 => Provisioning::Thin,
        2 => Provisioning::Fixed,
        other => Provisioning::Other(other),
    };
    let allocation_unit = c.varint()?;
    c.u8()?;
    let rest = c.remaining();
    let Some(prefix) = POLICY_PREFIXES.iter().find(|p| rest.starts_with(p)) else {
        return Ok(SpaceRecord {
            id,
            guid,
            name,
            is_child,
            role,
            record_version,
            size,
            provisioning,
            allocation_unit,
            policy: None,
            parent: None,
            range: None,
        });
    };
    c.skip(prefix.len())?;
    let resiliency = match c.u8()? {
        1 => Resiliency::Simple,
        2 => Resiliency::Mirror,
        3 => Resiliency::Parity,
        other => Resiliency::Other(other),
    };
    let redundancy = c.varint()?;
    let copies = c.varint()?;
    let groups = c.varint()?;
    let columns = c.varint()?;
    let interleave_log2 = c.u8()?;
    if columns == 0 || !(9..=32).contains(&interleave_log2) {
        return Err(format_err!(
            "implausible policy: {columns} columns, interleave 2^{interleave_log2}"
        ));
    }
    let policy = Policy {
        resiliency,
        redundancy,
        copies,
        groups,
        columns,
        interleave: 1 << interleave_log2,
    };

    let mut range = None;
    let parent = if is_child {
        c.varint()?;
        let parent = c.varint()?;
        // u32, then the start and length within the parent (u64 BE each).
        if c.remaining().len() >= 20 {
            c.skip(4)?;
            let start = be_u64(c.take(8)?);
            let length = be_u64(c.take(8)?);
            range = Some((start, length));
        }
        Some(parent)
    } else {
        for _ in 0..6 {
            c.varint()?;
        }
        Some(c.varint()?)
    };
    Ok(SpaceRecord {
        id,
        guid,
        name,
        is_child,
        role,
        record_version,
        size,
        provisioning,
        allocation_unit,
        policy: Some(policy),
        parent,
        range,
    })
}

fn decode_extent(c: &mut Cursor) -> Result<ExtentRecord> {
    c.varint()?;
    c.varint()?;
    c.varint()?;
    let flags = c.u8()?;
    let slab_count = c.varint()?;
    let space_id = c.varint()?;
    let virtual_slab = c.varint()?;
    let column = c.varint()?;
    let copy = c.varint()?;
    let stale_marker = c.varint()?;
    let disk_id = c.varint()?;
    let physical_slab = c.varint()?;
    Ok(ExtentRecord {
        flags,
        stale_marker,
        space_id,
        virtual_slab,
        column,
        copy,
        slab_count,
        disk_id,
        physical_slab,
    })
}

/// Sequential reader over a record body.
pub struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Cursor { data, pos: 0 }
    }

    pub fn remaining(&self) -> &'a [u8] {
        &self.data[self.pos..]
    }

    pub fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.data.len());
        let end = end.ok_or_else(|| format_err!("truncated at offset {}", self.pos))?;
        let s = &self.data[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    pub fn skip(&mut self, n: usize) -> Result<()> {
        self.take(n).map(|_| ())
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    /// Length-prefixed big-endian integer: one length byte, then that many bytes.
    pub fn varint(&mut self) -> Result<u64> {
        let len = self.u8()? as usize;
        if len > 8 {
            return Err(format_err!("integer of {len} bytes at offset {}", self.pos - 1));
        }
        Ok(self.take(len)?.iter().fold(0, |acc, &b| acc << 8 | b as u64))
    }

    pub fn guid(&mut self) -> Result<Guid> {
        Ok(Guid::from_slice(self.take(16)?).unwrap())
    }

    /// UTF-16BE string prefixed by its length in code units (including the terminator).
    pub fn string(&mut self) -> Result<String> {
        let units = self.u16()? as usize;
        let bytes = self.take(units * 2)?;
        let chars: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|p| u16::from_be_bytes([p[0], p[1]]))
            .collect();
        let s = String::from_utf16_lossy(&chars);
        Ok(s.trim_end_matches('\0').to_string())
    }
}

pub(crate) fn be_u32(b: &[u8]) -> u32 {
    u32::from_be_bytes(b[..4].try_into().unwrap())
}

pub(crate) fn be_u64(b: &[u8]) -> u64 {
    u64::from_be_bytes(b[..8].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        s.split_whitespace()
            .map(|b| u8::from_str_radix(b, 16).unwrap())
            .collect()
    }

    fn raw(kind: u8, body: &str) -> RawRecord {
        RawRecord {
            id: 1,
            kind,
            version: 0,
            body: hex(body),
        }
    }

    fn disk_header() -> Vec<u8> {
        let mut b = vec![0u8; DiskHeader::SIZE];
        b[0..8].copy_from_slice(SPACEDB_SIGNATURE);
        b[8..10].copy_from_slice(&3u16.to_be_bytes());
        b[0x20..0x30].copy_from_slice(&[0x11; 16]);
        b[0x30..0x40].copy_from_slice(&[0x22; 16]);
        let crc = crate::crc::crc32(&b);
        b[0x0c..0x10].copy_from_slice(&crc.to_be_bytes());
        b
    }

    #[test]
    fn checks_header_crc() {
        let mut b = disk_header();
        let h = DiskHeader::parse(&b).unwrap();
        assert_eq!(h.pool_guid, Guid([0x11; 16]));
        b[0x1ff] ^= 1; // inside the checksummed range
        assert!(DiskHeader::parse(&b).is_err());
    }

    #[test]
    fn decodes_pool() {
        // Pool "ss-sect512": version 29, 512-byte logical and 4 KiB physical sectors.
        let r = raw(
            1,
            "00 01 01 75 4b e5 85 b8 7c 43 e9 8c 59 99 77 1f d2 b4 fe 00 0b 00 73 00 73 00 2d 00 73 00 65 00 63 00 74 \
             00 35 00 31 00 32 00 00 00 00 00 00 1d 09 0c 46 01 01 00",
        );
        let Record::Pool(p) = Record::decode(&r).unwrap() else {
            panic!()
        };
        assert_eq!(p.name, "ss-sect512");
        assert_eq!(
            (p.version, p.logical_sector_size, p.physical_sector_size),
            (29, 512, 4096)
        );
    }

    #[test]
    fn decodes_extent() {
        // Column 1 of a thin 2-column space, virtual slab 2 (from pool "thin2c").
        let r = raw(
            4,
            "00 01 02 00 04 01 01 01 1e 01 02 01 01 00 04 ff ff ff ff 01 02 01 02",
        );
        let Record::Extent(e) = Record::decode(&r).unwrap() else {
            panic!()
        };
        assert_eq!(
            e,
            ExtentRecord {
                flags: 4,
                stale_marker: 0xffff_ffff,
                space_id: 0x1e,
                virtual_slab: 2,
                column: 1,
                copy: 0,
                slab_count: 1,
                disk_id: 2,
                physical_slab: 2
            }
        );
    }

    #[test]
    fn decodes_user_space() {
        // Fixed simple space "simple2c": 2 GiB, 2 columns, 64 KiB interleave.
        let r = raw(
            3,
            "01 05 01 02 ff c0 f2 4d f9 87 4f 9e 97 1f 64 6e 82 21 83 49 00 09 00 73 00 69 00 6d 00 70 00 6c 00 65 00 32 \
             00 63 00 00 00 00 00 00 02 04 80 00 00 00 00 02 04 40 00 00 00 00 01 00 01 00 00 01 00 01 01 01 01 01 02 10 \
             04 40 00 00 00 00 00 00 00 01 01 00 00 00 00 00",
        );
        let Record::Space(s) = Record::decode(&r).unwrap() else {
            panic!()
        };
        assert_eq!(s.id, 5);
        assert_eq!(s.name, "simple2c");
        assert_eq!(s.guid.to_string(), "ffc0f24d-f987-4f9e-971f-646e82218349");
        assert_eq!(s.role, SpaceRole::User);
        assert_eq!(s.size, Some(0x8000_0000));
        assert_eq!((s.provisioning, s.allocation_unit), (Provisioning::Fixed, 0x4000_0000));
        assert_eq!(s.parent, Some(0));
        let p = s.policy.unwrap();
        assert_eq!(
            (p.resiliency, p.copies, p.columns, p.interleave),
            (Resiliency::Simple, 1, 2, 0x10000)
        );
    }

    #[test]
    fn decodes_windows_11_24h2_space() {
        // The same configuration created by Windows 11 24H2 (pool version 28,
        // record layout 16): a shorter prefix before the policy.
        let mut r = raw(
            3,
            "01 05 01 02 a8 7d fe 00 b5 45 4b 30 ab a4 2b ab d7 dd 33 2f 00 0f 00 73 00 69 00 6d 00 70 00 6c 00 65 00 32 \
             00 63 00 5f 00 32 00 36 00 31 00 30 00 30 00 00 00 00 00 00 02 04 80 00 00 00 00 02 04 40 00 00 00 00 01 01 \
             00 00 01 00 01 01 01 01 01 02 10 00 00 00 00 00 01 01 00 00 00 00 00",
        );
        r.version = 16;
        let Record::Space(s) = Record::decode(&r).unwrap() else {
            panic!()
        };
        assert_eq!((s.name.as_str(), s.record_version), ("simple2c_26100", 16));
        assert_eq!((s.size, s.provisioning), (Some(0x8000_0000), Provisioning::Fixed));
        assert_eq!(s.parent, Some(0));
        let p = s.policy.unwrap();
        assert_eq!(
            (p.resiliency, p.copies, p.columns, p.interleave),
            (Resiliency::Simple, 1, 2, 0x10000)
        );
    }

    #[test]
    fn decodes_thin_and_child_spaces() {
        let thin = raw(
            3,
            "01 05 01 02 00 11 22 33 44 55 66 77 88 99 aa bb cc dd ee ff 00 07 00 74 00 68 00 69 00 6e 00 32 00 63 00 00 \
             00 00 00 00 02 05 04 00 00 00 00 00 01 04 10 00 00 00 00 01 00 01 00 00 01 00 01 01 01 01 01 02 12 04 40 \
             00 00 00 00 00 00 00 01 01 00 00 00 00 00",
        );
        let Record::Space(s) = Record::decode(&thin).unwrap() else {
            panic!()
        };
        assert_eq!(s.size, Some(0x4_0000_0000));
        assert_eq!((s.provisioning, s.allocation_unit), (Provisioning::Thin, 0x1000_0000));
        assert_eq!(s.policy.unwrap().interleave, 0x40000);

        let child = raw(
            6,
            "01 1e 01 02 00 11 22 33 44 55 66 77 88 99 aa bb cc dd ee ff 00 00 00 00 00 00 07 02 04 10 00 00 00 02 01 00 \
             01 00 00 01 00 01 01 01 01 01 02 12 00 01 1d 00 00 00 01 00 00 00 00 00 00 00 00 00 00 00 00 40 00 00 00",
        );
        let Record::Space(s) = Record::decode(&child).unwrap() else {
            panic!()
        };
        assert!(s.is_child);
        assert_eq!(s.parent, Some(0x1d));
        assert_eq!(s.policy.unwrap().columns, 2);
    }
}
