//! Mapping virtual space offsets to physical slabs.

use std::collections::BTreeMap;

use crate::error::{Error, Result, format_err};
use crate::format::{ExtentRecord, Policy, Resiliency, SLAB_SIZE};

/// Redundancy state of a space or layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Condition {
    /// Every copy and column is on a present disk.
    Healthy,
    /// Some copies or columns are on missing disks or out of date, but all
    /// data can still be read or rebuilt.
    Degraded,
    /// Some data is only on missing disks.
    Failed,
}

/// Largest slab number or count accepted from extent records.
const MAX_SLABS: u64 = 1 << 32;

/// A run of consecutive physical slabs backing consecutive rows of one column copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    pub first_row: u64,
    pub rows: u64,
    pub disk_id: u64,
    pub physical_slab: u64,
}

/// Where a virtual offset lives within the column layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Location {
    pub column: u64,
    /// Slab row within the column.
    pub row: u64,
    pub offset_in_slab: u64,
    /// Bytes that stay contiguous from this point (up to the interleave boundary).
    pub contiguous: u64,
}

/// Striping layout of a space: data is split into `interleave`-sized units
/// written round-robin across the columns; each column is a sequence of slab
/// rows, each backed by one physical slab per copy.
///
/// Parity spaces keep `r` parity units per stripe in consecutive columns
/// starting at `(C - r - r * s) mod C` for stripe `s`; the data units follow
/// them, wrapping around. For r = 1 this is the left-symmetric RAID-5 layout;
/// the stripe number counts from the first row of the extent run the stripe
/// lies in. `r` is the redundancy (1 or 2), or `groups + 1` for dual parity
/// with several groups (a local reconstruction code). What the parity units
/// hold is described by [`Layout::parity_code`].
#[derive(Debug, Clone)]
pub struct Layout {
    pub resiliency: Resiliency,
    pub columns: u64,
    pub data_columns: u64,
    /// Parity units per stripe (0 for simple and mirror).
    pub parity_units: u64,
    /// Local groups of a dual parity space (1 unless it uses a local
    /// reconstruction code).
    pub groups: u64,
    /// Start of the layout in the owner's address space (storage tiers);
    /// rows and parity rotation count from here.
    pub base: u64,
    pub copies: u64,
    pub interleave: u64,
    runs: BTreeMap<(u64, u64), Vec<Run>>,
    /// Out-of-date copies per column.
    stale: BTreeMap<u64, Vec<Run>>,
}

impl Layout {
    pub fn new(policy: &Policy, extents: &[ExtentRecord]) -> Result<Self> {
        Self::with_base(policy, extents, 0)
    }

