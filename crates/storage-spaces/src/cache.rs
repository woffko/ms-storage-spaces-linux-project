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
//! * checkpoint areas ("SPCHECK\0"): the whole chunk map as of a sequence,
//!   written before the log overwrites slots newer than the last checkpoint
//!   (see [`Checkpoint`]).
//!
//! Entries with block `0xffffffff` remove a chunk from the cache (written
//! when the chunk is destaged).
//!
//! Both structures carry a CRC-32 at `0x24` computed with that field zeroed,
//! over the size stored at `0x1c`.
//! * data area: `chunk_count` blocks of `chunk_size` bytes (one full stripe).

use std::collections::{HashMap, HashSet};

use crate::crc::{crc32, crc32_excluding};
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
/// words, bit 15 = valid, low 15 bits = length in logical sectors of the
/// space (`sector` bytes: 4096 on a space with 4 KiB sectors); a zero word
/// ends the list. The runs cover the chunk exactly: a list that does not
/// is read in the wrong unit or damaged, and is refused rather than read.
fn parse_runs(words: &[u8], chunk: u64, sector: u64) -> Result<Vec<(bool, u64)>> {
    let mut runs = Vec::new();
    let mut total = 0;
    for w in words.as_chunks::<2>().0.iter().map(|&w| u16::from_le_bytes(w)) {
        if w == 0 {
            break;
        }
        let sectors = (w & 0x7fff) as u64;
        total += sectors * sector;
        runs.push((w & 0x8000 != 0, sectors));
    }
    if total != chunk {
        return Err(format_err!(
            "cache runs cover {total:#x} bytes of a {chunk:#x} chunk (in {sector}-byte sectors)"
        ));
    }
    Ok(runs)
}

/// Checks the unit of a cache's run words, the logical sector size of its
/// space, against the cache's chunk size.
fn check_sector(header: &CacheHeader, sector: u32) -> Result<u64> {
    if !matches!(sector, 512 | 4096) || !header.chunk_size.is_multiple_of(sector) {
        return Err(format_err!(
            "cache of {}-byte chunks on a space of {sector}-byte sectors",
            header.chunk_size
        ));
    }
    Ok(sector as u64)
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

/// A write to the log of a cache: a slot, or a checkpoint (which must
/// reach the disks after everything before it and before anything after
/// it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogWrite {
    /// Slot `index` of the slot area.
    Slot(usize, Vec<u8>),
    /// Checkpoint area `area`, from its start.
    Checkpoint(usize, Vec<u8>),
}

impl LogWrite {
    /// The slot index and page of a slot.
    pub fn slot(&self) -> Option<(usize, &[u8])> {
        match self {
            LogWrite::Slot(i, page) => Some((*i, page)),
            LogWrite::Checkpoint(..) => None,
        }
    }
}

/// How Windows logs writes into a write-back cache (the model the scenario
/// tests check slot by slot): a new cache holds slot 0 of type 1; every
/// write that changes which sectors of a chunk are cached gets the next slot
/// with the next sequence and one mapping entry for each chunk it changes.
/// A chunk entering the cache takes the next block (parity caches start at
/// block 64, mirror caches at 0). A write into sectors already cached
/// changes no slot. Before a slot newer than the last checkpoint is
/// overwritten, a checkpoint of the whole map goes into the other
/// checkpoint area with a sequence of its own ([`Checkpoint`]).
#[derive(Debug, Clone)]
pub struct CacheWriter {
    header: CacheHeader,
    /// The space's logical sector size: what the cache tracks and the unit
    /// of the run words.
    sector: u64,
    next_slot: usize,
    sequence: u64,
    next_block: u32,
    first_block: u32,
    /// Chunk number -> (block, valid sectors).
    chunks: std::collections::BTreeMap<u64, (u32, Vec<bool>)>,
    /// Slots written since the cache was last empty.
    slots_in_use: usize,
    /// Sequence of the valid slot at each index (0: none).
    slot_seq: Vec<u64>,
    /// Sequence, area and next slot of the newest checkpoint.
    checkpoint: Option<(u64, usize, usize)>,
    /// Bytes the entries of all cached chunks take in a checkpoint.
    entry_bytes: usize,
}

impl CacheWriter {
    /// A new cache described by `header` of a space with `sector`-byte
    /// logical sectors, whose first block is `first_block`.
    pub fn new(header: CacheHeader, sector: u32, first_block: u32) -> Result<Self> {
        let sector = check_sector(&header, sector)?;
        // Slot 0 holds the type 1 record.
        let slot_seq = (0..header.slot_count).map(|i| u64::from(i == 0)).collect();
        Ok(CacheWriter {
            header,
            sector,
            next_slot: 1,
            sequence: 1,
            next_block: first_block,
            first_block,
            chunks: Default::default(),
            slots_in_use: 0,
            slot_seq,
            checkpoint: None,
            entry_bytes: 0,
        })
    }

    pub fn header(&self) -> &CacheHeader {
        &self.header
    }

    /// The space's logical sector size, the unit the cache tracks.
    pub fn sector(&self) -> u64 {
        self.sector
    }

    /// Byte offset in the cache space of block `block`.
    pub fn block_offset(&self, block: u32) -> u64 {
        self.header.data_offset + block as u64 * self.header.chunk_size as u64
    }

    /// The cached chunks as (owner offset, block).
    pub fn cached(&self) -> Vec<(u64, u32)> {
        let chunk = self.header.chunk_size as u64;
        self.chunks.iter().map(|(&c, &(b, _))| (c * chunk, b)).collect()
    }

    /// Where the sector at owner `offset` is cached: the cache space offset,
    /// or `None` if it is not.
    pub fn lookup(&self, offset: u64) -> Option<u64> {
        self.lookup_run(offset).0
    }

    /// Where the sector at owner `offset` is cached (the cache space offset,
    /// or `None` if it is not), and for how many bytes from `offset` the
    /// same holds (up to the end of its chunk).
    pub fn lookup_run(&self, offset: u64) -> (Option<u64>, u64) {
        let chunk = self.header.chunk_size as u64;
        let within = offset % chunk;
        let Some((block, valid)) = self.chunks.get(&(offset / chunk)) else {
            return (None, chunk - within);
        };
        let sector = (within / self.sector) as usize;
        let v = valid[sector];
        let same = valid[sector..].iter().take_while(|&&x| x == v).count() as u64;
        let len = (sector as u64 + same) * self.sector - within;
        (v.then(|| self.block_offset(*block) + within), len)
    }

