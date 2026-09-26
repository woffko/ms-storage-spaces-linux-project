//! The per-space write-back cache ("SPCACHE").
//!
//! Recent Windows builds give each virtual disk a hidden cache space. Writes
//! (notably to not yet allocated regions of thin spaces) land there first and
//! stay there until destaged, even across a clean shutdown, so reads have to
//! consult it. Layout of the cache space (little-endian):
//!
//! * `0x0` header: "SPCACHE\0", owner space GUID (mixed-endian), geometry.
//! * slot area: `slot_count` log records of `slot_size` bytes ("SPSLOT\0\0").
//!   Slots of type 0 map chunks of the owner space to cache blocks; other
//!   types carry state that is not decoded yet.
//!
//! Both structures carry a CRC-32 at `0x24` computed with that field zeroed,
//! over the size stored at `0x1c`.
//! * data area: `chunk_count` blocks of `chunk_size` bytes (one full stripe).

use std::collections::HashMap;

use crate::crc::crc32_excluding;
use crate::error::{Result, format_err};
use crate::guid::Guid;

pub const SPCACHE_SIGNATURE: &[u8; 8] = b"SPCACHE\0";
pub const SPSLOT_SIGNATURE: &[u8; 8] = b"SPSLOT\0\0";
const SLOT_TYPE_MAPPING: u32 = 0;

#[derive(Debug, Clone)]
pub struct CacheHeader {
    pub owner_guid: Guid,
    pub sequence: u64,
    pub slot_offset: u64,
    pub slot_size: u32,
    pub slot_count: u32,
    pub data_offset: u64,
    pub chunk_size: u32,
    pub chunk_count: u32,
}

impl CacheHeader {
    pub const SIZE: usize = 0x60;

    /// Returns `None` if the cache was never initialised.
    pub fn parse(b: &[u8]) -> Result<Option<Self>> {
        if &b[0..8] != SPCACHE_SIGNATURE {
            return Ok(None);
        }
        if le_u32(&b[0x1c..]) as usize != Self::SIZE || crc32_excluding(&b[..Self::SIZE], 0x24) != le_u32(&b[0x24..]) {
            return Err(format_err!("cache header checksum mismatch"));
        }
        let h = CacheHeader {
            owner_guid: Guid::from_mixed_endian(b[8..24].try_into().unwrap()),
            sequence: le_u64(&b[0x28..]),
            slot_offset: le_u64(&b[0x30..]),
            slot_size: le_u32(&b[0x38..]),
            slot_count: le_u32(&b[0x3c..]),
            data_offset: le_u64(&b[0x50..]),
            chunk_size: le_u32(&b[0x58..]),
            chunk_count: le_u32(&b[0x5c..]),
        };
        if !(0x40..=0x10000).contains(&h.slot_size) || h.chunk_size == 0 || h.chunk_size % 4096 != 0 {
            return Err(format_err!("implausible cache geometry: {h:?}"));
        }
        Ok(Some(h))
    }
}

/// Result of looking up an owner-space offset in the cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lookup {
    /// Data is in the cache space at `cache_offset` for `len` bytes.
    Hit { cache_offset: u64, len: u64 },
    /// Data is not cached for the next `len` bytes.
    Miss { len: u64 },
}

/// Mapping of cached chunks of the owner space.
#[derive(Debug, Clone)]
pub struct CacheIndex {
    pub header: CacheHeader,
    /// Granularity of the per-chunk validity bitmap.
    unit: u64,
    /// Owner chunk number -> (cache block, validity bitmap).
    chunks: HashMap<u64, (u64, u64)>,
}

impl CacheIndex {
    /// Builds the index from the cache header and slot area. `read` reads
    /// from the cache space; `unit` is the owner space interleave.
    pub fn load(header: CacheHeader, unit: u64, mut read: impl FnMut(u64, &mut [u8]) -> Result<()>) -> Result<Self> {
        let chunk = header.chunk_size as u64;
        if unit == 0 || chunk % unit != 0 || chunk / unit > 64 {
            return Err(format_err!("cache chunk {chunk:#x} does not fit interleave {unit:#x}"));
        }
        let slot_size = header.slot_size as usize;
        let max_entries = (slot_size - 0x38) / 16;
        let mut area = vec![0u8; slot_size * header.slot_count as usize];
        read(header.slot_offset, &mut area)?;

        // Latest mapping per owner chunk, then per cache block (a block may
        // have been reused for another chunk by a later slot).
        let mut by_chunk: HashMap<u64, (u64, u64, u64)> = HashMap::new();
        for slot in area.chunks_exact(slot_size) {
            if &slot[0..8] != SPSLOT_SIGNATURE
                || Guid::from_mixed_endian(slot[8..24].try_into().unwrap()) != header.owner_guid
                || le_u32(&slot[0x1c..]) as usize != slot_size
                || crc32_excluding(slot, 0x24) != le_u32(&slot[0x24..])
            {
                continue; // unused, stale or torn slot
            }
            if le_u32(&slot[0x20..]) != SLOT_TYPE_MAPPING {
                continue;
            }
            let sequence = le_u64(&slot[0x28..]);
            let count = le_u32(&slot[0x30..]) as usize;
            if count > max_entries {
                return Err(format_err!("cache slot with {count} entries"));
            }
            for e in slot[0x38..0x38 + count * 16].chunks_exact(16) {
                let offset = le_u64(e);
                let block = le_u32(&e[8..]) as u64;
                let valid = le_u32(&e[12..]) as u64;
                if offset % chunk != 0 || block >= header.chunk_count as u64 {
                    return Err(format_err!("bad cache entry: offset {offset:#x} block {block}"));
                }
                let key = offset / chunk;
                if by_chunk.get(&key).is_none_or(|&(s, _, _)| sequence > s) {
                    by_chunk.insert(key, (sequence, block, valid));
                }
            }
        }
        let mut by_block: HashMap<u64, (u64, u64, u64)> = HashMap::new();
        for (key, (sequence, block, valid)) in by_chunk {
            if by_block.get(&block).is_none_or(|&(s, _, _)| sequence > s) {
                by_block.insert(block, (sequence, key, valid));
            }
        }
        let chunks = by_block
            .into_iter()
            .map(|(block, (_, key, valid))| (key, (block, valid)))
            .collect();
        Ok(CacheIndex { header, unit, chunks })
    }

    /// Number of chunks currently held in the cache.
    pub fn cached_chunks(&self) -> usize {
        self.chunks.len()
    }

    pub fn lookup(&self, offset: u64) -> Lookup {
        let chunk = self.header.chunk_size as u64;
        let within = offset % chunk;
        let len = self.unit - within % self.unit;
        match self.chunks.get(&(offset / chunk)) {
            Some(&(block, valid)) if valid >> (within / self.unit) & 1 == 1 => Lookup::Hit {
                cache_offset: self.header.data_offset + block * chunk + within,
                len,
            },
            _ => Lookup::Miss { len },
        }
    }
}

fn le_u32(b: &[u8]) -> u32 {
    u32::from_le_bytes(b[..4].try_into().unwrap())
}

fn le_u64(b: &[u8]) -> u64 {
    u64::from_le_bytes(b[..8].try_into().unwrap())
}
