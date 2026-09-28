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
//! | 0x18 | 8 each | virtual slab where a listed extent run starts |
//!
//! The log lists the extent runs written since the space was last
//! disconnected; a Windows restart does not clear it. The copies of a mirror
//! can differ only inside a listed run (after a crash with writes in
//! flight), and Windows neither reconciles them nor prefers one when it
//! reads.

use std::collections::BTreeSet;

use crate::crc::crc32_excluding;
use crate::error::Result;

pub const SPACEDRT_SIGNATURE: &[u8; 8] = b"SPACEDRT";
const HEADER: usize = 0x1000;
/// Distance of the second header copy from the end of the space.
const SECOND_COPY_FROM_END: u64 = 0x2000;
/// Entries that fit into a header page.
const MAX_ENTRIES: usize = (HEADER - 0x18) / 8;

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
    /// The header page as read.
    pub page: Vec<u8>,
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
                page: h,
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

    /// The header copy Windows writes when a write reaches the extent run
    /// starting at virtual slab `run` of a tracking space of `size` bytes
    /// while the run is not listed: the next generation lists the runs still
    /// dirty (those listed minus `cleaned`, the runs Windows has found clean
    /// since the last header write) and then `run`. Odd generations go to the
    /// copy at the end, even ones to the start, so the older copy is
    /// replaced. `None` if the run is listed, or if the next generation or
    /// list would not fit (what Windows does then is not known).
    pub fn after_first_write(&self, size: u64, run: u64, cleaned: &[u64]) -> Option<(u64, Vec<u8>)> {
        if self.runs.contains(&run) {
            return None;
        }
        let current = self
            .copies
            .iter()
            .filter(|c| c.header.is_some())
            .max_by_key(|c| c.header.as_ref().unwrap().generation)?;
        let header = current.header.as_ref().unwrap();
        let mut runs: Vec<u64> = header.runs.iter().copied().filter(|r| !cleaned.contains(r)).collect();
        runs.push(run);
        if runs.len() > MAX_ENTRIES {
            return None;
        }
        let next = DrtHeader {
            generation: header.generation.checked_add(1)?,
            runs,
        };
        let offset = if next.generation % 2 == 1 {
            size.saturating_sub(SECOND_COPY_FROM_END)
        } else {
            0
        };
        Some((offset, next.encode()))
    }

    /// The header copies after `Disconnect-VirtualDisk` (and of a new
    /// space): generation 0 and no runs in both.
    pub fn after_disconnect(size: u64) -> [(u64, Vec<u8>); 2] {
        let empty = DrtHeader {
            generation: 0,
            runs: Vec::new(),
        }
        .encode();
        [(0, empty.clone()), (size.saturating_sub(SECOND_COPY_FROM_END), empty)]
    }

    /// A header copy, or `None` if it is missing, torn or implausible.
    fn parse(h: &[u8]) -> Option<DrtHeader> {
        if &h[0..8] != SPACEDRT_SIGNATURE {
            return None;
        }
        let count = u32::from_le_bytes(h[0x10..0x14].try_into().unwrap()) as usize;
        if count > MAX_ENTRIES {
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

impl DrtHeader {
    /// The header page as Windows writes it (the rest of the page is zero).
    /// Panics if more runs are listed than fit into the page.
    pub fn encode(&self) -> Vec<u8> {
        assert!(
            self.runs.len() <= MAX_ENTRIES,
            "{} dirty region entries",
            self.runs.len()
        );
        let mut h = vec![0u8; HEADER];
        h[0..8].copy_from_slice(SPACEDRT_SIGNATURE);
        h[8..16].copy_from_slice(&self.generation.to_le_bytes());
        h[0x10..0x14].copy_from_slice(&(self.runs.len() as u32).to_le_bytes());
        for (i, r) in self.runs.iter().enumerate() {
            h[0x18 + 8 * i..0x20 + 8 * i].copy_from_slice(&r.to_le_bytes());
        }
        let crc = crc32_excluding(&h[..0x18 + 8 * self.runs.len()], 0x14);
        h[0x14..0x18].copy_from_slice(&crc.to_le_bytes());
        h
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(generation: u64, runs: &[u64]) -> Vec<u8> {
        DrtHeader {
            generation,
            runs: runs.to_vec(),
        }
        .encode()
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

    #[test]
    fn no_prediction_past_the_last_generation_or_a_full_page() {
        // Found by fuzzing: the generation after u64::MAX overflowed.
        let mut space = vec![0u8; 0x10000];
        space[..HEADER].copy_from_slice(&header(u64::MAX, &[0]));
        assert_eq!(load(&space).unwrap().after_first_write(0x10000, 1, &[]), None);
        let full: Vec<u64> = (0..MAX_ENTRIES as u64).collect();
        space[..HEADER].copy_from_slice(&header(3, &full));
        let d = load(&space).unwrap();
        assert_eq!(d.after_first_write(0x10000, 1 << 40, &[]), None);
        // With one run gone clean the new one fits again.
        let (offset, page) = d.after_first_write(0x10000, 1 << 40, &[0]).unwrap();
        assert_eq!(offset, 0);
        assert_eq!(
            u32::from_le_bytes(page[0x10..0x14].try_into().unwrap()) as usize,
            MAX_ENTRIES
        );
    }
}
