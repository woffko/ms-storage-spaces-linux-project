//! The parity journal ("SPVDT"): which stripes of a parity space may have
//! parity that does not match their data (writes in flight at a crash).
//!
//! The journal is a hidden child space (role 0x0a) with the same header and
//! slot layout as the write-back cache. Each mapping slot (type 0) carries
//! entries keyed by the owner offset of an extent run:
//!
//! | Offset | Size | Field |
//! |---|---|---|
//! | 0x00 | 8 | owner offset where the extent run starts |
//! | 0x08 | 2 | state: 1 = bitmap, 2 = run list, 3 = whole run consistent |
//! | 0x0a | 2 | states 1 and 2: length in bytes of what follows |
//! | 0x0c | … | state 1: bitmap (bit set = consistent stripe); state 2: run words; state 3: 4 bytes |
//!
//! Run words are u16 LE: bit 15 = consistent, bits 0-14 = number of stripes.
//!
//! The header also names checkpoint areas (0x40: offset, 0x48: size of
//! each, 0x4c: count; then u64 owner size, u32 stripe size, u32 1), and the
//! log wraps behind checkpoints as the cache's does (`cache::Checkpoint`):
//! parity4 and lrc12 hold journal checkpoints of sequences 1025 and 2050
//! that continue at slot 0, one entry per run.

use std::collections::HashMap;

use crate::cache::{Checkpoint, LogWrite, SlotSource, load_checkpoints, merge_slot_copies};
use crate::crc::crc32_excluding;
use crate::error::{Result, format_err};
use crate::guid::Guid;

pub const SPVDT_SIGNATURE: &[u8; 8] = b"SPVDT\0\0\0";
const SPSLOT_SIGNATURE: &[u8; 8] = b"SPSLOT\0\0";

/// What a journal entry says about the stripes of an extent run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Consistency {
    All,
    /// (consistent, stripes) runs from the start of the extent run.
    Runs(Vec<(bool, u64)>),
    /// Bit set = consistent stripe.
    Bitmap(Vec<u8>),
}

/// Consistency of parity stripes, per extent run. After an unclean
/// shutdown the copies of the journal space can differ (a slot that reached
/// only some copies); a stripe counts as possibly inconsistent if any
/// version of the journal says so, since which one Windows keeps is not
/// known.
#[derive(Debug, Clone, Default)]
pub struct ParityJournal {
    runs: HashMap<u64, Vec<Consistency>>,
    slots: Vec<crate::cache::Slot>,
    /// Slot offset, size and count from the header.
    geometry: (u64, u32, u32),
    /// Checkpoint area offset, size and count from the header.
    checkpoint_geometry: (u64, u32, u32),
    checkpoint: Option<Checkpoint>,
    /// The newest entry per run of the merged slot area, and its bytes.
    current: HashMap<u64, (Consistency, Vec<u8>)>,
}

/// The header of a new parity journal (a 256 MiB child space): the cache
/// header's layout with 1024 slots of 4 KiB, two checkpoint areas of
/// 125 MiB at 6 MiB, then the size of the owner's extent runs (allocation
/// unit x data columns), its stripe width (data columns x interleave) and
/// the number of runs.
pub fn new_journal_header(owner: Guid, run_size: u64, stripe: u32, runs: u32) -> Vec<u8> {
    crate::cache::CacheHeader {
        owner_guid: owner,
        sequence: 1,
        slot_offset: 0x2000,
        slot_size: 0x1000,
        slot_count: 1024,
        checkpoint_offset: 0x60_0000,
        checkpoint_size: 125 << 20,
        checkpoint_count: 2,
        data_offset: run_size,
        chunk_size: stripe,
        chunk_count: runs,
    }
    .encode(SPVDT_SIGNATURE)
}

/// Newest entry per run: consistency and the entry's bytes.
type Entries = HashMap<u64, (Consistency, Vec<u8>)>;

