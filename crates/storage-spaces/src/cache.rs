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

use std::collections::{HashMap, HashSet};

use crate::crc::crc32_excluding;
use crate::error::{Result, format_err};
use crate::guid::Guid;

pub const SPCACHE_SIGNATURE: &[u8; 8] = b"SPCACHE\0";
pub const SPSLOT_SIGNATURE: &[u8; 8] = b"SPSLOT\0\0";
const SLOT_TYPE_MAPPING: u32 = 0;
/// Flag in the offset field of an entry logged before its data was written
/// to the cache; a later entry without the flag commits it. A flagged entry
/// that was never committed (the pool stopped in between) describes data
/// that may not exist, and Windows ignores it (crash experiment
/// `crashparitywc`).
const ENTRY_OFFSET_FLAG: u64 = 1 << 63;
/// Entry states: nothing valid yet, valid runs listed after the entry, whole chunk valid.
const STATE_EMPTY: u16 = 0;
const STATE_PARTIAL: u16 = 2;
const STATE_FULL: u16 = 3;

/// Parses the run list of a partially valid chunk: 16-bit little-endian
/// words, bit 15 = valid, low 15 bits = length in 512-byte sectors; a zero
/// word ends the list.
fn parse_runs(words: &[u8], chunk: u64) -> Result<Vec<(bool, u64)>> {
    let mut runs = Vec::new();
    let mut total = 0;
    for w in words.as_chunks::<2>().0.iter().map(|&w| u16::from_le_bytes(w)) {
        if w == 0 {
            break;
        }
        let sectors = (w & 0x7fff) as u64;
        total += sectors * 512;
        runs.push((w & 0x8000 != 0, sectors));
    }
    if total > chunk {
        return Err(format_err!("cache runs cover {total:#x} bytes of a {chunk:#x} chunk"));
    }
    Ok(runs)
}
/// Where the cache and journal loaders read from. A plain closure reads
/// the space; a mirrored space can also merge the slot areas of its copies.
pub trait SlotSource {
    fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<()>;

    /// Reads a slot area of `len` bytes from every copy of a mirrored
    /// space that can be read completely (copies can differ after an
    /// unclean shutdown, when a slot reached only some of them).
    fn read_slot_copies(&mut self, offset: u64, len: usize) -> Result<Vec<Vec<u8>>> {
        let mut area = vec![0u8; len];
        self.read(offset, &mut area)?;
        Ok(vec![area])
    }
}

/// The newest version of a slot area over its copies: each position takes
/// the valid slot with the highest sequence (slots validate themselves).
pub(crate) fn merge_slot_copies(copies: &[Vec<u8>], slot_size: usize) -> Vec<u8> {
    let mut merged = copies[0].clone();
    for other in &copies[1..] {
        for (mine, theirs) in merged.chunks_exact_mut(slot_size).zip(other.chunks_exact(slot_size)) {
            if slot_sequence(theirs) > slot_sequence(mine) {
                mine.copy_from_slice(theirs);
            }
        }
    }
    merged
}

impl<F: FnMut(u64, &mut [u8]) -> Result<()>> SlotSource for F {
    fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self(offset, buf)
    }
}

/// A valid slot ("SPSLOT") of a cache or parity journal slot area.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    pub index: usize,
    /// 0 = mapping entries; the cache's slot 0 has type 1.
    pub kind: u32,
    pub sequence: u64,
    pub entries: u32,
    /// The slot from its type field on, without trailing zeros.
    pub content: Vec<u8>,
}

/// The valid slots of a slot area.
pub(crate) fn valid_slots(area: &[u8], slot_size: usize) -> Vec<Slot> {
    area.chunks_exact(slot_size)
        .enumerate()
        .filter_map(|(index, slot)| {
            let sequence = slot_sequence(slot)?;
            let end = slot.iter().rposition(|&b| b != 0).map_or(0x20, |p| p + 1).max(0x20);
            Some(Slot {
                index,
                kind: le_u32(&slot[0x20..]),
                sequence,
                entries: le_u32(&slot[0x30..]),
                content: slot[0x20..end].to_vec(),
            })
        })
        .collect()
}

