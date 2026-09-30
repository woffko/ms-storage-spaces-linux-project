//! Complete models of the pool database records that management operations
//! write: pool (type 1), physical disk (type 2), space (type 3) and child
//! space (type 6). Every byte of a record is either a field here or a
//! constant that every pool Windows created carries; `decode` refuses
//! records it could not reproduce byte for byte, so a record is only ever
//! rewritten when it is fully understood (see docs/storage-spaces-format.md,
//! "Records").

use crate::error::{Result, format_err};
use crate::format::{Cursor, encode_string, encode_varint};
use crate::guid::Guid;

/// Pool record fields between the sector sizes and the security descriptor
/// (pool-wide defaults, among them the thin provisioning alert threshold of
/// 70 %), by pool record version: 15 (Windows 11 24H2) and 16 (Insider
/// build 26340).
const POOL_SETTINGS: [(u8, &[u8]); 2] = [
    (
        15,
        &[
            0x46, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x02, 0x08, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0x00, 0x01, 0x01, 0x02, 0x08, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00, 0x00,
            0x00,
        ],
    ),
    (
        16,
        &[
            0x46, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x02, 0x08, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0x00, 0x01, 0x00, 0x01, 0x02, 0x08, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00,
            0x00, 0x00,
        ],
    ),
];

/// The end of every pool record: the default placement of the three
/// resiliency settings (simple: no redundancy, one copy; mirror: one disk
/// failure, two copies; parity: one disk failure), each with automatic
/// columns and 256 KiB interleave.
const POOL_RESILIENCY_DEFAULTS: &[u8] = &[
    0x01, 0x00, 0x01, 0x01, 0x01, 0x01, 0x04, 0xff, 0xff, 0xff, 0xff, 0x12, 0x02, 0x01, 0x01, 0x01, 0x02, 0x01, 0x01,
    0x04, 0xff, 0xff, 0xff, 0xff, 0x12, 0x03, 0x01, 0x01, 0x01, 0x01, 0x04, 0xff, 0xff, 0xff, 0xff, 0x04, 0xff, 0xff,
    0xff, 0xff, 0x12,
];

/// Bytes between the allocation unit and the placement policy of space
/// records, by layout: space records version 16 and child records version 4
/// (Windows 11 24H2), versions 17 and 5 (Insider build 26340).
const SPACE_PREFIXES: [&[u8]; 2] = [&[0x01, 0x01, 0x00, 0x00], &[0x01, 0x00, 0x01, 0x00, 0x00]];

fn understood(what: &str, body: &[u8], encoded: &[u8]) -> Result<()> {
    if encoded == body {
        Ok(())
    } else {
        Err(format_err!("{what} record with a layout that is not understood"))
    }
}

/// A pool record (type 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolBody {
    /// The record's version (byte 1 of the record): 15 or 16.
    pub record_version: u8,
    pub sequence: u64,
    pub guid: Guid,
    pub name: String,
    pub description: String,
    /// Pool version: 28 (Windows 11 24H2), 29 (Insider).
    pub version: u16,
    pub logical_sector_log2: u8,
    pub physical_sector_log2: u8,
    /// Empty until the pool is changed through the management API.
    pub security_descriptor: Vec<u8>,
}

impl PoolBody {
    pub fn decode(record_version: u8, body: &[u8]) -> Result<Self> {
        let mut c = Cursor::new(body);
        let id = c.varint()?;
        let sequence = c.varint()?;
        let guid = c.guid()?;
        let name = c.string()?;
        let description = c.string()?;
        let a = c.varint()?;
        let version = c.u16()?;
        let logical_sector_log2 = c.u8()?;
        let physical_sector_log2 = c.u8()?;
        let settings = pool_settings(record_version)?;
        c.skip(settings.len())?;
        let len = c.u8()? as usize;
        let security_descriptor = c.take(len)?.to_vec();
        let pool = PoolBody {
            record_version,
            sequence,
            guid,
            name,
            description,
            version,
            logical_sector_log2,
            physical_sector_log2,
            security_descriptor,
        };
        if id != 0 || a != 0 {
            return Err(format_err!("pool record with a layout that is not understood"));
        }
        understood("pool", body, &pool.encode()?)?;
        Ok(pool)
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut out = encode_varint(0);
        out.extend(encode_varint(self.sequence));
        out.extend_from_slice(&self.guid.0);
        out.extend(encode_string(&self.name));
        out.extend(encode_string(&self.description));
        out.extend(encode_varint(0));
        out.extend_from_slice(&self.version.to_be_bytes());
        out.push(self.logical_sector_log2);
        out.push(self.physical_sector_log2);
        out.extend_from_slice(pool_settings(self.record_version)?);
        out.push(security_len(&self.security_descriptor)?);
        out.extend_from_slice(&self.security_descriptor);
        out.extend_from_slice(POOL_RESILIENCY_DEFAULTS);
        Ok(out)
    }
}