impl ParityJournal {
    /// Parses the journal. `read` reads from the journal space.
    pub fn load(owner: Guid, mut read: impl SlotSource) -> Result<Option<Self>> {
        let mut head = [0u8; 0x60];
        read.read(0, &mut head)?;
        if &head[0..8] != SPVDT_SIGNATURE {
            return Ok(None);
        }
        if crc32_excluding(&head, 0x24) != le_u32(&head[0x24..]) {
            return Err(format_err!("parity journal header checksum mismatch"));
        }
        if Guid::from_mixed_endian(head[8..24].try_into().unwrap()) != owner {
            return Err(format_err!("parity journal belongs to another space"));
        }
        let slot_offset = le_u64(&head[0x30..]);
        let slot_size = le_u32(&head[0x38..]) as usize;
        let slot_count = le_u32(&head[0x3c..]) as usize;
        if !(0x40..=0x10000).contains(&slot_size) || slot_count > 1 << 16 || slot_size * slot_count > 64 << 20 {
            return Err(format_err!("implausible parity journal geometry"));
        }
        let copies = read.read_slot_copies(slot_offset, slot_size * slot_count)?;
        let merged = merge_slot_copies(&copies, slot_size);
        let mut checkpoint_geometry = (le_u64(&head[0x40..]), le_u32(&head[0x48..]), le_u32(&head[0x4c..]));
        // As for the cache: a geometry no real journal has (Windows: two
        // areas of 125 MiB at 6 MiB) describes no usable checkpoints.
        if checkpoint_geometry.0 > 1 << 56 || checkpoint_geometry.2 > 4 {
            checkpoint_geometry.2 = 0;
        }
        let (cp_offset, cp_size, cp_count) = checkpoint_geometry;
        let checkpoints = load_checkpoints(&mut read, owner, cp_offset, cp_size, cp_count)?;
        let checkpoint = checkpoints.iter().flatten().max_by_key(|c| c.sequence).cloned();
        let current = Self::parse(&merged, slot_size, checkpoint.as_ref())?;
        let mut runs: HashMap<u64, Vec<Consistency>> = HashMap::new();
        let mut add = |entries: Entries| {
            for (offset, (c, _)) in entries {
                let versions = runs.entry(offset).or_default();
                if !versions.contains(&c) {
                    versions.push(c);
                }
            }
        };
        add(current.clone());
        for (i, copy) in copies.iter().enumerate() {
            // Each copy with its own checkpoint, where the copies line up.
            let own = if checkpoints.len() == copies.len() {
                checkpoints[i].as_ref()
            } else {
                checkpoint.as_ref()
            };
            if *copy != merged || own != checkpoint.as_ref() {
                add(Self::parse(copy, slot_size, own)?);
            }
        }
        Ok(Some(ParityJournal {
            runs,
            slots: crate::cache::valid_slots(&merged, slot_size),
            geometry: (slot_offset, slot_size as u32, slot_count as u32),
            checkpoint_geometry,
            checkpoint,
            current,
        }))
    }