    /// A layout that starts at byte `base` of the owner space (a tier).
    pub fn with_base(policy: &Policy, extents: &[ExtentRecord], base: u64) -> Result<Self> {
        if !base.is_multiple_of(SLAB_SIZE) {
            return Err(format_err!("layout starts at {base:#x}, not on a slab boundary"));
        }
        let base_slab = base / SLAB_SIZE;
        let data_columns = match policy.resiliency {
            Resiliency::Simple | Resiliency::Mirror => policy.columns,
            Resiliency::Parity if policy.groups > 1 => {
                // One local parity unit per group and one global one.
                if policy.redundancy != 2 || policy.groups > policy.columns / 2 {
                    return Err(Error::Unsupported(format!(
                        "parity with {} groups, {} columns and redundancy {}",
                        policy.groups, policy.columns, policy.redundancy
                    )));
                }
                policy.columns - policy.groups - 1
            }
            Resiliency::Parity if (1..=2).contains(&policy.redundancy) && policy.columns >= policy.redundancy + 2 => {
                policy.columns - policy.redundancy
            }
            Resiliency::Parity => {
                return Err(Error::Unsupported(format!(
                    "parity with {} columns and redundancy {}",
                    policy.columns, policy.redundancy
                )));
            }
            Resiliency::Other(r) => return Err(Error::Unsupported(format!("resiliency type {r}"))),
        };
        if !SLAB_SIZE.is_multiple_of(policy.interleave) {
            return Err(Error::Unsupported(format!("interleave {:#x}", policy.interleave)));
        }
        let mut runs: BTreeMap<(u64, u64), Vec<Run>> = BTreeMap::new();
        // Out-of-date copies and copies being rebuilt are not read, but
        // remembered so that a row without a current copy is an error.
        let mut stale: BTreeMap<u64, Vec<Run>> = BTreeMap::new();
        // Copy numbers can exceed the policy while a copy is regenerated.
        let copies = extents
            .iter()
            .map(|e| e.copy + 1)
            .max()
            .unwrap_or(1)
            .max(policy.copies.max(1));
        for e in extents {
            // Slab numbers beyond 2^32 (1 EiB) cannot be real; the limit keeps
            // all offset arithmetic far from overflowing.
            if [e.virtual_slab, e.slab_count, e.physical_slab]
                .iter()
                .any(|&n| n > MAX_SLABS)
            {
                return Err(format_err!(
                    "extent with implausible slab numbers: virtual {}, count {}, physical {}",
                    e.virtual_slab,
                    e.slab_count,
                    e.physical_slab
                ));
            }
            if e.column >= policy.columns || e.copy >= copies.max(policy.copies + 1) {
                return Err(format_err!(
                    "extent column {} copy {} outside a {}x{} layout",
                    e.column,
                    e.copy,
                    policy.columns,
                    policy.copies
                ));
            }
            let slab = e
                .virtual_slab
                .checked_sub(base_slab)
                .ok_or_else(|| format_err!("extent at virtual slab {} lies before the layout start", e.virtual_slab))?;
            if slab % data_columns != 0 {
                return Err(format_err!(
                    "extent starts at virtual slab {} not aligned to a row",
                    e.virtual_slab
                ));
            }
            let run = Run {
                first_row: slab / data_columns,
                rows: e.slab_count,
                disk_id: e.disk_id,
                physical_slab: e.physical_slab,
            };
            if e.is_current() {
                runs.entry((e.column, e.copy)).or_default().push(run);
            } else {
                stale.entry(e.column).or_default().push(run);
            }
        }
        for list in runs.values_mut() {
            list.sort_by_key(|r| r.first_row);
            if list.windows(2).any(|w| w[0].first_row + w[0].rows > w[1].first_row) {
                return Err(format_err!("overlapping extents"));
            }
        }
        Ok(Layout {
            resiliency: policy.resiliency,
            columns: policy.columns,
            data_columns,
            parity_units: policy.columns - data_columns,
            groups: if policy.resiliency == Resiliency::Parity {
                policy.groups.max(1)
            } else {
                1
            },
            base,
            copies,
            interleave: policy.interleave,
            runs,
            stale,
        })
    }

    /// Locates an offset of the owner space; `offset` must be at least `base`.
    pub fn locate(&self, offset: u64) -> Location {
        let offset = offset - self.base;
        let unit = offset / self.interleave;
        let within = offset % self.interleave;
        let stripe = unit / self.data_columns;
        let index = unit % self.data_columns;
        let column = match self.resiliency {
            Resiliency::Parity => {
                (self.parity_column(self.rotation_stripe(stripe)) + self.parity_units + index) % self.columns
            }
            _ => index,
        };
        let column_offset = stripe * self.interleave + within;
        Location {
            column,
            row: column_offset / SLAB_SIZE,
            offset_in_slab: column_offset % SLAB_SIZE,
            contiguous: self.interleave - within,
        }
    }

    /// Column holding the first (XOR) parity unit of a stripe; a second
    /// parity unit follows it.
    pub fn parity_column(&self, stripe: u64) -> u64 {
        let c = self.columns;
        let r = self.parity_units;
        (c - r + c * r - (r * stripe) % c) % c
    }