/// How Windows logs writes into a write-back cache (the model the scenario
/// tests check slot by slot): a new cache holds slot 0 of type 1; every
/// write that changes which sectors of a chunk are cached gets the next slot
/// with the next sequence and one mapping entry for each chunk it changes.
/// A chunk entering the cache takes the next block (parity caches start at
/// block 64, mirror caches at 0). A write into sectors already cached
/// changes no slot.
#[derive(Debug, Clone)]
pub struct CacheWriter {
    header: CacheHeader,
    next_slot: usize,
    sequence: u64,
    next_block: u32,
    /// Chunk number -> (block, valid sectors).
    chunks: std::collections::BTreeMap<u64, (u32, Vec<bool>)>,
}

impl CacheWriter {
    /// A new cache described by `header`, whose first block is `first_block`.
    pub fn new(header: CacheHeader, first_block: u32) -> Self {
        CacheWriter {
            header,
            next_slot: 1,
            sequence: 1,
            next_block: first_block,
            chunks: Default::default(),
        }
    }

    /// Slot 0 of a new cache: type 1, sequence 1, the entry (8, 1).
    pub fn init_slot(&self) -> Vec<u8> {
        let mut entry = 8u32.to_le_bytes().to_vec();
        entry.extend_from_slice(&1u32.to_le_bytes());
        self.slot(1, 1, 1, &entry)
    }

    /// A write of `len` bytes at owner offset `offset`: the slots Windows
    /// writes (index and page), none if no chunk changes. A write whose
    /// entries do not fit into one slot continues in the next (where Windows
    /// ends a slot then is not modelled, nor is destaging).
    pub fn write(&mut self, offset: u64, len: u64) -> Vec<(usize, Vec<u8>)> {
        let chunk = self.header.chunk_size as u64;
        let sectors = (chunk / 512) as usize;
        let capacity = (self.header.slot_size as usize).saturating_sub(0x38);
        let mut slots = Vec::new();
        let mut entries = Vec::new();
        let mut pos = 0;
        let mut count = 0;
        let mut at = offset;
        while at < offset + len {
            let key = at / chunk;
            let end = (offset + len).min((key + 1) * chunk);
            let (first, last) = ((at % chunk / 512) as usize, ((end - 1) % chunk / 512) as usize);
            let next_block = &mut self.next_block;
            let (block, valid) = self.chunks.entry(key).or_insert_with(|| {
                let b = *next_block;
                *next_block += 1;
                (b, vec![false; sectors])
            });
            if valid[first..=last].iter().any(|v| !v) {
                valid[first..=last].fill(true);
                let runs = runs_of(valid);
                let full = runs.len() == 1;
                let counted: u16 = if full { 0 } else { 2 * runs.len() as u16 };
                let mut e = (key * chunk).to_le_bytes().to_vec();
                e.extend_from_slice(&block.to_le_bytes());
                e.extend_from_slice(&(if full { STATE_FULL } else { STATE_PARTIAL }).to_le_bytes());
                e.extend_from_slice(&counted.to_le_bytes());
                // The run words are always written, but counted only for a
                // partly valid chunk; the next entry starts 8-byte aligned
                // after the counted part and so overwrites the others.
                for (v, n) in runs {
                    e.extend_from_slice(&((u16::from(v) << 15) | n as u16).to_le_bytes());
                }
                if pos + e.len() > capacity && count > 0 {
                    entries.truncate(pos);
                    slots.push(self.next(count, &entries));
                    (entries, pos, count) = (Vec::new(), 0, 0);
                }
                entries.truncate(pos);
                entries.resize(pos, 0);
                entries.extend_from_slice(&e);
                pos = (pos + 16 + counted as usize).next_multiple_of(8);
                count += 1;
            }
            at = end;
        }
        if count > 0 {
            slots.push(self.next(count, &entries));
        }
        slots
    }

