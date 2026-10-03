//! The block reference count table (checkpoint root 6): how many more
//! files share a cluster (block clones, deduplication). Its rows are keyed
//! by a range of virtual clusters (first, count), the key being the first
//! 16 bytes of the value. The value: 0x10 a u32 Windows changes with each
//! transaction (its low byte 1), 0x14 the kind, 0x18 the total of the
//! counts. Kind 1: a u16 per cluster from 0x1c (0x400 clusters, the value
//! 0x820 bytes), the references beyond the first. Kind 0: every cluster of
//! the range (any multiple of 0x400) has the same count, the u16 at 0x1c
//! (again at 0x1e); the value is 0x20 bytes and the total 0. Windows packs
//! rows whose clusters have one count into kind 0 rows, merging neighbours.

use crate::util::{le16, le32, le64};

/// Clusters a kind 1 row counts.
pub(crate) const BLOCK: u64 = 0x400;
/// The kinds (u32 at 0x14).
pub(crate) const COUNTS: u32 = 1;
pub(crate) const UNIFORM: u32 = 0;

/// The range a row's value covers: (first virtual cluster, clusters).
pub(crate) fn range(value: &[u8]) -> Option<(u64, u64)> {
    (value.len() >= 0x20).then(|| (le64(value, 0), le64(value, 8)))
}

/// A row's count for virtual cluster `v` (None: outside the row, or a
/// count the row does not hold).
pub(crate) fn count_of(value: &[u8], v: u64) -> Option<u16> {
    let (first, n) = range(value)?;
    if v < first || v >= first.saturating_add(n) {
        return None;
    }
    match le32(value, 0x14) {
        COUNTS => {
            let i = 0x1c + 2 * usize::try_from(v - first).ok()?;
            value.get(i..i + 2).map(|b| le16(b, 0))
        }
        _ => Some(le16(value, 0x1c)),
    }
}

/// A kind 0 row: every cluster of (first, n) counted `count`.
pub(crate) fn uniform_value(first: u64, n: u64, count: u16, stamp: u32) -> Vec<u8> {
    let mut v = vec![0u8; 0x20];
    v[0..8].copy_from_slice(&first.to_le_bytes());
    v[8..16].copy_from_slice(&n.to_le_bytes());
    v[0x10..0x14].copy_from_slice(&stamp.to_le_bytes());
    v[0x1c..0x1e].copy_from_slice(&count.to_le_bytes());
    v[0x1e..0x20].copy_from_slice(&count.to_le_bytes());
    v
}

/// A kind 1 row of the block at `first`, every count `count`.
pub(crate) fn counts_value(first: u64, count: u16, stamp: u32) -> Vec<u8> {
    let mut v = vec![0u8; (0x1c + 2 * BLOCK as usize).next_multiple_of(8)];
    v[0..8].copy_from_slice(&first.to_le_bytes());
    v[8..16].copy_from_slice(&BLOCK.to_le_bytes());
    v[0x10..0x14].copy_from_slice(&stamp.to_le_bytes());
    v[0x14..0x18].copy_from_slice(&COUNTS.to_le_bytes());
    v[0x18..0x1c].copy_from_slice(&(u32::from(count) * BLOCK as u32).to_le_bytes());
    for i in 0..BLOCK as usize {
        v[0x1c + 2 * i..0x1e + 2 * i].copy_from_slice(&count.to_le_bytes());
    }
    v
}