    /// What the parity units of a stripe hold, in column order: parity unit
    /// `i` is the sum over the data units `k` of `code[i][k] * D_k` in GF(16)
    /// (see `gf16`); a coefficient of 1 is plain XOR. `None` stands for a
    /// parity unit whose code is not known for this width.
    ///
    /// * single parity: P, the XOR of the data units;
    /// * dual parity: P and Q (`gf16::coefficients`);
    /// * dual parity with `g` groups: the data units are split into `g`
    ///   consecutive groups (the first `D mod g` of them one unit larger);
    ///   one local XOR parity per group, then a global one with the
    ///   coefficients 1, 2, 3, ... restarting in every group.
    pub fn parity_code(&self) -> Vec<Option<Vec<u8>>> {
        let d = self.data_columns as usize;
        if self.groups > 1 {
            let g = self.groups as usize;
            let mut code = Vec::with_capacity(g + 1);
            let mut global = Vec::with_capacity(d);
            let mut start = 0;
            for i in 0..g {
                let len = d / g + usize::from(i < d % g);
                let mut row = vec![0u8; d];
                row[start..start + len].fill(1);
                code.push(Some(row));
                global.extend((1..=len).map(|c| c as u8));
                start += len;
            }
            code.push(Some(global));
            return code;
        }
        let mut code = vec![Some(vec![1u8; d])];
        if self.parity_units == 2 {
            code.push(crate::gf16::coefficients(self.data_columns).map(<[u8]>::to_vec));
        }
        code
    }

    /// Stripe number of a location (parity layouts).
    pub fn stripe_of(&self, loc: &Location) -> u64 {
        self.rotation_stripe((loc.row * SLAB_SIZE + loc.offset_in_slab) / self.interleave)
    }

    /// Parity rotation restarts at the first row of every extent run.
    fn rotation_stripe(&self, stripe: u64) -> u64 {
        let per_row = SLAB_SIZE / self.interleave;
        stripe - self.run_first_row(stripe / per_row) * per_row
    }

    /// First row of the extent run holding `row` (0 if unallocated).
    pub fn run_first_row(&self, row: u64) -> u64 {
        self.runs
            .get(&(0, 0))
            .and_then(|runs| {
                let i = runs.partition_point(|r| r.first_row + r.rows <= row);
                runs.get(i).filter(|r| r.first_row <= row)
            })
            .map_or(0, |r| r.first_row)
    }

    /// Owner offset where the extent run holding `row` starts.
    pub fn run_start_offset(&self, row: u64) -> u64 {
        self.base + self.run_first_row(row) * SLAB_SIZE * self.data_columns
    }

    /// The copy numbers of a column (a moved copy may be numbered beyond the
    /// policy's copies).
    pub fn copies_of(&self, column: u64) -> Vec<u64> {
        self.runs
            .keys()
            .filter(|(c, _)| *c == column)
            .map(|(_, copy)| *copy)
            .collect()
    }

    /// Physical slab backing a row of a column copy, if allocated.
    pub fn physical(&self, column: u64, copy: u64, row: u64) -> Option<(u64, u64)> {
        let runs = self.runs.get(&(column, copy))?;
        let i = runs.partition_point(|r| r.first_row + r.rows <= row);
        let run = runs.get(i).filter(|r| r.first_row <= row)?;
        Some((run.disk_id, run.physical_slab + (row - run.first_row)))
    }

    /// Whether the row of a column has an out-of-date copy only.
    pub fn has_stale_copy(&self, column: u64, row: u64) -> bool {
        self.stale
            .get(&column)
            .is_some_and(|runs| runs.iter().any(|r| r.first_row <= row && row < r.first_row + r.rows))
    }

