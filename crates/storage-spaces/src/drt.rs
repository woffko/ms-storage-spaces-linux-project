//! Dirty region tracking of mirror spaces ("SPACEDRT").
//!
//! Mirror spaces have a hidden role 6 child whose space starts with a small
//! header, a second copy of which sits 8 KiB before its end:
//!
//! | Offset | Size | Field |
//! |---|---|---|
//! | 0x00 | 8 | "SPACEDRT" |
//! | 0x08 | 8 | generation (the copy with the higher one is current) |
//! | 0x10 | 4 | number of entries |
//! | 0x14 | 4 | CRC-32 (zlib) of the first `0x18 + 8 * count` bytes, this field zeroed |
//! | 0x18 | 8 each | virtual slab where an extent run with writes in flight starts |
//!
//! After a clean shutdown both copies are empty. The copies of a mirror may
//! differ inside a listed run: Windows resynchronises them when it mounts
//! the pool, and which copy it keeps is not recorded here.

use std::collections::BTreeSet;

use crate::crc::crc32_excluding;
use crate::error::{Result, format_err};

pub const SPACEDRT_SIGNATURE: &[u8; 8] = b"SPACEDRT";
const HEADER: usize = 0x1000;
/// Distance of the second header copy from the end of the space.
const SECOND_COPY_FROM_END: u64 = 0x2000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirtyRegions {
    /// Virtual slabs where dirty extent runs start.
    runs: BTreeSet<u64>,
}

impl DirtyRegions {
    /// Reads both header copies of a tracking space of `size` bytes; `None`
    /// when neither is valid.
    pub fn load(size: u64, mut read: impl FnMut(u64, &mut [u8]) -> Result<()>) -> Result<Option<Self>> {
        let mut best: Option<(u64, Vec<u64>)> = None;
        for offset in [0, size.saturating_sub(SECOND_COPY_FROM_END)] {
            let mut h = vec![0u8; HEADER];
            read(offset, &mut h)?;
            if &h[0..8] != SPACEDRT_SIGNATURE {
                continue;
            }
            let generation = u64::from_le_bytes(h[8..16].try_into().unwrap());
            let count = u32::from_le_bytes(h[0x10..0x14].try_into().unwrap()) as usize;
            if count > (HEADER - 0x18) / 8 {
                return Err(format_err!("dirty region tracking with {count} entries"));
            }
            let span = 0x18 + 8 * count;
            if crc32_excluding(&h[..span], 0x14) != u32::from_le_bytes(h[0x14..0x18].try_into().unwrap()) {
                continue; // torn copy
            }
            let entries = h[0x18..span]
                .as_chunks::<8>()
                .0
                .iter()
                .map(|&e| u64::from_le_bytes(e))
                .collect();
            if best.as_ref().is_none_or(|(g, _)| generation > *g) {
                best = Some((generation, entries));
            }
        }
        Ok(best.map(|(_, runs)| DirtyRegions {
            runs: runs.into_iter().collect(),
        }))
    }

    /// Whether the extent run starting at virtual slab `slab` had writes in
    /// flight.
    pub fn is_dirty(&self, slab: u64) -> bool {
        self.runs.contains(&slab)
    }

    /// Number of dirty extent runs (0 after a clean shutdown).
    pub fn dirty_runs(&self) -> usize {
        self.runs.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(generation: u64, runs: &[u64]) -> Vec<u8> {
        let mut h = vec![0u8; HEADER];
        h[0..8].copy_from_slice(SPACEDRT_SIGNATURE);
        h[8..16].copy_from_slice(&generation.to_le_bytes());
        h[0x10..0x14].copy_from_slice(&(runs.len() as u32).to_le_bytes());
        for (i, r) in runs.iter().enumerate() {
            h[0x18 + 8 * i..0x20 + 8 * i].copy_from_slice(&r.to_le_bytes());
        }
        let crc = crc32_excluding(&h[..0x18 + 8 * runs.len()], 0x14);
        h[0x14..0x18].copy_from_slice(&crc.to_le_bytes());
        h
    }

    fn load(space: &[u8]) -> Option<DirtyRegions> {
        DirtyRegions::load(space.len() as u64, |off, buf| {
            buf.copy_from_slice(&space[off as usize..off as usize + buf.len()]);
            Ok(())
        })
        .unwrap()
    }

    #[test]
    fn newest_valid_copy_wins() {
        // As in crashmirrorwc: generation 2 at the start lists runs 0 and 4,
        // generation 1 at the end lists run 0.
        let mut space = vec![0u8; 0x10000];
        space[..HEADER].copy_from_slice(&header(2, &[0, 4]));
        space[0xe000..0xf000].copy_from_slice(&header(1, &[0]));
        let d = load(&space).unwrap();
        assert!(d.is_dirty(0) && d.is_dirty(4) && !d.is_dirty(1));
        assert_eq!(d.dirty_runs(), 2);
        // A torn newer copy falls back to the older one.
        space[0x18] ^= 1;
        let d = load(&space).unwrap();
        assert_eq!(d.dirty_runs(), 1);
        // The empty header of a cleanly shut down mirror.
        let mut clean = vec![0u8; 0x10000];
        clean[..HEADER].copy_from_slice(&header(0, &[]));
        assert_eq!(load(&clean).unwrap().dirty_runs(), 0);
    }
}
