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

use std::collections::HashMap;

use crate::cache::{SlotSource, merge_slot_copies};
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
}

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
        let mut runs: HashMap<u64, Vec<Consistency>> = HashMap::new();
        for area in std::iter::once(&merged).chain(copies.iter().filter(|c| **c != merged)) {
            for (offset, c) in Self::parse(area, slot_size)? {
                let versions = runs.entry(offset).or_default();
                if !versions.contains(&c) {
                    versions.push(c);
                }
            }
        }
        Ok(Some(ParityJournal {
            runs,
            slots: crate::cache::valid_slots(&merged, slot_size),
        }))
    }

    /// The consistency per extent run that one version of the slot area
    /// describes: the newest entry per run.
    fn parse(area: &[u8], slot_size: usize) -> Result<HashMap<u64, Consistency>> {
        let mut newest: HashMap<u64, ((u64, usize), Consistency)> = HashMap::new();
        for slot in area.chunks_exact(slot_size) {
            if &slot[0..8] != SPSLOT_SIGNATURE
                || le_u32(&slot[0x1c..]) as usize != slot_size
                || crc32_excluding(slot, 0x24) != le_u32(&slot[0x24..])
                || le_u32(&slot[0x20..]) != 0
            {
                continue;
            }
            let sequence = le_u64(&slot[0x28..]);
            let count = le_u32(&slot[0x30..]) as usize;
            let mut pos = 0x38;
            for index in 0..count {
                let head = slot
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
                let body = slot
                    .get(pos + 12..pos + 12 + body_len)
                    .ok_or_else(|| format_err!("parity journal entry overflows its slot"))?;
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
                let version = (sequence, index);
                if newest.get(&offset).is_none_or(|(v, _)| version > *v) {
                    newest.insert(offset, (version, consistency));
                }
            }
        }
        Ok(newest.into_iter().map(|(k, (_, c))| (k, c)).collect())
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

    /// Number of extent runs with possibly inconsistent stripes.
    pub fn dirty_runs(&self) -> usize {
        self.runs
            .values()
            .filter(|v| v.iter().any(|c| *c != Consistency::All))
            .count()
    }
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