fn pool_settings(record_version: u8) -> Result<&'static [u8]> {
    POOL_SETTINGS
        .iter()
        .find(|(v, _)| *v == record_version)
        .map(|(_, s)| *s)
        .ok_or_else(|| format_err!("pool record version {record_version} is not understood"))
}

fn security_len(sd: &[u8]) -> Result<u8> {
    u8::try_from(sd.len()).map_err(|_| format_err!("security descriptor of {} bytes", sd.len()))
}

/// A physical disk record (type 2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskBody {
    pub id: u64,
    pub sequence: u64,
    pub guid: Guid,
    pub name: String,
    pub description: String,
    /// Whether the disk carries a copy of the pool database (2 in the
    /// record, 0 without one: retired disks and all but five members of
    /// larger pools).
    pub database_copy: bool,
    /// Usage as stored (1 Auto-Select ... 5 Retired, see `DiskUsage`).
    pub usage: u8,
    /// Manufacturer and model as the disk reports them, and two more
    /// strings (empty on every disk seen).
    pub manufacturer: String,
    pub model: String,
    pub extra: [String; 2],
    /// Media type as stored: 0 unspecified, 1 HDD, 2 SSD.
    pub media: u8,
    /// The disk's size in bytes.
    pub size: u64,
    /// The pool partition's data area: its length less the 512 MiB before
    /// physical slab 0.
    pub data_size: u64,
}

impl DiskBody {
    pub fn decode(body: &[u8]) -> Result<Self> {
        let mut c = Cursor::new(body);
        let id = c.varint()?;
        let sequence = c.varint()?;
        let guid = c.guid()?;
        let name = c.string()?;
        let description = c.string()?;
        c.u8()?;
        let database_copy = c.u8()? == 2;
        let usage = c.u8()?;
        let manufacturer = c.string()?;
        let model = c.string()?;
        let extra = [c.string()?, c.string()?];
        c.u8()?;
        let media = c.u8()?;
        c.skip(32)?;
        let size = c.varint()?;
        let data_size = c.varint()?;
        let disk = DiskBody {
            id,
            sequence,
            guid,
            name,
            description,
            database_copy,
            usage,
            manufacturer,
            model,
            extra,
            media,
            size,
            data_size,
        };
        understood("disk", body, &disk.encode())?;
        Ok(disk)
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = encode_varint(self.id);
        out.extend(encode_varint(self.sequence));
        out.extend_from_slice(&self.guid.0);
        out.extend(encode_string(&self.name));
        out.extend(encode_string(&self.description));
        out.extend([0, if self.database_copy { 2 } else { 0 }, self.usage]);
        for s in [&self.manufacturer, &self.model, &self.extra[0], &self.extra[1]] {
            out.extend(encode_string(s));
        }
        out.extend([0x0f, self.media]);
        out.extend([0; 32]);
        out.extend(encode_varint(self.size));
        out.extend(encode_varint(self.data_size));
        out.extend(encode_varint(u64::MAX));
        out
    }
}

/// A space record (type 3) or child space record (type 6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpaceBody {
    /// Type 6 (child: a tier, or the space inside a hidden container).
    pub child: bool,
    /// Record layout: 0 = Windows 11 24H2 (type 3 version 16, type 6
    /// version 4), 1 = Insider build 26340 (versions 17 and 5).
    pub layout: usize,
    pub id: u64,
    pub sequence: u64,
    pub guid: Guid,
    pub name: String,
    pub description: String,
    /// 1 on the metadata space and hidden containers, 0 on user spaces and
    /// children.
    pub internal: u8,
    /// Role byte (see `SpaceRole`).
    pub role: u8,
    /// Type 3 only: size in bytes and the space's number in the pool
    /// (0xffffffff on the metadata space).
    pub size: u64,
    pub number: u64,
    /// 1 thin, 2 fixed.
    pub provisioning: u8,
    pub allocation_unit: u64,
    /// 0, or 1-2 on tiered spaces, tiers and containers.
    pub tiering: u8,
    /// 1 simple, 2 mirror, 3 parity.
    pub resiliency: u8,
    pub redundancy: u64,
    pub copies: u64,
    pub groups: u64,
    pub columns: u64,
    pub interleave_log2: u8,
    /// Type 3 only: size of the write-back cache (0 without one).
    pub write_cache: u64,
    /// Type 3 only: empty until changed through the management API.
    pub security_descriptor: Vec<u8>,
    /// Type 3 only: 0 on the metadata space, 1 otherwise.
    pub linked: u64,
    pub parent: u64,
    /// Type 6 only: (1, start, length) within the parent's address space,
    /// or `None` on tier templates, which end in four zero bytes instead.
    pub range: Option<(u32, u64, u64)>,
}