    /// The block of the cached chunk at owner offset `offset` and its runs
    /// of equally valid bytes from the chunk start: (valid, bytes).
    pub fn chunk_runs(&self, offset: u64) -> Option<(u32, Vec<(bool, u64)>)> {
        let chunk = self.header.chunk_size as u64;
        let (block, valid) = self.chunks.get(&(offset / chunk))?;
        Some((
            *block,
            runs_of(valid)
                .into_iter()
                .map(|(v, n)| (v, n as u64 * self.sector))
                .collect(),
        ))
    }

    /// Whether writing `len` bytes at `offset` needs more slots or blocks
    /// than are left without reusing ones that still map cached data: then
    /// everything has to be destaged first.
    pub fn is_full_for(&self, offset: u64, len: u64) -> bool {
        let chunk = self.header.chunk_size as u64;
        let new = (offset / chunk..(offset + len).div_ceil(chunk))
            .filter(|c| !self.chunks.contains_key(c))
            .count() as u64;
        // Destaging everything needs slots too: for the entries that make
        // partly valid chunks whole and for the tombstones.
        let reserve = 2 * (self.chunks.len() + new as usize).div_ceil(200) + 2;
        self.slots_in_use + reserve + 2 >= self.header.slot_count as usize
            || self.next_block as u64 + new > self.header.chunk_count as u64
            || (self.header.checkpoint_count > 0
                && self.checkpoint_may_overflow((offset / chunk..(offset + len).div_ceil(chunk)).count() as u64))
    }

    /// Destaging `offsets` (owner offsets of cached chunks, whose data is in
    /// the space now): the slots with their entries of state 0 without a
    /// block, as Windows logs them. Once the cache is empty its blocks are
    /// handed out from the first again.
    pub fn destage(&mut self, offsets: &[u64]) -> Vec<LogWrite> {
        let chunk = self.header.chunk_size as u64;
        let capacity = (self.header.slot_size as usize).saturating_sub(0x38) / 16;
        let mut slots = Vec::new();
        for batch in offsets.chunks(capacity.max(1)) {
            let mut entries = Vec::with_capacity(batch.len() * 16);
            for &offset in batch {
                if let Some((_, valid)) = self.chunks.remove(&(offset / chunk)) {
                    self.entry_bytes = self.entry_bytes.saturating_sub(entry_len(&valid));
                }
                entries.extend_from_slice(&(offset / chunk * chunk).to_le_bytes());
                entries.extend_from_slice(&NO_BLOCK.to_le_bytes());
                entries.extend_from_slice(&STATE_EMPTY.to_le_bytes());
                entries.extend_from_slice(&0u16.to_le_bytes());
            }
            self.next(&mut slots, batch.len() as u32, &entries);
        }
        if self.chunks.is_empty() {
            self.next_block = self.first_block;
            self.slots_in_use = 0;
        }
        slots
    }

    /// Slot 0 of a new cache: see [`init_slot`].
    pub fn init_slot(&self) -> Vec<u8> {
        init_slot(&self.header)
    }

    /// A write of `len` bytes at owner offset `offset`: the slots Windows
    /// writes (index and page), none if no chunk changes. A write whose
    /// entries do not fit into one slot continues in the next (where Windows
    /// ends a slot then is not modelled).
    pub fn write(&mut self, offset: u64, len: u64) -> Vec<LogWrite> {
        let chunk = self.header.chunk_size as u64;
        let sector = self.sector;
        let sectors = (chunk / sector) as usize;
        let mut changed = Vec::new();
        let mut at = offset;
        while at < offset + len {
            let key = at / chunk;
            let end = (offset + len).min((key + 1) * chunk);
            let (first, last) = ((at % chunk / sector) as usize, ((end - 1) % chunk / sector) as usize);
            let next_block = &mut self.next_block;
            let mut added = false;
            let (_, valid) = self.chunks.entry(key).or_insert_with(|| {
                let b = *next_block;
                *next_block += 1;
                added = true;
                (b, vec![false; sectors])
            });
            if valid[first..=last].iter().any(|v| !v) {
                let before = if added { 0 } else { entry_len(valid) };
                valid[first..=last].fill(true);
                self.entry_bytes = self.entry_bytes - before + entry_len(valid);
                changed.push(key);
            }
            at = end;
        }
        self.log_chunks(&changed)
    }

    /// Makes the cached chunks at owner offsets `offsets` wholly valid
    /// (their missing sectors are in their blocks now): the slots to write.
    pub fn fill(&mut self, offsets: &[u64]) -> Vec<LogWrite> {
        let chunk = self.header.chunk_size as u64;
        let mut changed = Vec::new();
        for &offset in offsets {
            if let Some((_, valid)) = self.chunks.get_mut(&(offset / chunk))
                && valid.contains(&false)
            {
                let before = entry_len(valid);
                valid.fill(true);
                self.entry_bytes = self.entry_bytes - before + entry_len(valid);
                changed.push(offset / chunk);
            }
        }
        self.log_chunks(&changed)
    }

    /// Slots with one mapping entry for each of the chunks `keys`, as many
    /// entries per slot as fit.
    fn log_chunks(&mut self, keys: &[u64]) -> Vec<LogWrite> {
        let capacity = (self.header.slot_size as usize).saturating_sub(0x38);
        let mut slots = Vec::new();
        let mut entries = Vec::new();
        let mut pos = 0;
        let mut count = 0;
        for &key in keys {
            let (e, counted) = self.entry(key);
            if pos + e.len() > capacity && count > 0 {
                entries.truncate(pos);
                self.next(&mut slots, count, &entries);
                (entries, pos, count) = (Vec::new(), 0, 0);
            }
            entries.truncate(pos);
            entries.resize(pos, 0);
            entries.extend_from_slice(&e);
            pos = (pos + 16 + counted).next_multiple_of(8);
            count += 1;
        }
        if count > 0 {
            self.next(&mut slots, count, &entries);
        }
        slots
    }

    /// The mapping entry of cached chunk `key` and the length it counts:
    /// state 3 for a whole chunk, else state 2 with the runs of valid
    /// sectors. The run words are always written, but counted only for a
    /// partly valid chunk; the next entry starts 8-byte aligned after the
    /// counted part and so overwrites the others.
    fn entry(&self, key: u64) -> (Vec<u8>, usize) {
        let (block, valid) = &self.chunks[&key];
        let runs = runs_of(valid);
        let full = runs.len() == 1;
        let counted = if full { 0 } else { 2 * runs.len() };
        let mut e = (key * self.header.chunk_size as u64).to_le_bytes().to_vec();
        e.extend_from_slice(&block.to_le_bytes());
        e.extend_from_slice(&(if full { STATE_FULL } else { STATE_PARTIAL }).to_le_bytes());
        e.extend_from_slice(&(counted as u16).to_le_bytes());
        for (v, n) in runs {
            e.extend_from_slice(&((u16::from(v) << 15) | n as u16).to_le_bytes());
        }
        (e, counted)
    }