    /// The consistency per extent run that one version of the slot area and
    /// checkpoint describes, read as Windows reads a cache log: the
    /// checkpoint's entries, then the slots from the one it names on while
    /// their sequences increase (from slot 0 without a checkpoint); the
    /// newest entry per run counts.
    fn parse(area: &[u8], slot_size: usize, checkpoint: Option<&Checkpoint>) -> Result<Entries> {
        type Versioned = ((u64, usize), Consistency, Vec<u8>);
        let mut newest: HashMap<u64, Versioned> = HashMap::new();
        let mut apply = |sequence: u64, bytes: &[u8], count: usize| -> Result<()> {
            for (index, (offset, consistency, raw)) in decode_entries(bytes, count)?.into_iter().enumerate() {
                let version = (sequence, index);
                if newest.get(&offset).is_none_or(|(v, _, _)| version > *v) {
                    newest.insert(offset, (version, consistency, raw));
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
        let slots: Vec<&[u8]> = area.chunks_exact(slot_size).collect();
        for k in 0..slots.len() {
            let slot = slots[(start + k) % slots.len()];
            let sequence = le_u64(&slot[0x28..]);
            if &slot[0..8] != SPSLOT_SIGNATURE
                || le_u32(&slot[0x1c..]) as usize != slot_size
                || crc32_excluding(slot, 0x24) != le_u32(&slot[0x24..])
                || sequence <= last
            {
                break; // the end of the log
            }
            last = sequence;
            if le_u32(&slot[0x20..]) == 0 {
                apply(sequence, &slot[0x38..], le_u32(&slot[0x30..]) as usize)?;
            }
        }
        Ok(newest.into_iter().map(|(k, (_, c, raw))| (k, (c, raw))).collect())
    }

    /// Whether stripe `stripe` (counted from the start of the extent run that
    /// begins at owner offset `run_start`) may have stale parity.
    pub fn is_dirty(&self, run_start: u64, stripe: u64) -> bool {
        self.runs
            .get(&run_start)
            .is_some_and(|versions| versions.iter().any(|c| c.is_dirty(stripe)))
    }

    /// Every version of the entry per extent run (keyed by the owner offset
    /// where the run starts), in offset order.
    pub fn entries(&self) -> Vec<(u64, &[Consistency])> {
        let mut entries: Vec<_> = self.runs.iter().map(|(k, v)| (*k, v.as_slice())).collect();
        entries.sort_by_key(|e| e.0);
        entries
    }

    /// The valid slots of the slot area (the newest version of each).
    pub fn slots(&self) -> &[crate::cache::Slot] {
        &self.slots
    }

    /// Where the slot area is: (offset in the journal space, slot size,
    /// slot count).
    pub fn geometry(&self) -> (u64, u32, u32) {
        self.geometry
    }

    /// Where the checkpoint areas are: (offset in the journal space, size
    /// of each, count).
    pub fn checkpoint_geometry(&self) -> (u64, u32, u32) {
        self.checkpoint_geometry
    }

    /// The newest valid checkpoint.
    pub fn checkpoint(&self) -> Option<&Checkpoint> {
        self.checkpoint.as_ref()
    }

    /// A writer that continues this journal for the space `owner`: the
    /// next slot follows the newest one, with the next sequence, and each
    /// run starts from its newest entry.
    pub fn writer(&self, owner: Guid) -> JournalWriter {
        let (_, slot_size, slot_count) = self.geometry;
        let mut w = JournalWriter::new(owner, slot_size, slot_count);
        // A checkpoint newer than every slot continues at the slot it names.
        if let Some(c) = &self.checkpoint {
            (w.sequence, w.next_slot) = (c.sequence, c.next_slot as usize % slot_count.max(1) as usize);
        }
        if let Some(newest) = self.slots.iter().max_by_key(|s| s.sequence)
            && newest.sequence > w.sequence
        {
            w.sequence = newest.sequence;
            w.next_slot = (newest.index + 1) % slot_count.max(1) as usize;
        }
        for s in &self.slots {
            if let Some(v) = w.slot_seq.get_mut(s.index) {
                *v = s.sequence;
            }
        }
        w.checkpoint_count = self.checkpoint_geometry.2.min(4);
        w.checkpoint = self
            .checkpoint
            .as_ref()
            .map(|c| (c.sequence, c.area, c.next_slot as usize));
        w.loaded = self.current.iter().map(|(k, (c, _))| (*k, c.clone())).collect();
        w.loaded_bytes = self.current.iter().map(|(k, (_, raw))| (*k, raw.clone())).collect();
        w.listed = self.current.keys().copied().collect();
        w
    }

    /// Number of extent runs with possibly inconsistent stripes.
    pub fn dirty_runs(&self) -> usize {
        self.runs
            .values()
            .filter(|v| {
                v.iter().any(|c| match c {
                    Consistency::All => false,
                    Consistency::Runs(runs) => runs.iter().any(|&(consistent, n)| !consistent && n > 0),
                    Consistency::Bitmap(bits) => bits.iter().any(|&b| b != 0xff),
                })
            })
            .count()
    }
}

/// How Windows logs writes of whole stripes that bypass the write-back
/// cache (the model the scenario tests check slot by slot): the journal
/// starts without slots; every write request gets the next slot with the
/// next sequence and one entry for the extent run it touches, listing the
/// consistency of all stripes of the run (runs of stripes, bit 15 =
/// consistent), in which the written stripes are now consistent and those
/// never written count as not consistent.
#[derive(Debug, Clone)]
pub struct JournalWriter {
    owner: Guid,
    slot_size: u32,
    slot_count: u32,
    next_slot: usize,
    sequence: u64,
    /// Owner offset where an extent run starts -> consistent stripes.
    runs: std::collections::BTreeMap<u64, Vec<bool>>,
    /// Entries of a journal this writer continues, not yet expanded, and
    /// their bytes (for checkpoints).
    loaded: HashMap<u64, Consistency>,
    loaded_bytes: HashMap<u64, Vec<u8>>,
    /// Runs the journal has an entry for.
    listed: std::collections::HashSet<u64>,
    /// Sequence of the valid slot at each index (0: none).
    slot_seq: Vec<u64>,
    /// Number of checkpoint areas (0: none known), and the sequence, area
    /// and next slot of the newest checkpoint.
    checkpoint_count: u32,
    checkpoint: Option<(u64, usize, usize)>,
}

impl JournalWriter {
    /// An empty journal of the space `owner`.
    pub fn new(owner: Guid, slot_size: u32, slot_count: u32) -> Self {
        JournalWriter {
            owner,
            slot_size,
            slot_count,
            next_slot: 0,
            sequence: 0,
            runs: Default::default(),
            loaded: Default::default(),
            loaded_bytes: Default::default(),
            listed: Default::default(),
            slot_seq: vec![0; slot_count as usize],
            checkpoint_count: 0,
            checkpoint: None,
        }
    }

    /// The same writer for a journal with `count` checkpoint areas, which it
    /// writes checkpoints into when its log wraps.
    pub fn with_checkpoint_areas(mut self, count: u32) -> Self {
        self.checkpoint_count = count.min(4);
        self
    }

    /// Whether the journal has an entry for the run at `run_start` (a run
    /// without one is not checked by readers).
    pub fn is_listed(&self, run_start: u64) -> bool {
        self.listed.contains(&run_start)
    }

    /// Whether stripe `stripe` of the run at `run_start` (of `stripes`
    /// stripes) is recorded as consistent.
    pub fn is_consistent(&mut self, run_start: u64, stripes: u64, stripe: u64) -> bool {
        self.run(run_start, stripes)
            .get(stripe as usize)
            .copied()
            .unwrap_or(false)
    }

    fn run(&mut self, run_start: u64, stripes: u64) -> &mut Vec<bool> {
        let loaded = self.loaded.remove(&run_start);
        self.runs.entry(run_start).or_insert_with(|| {
            (0..stripes)
                .map(|s| loaded.as_ref().is_some_and(|c| !c.is_dirty(s)))
                .collect()
        })
    }

    /// A write request of stripes `first..first + count` of the extent run
    /// that starts at owner offset `run_start` and has `stripes` stripes:
    /// the slot Windows writes (after a checkpoint where one is due).
    pub fn write(&mut self, run_start: u64, stripes: u64, first: u64, count: u64) -> Vec<LogWrite> {
        self.mark(run_start, stripes, first, count, true)
    }

    /// Records stripes `first..first + count` of the run as consistent or
    /// not: the log writes.
    pub fn mark(&mut self, run_start: u64, stripes: u64, first: u64, count: u64, value: bool) -> Vec<LogWrite> {
        let consistent = self.run(run_start, stripes);
        let end = (first + count).min(consistent.len() as u64);
        for s in first.min(end)..end {
            consistent[s as usize] = value;
        }
        self.entry(run_start)
    }

    /// Records the stripes `list` of the run as consistent or not: the log
    /// writes.
    pub fn mark_stripes(&mut self, run_start: u64, stripes: u64, list: &[u64], value: bool) -> Vec<LogWrite> {
        let consistent = self.run(run_start, stripes);
        for &s in list {
            if let Some(c) = consistent.get_mut(s as usize) {
                *c = value;
            }
        }
        self.entry(run_start)
    }

    /// The next slot, with the entry for the run at `run_start`, after a
    /// checkpoint where the log comes round to the slot the newest one
    /// continues at.
    fn entry(&mut self, run_start: u64) -> Vec<LogWrite> {
        let entry = self.encode(run_start);
        self.listed.insert(run_start);
        let mut out = Vec::new();
        let index = self.next_slot;
        let (cp_seq, cp_area, cp_next) = self.checkpoint.unwrap_or((0, usize::MAX, 0));
        let overwritten = self.slot_seq.get(index).copied().unwrap_or(0);
        if index == cp_next && overwritten > cp_seq && self.checkpoint_count > 0 {
            self.sequence += 1;
            let area = cp_area.wrapping_add(1) % self.checkpoint_count as usize;
            let (entries, count) = self.checkpoint_entries();
            out.push(LogWrite::Checkpoint(
                area,
                Checkpoint::encode(self.owner, 1, self.sequence, index as u32, count, &entries),
            ));
            self.checkpoint = Some((self.sequence, area, index));
        }
        self.sequence += 1;
        if let Some(s) = self.slot_seq.get_mut(index) {
            *s = self.sequence;
        }
        self.next_slot = (self.next_slot + 1) % self.slot_count.max(1) as usize;
        out.push(LogWrite::Slot(
            index,
            crate::cache::encode_slot(self.owner, self.slot_size, 0, self.sequence, 1, &entry),
        ));
        out
    }

    /// The entries of every run the journal lists, 8-byte aligned, and
    /// their count.
    fn checkpoint_entries(&mut self) -> (Vec<u8>, u32) {
        let mut listed: Vec<u64> = self.listed.iter().copied().collect();
        listed.sort();
        let mut out = Vec::new();
        for run in &listed {
            let entry = match self.runs.contains_key(run) {
                true => self.encode(*run),
                false => self.loaded_bytes.get(run).cloned().unwrap_or_default(),
            };
            if entry.is_empty() {
                continue;
            }
            out.extend_from_slice(&entry);
            out.resize(out.len().next_multiple_of(8), 0);
        }
        (out, listed.len() as u32)
    }

    /// The entry for the run at `run_start`: a run list (state 2, as
    /// Windows writes it), or a bitmap (state 1) where the run list does not
    /// fit into a slot. Where neither fits, the run is recorded as not
    /// consistent as a whole, which is always safe.
    fn encode(&mut self, run_start: u64) -> Vec<u8> {
        let capacity = (self.slot_size as usize).saturating_sub(0x38 + 12);
        let consistent = self.runs.get_mut(&run_start).expect("run expanded by the caller");
        let words = |c: &[bool]| {
            let mut words = Vec::new();
            let mut i = 0;
            while i < c.len() {
                let v = c[i];
                let n = c[i..].iter().take(0x7fff).take_while(|&&x| x == v).count();
                words.push((u16::from(v) << 15) | n as u16);
                i += n;
            }
            words
        };
        let mut body = Vec::new();
        let mut state = 2u16;
        let w = words(consistent);
        let mut counted = None;
        if !consistent.contains(&false) && 2 * w.len() <= capacity {
            // Whole run consistent: state 3; the run words are written but
            // not counted (parity3_26100, the slot after the last stripe).
            state = 3;
            counted = Some(0);
            w.iter().for_each(|w| body.extend_from_slice(&w.to_le_bytes()));
        } else if 2 * w.len() <= capacity {
            w.iter().for_each(|w| body.extend_from_slice(&w.to_le_bytes()));
        } else if consistent.len().div_ceil(8) <= capacity {
            state = 1;
            body = vec![0u8; consistent.len().div_ceil(8)];
            for (i, _) in consistent.iter().enumerate().filter(|c| *c.1) {
                body[i / 8] |= 1 << (i % 8);
            }
            // Bits past the last stripe are set, as in all-set bitmaps.
            if !consistent.len().is_multiple_of(8) {
                *body.last_mut().unwrap() |= 0xff << (consistent.len() % 8);
            }
        } else {
            consistent.fill(false);
            words(consistent)
                .iter()
                .for_each(|w| body.extend_from_slice(&w.to_le_bytes()));
        }
        let mut entry = run_start.to_le_bytes().to_vec();
        entry.extend_from_slice(&state.to_le_bytes());
        entry.extend_from_slice(&counted.unwrap_or(body.len() as u16).to_le_bytes());
        entry.extend_from_slice(&body);
        entry
    }
}

/// The entries of a journal slot or checkpoint (`count` of them, from the
/// start of `bytes`): run offset, consistency and the entry's bytes.
fn decode_entries(bytes: &[u8], count: usize) -> Result<Vec<(u64, Consistency, Vec<u8>)>> {
    let mut out = Vec::with_capacity(count.min(4096));
    let mut pos = 0;
    for _ in 0..count {
        let head = bytes
            .get(pos..pos + 12)
            .ok_or_else(|| format_err!("parity journal slot overflows"))?;
        let offset = le_u64(head);
        let state = u16::from_le_bytes([head[8], head[9]]);
        let n = u16::from_le_bytes([head[10], head[11]]) as usize;
        let body_len = match state {
            1 | 2 => n,
            3 => 4,
            other => return Err(crate::Error::Unsupported(format!("parity journal entry state {other}"))),
        };
        let body = bytes
            .get(pos + 12..pos + 12 + body_len)
            .ok_or_else(|| format_err!("parity journal entry overflows its slot"))?;
        let raw = bytes[pos..pos + 12 + body_len].to_vec();
        // Entries start 8-byte aligned.
        pos = (pos + 12 + body_len).next_multiple_of(8);
        let consistency = match state {
            1 => Consistency::Bitmap(body.to_vec()),
            3 => Consistency::All,
            _ => {
                let mut runs = Vec::new();
                for w in body.as_chunks::<2>().0.iter().map(|&w| u16::from_le_bytes(w)) {
                    if w == 0 {
                        break;
                    }
                    runs.push((w & 0x8000 != 0, (w & 0x7fff) as u64));
                }
                Consistency::Runs(runs)
            }
        };
        out.push((offset, consistency, raw));
    }
    Ok(out)
}

impl Consistency {
    fn is_dirty(&self, stripe: u64) -> bool {
        match self {
            Consistency::All => false,
            Consistency::Bitmap(bits) => bits
                .get((stripe / 8) as usize)
                .is_none_or(|b| b >> (stripe % 8) & 1 == 0),
            Consistency::Runs(runs) => {
                let mut start = 0;
                for &(consistent, len) in runs {
                    if stripe < start + len {
                        return !consistent;
                    }
                    start += len;
                }
                true
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

    const OWNER: [u8; 16] = [3; 16];

    fn set_crc(b: &mut [u8]) {
        let crc = crc32_excluding(b, 0x24);
        b[0x24..0x28].copy_from_slice(&crc.to_le_bytes());
    }

    /// A journal whose one slot holds `entries` (run offset, state, body).
    fn journal(entries: &[(u64, u16, Vec<u8>)]) -> Vec<u8> {
        let mut j = vec![0u8; 0x1000 + 0x1000];
        j[0..8].copy_from_slice(SPVDT_SIGNATURE);
        j[8..24].copy_from_slice(&OWNER);
        j[0x30..0x38].copy_from_slice(&0x1000u64.to_le_bytes());
        j[0x38..0x3c].copy_from_slice(&0x1000u32.to_le_bytes());
        j[0x3c..0x40].copy_from_slice(&1u32.to_le_bytes());
        set_crc(&mut j[..0x60]);
        let s = &mut j[0x1000..];
        s[0..8].copy_from_slice(SPSLOT_SIGNATURE);
        s[0x1c..0x20].copy_from_slice(&0x1000u32.to_le_bytes());
        s[0x28..0x30].copy_from_slice(&1u64.to_le_bytes());
        s[0x30..0x34].copy_from_slice(&(entries.len() as u32).to_le_bytes());
        let mut pos = 0x38;
        for (offset, state, body) in entries {
            s[pos..pos + 8].copy_from_slice(&offset.to_le_bytes());
            s[pos + 8..pos + 10].copy_from_slice(&state.to_le_bytes());
            s[pos + 10..pos + 12].copy_from_slice(&(body.len() as u16).to_le_bytes());
            s[pos + 12..pos + 12 + body.len()].copy_from_slice(body);
            pos = (pos + 12 + body.len()).next_multiple_of(8);
        }
        set_crc(s);
        j
    }

    fn load(j: &[u8]) -> ParityJournal {
        let owner = Guid::from_mixed_endian(&OWNER);
        ParityJournal::load(owner, |off: u64, buf: &mut [u8]| {
            buf.copy_from_slice(&j[off as usize..off as usize + buf.len()]);
            Ok(())
        })
        .unwrap()
        .unwrap()
    }

    /// Checkpoint areas at an offset near the end of the address space
    /// (found by fuzzing: the offset of the second area overflowed) are
    /// not used; the slots still are.
    #[test]
    fn checkpoint_areas_beyond_reach_are_ignored() {
        let mut j = journal(&[(0x3000_0000, 1, Vec::new())]);
        j[0x40..0x48].copy_from_slice(&(u64::MAX - 0x100).to_le_bytes());
        j[0x48..0x4c].copy_from_slice(&0x1000u32.to_le_bytes());
        j[0x4c..0x50].copy_from_slice(&2u32.to_le_bytes());
        set_crc(&mut j[..0x60]);
        let pj = load(&j);
        assert_eq!(pj.checkpoint_geometry().2, 0);
        assert!(pj.checkpoint.is_none());
        assert_eq!(pj.runs.len(), 1);
    }

    /// The slot Windows wrote when the last stripes of a run became
    /// consistent (parity3_26100 was filled front to back in whole
    /// stripes): state 3, rebuilt byte for byte.
    #[test]
    fn whole_run_consistent_is_state_3_as_windows_writes_it() {
        use crate::io::SparseImage;
        use crate::pool::Pool;
        use std::fs::File;
        use std::path::Path;
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/parity3_26100");
        let disks: Vec<SparseImage> = (0..3)
            .map(|i| SparseImage::read_from(File::open(dir.join(format!("disk{i}.fixture"))).unwrap()).unwrap())
            .collect();
        let pool = Pool::open(disks).unwrap();
        let space = pool.find_space("parity3_26100").unwrap();
        let reader = pool.open_space(space.id()).unwrap();
        let journal = reader.journal().unwrap();
        let slots = journal.slots();
        let last = slots.iter().max_by_key(|s| s.sequence).unwrap();
        let before = slots.iter().find(|s| s.sequence == last.sequence - 1).unwrap();
        // The slot before: consistent 16352, not consistent 32.
        assert_eq!(&before.content[0x18 + 8..], &[2, 0, 4, 0, 0xe0, 0xbf, 0x20]);
        let mut w = JournalWriter::new(space.info.guid, 4096, 1024);
        w.mark(0, 16384, 0, 16352, true);
        w.sequence = before.sequence;
        w.next_slot = last.index;
        let records = w.write(0, 16384, 16352, 32);
        let (index, page) = records[0].slot().unwrap();
        assert_eq!((records.len(), index), (1, last.index));
        let end = page.iter().rposition(|&b| b != 0).unwrap() + 1;
        assert_eq!(&page[0x20..end], &last.content[..]);
    }

    #[test]
    fn reads_several_aligned_entries_per_slot() {
        // As Windows 11 24H2 writes them under NTFS: stripe runs of odd
        // length (17 words), then a bitmap, then a clean run.
        let mut runs: Vec<u8> = Vec::new();
        for w in [3u16, 0x8001].iter().cycle().take(16).chain([0x07e2].iter()) {
            runs.extend(w.to_le_bytes());
        }
        let j = journal(&[
            (0, 2, runs),
            (1 << 28, 1, vec![0b0000_0100, 0]),
            (2 << 28, 3, vec![0; 4]),
        ]);
        let journal = load(&j);
        assert_eq!(journal.dirty_runs(), 2);
        // Runs and bitmap bits mark consistent stripes.
        assert!(journal.is_dirty(0, 0));
        assert!(!journal.is_dirty(0, 3));
        assert!(journal.is_dirty(0, 4));
        assert!(!journal.is_dirty(1 << 28, 2));
        assert!(journal.is_dirty(1 << 28, 3));
        assert!(!journal.is_dirty(2 << 28, 5));
    }
}
