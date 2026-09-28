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
//! Pools whose disks were removed while the pool was online list the runs
//! written last even when the copies agree; whether a clean shutdown empties
//! the log is not known. The copies of a mirror may differ inside a listed
//! run: Windows resynchronises them when it mounts the pool, and which copy
//! it keeps is not recorded here.

use std::collections::BTreeSet;

use crate::crc::crc32_excluding;
use crate::error::Result;

pub const SPACEDRT_SIGNATURE: &[u8; 8] = b"SPACEDRT";
const HEADER: usize = 0x1000;
/// Distance of the second header copy from the end of the space.
const SECOND_COPY_FROM_END: u64 = 0x2000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirtyRegions {
    /// Virtual slabs where dirty extent runs start.
    runs: BTreeSet<u64>,
    copies: Vec<DrtCopy>,
}

/// One of the two header copies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrtCopy {
    /// Offset of the copy in the tracking space.
    pub offset: u64,
    /// `None` when the copy has no signature or does not check out.
    pub header: Option<DrtHeader>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrtHeader {
    pub generation: u64,
    /// Virtual slabs where the listed extent runs start, in on-disk order.
    pub runs: Vec<u64>,
}

impl DirtyRegions {
    /// Reads both header copies of a tracking space of `size` bytes; `None`
    /// when neither is valid.
    pub fn load(size: u64, mut read: impl FnMut(u64, &mut [u8]) -> Result<()>) -> Result<Option<Self>> {
        let mut copies = Vec::new();
        for offset in [0, size.saturating_sub(SECOND_COPY_FROM_END)] {
            let mut h = vec![0u8; HEADER];
            read(offset, &mut h)?;
            copies.push(DrtCopy {
                offset,
                header: Self::parse(&h),
            });
        }
        let best = copies
            .iter()
            .filter_map(|c| c.header.as_ref())
            .reduce(|a, b| if b.generation > a.generation { b } else { a });
        Ok(best.map(|h| DirtyRegions {
            runs: h.runs.iter().copied().collect(),
            copies: copies.clone(),
        }))
    }

    /// A header copy, or `None` if it is missing, torn or implausible.
    fn parse(h: &[u8]) -> Option<DrtHeader> {
        if &h[0..8] != SPACEDRT_SIGNATURE {
            return None;
        }
        let count = u32::from_le_bytes(h[0x10..0x14].try_into().unwrap()) as usize;
        if count > (HEADER - 0x18) / 8 {
            return None;
        }
        let span = 0x18 + 8 * count;
        if crc32_excluding(&h[..span], 0x14) != u32::from_le_bytes(h[0x14..0x18].try_into().unwrap()) {
            return None; // torn copy
        }
        Some(DrtHeader {
            generation: u64::from_le_bytes(h[8..16].try_into().unwrap()),
            runs: h[0x18..span]
                .as_chunks::<8>()
                .0
                .iter()
                .map(|&e| u64::from_le_bytes(e))
                .collect(),
        })
    }

    /// Whether the extent run starting at virtual slab `slab` had writes in
    /// flight.
    pub fn is_dirty(&self, slab: u64) -> bool {
        self.runs.contains(&slab)
    }

    /// Number of dirty extent runs in the current copy.
    pub fn dirty_runs(&self) -> usize {
        self.runs.len()
    }

    /// Both header copies as found on disk.
    pub fn copies(&self) -> &[DrtCopy] {
        &self.copies
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
        assert_eq!(d.copies()[1].offset, 0xe000);
        assert_eq!(d.copies()[1].header.as_ref().unwrap().runs, [0]);
        // A torn newer copy falls back to the older one.
        space[0x18] ^= 1;
        let d = load(&space).unwrap();
        assert_eq!(d.dirty_runs(), 1);
        assert_eq!(d.copies()[0].header, None);
        // So does one whose entry count cannot fit.
        space[..HEADER].copy_from_slice(&header(2, &[0, 4]));
        space[0x10] = 0xff;
        assert_eq!(load(&space).unwrap().dirty_runs(), 1);
        // An empty log.
        let mut clean = vec![0u8; 0x10000];
        clean[..HEADER].copy_from_slice(&header(0, &[]));
        assert_eq!(load(&clean).unwrap().dirty_runs(), 0);
    }
}
