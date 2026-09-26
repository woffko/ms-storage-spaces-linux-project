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

use crate::crc::crc32_excluding;
use crate::error::{Result, format_err};
use crate::guid::Guid;

pub const SPVDT_SIGNATURE: &[u8; 8] = b"SPVDT\0\0\0";
const SPSLOT_SIGNATURE: &[u8; 8] = b"SPSLOT\0\0";

#[derive(Debug, Clone, PartialEq, Eq)]
enum Consistency {
    All,
    /// (consistent, stripes) runs from the start of the extent run.
    Runs(Vec<(bool, u64)>),
    /// Bit set = consistent stripe.
    Bitmap(Vec<u8>),
}

/// Consistency of parity stripes, per extent run.
#[derive(Debug, Clone, Default)]
pub struct ParityJournal {
    runs: HashMap<u64, Consistency>,
}

impl ParityJournal {
    /// Parses the journal. `read` reads from the journal space.
    pub fn load(owner: Guid, mut read: impl FnMut(u64, &mut [u8]) -> Result<()>) -> Result<Option<Self>> {
        let mut head = [0u8; 0x60];
        read(0, &mut head)?;
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
        let mut area = vec![0u8; slot_size * slot_count];
        read(slot_offset, &mut area)?;

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
                pos += 12 + body_len;
                let consistency = match state {
                    1 => Consistency::Bitmap(body.to_vec()),
                    3 => Consistency::All,
                    _ => {
                        let mut runs = Vec::new();
                        for w in body.chunks_exact(2).map(|w| u16::from_le_bytes([w[0], w[1]])) {
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
        Ok(Some(ParityJournal {
            runs: newest.into_iter().map(|(k, (_, c))| (k, c)).collect(),
        }))
    }

    /// Whether stripe `stripe` (counted from the start of the extent run that
    /// begins at owner offset `run_start`) may have stale parity.
    pub fn is_dirty(&self, run_start: u64, stripe: u64) -> bool {
        match self.runs.get(&run_start) {
            None | Some(Consistency::All) => false,
            Some(Consistency::Bitmap(bits)) => bits
                .get((stripe / 8) as usize)
                .is_none_or(|b| b >> (stripe % 8) & 1 == 0),
            Some(Consistency::Runs(runs)) => {
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

    /// Number of extent runs with possibly inconsistent stripes.
    pub fn dirty_runs(&self) -> usize {
        self.runs.values().filter(|c| **c != Consistency::All).count()
    }
}

fn le_u32(b: &[u8]) -> u32 {
    u32::from_le_bytes(b[..4].try_into().unwrap())
}

fn le_u64(b: &[u8]) -> u64 {
    u64::from_le_bytes(b[..8].try_into().unwrap())
}