impl SpaceBody {
    pub fn decode(child: bool, body: &[u8]) -> Result<Self> {
        let mut c = Cursor::new(body);
        let id = c.varint()?;
        let sequence = c.varint()?;
        let guid = c.guid()?;
        let name = c.string()?;
        let description = c.string()?;
        c.u8()?;
        let internal = c.u8()?;
        let role = c.u8()?;
        let (size, number) = if child { (0, 0) } else { (c.varint()?, c.varint()?) };
        let provisioning = c.u8()?;
        let allocation_unit = c.varint()?;
        let tiering = c.u8()?;
        let layout = SPACE_PREFIXES
            .iter()
            .position(|p| c.remaining().starts_with(p))
            .ok_or_else(|| format_err!("space record with a layout that is not understood"))?;
        c.skip(SPACE_PREFIXES[layout].len())?;
        let resiliency = c.u8()?;
        let redundancy = c.varint()?;
        let copies = c.varint()?;
        let groups = c.varint()?;
        let columns = c.varint()?;
        let interleave_log2 = c.u8()?;
        let mut space = SpaceBody {
            child,
            layout,
            id,
            sequence,
            guid,
            name,
            description,
            internal,
            role,
            size,
            number,
            provisioning,
            allocation_unit,
            tiering,
            resiliency,
            redundancy,
            copies,
            groups,
            columns,
            interleave_log2,
            write_cache: 0,
            security_descriptor: Vec::new(),
            linked: 0,
            parent: 0,
            range: None,
        };
        if child {
            c.varint()?;
            space.parent = c.varint()?;
            if c.remaining().len() == 20 {
                let k = u32::from_be_bytes(c.take(4)?.try_into().unwrap());
                let start = u64::from_be_bytes(c.take(8)?.try_into().unwrap());
                let length = u64::from_be_bytes(c.take(8)?.try_into().unwrap());
                space.range = Some((k, start, length));
            }
        } else {
            space.write_cache = c.varint()?;
            for _ in 0..3 {
                c.varint()?;
            }
            let len = c.u8()? as usize;
            space.security_descriptor = c.take(len)?.to_vec();
            space.linked = c.varint()?;
            space.parent = c.varint()?;
        }
        understood("space", body, &space.encode()?)?;
        Ok(space)
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut out = encode_varint(self.id);
        out.extend(encode_varint(self.sequence));
        out.extend_from_slice(&self.guid.0);
        out.extend(encode_string(&self.name));
        out.extend(encode_string(&self.description));
        out.extend([0, self.internal, self.role]);
        if !self.child {
            out.extend(encode_varint(self.size));
            out.extend(encode_varint(self.number));
        }
        out.push(self.provisioning);
        out.extend(encode_varint(self.allocation_unit));
        out.push(self.tiering);
        let prefix = SPACE_PREFIXES
            .get(self.layout)
            .ok_or_else(|| format_err!("space record layout {}", self.layout))?;
        out.extend_from_slice(prefix);
        out.push(self.resiliency);
        for v in [self.redundancy, self.copies, self.groups, self.columns] {
            out.extend(encode_varint(v));
        }
        out.push(self.interleave_log2);
        if self.child {
            out.extend(encode_varint(0));
            out.extend(encode_varint(self.parent));
            match self.range {
                Some((k, start, length)) => {
                    out.extend(k.to_be_bytes());
                    out.extend(start.to_be_bytes());
                    out.extend(length.to_be_bytes());
                }
                None => out.extend([0; 4]),
            }
        } else {
            out.extend(encode_varint(self.write_cache));
            out.extend([0, 0, 0]);
            out.push(security_len(&self.security_descriptor)?);
            out.extend_from_slice(&self.security_descriptor);
            out.extend(encode_varint(self.linked));
            out.extend(encode_varint(self.parent));
            out.extend([0; 4]);
        }
        Ok(out)
    }
}
