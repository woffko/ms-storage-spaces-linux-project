//! Describing a space as linear/striped segments over member devices.
//!
//! This is the form device-mapper tables (`linear`, `striped`, `zero`) need,
//! which lets the kernel serve a space without a userspace daemon. Only
//! simple and mirror spaces whose write-back cache holds no data can be
//! described this way; mirrors are mapped to one present copy per row.

use crate::error::{Error, Result, format_err};
use crate::format::{Resiliency, SLAB_SIZE};
use crate::io::ReadAt;
use crate::reader::SpaceReader;

/// One column of a striped segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stripe {
    /// Index into the device list the pool was opened with.
    pub device: usize,
    /// Byte offset on that device.
    pub offset: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SegmentKind {
    /// Unallocated range that reads as zeros.
    Zero,
    /// Data striped over the columns in `chunk`-byte units (one stripe = linear).
    Striped { chunk: u64, stripes: Vec<Stripe> },
}

/// A byte range of the space.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub start: u64,
    pub length: u64,
    pub kind: SegmentKind,
}

impl<D: ReadAt> SpaceReader<'_, D> {
    /// Describes the space as segments, or explains why it cannot be.
    pub fn segments(&self) -> Result<Vec<Segment>> {
        let l = self.layout();
        if !matches!(l.resiliency, Resiliency::Simple | Resiliency::Mirror) {
            return Err(Error::Unsupported(format!(
                "{:?} spaces have no linear mapping",
                l.resiliency
            )));
        }
        if self.cache().is_some_and(|c| c.cached_chunks() > 0) {
            return Err(Error::Unsupported("the space has data in its write-back cache".into()));
        }
        let row_size = SLAB_SIZE * l.columns;
        if self.size() % (l.interleave * l.columns) != 0 {
            return Err(format_err!(
                "space size {:#x} is not a whole number of stripes",
                self.size()
            ));
        }
        let rows = self.size().div_ceil(row_size);
        let mut out: Vec<Segment> = Vec::new();
        for row in 0..rows {
            let start = row * row_size;
            let length = row_size.min(self.size() - start);
            let mut stripes = Vec::with_capacity(l.columns as usize);
            let mut allocated = 0;
            for column in 0..l.columns {
                let mut found = None;
                for copy in 0..l.copies {
                    if let Some((disk, slab)) = l.physical(column, copy, row) {
                        allocated += 1;
                        if let Some(location) = self.pool().slab_location(disk, slab)? {
                            found = Some(location);
                            break;
                        }
                    }
                }
                if let Some((device, offset)) = found {
                    stripes.push(Stripe { device, offset });
                }
            }
            let kind = if allocated == 0 {
                SegmentKind::Zero
            } else if stripes.len() as u64 == l.columns {
                SegmentKind::Striped {
                    chunk: l.interleave,
                    stripes,
                }
            } else if allocated < l.columns {
                return Err(format_err!("row {row} is only partly allocated"));
            } else {
                return Err(Error::Pool(format!("row {row} has a column without a present copy")));
            };
            if let Some(prev) = out.last_mut()
                && continues(prev, &kind)
            {
                prev.length += length;
                continue;
            }
            out.push(Segment { start, length, kind });
        }
        Ok(out)
    }
}

/// Whether `next` (one row) directly continues `prev` on the same devices.
fn continues(prev: &Segment, next: &SegmentKind) -> bool {
    match (&prev.kind, next) {
        (SegmentKind::Zero, SegmentKind::Zero) => true,
        (SegmentKind::Striped { chunk: a, stripes: p }, SegmentKind::Striped { chunk: b, stripes: n }) => {
            let per_column = prev.length / p.len() as u64;
            a == b
                && p.len() == n.len()
                && p.iter()
                    .zip(n)
                    .all(|(p, n)| p.device == n.device && p.offset + per_column == n.offset)
        }
        _ => false,
    }
}