    /// The next slot with `count` entries.
    fn next(&mut self, count: u32, entries: &[u8]) -> (usize, Vec<u8>) {
        self.sequence += 1;
        let index = self.next_slot;
        // Past the last slot the log continues at slot 0, over the type 1
        // record (m5wbc2).
        self.next_slot = (self.next_slot + 1) % self.header.slot_count.max(1) as usize;
        (index, self.slot(0, self.sequence, count, entries))
    }

    fn slot(&self, kind: u32, sequence: u64, count: u32, entries: &[u8]) -> Vec<u8> {
        encode_slot(
            self.header.owner_guid,
            self.header.slot_size,
            kind,
            sequence,
            count,
            entries,
        )
    }
}

/// A slot of a cache or parity journal as Windows writes it: "SPSLOT", the
/// owner GUID (mixed-endian), u32 1, the slot size, the type, the CRC-32 of
/// the slot with its field zeroed, the sequence, the entry count, u32 0 and
/// the entries (cut off at the end of the slot).
pub(crate) fn encode_slot(
    owner: Guid,
    slot_size: u32,
    kind: u32,
    sequence: u64,
    count: u32,
    entries: &[u8],
) -> Vec<u8> {
    let mut s = vec![0u8; slot_size as usize];
    s[..8].copy_from_slice(SPSLOT_SIGNATURE);
    s[8..24].copy_from_slice(&owner.to_mixed_endian());
    s[0x18..0x1c].copy_from_slice(&1u32.to_le_bytes());
    s[0x1c..0x20].copy_from_slice(&slot_size.to_le_bytes());
    s[0x20..0x24].copy_from_slice(&kind.to_le_bytes());
    s[0x28..0x30].copy_from_slice(&sequence.to_le_bytes());
    s[0x30..0x34].copy_from_slice(&count.to_le_bytes());
    let n = entries.len().min(s.len().saturating_sub(0x38));
    s[0x38..0x38 + n].copy_from_slice(&entries[..n]);
    let crc = crc32_excluding(&s, 0x24);
    s[0x24..0x28].copy_from_slice(&crc.to_le_bytes());
    s
}

/// Runs of equally valid sectors, from the chunk start.
fn runs_of(valid: &[bool]) -> Vec<(bool, usize)> {
    let mut runs: Vec<(bool, usize)> = Vec::new();
    for &v in valid {
        match runs.last_mut() {
            Some((last, n)) if *last == v => *n += 1,
            _ => runs.push((v, 1)),
        }
    }
    runs
}

/// Sequence of a slot whose signature and CRC are valid.
pub(crate) fn slot_sequence(slot: &[u8]) -> Option<u64> {
    (slot.len() >= 0x38
        && &slot[0..8] == SPSLOT_SIGNATURE
        && le_u32(&slot[0x1c..]) as usize == slot.len()
        && crc32_excluding(slot, 0x24) == le_u32(&slot[0x24..]))
    .then(|| le_u64(&slot[0x28..]))
}

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
        if !(0x40..=0x10000).contains(&h.slot_size)
            || h.slot_count > 1 << 16
            || (h.slot_size as u64) * (h.slot_count as u64) > 64 << 20
            || h.chunk_size == 0
            || !h.chunk_size.is_multiple_of(4096)
            // Keeps cache offsets far from overflowing (real caches: GiBs).
            || h.data_offset > 1 << 56
            || (h.chunk_size as u64) * (h.chunk_count as u64) > 1 << 56
        {
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

/// Which part of a cached chunk holds valid data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Validity {
    Full,
    /// Runs of 512-byte sectors from the chunk start: (valid, sectors).
    Runs(Vec<(bool, u64)>),
}

/// Owner chunk number -> (cache block, valid part).
type ChunkMap = HashMap<u64, (u64, Validity)>;

/// Mapping of cached chunks of the owner space.
#[derive(Debug, Clone)]
pub struct CacheIndex {
    pub header: CacheHeader,
    chunks: ChunkMap,
    /// Chunks mapped differently by the copies of the slot area (after an
    /// unclean shutdown): which one Windows keeps is not known.
    conflicts: HashSet<u64>,
    slots: Vec<Slot>,
}

