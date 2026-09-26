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
//! Entries with block `0xffffffff` remove a chunk from the cache (written
//! when the chunk is destaged).
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
/// Block number of an entry that removes a chunk from the cache (destaged).
const NO_BLOCK: u32 = u32::MAX;

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

        // A mapping is current if it is the newest entry for its owner chunk
        // (a newer entry may be a tombstone written when the chunk was
        // destaged) and the newest assignment of its cache block (blocks are
        // reused for other chunks).
        let mut newest_for_chunk: HashMap<u64, (u64, Option<(u64, u64)>)> = HashMap::new();
        let mut newest_for_block: HashMap<u64, u64> = HashMap::new();
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
                let block = le_u32(&e[8..]);
                let valid = le_u32(&e[12..]) as u64;
                if offset % chunk != 0 {
                    return Err(format_err!("bad cache entry: offset {offset:#x}"));
                }
                let target = match block {
                    NO_BLOCK => None,
                    b if (b as u64) < header.chunk_count as u64 => Some((b as u64, valid)),
                    b => return Err(format_err!("bad cache entry: block {b}")),
                };
                let key = offset / chunk;
                if newest_for_chunk.get(&key).is_none_or(|&(s, _)| sequence > s) {
                    newest_for_chunk.insert(key, (sequence, target));
                }
                if let Some((b, _)) = target
                    && newest_for_block.get(&b).is_none_or(|&s| sequence > s)
                {
                    newest_for_block.insert(b, sequence);
                }
            }
        }
        let chunks = newest_for_chunk
            .into_iter()
            .filter_map(|(key, (sequence, target))| {
                let (block, valid) = target?;
                (newest_for_block.get(&block) == Some(&sequence)).then_some((key, (block, valid)))
            })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crc::crc32;

    const GUID: [u8; 16] = [7; 16];
    const CHUNK: u64 = 0x20000;

    fn header() -> CacheHeader {
        CacheHeader {
            owner_guid: Guid::from_mixed_endian(&GUID),
            sequence: 1,
            slot_offset: 0,
            slot_size: 0x1000,
            slot_count: 4,
            data_offset: 0x10_0000,
            chunk_size: CHUNK as u32,
            chunk_count: 16,
        }
    }

    fn slot(sequence: u64, entries: &[(u64, u32)]) -> Vec<u8> {
        let mut s = vec![0u8; 0x1000];
        s[0..8].copy_from_slice(SPSLOT_SIGNATURE);
        s[8..24].copy_from_slice(&GUID);
        s[0x18..0x1c].copy_from_slice(&1u32.to_le_bytes());
        s[0x1c..0x20].copy_from_slice(&0x1000u32.to_le_bytes());
        s[0x28..0x30].copy_from_slice(&sequence.to_le_bytes());
        s[0x30..0x34].copy_from_slice(&(entries.len() as u32).to_le_bytes());
        for (i, &(offset, block)) in entries.iter().enumerate() {
            let e = &mut s[0x38 + i * 16..0x48 + i * 16];
            e[0..8].copy_from_slice(&offset.to_le_bytes());
            e[8..12].copy_from_slice(&block.to_le_bytes());
            e[12..16].copy_from_slice(&3u32.to_le_bytes());
        }
        let crc = crc32(&s);
        s[0x24..0x28].copy_from_slice(&crc.to_le_bytes());
        s
    }

    fn index(slots: &[Vec<u8>]) -> CacheIndex {
        let mut area = slots.concat();
        area.resize(4 * 0x1000, 0);
        CacheIndex::load(header(), CHUNK / 2, |off, buf| {
            buf.copy_from_slice(&area[off as usize..off as usize + buf.len()]);
            Ok(())
        })
        .unwrap()
    }

    fn hit(index: &CacheIndex, offset: u64) -> Option<u64> {
        match index.lookup(offset) {
            Lookup::Hit { cache_offset, .. } => Some(cache_offset),
            Lookup::Miss { .. } => None,
        }
    }

    #[test]
    fn maps_chunks_and_honours_tombstones() {
        let i = index(&[slot(1, &[(0, 2), (CHUNK, 3)]), slot(2, &[(0, NO_BLOCK)])]);
        assert_eq!(hit(&i, 5), None);
        assert_eq!(hit(&i, CHUNK + 5), Some(0x10_0000 + 3 * CHUNK + 5));
    }

    #[test]
    fn reused_block_does_not_resurrect_old_mapping() {
        // Block 2 holds chunk 0, is reused for chunk 1, and chunk 1 is destaged.
        let i = index(&[slot(1, &[(0, 2)]), slot(2, &[(CHUNK, 2)]), slot(3, &[(CHUNK, NO_BLOCK)])]);
        assert_eq!(hit(&i, 0), None);
        assert_eq!(hit(&i, CHUNK), None);
    }

    #[test]
    fn ignores_corrupt_slots() {
        let mut bad = slot(5, &[(0, 1)]);
        bad[0x40] ^= 1;
        let i = index(&[slot(1, &[(0, 2)]), bad]);
        assert_eq!(hit(&i, 0), Some(0x10_0000 + 2 * CHUNK));
    }
}