    /// How much redundancy is left when only the disks for which `present`
    /// returns true can be read. Rows without any extent (thin provisioning)
    /// do not count.
    pub fn condition(&self, present: impl Fn(u64) -> bool) -> Condition {
        // The state is constant between run boundaries.
        let mut bounds: Vec<u64> = self
            .runs
            .values()
            .chain(self.stale.values())
            .flatten()
            .flat_map(|r| [r.first_row, r.first_row + r.rows])
            .collect();
        bounds.sort_unstable();
        bounds.dedup();
        // Disk failures a row survives.
        let tolerance = match self.resiliency {
            Resiliency::Parity if self.groups > 1 => 2,
            Resiliency::Parity => self.parity_units,
            _ => self.copies.saturating_sub(1),
        };
        let mut worst = Condition::Healthy;
        for row in bounds {
            let mut lost_columns = 0;
            let mut reduced = false;
            let mut allocated = false;
            for column in 0..self.columns {
                let stale = self.has_stale_copy(column, row);
                let copies: Vec<u64> = (0..self.copies)
                    .filter_map(|copy| self.physical(column, copy, row).map(|(disk, _)| disk))
                    .collect();
                if copies.is_empty() && !stale {
                    continue;
                }
                allocated = true;
                let readable = copies.iter().filter(|&&d| present(d)).count();
                if readable == 0 {
                    lost_columns += 1;
                } else if readable < copies.len() || stale {
                    reduced = true;
                }
            }
            if !allocated {
                continue;
            }
            let row_condition = match self.resiliency {
                Resiliency::Parity if lost_columns > tolerance => Condition::Failed,
                Resiliency::Parity if lost_columns > 0 || reduced => Condition::Degraded,
                _ if lost_columns > 0 => Condition::Failed,
                _ if reduced => Condition::Degraded,
                _ => Condition::Healthy,
            };
            worst = worst.max(row_condition);
        }
        worst
    }

    /// Bytes of the owner space the extents cover, up to the end of the
    /// last row (the size of spaces whose record gives none).
    pub fn mapped_size(&self) -> u64 {
        let rows = self
            .runs
            .values()
            .flatten()
            .map(|r| r.first_row + r.rows)
            .max()
            .unwrap_or(0);
        rows * SLAB_SIZE * self.data_columns
    }