impl CacheIndex {
    /// Builds the index from the cache header and slot area. `read` reads
    /// from the cache space; `unit` is the owner space interleave.
    pub fn load(header: CacheHeader, mut read: impl SlotSource) -> Result<Self> {
        let slot_size = header.slot_size as usize;
        let copies = read.read_slot_copies(header.slot_offset, slot_size * header.slot_count as usize)?;
        let merged = merge_slot_copies(&copies, slot_size);
        let chunks = Self::map(&header, &merged)?;
        let mut conflicts = HashSet::new();
        for copy in copies.iter().filter(|c| **c != merged) {
            let other = Self::map(&header, copy)?;
            for key in chunks.keys().chain(other.keys()) {
                if chunks.get(key) != other.get(key) {
                    conflicts.insert(*key);
                }
            }
        }
        let slots = valid_slots(&merged, slot_size);
        Ok(CacheIndex {
            header,
            chunks,
            conflicts,
            slots,
        })
    }

    /// The chunk mapping one version of the slot area describes.
    fn map(header: &CacheHeader, area: &[u8]) -> Result<ChunkMap> {
        let chunk = header.chunk_size as u64;
        let slot_size = header.slot_size as usize;

        // A mapping is current if it is the newest entry for its owner chunk
        // (a newer entry may be a tombstone written when the chunk was
        // destaged) and the newest assignment of its cache block (blocks are
        // reused for other chunks). Entries are ordered by (slot sequence,
        // position in the slot).
        type Version = (u64, usize);
        let mut newest_for_chunk: HashMap<u64, (Version, Option<(u64, Validity)>)> = HashMap::new();
        let mut newest_for_block: HashMap<u64, Version> = HashMap::new();
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
            let mut pos = 0x38;
            for index in 0..count {
                let e = slot
                    .get(pos..pos + 16)
                    .ok_or_else(|| format_err!("cache slot with {count} entries overflows"))?;
                let provisional = le_u64(e) & ENTRY_OFFSET_FLAG != 0;
                let offset = le_u64(e) & !ENTRY_OFFSET_FLAG;
                let block = le_u32(&e[8..]);
                let state = u16::from_le_bytes([e[12], e[13]]);
                // The high half is the length of the data that follows the
                // entry, in bytes; the next entry starts 8-byte aligned.
                let len = u16::from_le_bytes([e[14], e[15]]) as usize;
                let extra = slot
                    .get(pos + 16..pos + 16 + len)
                    .ok_or_else(|| format_err!("cache entry overflows its slot"))?;
                pos += 16 + len.next_multiple_of(8);
                if !offset.is_multiple_of(chunk) {
                    return Err(format_err!("bad cache entry: offset {offset:#x}"));
                }
                let validity = match state {
                    STATE_EMPTY => Validity::Runs(Vec::new()),
                    STATE_PARTIAL => Validity::Runs(parse_runs(extra, chunk)?),
                    STATE_FULL => Validity::Full,
                    other => return Err(crate::Error::Unsupported(format!("cache entry state {other}"))),
                };
                let target = match block {
                    NO_BLOCK => None,
                    b if (b as u64) < header.chunk_count as u64 => Some((b as u64, validity)),
                    b => return Err(format_err!("bad cache entry: block {b}")),
                };
                if provisional {
                    continue;
                }
                let version = (sequence, index);
                let key = offset / chunk;
                if let Some((b, _)) = &target
                    && newest_for_block.get(b).is_none_or(|&v| version > v)
                {
                    newest_for_block.insert(*b, version);
                }
                if newest_for_chunk.get(&key).is_none_or(|(v, _)| version > *v) {
                    newest_for_chunk.insert(key, (version, target));
                }
            }
        }
        Ok(newest_for_chunk
            .into_iter()
            .filter_map(|(key, (version, target))| {
                let (block, valid) = target?;
                (newest_for_block.get(&block) == Some(&version)).then_some((key, (block, valid)))
            })
            .collect())
    }

    /// Whether the copies of the cache disagree about the chunk holding
    /// owner offset `offset`.
    pub fn is_ambiguous(&self, offset: u64) -> bool {
        self.conflicts.contains(&(offset / self.header.chunk_size as u64))
    }

    /// The cached chunks as (owner offset, cache block, valid part), in
    /// offset order.
    pub fn mappings(&self) -> Vec<(u64, u64, &Validity)> {
        let chunk = self.header.chunk_size as u64;
        let mut all: Vec<_> = self.chunks.iter().map(|(k, (b, v))| (k * chunk, *b, v)).collect();
        all.sort_by_key(|m| m.0);
        all
    }

    /// The valid slots of the slot area (the newest version of each).
    pub fn slots(&self) -> &[Slot] {
        &self.slots
    }

    /// Number of chunks the copies of the cache disagree about.
    pub fn conflicting_chunks(&self) -> usize {
        self.conflicts.len()
    }

    /// Number of chunks currently held in the cache.
    pub fn cached_chunks(&self) -> usize {
        self.chunks.len()
    }

    pub fn lookup(&self, offset: u64) -> Lookup {
        let chunk = self.header.chunk_size as u64;
        let within = offset % chunk;
        let hit = |block: u64, len: u64| Lookup::Hit {
            cache_offset: self.header.data_offset + block * chunk + within,
            len,
        };
        match self.chunks.get(&(offset / chunk)) {
            None => Lookup::Miss { len: chunk - within },
            Some((block, Validity::Full)) => hit(*block, chunk - within),
            Some((block, Validity::Runs(runs))) => {
                let mut start = 0;
                for &(valid, sectors) in runs {
                    let end = start + sectors * 512;
                    if within < end {
                        return if valid {
                            hit(*block, end - within)
                        } else {
                            Lookup::Miss { len: end - within }
                        };
                    }
                    start = end;
                }
                Lookup::Miss { len: chunk - within }
            }
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

    #[test]
    fn rejects_implausible_geometry() {
        let mut b = vec![0u8; CacheHeader::SIZE];
        b[0..8].copy_from_slice(SPCACHE_SIGNATURE);
        b[0x1c..0x20].copy_from_slice(&(CacheHeader::SIZE as u32).to_le_bytes());
        b[0x38..0x3c].copy_from_slice(&0x1000u32.to_le_bytes());
        b[0x3c..0x40].copy_from_slice(&0x400u32.to_le_bytes());
        b[0x58..0x5c].copy_from_slice(&0x2_0000u32.to_le_bytes());
        b[0x5c..0x60].copy_from_slice(&8u32.to_le_bytes());
        let with = |at: usize, v: &[u8]| {
            let mut h = b.clone();
            h[at..at + v.len()].copy_from_slice(v);
            if at == 0x58 {
                h[0x5c..0x60].copy_from_slice(&u32::MAX.to_le_bytes()); // chunk count
            }
            let crc = crc32_excluding(&h, 0x24);
            h[0x24..0x28].copy_from_slice(&crc.to_le_bytes());
            CacheHeader::parse(&h)
        };
        assert!(with(0x50, &0x1000u64.to_le_bytes()).unwrap().is_some());
        // Offsets near the end of the address space would overflow lookups.
        assert!(with(0x50, &u64::MAX.to_le_bytes()).is_err());
        assert!(with(0x58, &0xffff_f000u32.to_le_bytes()).is_err());
    }

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
        let full: Vec<(u64, u32, u16, Vec<u16>)> = entries.iter().map(|&(o, b)| (o, b, STATE_FULL, vec![])).collect();
        slot_with(sequence, &full)
    }

    fn slot_with(sequence: u64, entries: &[(u64, u32, u16, Vec<u16>)]) -> Vec<u8> {
        let mut s = vec![0u8; 0x1000];
        s[0..8].copy_from_slice(SPSLOT_SIGNATURE);
        s[8..24].copy_from_slice(&GUID);
        s[0x18..0x1c].copy_from_slice(&1u32.to_le_bytes());
        s[0x1c..0x20].copy_from_slice(&0x1000u32.to_le_bytes());
        s[0x28..0x30].copy_from_slice(&sequence.to_le_bytes());
        s[0x30..0x34].copy_from_slice(&(entries.len() as u32).to_le_bytes());
        let mut pos = 0x38;
        for (offset, block, state, words) in entries {
            s[pos..pos + 8].copy_from_slice(&offset.to_le_bytes());
            s[pos + 8..pos + 12].copy_from_slice(&block.to_le_bytes());
            s[pos + 12..pos + 14].copy_from_slice(&state.to_le_bytes());
            s[pos + 14..pos + 16].copy_from_slice(&(2 * words.len() as u16).to_le_bytes());
            pos += 16;
            for w in words {
                s[pos..pos + 2].copy_from_slice(&w.to_le_bytes());
                pos += 2;
            }
            pos = pos.next_multiple_of(8);
        }
        let crc = crc32(&s);
        s[0x24..0x28].copy_from_slice(&crc.to_le_bytes());
        s
    }

    fn index(slots: &[Vec<u8>]) -> CacheIndex {
        let mut area = slots.concat();
        area.resize(4 * 0x1000, 0);
        CacheIndex::load(header(), |off: u64, buf: &mut [u8]| {
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
    fn writer_splits_entries_that_do_not_fit_into_one_slot() {
        let header = CacheHeader {
            owner_guid: Guid([7; 16]),
            sequence: 1,
            slot_offset: 0,
            slot_size: 0x38 + 2 * 16 + 8,
            slot_count: 8,
            data_offset: 1 << 20,
            chunk_size: 64 << 10,
            chunk_count: 100,
        };
        let mut w = CacheWriter::new(header.clone(), 0);
        let slot = header.slot_size as usize;
        let mut area = vec![0u8; 8 * slot];
        area[..slot].copy_from_slice(&w.init_slot());
        // Five whole chunks: two entries fit into a slot.
        let slots = w.write(0, 5 * (64 << 10));
        assert_eq!(slots.iter().map(|s| s.0).collect::<Vec<_>>(), [1, 2, 3]);
        for (i, page) in slots {
            area[i * slot..(i + 1) * slot].copy_from_slice(&page);
        }
        let index = CacheIndex::load(header, |off: u64, buf: &mut [u8]| {
            buf.copy_from_slice(&area[off as usize..off as usize + buf.len()]);
            Ok(())
        })
        .unwrap();
        for c in 0..5u64 {
            assert!(
                matches!(index.lookup(c * (64 << 10)), Lookup::Hit { cache_offset, .. } if cache_offset == (1 << 20) + c * (64 << 10))
            );
        }
        assert!(matches!(index.lookup(5 * (64 << 10)), Lookup::Miss { .. }));
    }

    #[test]
    fn skips_the_initialisation_slot() {
        // Slot 0 of mirror and parity caches, as Windows writes it: type 1,
        // sequence 1, one 8-byte entry. Read as a mapping it would be invalid.
        let mut init = vec![0u8; 0x1000];
        init[0..8].copy_from_slice(SPSLOT_SIGNATURE);
        init[8..24].copy_from_slice(&GUID);
        init[0x18..0x1c].copy_from_slice(&1u32.to_le_bytes());
        init[0x1c..0x20].copy_from_slice(&0x1000u32.to_le_bytes());
        init[0x20..0x24].copy_from_slice(&1u32.to_le_bytes());
        init[0x28..0x30].copy_from_slice(&1u64.to_le_bytes());
        init[0x30..0x34].copy_from_slice(&1u32.to_le_bytes());
        init[0x38..0x40].copy_from_slice(&[8, 0, 0, 0, 1, 0, 0, 0]);
        let crc = crc32(&init);
        init[0x24..0x28].copy_from_slice(&crc.to_le_bytes());
        let i = index(&[init, slot(2, &[(CHUNK, 1)])]);
        assert_eq!(i.cached_chunks(), 1);
        assert_eq!(hit(&i, CHUNK), Some(0x10_0000 + CHUNK));
    }

    #[test]
    fn entry_data_is_counted_in_bytes_and_padded() {
        // As in a Windows 11 24H2 cache after NTFS writes: partial entries
        // with four and three runs, each followed by a full one.
        let runs4 = vec![0x801d, 0x0003, 0x8008, (CHUNK / 512 - 40) as u16];
        let runs3 = vec![0x0001, 0x8001, (CHUNK / 512 - 2) as u16];
        let i = index(&[slot_with(
            1,
            &[
                (0, 1, STATE_PARTIAL, runs4),
                (CHUNK, 2, STATE_FULL, vec![]),
                (2 * CHUNK, 3, STATE_PARTIAL, runs3),
                (3 * CHUNK, 4, STATE_FULL, vec![]),
            ],
        )]);
        assert_eq!(i.cached_chunks(), 4);
        assert_eq!(hit(&i, 0), Some(0x10_0000 + CHUNK));
        assert_eq!(hit(&i, 29 * 512), None);
        assert_eq!(hit(&i, CHUNK + 7), Some(0x10_0000 + 2 * CHUNK + 7));
        assert_eq!(hit(&i, 2 * CHUNK + 512), Some(0x10_0000 + 3 * CHUNK + 512));
        assert_eq!(hit(&i, 3 * CHUNK), Some(0x10_0000 + 4 * CHUNK));
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
        let i = index(&[
            slot(1, &[(0, 2)]),
            slot(2, &[(CHUNK, 2)]),
            slot(3, &[(CHUNK, NO_BLOCK)]),
        ]);
        assert_eq!(hit(&i, 0), None);
        assert_eq!(hit(&i, CHUNK), None);
    }

    #[test]
    fn partial_chunks_follow_their_runs() {
        // Chunk 0: the first 32 KiB valid, then 96 KiB not (as a 1 MiB-split
        // write leaves it); chunk 2 has a provisional entry only.
        let partial = vec![0x8000 | 64, 192, 0, 0];
        let i = index(&[slot_with(
            1,
            &[
                (0, 2, STATE_PARTIAL, partial),
                ((2 * CHUNK) | ENTRY_OFFSET_FLAG, 4, STATE_FULL, vec![]),
                (CHUNK, 3, STATE_EMPTY, vec![]),
            ],
        )]);
        assert_eq!(
            i.lookup(0x100),
            Lookup::Hit {
                cache_offset: 0x10_0000 + 2 * CHUNK + 0x100,
                len: 0x8000 - 0x100
            }
        );
        assert_eq!(i.lookup(0x8000), Lookup::Miss { len: CHUNK - 0x8000 });
        assert_eq!(hit(&i, CHUNK + 5), None);
        assert_eq!(hit(&i, 2 * CHUNK), None);
    }

    #[test]
    fn provisional_entries_count_once_committed() {
        let provisional = ((2 * CHUNK) | ENTRY_OFFSET_FLAG, 4, STATE_FULL, vec![]);
        // Committed by the next slot: the chunk is cached.
        let i = index(&[
            slot_with(1, std::slice::from_ref(&provisional)),
            slot(2, &[(2 * CHUNK, 4)]),
        ]);
        assert_eq!(hit(&i, 2 * CHUNK), Some(0x10_0000 + 4 * CHUNK));
        // Not committed: an older committed mapping of the chunk stays.
        let i = index(&[slot(1, &[(2 * CHUNK, 1)]), slot_with(2, &[provisional])]);
        assert_eq!(hit(&i, 2 * CHUNK), Some(0x10_0000 + CHUNK));
    }

    #[test]
    fn ignores_corrupt_slots() {
        let mut bad = slot(5, &[(0, 1)]);
        bad[0x40] ^= 1;
        let i = index(&[slot(1, &[(0, 2)]), bad]);
        assert_eq!(hit(&i, 0), Some(0x10_0000 + 2 * CHUNK));
    }
}