    /// The entries of a checkpoint of the whole map, in offset order, and
    /// their count.
    fn checkpoint_entries(&self) -> (Vec<u8>, u32) {
        let mut entries = Vec::new();
        for &key in self.chunks.keys() {
            let (e, counted) = self.entry(key);
            let start = entries.len();
            entries.extend_from_slice(&e);
            entries.resize((start + 16 + counted).next_multiple_of(8), 0);
        }
        (entries, self.chunks.len() as u32)
    }

    /// Whether a checkpoint of the map might not fit into a checkpoint area
    /// once a write touches `touched` chunks (each new one, or one more run
    /// in each, takes at most 24 bytes more).
    fn checkpoint_may_overflow(&self, touched: u64) -> bool {
        let bound = CHECKPOINT_ENTRIES as u64 + self.entry_bytes as u64 + touched * 24;
        bound > u64::from(self.header.checkpoint_size)
    }

    /// Recounts what the entries of the cached chunks take.
    fn recount(&mut self) {
        self.entry_bytes = self.chunks.values().map(|(_, valid)| entry_len(valid)).sum();
    }

    /// Appends the next slot with `count` entries, after a checkpoint where
    /// it would overwrite the slot the newest checkpoint continues at (the
    /// log has come round to it).
    fn next(&mut self, out: &mut Vec<LogWrite>, count: u32, entries: &[u8]) {
        let index = self.next_slot;
        let (cp_seq, cp_area, cp_next) = self.checkpoint.unwrap_or((0, usize::MAX, 0));
        let overwritten = self.slot_seq.get(index).copied().unwrap_or(0);
        if index == cp_next && overwritten > cp_seq && self.header.checkpoint_count > 0 {
            self.sequence += 1;
            let area = cp_area.wrapping_add(1) % self.header.checkpoint_count as usize;
            let (e, n) = self.checkpoint_entries();
            out.push(LogWrite::Checkpoint(
                area,
                Checkpoint::encode(self.header.owner_guid, 1, self.sequence, index as u32, n, &e),
            ));
            self.checkpoint = Some((self.sequence, area, index));
        }
        self.sequence += 1;
        if let Some(s) = self.slot_seq.get_mut(index) {
            *s = self.sequence;
        }
        self.slots_in_use += 1;
        // Past the last slot the log continues at slot 0, over the type 1
        // record (m5wbc2).
        self.next_slot = (self.next_slot + 1) % self.header.slot_count.max(1) as usize;
        out.push(LogWrite::Slot(index, self.slot(0, self.sequence, count, entries)));
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

/// Slot 0 of the new cache `header` describes: type 1, sequence 1, the
/// entry (8, 1).
pub fn init_slot(header: &CacheHeader) -> Vec<u8> {
    let mut entry = 8u32.to_le_bytes().to_vec();
    entry.extend_from_slice(&1u32.to_le_bytes());
    encode_slot(header.owner_guid, header.slot_size, 1, 1, 1, &entry)
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

/// Bytes the entry of a chunk with these valid sectors takes in a slot or
/// checkpoint (8-byte aligned).
fn entry_len(valid: &[bool]) -> usize {
    let runs = valid.windows(2).filter(|w| w[0] != w[1]).count() + 1;
    let counted = if runs == 1 { 0 } else { 2 * runs };
    (16 + counted).next_multiple_of(8)
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

pub const SPCHECK_SIGNATURE: &[u8; 8] = b"SPCHECK\0";
const CHECKPOINT_HEADER: usize = 0x50;
/// Where a checkpoint's entries start.
const CHECKPOINT_ENTRIES: usize = 0x200;
/// The largest checkpoint read.
const MAX_CHECKPOINT: usize = 16 << 20;
/// The largest chunk taken: the owner's full data stripe, columns x
/// interleave (16 MiB with 16 columns of 1 MiB), with room to spare.
const MAX_CHUNK: u32 = 64 << 20;

/// A checkpoint of the chunk map ("SPCHECK\0", in one of the checkpoint
/// areas after the slot area): the owner GUID (mixed-endian), u32 1, u32
/// 0x50 (header size), u32 kind (0 when attaching, 1 otherwise), CRC-32 of
/// the header with its field zeroed, u64 sequence, u32 CRC-32 of the
/// entries, u32 size used (0x200 plus the entries), u32 the slot the log
/// continues at after the checkpoint, u32 0, u32 0x200 (where the entries
/// start), u32 0, u32 bytes of entries, u32 entry count; the entries are
/// encoded as in mapping slots (**verified**: parity4, lrc12, m5wbc2).
///
/// How Windows loads the log (**verified** with logs written from Linux
/// and read back by Windows 11 24H2): it takes the newest checkpoint and
/// replays the slots from the one the checkpoint names on, in slot order,
/// while their sequences increase; without a checkpoint it replays from
/// slot 0. So before the log comes round to that slot again, Windows writes
/// a new checkpoint into the other area, with a sequence of its own (the
/// sequence is a counter of slots and checkpoints): at the first wrap of a
/// log (parity4, lrc12: before slot 0) or before the slot that followed the
/// checkpoint written when the pool was attached (m5wbc2: 1028, before
/// slot 2). A checkpoint holds the map as of its sequence (Windows includes
/// the write that makes it wrap).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    /// Which checkpoint area holds it.
    pub area: usize,
    pub kind: u32,
    pub sequence: u64,
    /// The slot the log continues at.
    pub next_slot: u32,
    pub count: u32,
    /// The entries, encoded as in mapping slots.
    pub entries: Vec<u8>,
}

impl Checkpoint {
    /// The checkpoint in checkpoint area `area` (its bytes `b`), if it holds
    /// a valid one of the space `owner`.
    fn parse(area: usize, b: &[u8], owner: Guid) -> Option<Self> {
        if b.len() < CHECKPOINT_ENTRIES
            || &b[..8] != SPCHECK_SIGNATURE
            || Guid::from_mixed_endian(b[8..24].try_into().unwrap()) != owner
            || le_u32(&b[0x1c..]) as usize != CHECKPOINT_HEADER
            || crc32_excluding(&b[..CHECKPOINT_HEADER], 0x24) != le_u32(&b[0x24..])
        {
            return None;
        }
        let start = le_u32(&b[0x40..]) as usize;
        let len = le_u32(&b[0x48..]) as usize;
        let entries = b.get(start..start.checked_add(len)?)?;
        if le_u32(&b[0x34..]) as usize != start + len || crc32(entries) != le_u32(&b[0x30..]) {
            return None;
        }
        Some(Checkpoint {
            area,
            kind: le_u32(&b[0x20..]),
            sequence: le_u64(&b[0x28..]),
            next_slot: le_u32(&b[0x38..]),
            count: le_u32(&b[0x4c..]),
            entries: entries.to_vec(),
        })
    }

    /// The bytes of a checkpoint (header and entries, whole pages).
    pub fn encode(owner: Guid, kind: u32, sequence: u64, next_slot: u32, count: u32, entries: &[u8]) -> Vec<u8> {
        let used = CHECKPOINT_ENTRIES + entries.len();
        let mut b = vec![0u8; used.next_multiple_of(4096)];
        b[..8].copy_from_slice(SPCHECK_SIGNATURE);
        b[8..24].copy_from_slice(&owner.to_mixed_endian());
        b[0x18..0x1c].copy_from_slice(&1u32.to_le_bytes());
        b[0x1c..0x20].copy_from_slice(&(CHECKPOINT_HEADER as u32).to_le_bytes());
        b[0x20..0x24].copy_from_slice(&kind.to_le_bytes());
        b[0x28..0x30].copy_from_slice(&sequence.to_le_bytes());
        b[0x30..0x34].copy_from_slice(&crc32(entries).to_le_bytes());
        b[0x34..0x38].copy_from_slice(&(used as u32).to_le_bytes());
        b[0x38..0x3c].copy_from_slice(&next_slot.to_le_bytes());
        b[0x40..0x44].copy_from_slice(&(CHECKPOINT_ENTRIES as u32).to_le_bytes());
        b[0x48..0x4c].copy_from_slice(&(entries.len() as u32).to_le_bytes());
        b[0x4c..0x50].copy_from_slice(&count.to_le_bytes());
        b[CHECKPOINT_ENTRIES..used].copy_from_slice(entries);
        let crc = crc32_excluding(&b[..CHECKPOINT_HEADER], 0x24);
        b[0x24..0x28].copy_from_slice(&crc.to_le_bytes());
        b
    }
}

/// The newest valid checkpoint of `owner` in each copy of the `count`
/// checkpoint areas of `size` bytes from `offset` (of a cache or parity
/// journal space), reading only what each checkpoint uses.
pub(crate) fn load_checkpoints(
    read: &mut impl SlotSource,
    owner: Guid,
    offset: u64,
    size: u32,
    count: u32,
) -> Result<Vec<Option<Checkpoint>>> {
    let mut newest: Vec<Option<Checkpoint>> = Vec::new();
    if count > 4 || (size as usize) < CHECKPOINT_ENTRIES {
        return Ok(newest);
    }
    for area in 0..count as usize {
        let Some(at) = offset.checked_add(area as u64 * u64::from(size)) else {
            break;
        };
        let heads = read.read_slot_copies(at, 4096.min(size as usize))?;
        for (copy, head) in heads.iter().enumerate() {
            let used = if head.starts_with(SPCHECK_SIGNATURE) {
                le_u32(&head[0x34..]) as usize
            } else {
                0
            };
            // Real checkpoints take a few MiB at most (a full 1 GiB cache
            // about 4 MiB); a larger claim is not read (journal areas are
            // 125 MiB, and the disk may be hostile).
            let checkpoint = if used > head.len() && used <= (size as usize).min(MAX_CHECKPOINT) {
                read.read_slot_copies(at, used)?
                    .get(copy)
                    .and_then(|b| Checkpoint::parse(area, b, owner))
            } else {
                Checkpoint::parse(area, head, owner)
            };
            if newest.len() <= copy {
                newest.resize(copy + 1, None);
            }
            if let Some(c) = checkpoint
                && newest[copy].as_ref().is_none_or(|n| c.sequence > n.sequence)
            {
                newest[copy] = Some(c);
            }
        }
    }
    Ok(newest)
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
    /// Checkpoint areas: where the first starts, the size of each and how
    /// many there are (0 if the header does not describe usable ones).
    pub checkpoint_offset: u64,
    pub checkpoint_size: u32,
    pub checkpoint_count: u32,
    pub data_offset: u64,
    pub chunk_size: u32,
    pub chunk_count: u32,
}

impl CacheHeader {
    pub const SIZE: usize = 0x60;

    /// The header of a new write-back cache of `size` bytes whose chunks
    /// are `chunk_size` bytes (the data stripe width of its owner): 1024
    /// slots of 4 KiB from 8 KiB on, two checkpoint areas after them, each
    /// with room for 0x200 bytes and 16 + chunk_size / 4096 bytes per
    /// chunk (rounded up to 4 KiB), then the chunks from the next chunk
    /// boundary to the end.
    pub fn new(owner_guid: Guid, size: u64, chunk_size: u32) -> Self {
        let slot_offset = 0x2000;
        let (slot_size, slot_count) = (0x1000u32, 1024u32);
        let checkpoint_offset = slot_offset + slot_size as u64 * slot_count as u64;
        let chunk = chunk_size as u64;
        let per_chunk = 16 + chunk / 4096;
        let mut chunk_count = size.saturating_sub(checkpoint_offset) / chunk;
        loop {
            let checkpoint_size = (0x200 + chunk_count * per_chunk).next_multiple_of(4096);
            let data_offset = (checkpoint_offset + 2 * checkpoint_size).next_multiple_of(chunk);
            let count = size.saturating_sub(data_offset) / chunk;
            if count == chunk_count {
                return CacheHeader {
                    owner_guid,
                    sequence: 1,
                    slot_offset,
                    slot_size,
                    slot_count,
                    checkpoint_offset,
                    checkpoint_size: checkpoint_size as u32,
                    checkpoint_count: 2,
                    data_offset,
                    chunk_size,
                    chunk_count: chunk_count as u32,
                };
            }
            chunk_count = count;
        }
    }

    /// The header as written (the rest of its page is zero). The parity
    /// journal's header has the same layout under its own signature.
    pub fn encode(&self, signature: &[u8; 8]) -> Vec<u8> {
        let mut b = vec![0u8; Self::SIZE];
        b[..8].copy_from_slice(signature);
        b[8..24].copy_from_slice(&self.owner_guid.to_mixed_endian());
        b[0x18..0x1c].copy_from_slice(&1u32.to_le_bytes());
        b[0x1c..0x20].copy_from_slice(&(Self::SIZE as u32).to_le_bytes());
        b[0x28..0x30].copy_from_slice(&self.sequence.to_le_bytes());
        b[0x30..0x38].copy_from_slice(&self.slot_offset.to_le_bytes());
        b[0x38..0x3c].copy_from_slice(&self.slot_size.to_le_bytes());
        b[0x3c..0x40].copy_from_slice(&self.slot_count.to_le_bytes());
        b[0x40..0x48].copy_from_slice(&self.checkpoint_offset.to_le_bytes());
        b[0x48..0x4c].copy_from_slice(&self.checkpoint_size.to_le_bytes());
        b[0x4c..0x50].copy_from_slice(&self.checkpoint_count.to_le_bytes());
        b[0x50..0x58].copy_from_slice(&self.data_offset.to_le_bytes());
        b[0x58..0x5c].copy_from_slice(&self.chunk_size.to_le_bytes());
        b[0x5c..0x60].copy_from_slice(&self.chunk_count.to_le_bytes());
        let crc = crc32_excluding(&b, 0x24);
        b[0x24..0x28].copy_from_slice(&crc.to_le_bytes());
        b
    }

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
            checkpoint_offset: le_u64(&b[0x40..]),
            checkpoint_size: le_u32(&b[0x48..]),
            checkpoint_count: le_u32(&b[0x4c..]),
            data_offset: le_u64(&b[0x50..]),
            chunk_size: le_u32(&b[0x58..]),
            chunk_count: le_u32(&b[0x5c..]),
        };
        if !(0x40..=0x10000).contains(&h.slot_size)
            || h.slot_count > 1 << 16
            || (h.slot_size as u64) * (h.slot_count as u64) > 64 << 20
            || h.chunk_size == 0
            || h.chunk_size > MAX_CHUNK
            || !h.chunk_size.is_multiple_of(4096)
            // Keeps cache offsets far from overflowing (real caches: GiBs).
            || h.data_offset > 1 << 56
            || (h.chunk_size as u64) * (h.chunk_count as u64) > 1 << 56
        {
            return Err(format_err!("implausible cache geometry: {h:?}"));
        }
        let mut h = h;
        if h.checkpoint_count > 4
            || !(CHECKPOINT_ENTRIES as u32..=16 << 20).contains(&h.checkpoint_size)
            || h.checkpoint_offset > 1 << 56
        {
            h.checkpoint_count = 0;
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
    /// Runs of logical sectors of the space from the chunk start: (valid,
    /// sectors).
    Runs(Vec<(bool, u64)>),
}

/// Owner chunk number -> (cache block, valid part).
type ChunkMap = HashMap<u64, (u64, Validity)>;

/// Mapping of cached chunks of the owner space.
#[derive(Debug, Clone)]
pub struct CacheIndex {
    pub header: CacheHeader,
    /// The space's logical sector size, the unit of the run words
    /// (**verified**: 4096 on a space with 4 KiB sectors, pool `wc4k`).
    sector: u64,
    chunks: ChunkMap,
    /// Chunks mapped differently by the copies of the slot area (after an
    /// unclean shutdown): which one Windows keeps is not known.
    conflicts: HashSet<u64>,
    slots: Vec<Slot>,
    checkpoint: Option<Checkpoint>,
}

impl CacheIndex {
    /// Builds the index from the cache header, the slot area and the
    /// checkpoints of the cache of a space with `sector`-byte logical
    /// sectors. `read` reads from the cache space.
    pub fn load(header: CacheHeader, sector: u32, mut read: impl SlotSource) -> Result<Self> {
        let sector = check_sector(&header, sector)?;
        let slot_size = header.slot_size as usize;
        let copies = read.read_slot_copies(header.slot_offset, slot_size * header.slot_count as usize)?;
        let merged = merge_slot_copies(&copies, slot_size);
        let checkpoints = load_checkpoints(
            &mut read,
            header.owner_guid,
            header.checkpoint_offset,
            header.checkpoint_size,
            header.checkpoint_count,
        )?;
        let checkpoint = checkpoints.iter().flatten().max_by_key(|c| c.sequence).cloned();
        let chunks = Self::map(&header, sector, &merged, checkpoint.as_ref())?;
        let mut conflicts = HashSet::new();
        for (i, copy) in copies.iter().enumerate() {
            // Each copy with its own checkpoint, where the copies line up.
            let own = if checkpoints.len() == copies.len() {
                checkpoints[i].as_ref()
            } else {
                checkpoint.as_ref()
            };
            if *copy == merged && own == checkpoint.as_ref() {
                continue;
            }
            let other = Self::map(&header, sector, copy, own)?;
            for key in chunks.keys().chain(other.keys()) {
                if chunks.get(key) != other.get(key) {
                    conflicts.insert(*key);
                }
            }
        }
        let slots = valid_slots(&merged, slot_size);
        Ok(CacheIndex {
            header,
            sector,
            chunks,
            conflicts,
            slots,
            checkpoint,
        })
    }

    /// The chunk mapping one version of the slot area and checkpoint
    /// describes, read as Windows does (see [`Checkpoint`]): the
    /// checkpoint, then the slots from the one it names on while their
    /// sequences increase (from slot 0 without a checkpoint).
    fn map(header: &CacheHeader, sector: u64, area: &[u8], checkpoint: Option<&Checkpoint>) -> Result<ChunkMap> {
        let slot_size = header.slot_size as usize;
        let valid = |slot: &[u8]| {
            &slot[0..8] == SPSLOT_SIGNATURE
                && Guid::from_mixed_endian(slot[8..24].try_into().unwrap()) == header.owner_guid
                && le_u32(&slot[0x1c..]) as usize == slot_size
                && crc32_excluding(slot, 0x24) == le_u32(&slot[0x24..])
        };
        let slots: Vec<&[u8]> = area.chunks_exact(slot_size).collect();

        // A mapping is current if it is the newest entry for its owner chunk
        // (a newer entry may be a tombstone written when the chunk was
        // destaged) and the newest assignment of its cache block (blocks are
        // reused for other chunks). Entries are ordered by (sequence,
        // position in the slot or checkpoint).
        type Version = (u64, usize);
        let mut newest_for_chunk: HashMap<u64, (Version, Option<(u64, Validity)>)> = HashMap::new();
        let mut newest_for_block: HashMap<u64, Version> = HashMap::new();
        let mut apply = |sequence: u64, entries: &[u8], count: usize| -> Result<()> {
            for (index, entry) in decode_entries(header, sector, entries, count)?.into_iter().enumerate() {
                let Some((offset, target)) = entry else {
                    continue; // provisional
                };
                let version = (sequence, index);
                let key = offset / header.chunk_size as u64;
                if let Some((b, _)) = &target
                    && newest_for_block.get(b).is_none_or(|&v| version > v)
                {
                    newest_for_block.insert(*b, version);
                }
                if newest_for_chunk.get(&key).is_none_or(|(v, _)| version > *v) {
                    newest_for_chunk.insert(key, (version, target));
                }
            }
            Ok(())
        };
        let (mut last, start) = match checkpoint {
            Some(c) => {
                apply(c.sequence, &c.entries, c.count as usize)?;
                (c.sequence, c.next_slot as usize)
            }
            None => (0, 0),
        };
        for k in 0..slots.len() {
            let slot = slots[(start + k) % slots.len()];
            let sequence = le_u64(&slot[0x28..]);
            if !valid(slot) || sequence <= last {
                break; // the end of the log
            }
            last = sequence;
            if le_u32(&slot[0x20..]) == SLOT_TYPE_MAPPING {
                apply(sequence, &slot[0x38..], le_u32(&slot[0x30..]) as usize)?;
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

    /// The newest valid checkpoint.
    pub fn checkpoint(&self) -> Option<&Checkpoint> {
        self.checkpoint.as_ref()
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

    /// A writer that continues this log: the next slot follows the newest
    /// one, with the next sequence; the cached chunks stay mapped; blocks
    /// are handed out after the highest one mapped (from `first_block` if
    /// none is).
    pub fn writer(&self, first_block: u32) -> CacheWriter {
        let mut w = CacheWriter::new(self.header.clone(), self.sector as u32, first_block)
            .expect("the sector size was checked when the index was loaded");
        w.next_slot = 0;
        w.sequence = 0;
        w.slot_seq.fill(0);
        w.checkpoint = self
            .checkpoint
            .as_ref()
            .map(|c| (c.sequence, c.area, c.next_slot as usize));
        for s in &self.slots {
            if let Some(v) = w.slot_seq.get_mut(s.index) {
                *v = s.sequence;
            }
        }
        // A checkpoint newer than every slot continues at the slot it names.
        if let Some(c) = &self.checkpoint {
            w.sequence = c.sequence;
            w.next_slot = c.next_slot as usize % self.header.slot_count.max(1) as usize;
        }
        if let Some(newest) = self.slots.iter().max_by_key(|s| s.sequence)
            && newest.sequence > w.sequence
        {
            w.sequence = newest.sequence;
            w.next_slot = (newest.index + 1) % self.header.slot_count.max(1) as usize;
        }
        let sectors = (self.header.chunk_size as u64 / self.sector) as usize;
        for (&key, (block, validity)) in &self.chunks {
            let valid = match validity {
                Validity::Full => vec![true; sectors],
                Validity::Runs(runs) => {
                    let mut v = Vec::with_capacity(sectors);
                    for &(ok, n) in runs {
                        v.extend(std::iter::repeat_n(ok, n as usize));
                    }
                    v.resize(sectors, false);
                    v
                }
            };
            w.chunks.insert(key, (*block as u32, valid));
            w.next_block = w.next_block.max(*block as u32 + 1);
        }
        w.recount();
        w.slots_in_use = if w.chunks.is_empty() { 0 } else { self.slots.len() };
        w
    }

    /// Number of chunks the copies of the cache disagree about.
    pub fn conflicting_chunks(&self) -> usize {
        self.conflicts.len()
    }

    /// Owner offsets of the chunks the copies of the cache disagree about,
    /// in offset order.
    pub fn conflicting_offsets(&self) -> Vec<u64> {
        let mut all: Vec<u64> = self
            .conflicts
            .iter()
            .map(|k| k * self.header.chunk_size as u64)
            .collect();
        all.sort_unstable();
        all
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
                    let end = start + sectors * self.sector;
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

/// The entries of a mapping slot or checkpoint (`count` of them, encoded
/// from the start of `bytes`): owner offset and block with the valid part,
/// or no block for a tombstone; `None` for a provisional entry.
type Entry = Option<(u64, Option<(u64, Validity)>)>;

fn decode_entries(header: &CacheHeader, sector: u64, bytes: &[u8], count: usize) -> Result<Vec<Entry>> {
    let chunk = header.chunk_size as u64;
    let mut out = Vec::with_capacity(count.min(4096));
    let mut pos = 0;
    for _ in 0..count {
        let e = bytes
            .get(pos..pos + 16)
            .ok_or_else(|| format_err!("cache slot with {count} entries overflows"))?;
        let provisional = le_u64(e) & ENTRY_OFFSET_FLAG != 0;
        let offset = le_u64(e) & !ENTRY_OFFSET_FLAG;
        let block = le_u32(&e[8..]);
        let state = u16::from_le_bytes([e[12], e[13]]);
        // The high half is the length of the data that follows the entry,
        // in bytes; the next entry starts 8-byte aligned.
        let len = u16::from_le_bytes([e[14], e[15]]) as usize;
        let extra = bytes
            .get(pos + 16..pos + 16 + len)
            .ok_or_else(|| format_err!("cache entry overflows its slot"))?;
        pos += 16 + len.next_multiple_of(8);
        if !offset.is_multiple_of(chunk) {
            return Err(format_err!("bad cache entry: offset {offset:#x}"));
        }
        let validity = match state {
            STATE_EMPTY => Validity::Runs(Vec::new()),
            STATE_PARTIAL => Validity::Runs(parse_runs(extra, chunk, sector)?),
            STATE_FULL => Validity::Full,
            other => return Err(crate::Error::Unsupported(format!("cache entry state {other}"))),
        };
        let target = match block {
            NO_BLOCK => None,
            b if (b as u64) < header.chunk_count as u64 => Some((b as u64, validity)),
            b => return Err(format_err!("bad cache entry: block {b}")),
        };
        out.push((!provisional).then_some((offset, target)));
    }
    Ok(out)
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
        // A chunk is the owner's whole data stripe (columns x interleave: 16
        // MiB at most in practice); every cached chunk costs a flag per
        // sector, so a header asking for gigabytes is refused.
        let sized = |chunk: u32| {
            let mut h = b.clone();
            h[0x58..0x5c].copy_from_slice(&chunk.to_le_bytes());
            let crc = crc32_excluding(&h, 0x24);
            h[0x24..0x28].copy_from_slice(&crc.to_le_bytes());
            CacheHeader::parse(&h)
        };
        assert!(sized(16 << 20).unwrap().is_some());
        assert!(sized(1 << 28).is_err());
        assert!(sized(0xffff_f000).is_err());
    }

    fn header() -> CacheHeader {
        CacheHeader {
            owner_guid: Guid::from_mixed_endian(&GUID),
            sequence: 1,
            slot_offset: 0,
            slot_size: 0x1000,
            slot_count: 4,
            checkpoint_offset: 0,
            checkpoint_size: 0,
            checkpoint_count: 0,
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
        CacheIndex::load(header(), 512, |off: u64, buf: &mut [u8]| {
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

    /// The running size of the checkpoint entries follows the map through
    /// writes that add and split runs, fills and destages.
    #[test]
    fn checkpoint_size_is_tracked_as_the_map_changes() {
        let mut w = CacheWriter::new(header(), 512, 0).unwrap();
        let check = |w: &CacheWriter| assert_eq!(w.entry_bytes, w.checkpoint_entries().0.len());
        let mut seed = 7u64;
        for step in 0..400 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let offset = (seed >> 33) % (16 * CHUNK) / 512 * 512;
            let len = ((seed >> 20) % 16 + 1) * 512;
            let len = len.min(16 * CHUNK - offset);
            if w.is_full_for(offset, len) || step % 97 == 96 {
                let cached: Vec<u64> = w.cached().iter().map(|c| c.0).collect();
                w.destage(&cached[..cached.len() / 2]);
                check(&w);
                w.fill(&cached[cached.len() / 2..]);
                check(&w);
                let cached: Vec<u64> = w.cached().iter().map(|c| c.0).collect();
                w.destage(&cached);
            }
            w.write(offset, len);
            check(&w);
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
            checkpoint_offset: 0,
            checkpoint_size: 0,
            checkpoint_count: 0,
            data_offset: 1 << 20,
            chunk_size: 64 << 10,
            chunk_count: 100,
        };
        let mut w = CacheWriter::new(header.clone(), 512, 0).unwrap();
        let slot = header.slot_size as usize;
        let mut area = vec![0u8; 8 * slot];
        area[..slot].copy_from_slice(&w.init_slot());
        // Five whole chunks: two entries fit into a slot.
        let slots: Vec<(usize, Vec<u8>)> = w
            .write(0, 5 * (64 << 10))
            .iter()
            .map(|r| r.slot().map(|(i, p)| (i, p.to_vec())).unwrap())
            .collect();
        assert_eq!(slots.iter().map(|s| s.0).collect::<Vec<_>>(), [1, 2, 3]);
        for (i, page) in slots {
            area[i * slot..(i + 1) * slot].copy_from_slice(&page);
        }
        let index = CacheIndex::load(header, 512, |off: u64, buf: &mut [u8]| {
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

    /// m5wbc2's log as the model continues it: attached with a checkpoint
    /// of sequence 3 that continues at slot 2 (s3), the log wraps after
    /// slot 1023 (sequence 1025); slots 0 and 1 take 1026 and 1027, and
    /// before slot 2 comes round a checkpoint of sequence 1028 goes into
    /// the second area, then slot 2 takes 1029, as Windows wrote them (s4,
    /// up to slot 490 at 1517).
    #[test]
    fn the_log_writes_a_checkpoint_where_the_last_one_continues() {
        use crate::io::SparseImage;
        use crate::pool::Pool;
        use std::fs::File;
        use std::path::Path;
        let index = |label: &str| {
            let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/scenarios/m5wbc2")
                .join(label);
            let disks: Vec<SparseImage> = (0..3)
                .map(|i| SparseImage::read_from(File::open(dir.join(format!("disk{i}.fixture"))).unwrap()).unwrap())
                .collect();
            let pool = Pool::open(disks).unwrap();
            let space = pool.user_spaces().next().unwrap();
            pool.open_space(space.id()).unwrap().cache().unwrap().clone()
        };
        let (s3, s4) = (index("s3"), index("s4"));
        let c = s3.checkpoint().unwrap();
        assert_eq!((c.sequence, c.area, c.kind, c.next_slot, c.count), (3, 0, 0, 2, 1));
        let mut w = s3.writer(64);
        // The rest of the first lap as Windows wrote it.
        let first_lap: Vec<u64> = s4
            .slots()
            .iter()
            .filter(|s| s.index > 490)
            .map(|s| s.sequence)
            .collect();
        assert_eq!(first_lap[0], 493);
        for (i, seq) in (3..1024).zip(5..) {
            w.slot_seq[i] = seq;
        }
        (w.sequence, w.next_slot) = (1025, 0);
        let mut records = Vec::new();
        for _ in 0..491 {
            w.next(&mut records, 0, &[]);
        }
        let windows: Vec<(usize, u64)> = s4
            .slots()
            .iter()
            .filter(|s| s.index <= 490)
            .map(|s| (s.index, s.sequence))
            .collect();
        let ours: Vec<(usize, u64)> = records
            .iter()
            .filter_map(|r| r.slot())
            .map(|(i, page)| (i, u64::from_le_bytes(page[0x28..0x30].try_into().unwrap())))
            .collect();
        assert_eq!(ours, windows);
        let checkpoints: Vec<usize> = records
            .iter()
            .enumerate()
            .filter(|(_, r)| matches!(r, LogWrite::Checkpoint(1, _)))
            .map(|(k, _)| k)
            .collect();
        assert_eq!(checkpoints, [2]);
        let c = s4.checkpoint().unwrap();
        assert_eq!((c.sequence, c.area, c.next_slot), (1028, 1, 2));
        assert_eq!(w.checkpoint.map(|c| (c.0, c.1, c.2)), Some((1028, 1, 2)));
    }

    /// A slot of tombstones Windows wrote when it destaged (m5wbc2, its
    /// largest batch of tombstones only), rebuilt byte for byte by
    /// `destage` from the same chunks in the same order.
    #[test]
    fn destaging_logs_tombstones_as_windows_does() {
        use crate::io::SparseImage;
        use crate::pool::Pool;
        use std::fs::File;
        use std::path::Path;
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/scenarios/m5wbc2/s4");
        let disks: Vec<SparseImage> = (0..3)
            .map(|i| SparseImage::read_from(File::open(dir.join(format!("disk{i}.fixture"))).unwrap()).unwrap())
            .collect();
        let pool = Pool::open(disks).unwrap();
        let space = pool.user_spaces().next().unwrap();
        let reader = pool.open_space(space.id()).unwrap();
        let cache = reader.cache().unwrap();
        // Entries: (owner offset, state), each after the previous one's
        // counted length, 8-byte aligned.
        let entries = |c: &[u8], n: u32| {
            // The stored content ends at its last non-zero byte.
            let mut c = c.to_vec();
            c.resize(c.len() + 64, 0);
            let mut pos = 0x18;
            let mut out = Vec::new();
            for _ in 0..n {
                let offset = u64::from_le_bytes(c[pos..pos + 8].try_into().unwrap());
                let state = u16::from_le_bytes([c[pos + 12], c[pos + 13]]);
                let len = u16::from_le_bytes([c[pos + 14], c[pos + 15]]) as usize;
                out.push((offset, state));
                pos = (pos + 16 + len).next_multiple_of(8);
            }
            out
        };
        // The largest batch of nothing but tombstones.
        let slot = cache
            .slots()
            .iter()
            .filter(|s| s.kind == 0 && s.entries > 10 && entries(&s.content, s.entries).iter().all(|e| e.1 == 0))
            .max_by_key(|s| s.entries)
            .unwrap();
        let offsets: Vec<u64> = entries(&slot.content, slot.entries).iter().map(|e| e.0).collect();
        let mut w = cache.writer(64);
        w.next_slot = slot.index;
        w.sequence = slot.sequence - 1;
        // The writer continues from the checkpoint Windows wrote when the
        // log wrapped, which does not come due in this slot.
        let checkpoint = cache.checkpoint().unwrap();
        assert_eq!(
            (
                checkpoint.sequence,
                checkpoint.area,
                checkpoint.kind,
                checkpoint.next_slot,
                checkpoint.count
            ),
            (1028, 1, 1, 2, 479)
        );
        assert_eq!(w.checkpoint, Some((1028, 1, 2)));
        let chunk = cache.header.chunk_size as u64;
        for &o in &offsets {
            w.chunks
                .insert(o / chunk, (0, vec![true; (chunk / w.sector()) as usize]));
        }
        w.recount();
        let written = w.destage(&offsets);
        assert_eq!(written.len(), 1);
        let (index, page) = written[0].slot().unwrap();
        assert_eq!(index, slot.index);
        let end = page.iter().rposition(|&b| b != 0).unwrap() + 1;
        let ours = &page[0x20..end];
        let first = ours.iter().zip(&slot.content).position(|(a, b)| a != b);
        assert!(
            ours == &slot.content[..],
            "{} entries: lengths {} / {}, first difference at {first:?}",
            slot.entries,
            ours.len(),
            slot.content.len()
        );
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

    /// The cache of pool `wc4k` (Windows 11 build 26340; a thin simple
    /// space with 4 KiB sectors and 256 KiB chunks, GPT and NTFS written
    /// through the cache). Two of its partly valid chunks as Windows logged
    /// them: the last chunk of the space holds the backup GPT in its last
    /// five sectors (runs: 59 not valid, 5 valid), chunk 0x40740000 is
    /// valid from its fourth sector on. The runs count 4 KiB sectors; read
    /// as 512-byte sectors they cover an eighth of the chunk and are
    /// refused (they were taken that way once, and the backup GPT read as
    /// zeros).
    #[test]
    fn runs_count_the_logical_sectors_of_the_space() {
        const CHUNK4K: u64 = 0x40000;
        let header = CacheHeader {
            chunk_size: CHUNK4K as u32,
            chunk_count: 4079,
            ..header()
        };
        let mut area = [
            slot_with(1, &[(0xfffc_0000, 0, STATE_PARTIAL, vec![0x003b, 0x8005])]),
            slot_with(2, &[(0x4074_0000, 3, STATE_PARTIAL, vec![0x0003, 0x803d])]),
        ]
        .concat();
        area.resize(4 * 0x1000, 0);
        let load = |sector| {
            CacheIndex::load(header.clone(), sector, |off: u64, buf: &mut [u8]| {
                buf.copy_from_slice(&area[off as usize..off as usize + buf.len()]);
                Ok(())
            })
        };
        let i = load(4096).unwrap();
        assert_eq!(i.lookup(0xfffc_0000), Lookup::Miss { len: 59 * 4096 });
        assert_eq!(
            i.lookup(0xfffc_0000 + 59 * 4096),
            Lookup::Hit {
                cache_offset: 0x10_0000 + 59 * 4096,
                len: 5 * 4096
            }
        );
        assert_eq!(i.lookup(0x4074_0000 + 4095), Lookup::Miss { len: 2 * 4096 + 1 });
        assert_eq!(
            i.lookup(0x4074_0000 + 3 * 4096),
            Lookup::Hit {
                cache_offset: 0x10_0000 + 3 * CHUNK4K + 3 * 4096,
                len: 61 * 4096
            }
        );
        let err = load(512).unwrap_err().to_string();
        assert!(
            err.contains("cache runs cover 0x8000 bytes of a 0x40000 chunk"),
            "{err}"
        );
    }

    /// A write into the last five sectors of that space is logged as
    /// Windows logged it: the entry of slot 0 of the cache of `wc4k`, byte
    /// for byte (offset 0xfffc0000, block 0, state 2, 4 bytes of runs: 59
    /// not valid, 5 valid).
    #[test]
    fn the_writer_counts_the_logical_sectors_of_the_space() {
        let header = CacheHeader {
            chunk_size: 0x40000,
            chunk_count: 4079,
            ..header()
        };
        let mut w = CacheWriter::new(header, 4096, 0).unwrap();
        let written = w.write(0x1_0000_0000 - 5 * 4096, 5 * 4096);
        let (_, page) = written[0].slot().unwrap();
        let windows = [
            0x00, 0x00, 0xfc, 0xff, 0, 0, 0, 0, 0, 0, 0, 0, 0x02, 0x00, 0x04, 0x00, 0x3b, 0x00, 0x05, 0x80,
        ];
        assert_eq!(&page[0x38..0x38 + windows.len()], &windows);
        assert_eq!(w.lookup_run(0xfffc_0000), (None, 59 * 4096));
        assert_eq!(
            w.lookup_run(0xfffc_0000 + 59 * 4096),
            (Some(0x10_0000 + 59 * 4096), 5 * 4096)
        );
    }

    #[test]
    fn refuses_sector_sizes_other_than_512_and_4096() {
        for sector in [0, 1024, 8192] {
            assert!(CacheWriter::new(header(), sector, 0).is_err(), "{sector}");
        }
        assert!(CacheWriter::new(header(), 512, 0).is_ok());
        assert!(CacheWriter::new(header(), 4096, 0).is_ok());
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

#[cfg(test)]
mod new_header_tests {
    use super::*;

    /// The geometry of new caches as Windows 11 24H2 lays them out: 1 GiB
    /// with 512 KiB chunks (parity of 3 columns, 256 KiB interleave:
    /// c9new, paritythin_26100, mapar_26100), 1 GiB with 128 KiB chunks
    /// (parity3_26100, 64 KiB interleave) and 512 MiB with 512 KiB chunks
    /// (wc64).
    #[test]
    fn new_caches_have_windows_geometry() {
        for (size, chunk, checkpoint, data, count) in [
            (1u64 << 30, 512 << 10, 294912, 5242880, 2038),
            (1 << 30, 128 << 10, 393216, 5111808, 8153),
            (512 << 20, 512 << 10, 147456, 4718592, 1015),
        ] {
            let h = CacheHeader::new(Guid::default(), size, chunk);
            assert_eq!(
                (h.checkpoint_offset, h.checkpoint_size, h.data_offset, h.chunk_count),
                (4202496, checkpoint, data, count),
                "{size} {chunk}"
            );
            let back = CacheHeader::parse(&h.encode(SPCACHE_SIGNATURE)).unwrap().unwrap();
            assert_eq!(format!("{back:?}"), format!("{h:?}"));
        }
    }
}