    /// All runs, keyed by (column, copy).
    pub fn runs(&self) -> &BTreeMap<(u64, u64), Vec<Run>> {
        &self.runs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(columns: u64, interleave: u64) -> Policy {
        Policy {
            resiliency: Resiliency::Simple,
            redundancy: 0,
            copies: 1,
            groups: 1,
            columns,
            interleave,
        }
    }

    #[test]
    fn stripes_round_robin() {
        let l = Layout::new(&policy(2, 0x10000), &[]).unwrap();
        assert_eq!(l.locate(0x10000).column, 1);
        let loc = l.locate(0x2_0000 + 5);
        assert_eq!((loc.column, loc.row, loc.offset_in_slab), (0, 0, 0x10005));
        let loc = l.locate(2 * SLAB_SIZE + 0x10000);
        assert_eq!((loc.column, loc.row, loc.offset_in_slab), (1, 1, 0));
    }

    #[test]
    fn rotates_parity_left_symmetric() {
        let p = Policy {
            resiliency: Resiliency::Parity,
            redundancy: 1,
            copies: 1,
            groups: 1,
            columns: 3,
            interleave: 0x10000,
        };
        let l = Layout::new(&p, &[]).unwrap();
        // Units D0..D5 as observed on a Windows-created 3-column parity space.
        let columns: Vec<u64> = (0..6).map(|u| l.locate(u * 0x10000).column).collect();
        assert_eq!(columns, [0, 1, 2, 0, 1, 2]);
        assert_eq!(l.locate(3 * 0x10000).column, 0);
        assert_eq!(l.locate(2 * 0x10000).column, 2);
        assert_eq!((0..3).map(|s| l.parity_column(s)).collect::<Vec<_>>(), [2, 1, 0]);
        assert_eq!(l.locate(5 * 0x10000).offset_in_slab, 2 * 0x10000);
    }

    #[test]
    fn rotates_dual_parity() {
        // Seven columns, two parity units per stripe, as Windows lays them out.
        let p = Policy {
            resiliency: Resiliency::Parity,
            redundancy: 2,
            copies: 1,
            groups: 1,
            columns: 7,
            interleave: 0x10000,
        };
        let l = Layout::new(&p, &[]).unwrap();
        assert_eq!(l.data_columns, 5);
        assert_eq!(
            (0..8).map(|s| l.parity_column(s)).collect::<Vec<_>>(),
            [5, 3, 1, 6, 4, 2, 0, 5]
        );
        // Stripe 1: P in 3, Q in 4, data units 5..9 in columns 5, 6, 0, 1, 2.
        let cols: Vec<u64> = (5..10).map(|u| l.locate(u * 0x10000).column).collect();
        assert_eq!(cols, [5, 6, 0, 1, 2]);
    }

    #[test]
    fn rejects_implausible_values() {
        let lrc = Policy {
            resiliency: Resiliency::Parity,
            redundancy: 2,
            copies: 1,
            groups: u64::MAX,
            columns: 12,
            interleave: 0x10000,
        };
        assert!(Layout::new(&lrc, &[]).is_err());
        let e = ExtentRecord {
            flags: 0,
            stale_marker: 0xffff_ffff,
            space_id: 1,
            virtual_slab: 0,
            column: 0,
            copy: 0,
            slab_count: u64::MAX,
            disk_id: 1,
            physical_slab: 0,
        };
        assert!(Layout::new(&policy(1, 0x10000), &[e]).is_err());
    }

    #[test]
    fn local_reconstruction_code() {
        // Twelve columns in two groups, as measured on a Windows-created
        // space: nine data units, the local parities of units 0-4 and 5-8,
        // then the global parity; three parity units rotating by three.
        let p = Policy {
            resiliency: Resiliency::Parity,
            redundancy: 2,
            copies: 1,
            groups: 2,
            columns: 12,
            interleave: 0x10000,
        };
        let l = Layout::new(&p, &[]).unwrap();
        assert_eq!((l.data_columns, l.parity_units), (9, 3));
        assert_eq!((0..5).map(|s| l.parity_column(s)).collect::<Vec<_>>(), [9, 6, 3, 0, 9]);
        let code: Vec<Vec<u8>> = l.parity_code().into_iter().map(Option::unwrap).collect();
        assert_eq!(
            code,
            [
                vec![1, 1, 1, 1, 1, 0, 0, 0, 0],
                vec![0, 0, 0, 0, 0, 1, 1, 1, 1],
                vec![1, 2, 3, 4, 5, 1, 2, 3, 4],
            ]
        );
        // Seventeen columns: three groups of 13 data units.
        let l = Layout::new(
            &Policy {
                groups: 3,
                columns: 17,
                ..p
            },
            &[],
        )
        .unwrap();
        assert_eq!((l.data_columns, l.parity_units), (13, 4));
        assert_eq!(
            l.parity_code()[3].as_deref(),
            Some(&[1, 2, 3, 4, 5, 1, 2, 3, 4, 1, 2, 3, 4][..])
        );
    }

    #[test]
    fn finds_runs() {
        let e = |virtual_slab, slab_count, physical_slab| ExtentRecord {
            flags: 0,
            stale_marker: 0xffff_ffff,
            space_id: 5,
            virtual_slab,
            column: 0,
            copy: 0,
            slab_count,
            disk_id: 1,
            physical_slab,
        };
        let l = Layout::new(&policy(1, 0x40000), &[e(0, 2, 10), e(5, 1, 3)]).unwrap();
        assert_eq!(l.physical(0, 0, 1), Some((1, 11)));
        assert_eq!(l.physical(0, 0, 2), None);
        assert_eq!(l.physical(0, 0, 5), Some((1, 3)));
    }
}
