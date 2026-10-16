//! Reading the contents of a space.

use std::io::{self, Read, Seek, SeekFrom};

use crate::cache::{CacheHeader, CacheIndex, Lookup, SlotSource};
use crate::crc::crc32_excluding;
use crate::drt::{DirtyRegions, SPACEDRT_SIGNATURE};
use crate::error::{Error, Result, format_err};
use crate::format::{Resiliency, SLAB_SIZE, SpaceRole};
use crate::gf16;
use crate::io::ReadAt;
use crate::journal::{ParityJournal, SPVDT_SIGNATURE};
use crate::layout::{Condition, Layout, Location};
use crate::limits::MAX_SPACE_SIZE;
use crate::pool::{Pool, Space};
use crate::report::{Status, signature};
use std::sync::Arc;

/// What to do with a parity stripe whose data does not match its parity
/// after an unclean shutdown (the parity journal marks such stripes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UncleanParity {
    /// Fail the read: Windows may resolve the stripe either way.
    #[default]
    Refuse,
    /// Return the data columns as they are on disk.
    PreferData,
}

/// Options for opening a space.
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenOptions {
    pub unclean_parity: UncleanParity,
}

/// The part of a space that could not be opened ([`SpaceReader::open_parts`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    /// The space's own slab map.
    Layout,
    /// The slab map of a storage tier.
    Tiers,
    Cache,
    Journal,
    DirtyRegions,
}

/// What one copy of the header of a hidden structure held.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeaderCopy {
    Valid,
    /// Every byte zero.
    Zero,
    /// Something that is not a valid header: why.
    Damaged(String),
}

/// The copies of a header of a hidden structure of a space (the write-back
/// cache, the parity journal, a header page of the dirty region log), when
/// they are not all valid and alike. Windows writes each on every copy of
/// its mirror; the space is read through a valid one where there is one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderNote {
    pub part: Part,
    /// Degraded: a copy damaged, another valid; suspect: copies that
    /// disagree, or no valid copy of a structure the pool lists; info:
    /// copies of a dirty region log page that differ (one reached later).
    pub status: Status,
    pub summary: String,
    /// Byte of the hidden space where the header is.
    pub offset: u64,
    /// Every copy at hand: its number, where it is, what it held.
    pub copies: Vec<(u64, Option<Place>, HeaderCopy)>,
}

/// Where a byte of a copy is: (disk, physical slab, byte in the slab).
pub type Place = (u64, u64, u64);

/// What checking the stripes a parity journal does not record as
/// consistent found ([`SpaceReader::check_unclean_parity`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParityCheck {
    /// Stripes in allocated rows the journal does not record as consistent
    /// (never written, or written when the space was last in use).
    pub listed: u64,
    /// Of those, stripes whose units were read and compared.
    pub checked: u64,
    /// Stripes the write-back cache holds whole (their data is read from
    /// the cache).
    pub cached: u64,
    /// Stripes with a unit on a missing disk (nothing to compare).
    pub unreadable: u64,
    /// Bytes read.
    pub bytes: u64,
    /// Space offsets of the stripes whose parity does not match their data.
    pub mismatches: Vec<u64>,
    /// The budget ran out before every listed stripe was checked.
    pub incomplete: bool,
}

/// A space layout bound to the pool it reads from.
struct Mapped<'p, D> {
    pool: &'p Pool<D>,
    layout: Layout,
    journal: Option<Arc<ParityJournal>>,
    /// Dirty region tracking of mirror spaces.
    drt: Option<Arc<DirtyRegions>>,
    unclean: UncleanParity,
}

impl<D: ReadAt> Mapped<'_, D> {
    fn new<'p>(pool: &'p Pool<D>, space: &Space) -> Result<Mapped<'p, D>> {
        let policy = space.info.policy.ok_or_else(|| {
            Error::Unsupported(format!(
                "space {}: placement policy not found in a type {} record of layout version {}",
                space.id(),
                if space.info.is_child { 6 } else { 3 },
                space.info.record_version
            ))
        })?;
        let base = space.info.range.map_or(0, |(start, _)| start);
        let layout = Layout::with_base(&policy, &space.extents, base)?;
        if let Some(problem) = pool.slab_map_problem(space.id()) {
            return Err(Error::Format(problem));
        }
        // The extents lie inside the space (a tier: inside its range),
        // whole rows of it.
        let bounds = match (space.info.range, space.info.size) {
            (Some(range), _) => Some(range),
            (None, Some(size)) => Some((0, size)),
            (None, None) => None,
        };
        if let Some((start, len)) = bounds {
            if start > MAX_SPACE_SIZE || len > MAX_SPACE_SIZE {
                return Err(format_err!(
                    "space {} of {len:#x} bytes from {start:#x}: beyond 1 EiB",
                    space.id()
                ));
            }
            let row = SLAB_SIZE * layout.data_columns;
            let end = len.checked_next_multiple_of(row).and_then(|l| l.checked_add(start));
            for e in &space.extents {
                let first = e.virtual_slab.checked_mul(SLAB_SIZE);
                let last = e
                    .slab_count
                    .checked_mul(layout.data_columns)
                    .and_then(|n| n.checked_add(e.virtual_slab))
                    .and_then(|n| n.checked_mul(SLAB_SIZE));
                let inside = matches!((first, last, end), (Some(f), Some(l), Some(end)) if f >= start && l <= end);
                if !inside {
                    return Err(format_err!(
                        "the extent of space {} at virtual slab {} ({} rows) lies outside the space ({start:#x} and {len:#x} bytes)",
                        space.id(),
                        e.virtual_slab,
                        e.slab_count
                    ));
                }
            }
        }
        Ok(Mapped {
            pool,
            layout,
            journal: None,
            drt: None,
            unclean: UncleanParity::default(),
        })
    }

    /// Reads from `offset` up to the next interleave boundary. Returns the
    /// length covered and whether it is allocated; unallocated ranges leave
    /// `buf` untouched.
    fn read_some(&self, offset: u64, buf: &mut [u8]) -> Result<(usize, bool)> {
        let l = &self.layout;
        if offset < l.base {
            return Ok((buf.len().min((l.base - offset) as usize), false));
        }
        let loc = l.locate(offset);
        let n = buf.len().min(loc.contiguous as usize);
        let buf = &mut buf[..n];
        if let Some(journal) = &self.journal
            && l.resiliency == Resiliency::Parity
            && self.unclean == UncleanParity::Refuse
            && journal.is_dirty(l.run_start_offset(loc.row), l.stripe_of(&loc))
            && !self.stripe_consistent(&loc, n)?
        {
            return Err(Error::Pool(format!(
                "parity stripe at {offset:#x} does not match its data after an unclean shutdown"
            )));
        }
        if l.resiliency == Resiliency::Mirror
            && let Some(drt) = &self.drt
            && drt.is_dirty(l.run_start_offset(loc.row) / SLAB_SIZE)
            && self.read_unclean_mirror(offset, &loc, buf)?
        {
            return Ok((n, true));
        }
        let mut allocated = false;
        let mut last_error = None;
        for copy in 0..l.copies {
            if let Some((disk, slab)) = l.physical(loc.column, copy, loc.row) {
                allocated = true;
                // A read error (a disk that went away) falls through to the
                // next copy or to parity reconstruction.
                match self.pool.read_slab(disk, slab, loc.offset_in_slab, buf) {
                    Ok(true) => return Ok((n, true)),
                    Ok(false) => {}
                    Err(Error::Io(e)) => last_error = Some(e),
                    Err(e) => return Err(e),
                }
            }
        }
        if allocated && l.resiliency == Resiliency::Parity {
            self.reconstruct(&loc, buf)?;
            return Ok((n, true));
        }
        if let Some(e) = last_error {
            return Err(Error::Io(e));
        }
        if !allocated && l.has_stale_copy(loc.column, loc.row) {
            return Err(Error::Pool(format!(
                "column {} row {} has only an out-of-date copy (its disk missed writes)",
                loc.column, loc.row
            )));
        }
        if allocated {
            return Err(Error::Pool(format!(
                "no copy of column {} row {} is on a present disk",
                loc.column, loc.row
            )));
        }
        Ok((n, false)) // not allocated (thin provisioning, or another tier)
    }

    /// Reads a mirror row of an extent run the dirty region log lists (one
    /// written since the space was last disconnected): every present copy is
    /// read, and copies that differ are refused, or with
    /// [`UncleanParity::PreferData`] the highest copy is returned. Windows
    /// itself serves either copy and never reconciles them, so differing
    /// copies have no right answer. Returns false when fewer than two copies
    /// can be read.
    fn read_unclean_mirror(&self, offset: u64, loc: &Location, buf: &mut [u8]) -> Result<bool> {
        let l = &self.layout;
        let mut read = 0;
        let mut differ = false;
        let mut other = Vec::new();
        for copy in (0..l.copies).rev() {
            let Some((disk, slab)) = l.physical(loc.column, copy, loc.row) else {
                continue;
            };
            // The highest copy goes straight into `buf`, the others are
            // compared with it.
            let target = if read == 0 {
                &mut *buf
            } else {
                other.resize(buf.len(), 0);
                &mut other[..]
            };
            match self.pool.read_slab(disk, slab, loc.offset_in_slab, target) {
                Ok(true) => {
                    differ |= read > 0 && other[..] != buf[..];
                    read += 1;
                }
                Ok(false) | Err(Error::Io(_)) => {}
                Err(e) => return Err(e),
            }
        }
        if read < 2 {
            return Ok(false);
        }
        if differ && self.unclean == UncleanParity::Refuse {
            return Err(Error::Pool(format!(
                "mirror copies differ at {offset:#x} (writes in flight at a crash, or a disk that missed writes and was not repaired)"
            )));
        }
        Ok(true)
    }

    /// Whether the XOR parity of the stripe holding `loc` matches its data
    /// over `len` bytes: the XOR of all units but the last parity unit of a
    /// dual parity stripe is zero (P, or the local parities of all groups,
    /// sum up to the XOR of the data). Missing columns count as consistent
    /// (nothing to check).
    fn stripe_consistent(&self, loc: &Location, len: usize) -> Result<bool> {
        Ok(self.stripe_check(loc, len)?.unwrap_or(true))
    }

    /// [`Mapped::stripe_consistent`], or `None` when a unit is not
    /// allocated or cannot be read.
    fn stripe_check(&self, loc: &Location, len: usize) -> Result<Option<bool>> {
        let l = &self.layout;
        let p = l.parity_column(l.stripe_of(loc));
        let last = (l.parity_units >= 2).then(|| (p + l.parity_units - 1) % l.columns);
        let mut acc = vec![0u8; len];
        let mut unit = vec![0u8; len];
        for column in (0..l.columns).filter(|&c| Some(c) != last) {
            let Some((disk, slab)) = l.physical(column, 0, loc.row) else {
                return Ok(None);
            };
            match self.pool.read_slab(disk, slab, loc.offset_in_slab, &mut unit) {
                Ok(true) => acc.iter_mut().zip(&unit).for_each(|(a, u)| *a ^= u),
                Ok(false) | Err(Error::Io(_)) => return Ok(None),
                Err(e) => return Err(e),
            }
        }
        Ok(Some(acc.iter().all(|&b| b == 0)))
    }

    /// Where owner offset `offset` lies: the column and row, and for every
    /// copy the physical slab with the device (named by `names`) and the
    /// byte offset on it.
    fn describe(&self, offset: u64, names: &[String]) -> String {
        let l = &self.layout;
        if offset < l.base {
            return "outside this layout".into();
        }
        let loc = l.locate(offset);
        let copies: Vec<String> = l
            .copies_of(loc.column)
            .into_iter()
            .filter_map(|copy| l.physical(loc.column, copy, loc.row).map(|p| (copy, p)))
            .map(|(copy, (disk, slab))| {
                let at = match self.pool.slab_location(disk, slab) {
                    Ok(Some((device, start))) => format!(
                        "{} at {:#x}",
                        names.get(device).cloned().unwrap_or_else(|| format!("device {device}")),
                        start + loc.offset_in_slab
                    ),
                    Ok(None) => "a missing disk".into(),
                    Err(e) => e.to_string(),
                };
                let copy = if l.copies > 1 {
                    format!("copy {copy}: ")
                } else {
                    String::new()
                };
                format!("{copy}disk {disk} slab {slab} -> {at}")
            })
            .collect();
        if copies.is_empty() {
            return format!("column {} row {}: not allocated", loc.column, loc.row);
        }
        format!("column {} row {}: {}", loc.column, loc.row, copies.join(", "))
    }

    /// Reads `len` bytes of a column at `offset_in_slab` of `row`; `None`
    /// when its disk is missing or fails.
    fn read_column(&self, column: u64, row: u64, offset_in_slab: u64, len: usize) -> Result<Option<Vec<u8>>> {
        let Some((disk, slab)) = self.layout.physical(column, 0, row) else {
            return Err(format_err!("parity stripe with an unallocated column {column}"));
        };
        let mut v = vec![0u8; len];
        match self.pool.read_slab(disk, slab, offset_in_slab, &mut v) {
            Ok(true) => Ok(Some(v)),
            Ok(false) | Err(Error::Io(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Rebuilds a unit of a missing data column from the parity units and
    /// the other data units of its stripe (see [`Layout::parity_code`]).
    /// Parity equations are taken in column order and solved by Gaussian
    /// elimination over GF(16) until the lost unit is determined, so a single
    /// loss reads one parity unit and the units it covers.
    fn reconstruct(&self, loc: &Location, buf: &mut [u8]) -> Result<()> {
        let l = &self.layout;
        let lost = || Error::Pool(format!("row {} lost more columns than its parity covers", loc.row));
        let p = l.parity_column(l.stripe_of(loc));
        let data: Vec<u64> = (0..l.data_columns)
            .map(|i| (p + l.parity_units + i) % l.columns)
            .collect();
        let target = data.iter().position(|&c| c == loc.column).ok_or_else(lost)?;

        // Work on whole 512-byte chunks of the unit (the GF(16) code's granularity).
        let within = loc.offset_in_slab % l.interleave;
        let unit_start = loc.offset_in_slab - within;
        let from = within / gf16::CHUNK as u64 * gf16::CHUNK as u64;
        let to = (within + buf.len() as u64).div_ceil(gf16::CHUNK as u64) * gf16::CHUNK as u64;
        let (at, len) = (unit_start + from, (to.min(l.interleave) - from) as usize);

        // Data units read so far: Some(Some(bytes)) present, Some(None) lost.
        let mut units: Vec<Option<Option<Vec<u8>>>> = vec![None; data.len()];
        units[target] = Some(None);
        // Unknown data units, and the reduced equations over them: each row
        // is (coefficients per unknown, right-hand side), in reduced row
        // echelon form; `pivots[i]` is the unknown row `i` solves for.
        let mut unknowns = vec![target];
        let mut rows: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        let mut pivots: Vec<usize> = Vec::new();
        let mut unknown_code = false;
        for (i, code) in l.parity_code().iter().enumerate() {
            let Some(code) = code else {
                unknown_code = true;
                continue;
            };
            if !unknowns.iter().any(|&k| code[k] != 0) {
                continue;
            }
            let Some(mut rhs) = self.read_column((p + i as u64) % l.columns, loc.row, at, len)? else {
                continue;
            };
            for (k, &c) in data.iter().enumerate() {
                if code[k] == 0 {
                    continue;
                }
                if units[k].is_none() {
                    let u = self.read_column(c, loc.row, at, len)?;
                    if u.is_none() {
                        unknowns.push(k);
                    }
                    units[k] = Some(u);
                }
                if let Some(Some(u)) = &units[k] {
                    gf16::mul_region_xor(code[k], u, &mut rhs);
                }
            }
            let mut coef: Vec<u8> = unknowns.iter().map(|&k| code[k]).collect();
            for (j, (row, row_rhs)) in rows.iter().enumerate() {
                let f = coef[pivots[j]];
                if f != 0 {
                    coef.resize(unknowns.len(), 0);
                    coef.iter_mut().zip(row).for_each(|(c, r)| *c ^= gf16::mul(f, *r));
                    gf16::mul_region_xor(f, row_rhs, &mut rhs);
                }
            }
            let Some(pivot) = coef.iter().position(|&c| c != 0) else {
                continue; // adds nothing
            };
            // Normalize the new row and remove its unknown from the others.
            let inv = gf16::inv(coef[pivot]).ok_or_else(lost)?;
            let coef: Vec<u8> = coef.iter().map(|&c| gf16::mul(inv, c)).collect();
            let mut scaled = vec![0u8; len];
            gf16::mul_region_xor(inv, &rhs, &mut scaled);
            for (row, row_rhs) in &mut rows {
                row.resize(unknowns.len(), 0);
                let f = row[pivot];
                if f != 0 {
                    row.iter_mut().zip(&coef).for_each(|(r, c)| *r ^= gf16::mul(f, *c));
                    gf16::mul_region_xor(f, &scaled, row_rhs);
                }
            }
            rows.push((coef, scaled));
            pivots.push(pivot);
            // Solved once a row holds the target alone.
            if let Some((_, rhs)) = rows
                .iter()
                .find(|(row, _)| row[0] != 0 && row.iter().skip(1).all(|&c| c == 0))
            {
                let skip = (within - from) as usize;
                buf.copy_from_slice(&rhs[skip..skip + buf.len()]);
                return Ok(());
            }
        }
        if unknown_code {
            return Err(Error::Unsupported("dual parity Q for this column count".into()));
        }
        Err(lost())
    }

    fn read_exact(&self, mut offset: u64, mut buf: &mut [u8]) -> Result<()> {
        let beyond = || format_err!("a read of {} bytes at {offset:#x} reaches beyond any space", buf.len());
        offset.checked_add(buf.len() as u64).ok_or_else(beyond)?;
        while !buf.is_empty() {
            let (n, allocated) = self.read_some(offset, buf)?;
            if !allocated {
                buf[..n].fill(0);
            }
            offset += n as u64;
            buf = &mut buf[n..];
        }
        Ok(())
    }

    /// Reads one copy of a simple or mirror layout; false when that copy is
    /// not complete on the disks at hand.
    fn read_copy(&self, copy: u64, mut offset: u64, mut buf: &mut [u8]) -> Result<bool> {
        let l = &self.layout;
        if offset.checked_add(buf.len() as u64).is_none() {
            return Err(format_err!(
                "a read of {} bytes at {offset:#x} reaches beyond any space",
                buf.len()
            ));
        }
        while !buf.is_empty() {
            let loc = l.locate(offset);
            let n = buf.len().min(loc.contiguous as usize);
            let Some((disk, slab)) = l.physical(loc.column, copy, loc.row) else {
                return Ok(false);
            };
            match self.pool.read_slab(disk, slab, loc.offset_in_slab, &mut buf[..n]) {
                Ok(true) => {}
                Ok(false) | Err(Error::Io(_)) => return Ok(false),
                Err(e) => return Err(e),
            }
            offset += n as u64;
            buf = &mut buf[n..];
        }
        Ok(true)
    }

    /// Reads a slot area (write-back cache or parity journal) from every
    /// copy that is complete on the disks at hand.
    fn read_slot_copies(&self, offset: u64, len: usize) -> Result<Vec<Vec<u8>>> {
        let mut copies = Vec::new();
        if self.layout.resiliency == Resiliency::Mirror {
            for copy in 0..self.layout.copies {
                let mut area = vec![0u8; len];
                if self.read_copy(copy, offset, &mut area)? {
                    copies.push(area);
                }
            }
        }
        if copies.is_empty() {
            let mut area = vec![0u8; len];
            self.read_exact(offset, &mut area)?;
            copies.push(area);
        }
        Ok(copies)
    }

    /// The `len` bytes at `offset` of every copy of a mirror complete on
    /// the disks at hand, with the copy's number (of another layout, what
    /// reading gives, as copy 0).
    fn copies_at(&self, offset: u64, len: usize) -> Result<Vec<(u64, Vec<u8>)>> {
        let l = &self.layout;
        let mut out = Vec::new();
        if l.resiliency == Resiliency::Mirror && offset >= l.base {
            for copy in l.copies_of(l.locate(offset).column) {
                let mut b = vec![0u8; len];
                if self.read_copy(copy, offset, &mut b)? {
                    out.push((copy, b));
                }
            }
        }
        if out.is_empty() {
            let mut b = vec![0u8; len];
            self.read_exact(offset, &mut b)?;
            out.push((0, b));
        }
        Ok(out)
    }

    /// Where copy `copy` of owner offset `offset` is: (disk, physical slab,
    /// byte in the slab).
    fn place(&self, copy: u64, offset: u64) -> Option<Place> {
        let l = &self.layout;
        if offset < l.base {
            return None;
        }
        let loc = l.locate(offset);
        l.physical(loc.column, copy, loc.row)
            .map(|(disk, slab)| (disk, slab, loc.offset_in_slab))
    }

    /// The copy of the header at `offset` (`len` bytes) of the hidden
    /// structure `part` to read, by `state` of each copy: the first valid
    /// one, with a note when not all copies are valid and `alike`; `None`
    /// with a note when every copy is zero (the pool lists the structure);
    /// an error when none is valid and some holds something else.
    fn header(
        &self,
        part: Part,
        offset: u64,
        len: usize,
        state: impl Fn(&[u8]) -> HeaderCopy,
        alike: impl Fn(&[u8], &[u8]) -> bool,
    ) -> Result<(Option<Vec<u8>>, Option<HeaderNote>)> {
        let what = part.name();
        let copies: Vec<(u64, Vec<u8>, HeaderCopy)> = self
            .copies_at(offset, len)?
            .into_iter()
            .map(|(c, b)| {
                let s = state(&b);
                (c, b, s)
            })
            .collect();
        let note = |status: Status, summary: String| HeaderNote {
            part,
            status,
            summary,
            offset,
            copies: copies
                .iter()
                .map(|(c, _, s)| (*c, self.place(*c, offset), s.clone()))
                .collect(),
        };
        let mut valid = copies.iter().filter(|c| c.2 == HeaderCopy::Valid);
        if let Some(first) = valid.next() {
            let bytes = first.1.clone();
            if valid.any(|c| !alike(&c.1, &bytes)) {
                let summary = format!("the copies of the header of the {what} disagree");
                return Ok((Some(bytes), Some(note(Status::Suspect, summary))));
            }
            let bad = copies.iter().filter(|c| c.2 != HeaderCopy::Valid).count();
            if bad > 0 {
                let summary = format!(
                    "{bad} of {} copies of the header of the {what} {}; another is read",
                    copies.len(),
                    if bad == 1 { "is damaged" } else { "are damaged" }
                );
                return Ok((Some(bytes), Some(note(Status::Degraded, summary))));
            }
            return Ok((Some(bytes), None));
        }
        if copies.iter().all(|c| c.2 == HeaderCopy::Zero) {
            // Windows lists a new space's structures before it writes
            // their headers (snapshots/m5ids): never written, as far as the
            // copies at hand tell. With a copy on a missing disk, that is
            // not known.
            let all = if self.layout.resiliency == Resiliency::Mirror && offset >= self.layout.base {
                self.layout.copies_of(self.layout.locate(offset).column).len()
            } else {
                1
            };
            if copies.len() < all {
                let summary = format!("the header of the {what} is zero on every copy at hand, and not every copy is");
                return Ok((None, Some(note(Status::Suspect, summary))));
            }
            let summary = format!("the {what} the pool lists was never written (its header is zero on every copy)");
            return Ok((None, Some(note(Status::Info, summary))));
        }
        let why = copies
            .iter()
            .find_map(|c| match &c.2 {
                HeaderCopy::Damaged(w) => Some(w.clone()),
                _ => None,
            })
            .unwrap_or_default();
        Err(format_err!(
            "the header of the {what} does not check out on any copy at hand: {why}"
        ))
    }
}

impl<D: ReadAt> Mapped<'_, D> {
    /// A cache or journal whose header is zero on every copy was never
    /// written, unless its slot area (where Windows puts it, 8 KiB in)
    /// holds a slot: then the header is lost, and with it what the
    /// structure holds.
    fn no_slots_without_header(&self, part: Part) -> Result<()> {
        for (copy, slot) in self.copies_at(0x2000, 8)? {
            if slot == *b"SPSLOT\0\0" {
                return Err(format_err!(
                    "the header of the {} is zero on every copy, but copy {copy} holds slots: the header is lost",
                    part.name()
                ));
            }
        }
        Ok(())
    }
}

impl Part {
    /// What the part is, in words.
    pub fn name(self) -> &'static str {
        match self {
            Part::Layout => "slab map",
            Part::Tiers => "storage tier",
            Part::Cache => "write-back cache",
            Part::Journal => "parity journal",
            Part::DirtyRegions => "dirty region log",
        }
    }
}

/// What a copy of a write-back cache's header holds.
fn cache_header_state(b: &[u8]) -> HeaderCopy {
    if b.iter().all(|&x| x == 0) {
        return HeaderCopy::Zero;
    }
    match CacheHeader::parse(b) {
        Ok(Some(_)) => HeaderCopy::Valid,
        Ok(None) => HeaderCopy::Damaged(format!("{} where {:?} belongs", signature(&b[..8]), "SPCACHE")),
        Err(e) => HeaderCopy::Damaged(e.to_string()),
    }
}

/// What a copy of the header of the parity journal of space `owner` holds.
fn journal_header_state(owner: crate::Guid, b: &[u8]) -> HeaderCopy {
    if b.iter().all(|&x| x == 0) {
        return HeaderCopy::Zero;
    }
    if &b[..8] != SPVDT_SIGNATURE {
        return HeaderCopy::Damaged(format!("{} where {:?} belongs", signature(&b[..8]), "SPVDT"));
    }
    if crc32_excluding(&b[..0x60], 0x24) != u32::from_le_bytes(b[0x24..0x28].try_into().unwrap()) {
        return HeaderCopy::Damaged("a checksum that does not match".into());
    }
    let of = crate::Guid::from_mixed_endian(b[8..24].try_into().unwrap());
    if of != owner {
        return HeaderCopy::Damaged(format!("the header of the journal of {of}"));
    }
    HeaderCopy::Valid
}

/// What a copy of a header page of a dirty region log holds.
fn drt_page_state(b: &[u8]) -> HeaderCopy {
    if b.iter().all(|&x| x == 0) {
        return HeaderCopy::Zero;
    }
    if &b[..8] != SPACEDRT_SIGNATURE {
        return HeaderCopy::Damaged(format!("{} where {:?} belongs", signature(&b[..8]), "SPACEDRT"));
    }
    match DirtyRegions::parse(b) {
        Some(_) => HeaderCopy::Valid,
        None => HeaderCopy::Damaged("a checksum that does not match, or too many entries".into()),
    }
}

/// Whether two valid headers of a cache or journal agree: all but the
/// checksum and the sequence (0x24..0x30).
fn same_log_header(a: &[u8], b: &[u8]) -> bool {
    a[..0x24] == b[..0x24] && a[0x30..0x60] == b[0x30..0x60]
}

/// Reads a cache or journal space, merging the slot areas of its copies;
/// its header (at byte 0) as the copy chosen ([`Mapped::header`]).
struct Slots<'a, 'p, D>(&'a Mapped<'p, D>, Vec<u8>);

impl<D: ReadAt> SlotSource for Slots<'_, '_, D> {
    fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.0.read_exact(offset, buf)?;
        // What of the header the read covers comes from the copy chosen.
        let header = &self.1;
        if let Ok(at) = usize::try_from(offset)
            && at < header.len()
        {
            let n = (header.len() - at).min(buf.len());
            buf[..n].copy_from_slice(&header[at..at + n]);
        }
        Ok(())
    }

    fn read_slot_copies(&mut self, offset: u64, len: usize) -> Result<Vec<Vec<u8>>> {
        self.0.read_slot_copies(offset, len)
    }
}

/// Random-access reader for a space, including its write-back cache.
pub struct SpaceReader<'p, D> {
    pub space: &'p Space,
    size: u64,
    base: Mapped<'p, D>,
    /// Storage tiers: child spaces sharing the space's virtual slab numbers.
    tiers: Vec<Mapped<'p, D>>,
    cache: Option<(Mapped<'p, D>, CacheIndex)>,
    /// The headers of hidden structures whose copies are not all valid.
    notes: Vec<HeaderNote>,
}

impl<'p, D: ReadAt> SpaceReader<'p, D> {
    pub(crate) fn new(pool: &'p Pool<D>, id: u64, options: OpenOptions) -> Result<Self> {
        Self::open_parts(pool, id, options).map_err(|(_, e)| e)
    }

    /// Opens a space as [`Pool::open_space_with`] does; an error names the
    /// part of the space that could not be opened.
    pub fn open_parts(pool: &'p Pool<D>, id: u64, options: OpenOptions) -> std::result::Result<Self, (Part, Error)> {
        let space = pool
            .spaces
            .get(&id)
            .ok_or_else(|| (Part::Layout, Error::Pool(format!("no space with id {id}"))))?;
        let mut base = Mapped::new(pool, space).map_err(|e| (Part::Layout, e))?;
        let size = match space.info.size {
            Some(size) => size,
            None => base.layout.mapped_size(),
        };
        // The hidden spaces under it, looked up once (a pool can have
        // tens of thousands).
        let children: Vec<&'p Space> = pool.children(space.id()).collect();
        let mut notes = Vec::new();
        let cache = Self::open_cache(pool, space, &children, &mut notes).map_err(|e| (Part::Cache, e))?;
        let mut tiers = children
            .iter()
            .filter(|c| c.info.is_child && !c.extents.is_empty())
            .map(|c| Mapped::new(pool, c))
            .collect::<Result<Vec<_>>>()
            .map_err(|e| (Part::Tiers, e))?;
        let journal = Self::open_journal(pool, space, &children, &mut notes)
            .map_err(|e| (Part::Journal, e))?
            .map(Arc::new);
        let drt = Self::open_drt(pool, &children, &mut notes)
            .map_err(|e| (Part::DirtyRegions, e))?
            .map(Arc::new);
        // What the logs name lies inside the space.
        if let Some((_, index)) = &cache {
            index.check_inside(size).map_err(|e| (Part::Cache, e))?;
        }
        if let Some(j) = &journal {
            j.check_inside(size).map_err(|e| (Part::Journal, e))?;
        }
        if let Some(d) = &drt {
            d.check_inside(size).map_err(|e| (Part::DirtyRegions, e))?;
        }
        for m in std::iter::once(&mut base).chain(tiers.iter_mut()) {
            m.journal = journal.clone();
            m.drt = drt.clone();
            m.unclean = options.unclean_parity;
        }
        Ok(SpaceReader {
            space,
            size,
            base,
            tiers,
            cache,
            notes,
        })
    }

    fn open_cache(
        pool: &'p Pool<D>,
        space: &Space,
        children: &[&'p Space],
        notes: &mut Vec<HeaderNote>,
    ) -> Result<Option<(Mapped<'p, D>, CacheIndex)>> {
        let mut found = None;
        for container in children.iter().filter(|c| c.info.role == SpaceRole::Cache) {
            for child in pool.children(container.id()).filter(|c| !c.extents.is_empty()) {
                if found.is_some() {
                    return Err(Error::Unsupported(format!("space {} has several caches", space.id())));
                }
                found = Some(child);
            }
        }
        let Some(child) = found else { return Ok(None) };
        let mapped = Mapped::new(pool, child)?;
        // The header from a valid copy (a damaged one made the cache
        // vanish, and data only it held with it).
        let (head, note) = mapped.header(Part::Cache, 0, CacheHeader::SIZE, cache_header_state, same_log_header)?;
        notes.extend(note);
        let Some(head) = head else {
            mapped.no_slots_without_header(Part::Cache)?;
            return Ok(None);
        };
        let header = CacheHeader::parse(&head)?.ok_or_else(|| format_err!("cache header vanished"))?;
        if header.owner_guid != space.info.guid {
            return Err(format_err!(
                "cache of space {} belongs to {}",
                space.id(),
                header.owner_guid
            ));
        }
        let index = CacheIndex::load(header, pool.logical_sector_size, Slots(&mapped, head))?;
        Ok(Some((mapped, index)))
    }

    pub fn pool(&self) -> &'p Pool<D> {
        self.base.pool
    }

    /// Loads the parity journal of a space, if it has one.
    fn open_journal(
        pool: &'p Pool<D>,
        space: &Space,
        children: &[&'p Space],
        notes: &mut Vec<HeaderNote>,
    ) -> Result<Option<ParityJournal>> {
        for container in children.iter().filter(|c| c.info.role == SpaceRole::Other(0x0a)) {
            if let Some(child) = pool.children(container.id()).find(|c| !c.extents.is_empty()) {
                let mapped = Mapped::new(pool, child)?;
                let owner = space.info.guid;
                let state = |b: &[u8]| journal_header_state(owner, b);
                let (head, note) = mapped.header(Part::Journal, 0, 0x60, state, same_log_header)?;
                notes.extend(note);
                let Some(head) = head else {
                    mapped.no_slots_without_header(Part::Journal)?;
                    return Ok(None);
                };
                return ParityJournal::load(owner, Slots(&mapped, head));
            }
        }
        Ok(None)
    }

    fn open_drt(
        pool: &'p Pool<D>,
        children: &[&'p Space],
        notes: &mut Vec<HeaderNote>,
    ) -> Result<Option<DirtyRegions>> {
        for container in children.iter().filter(|c| c.info.role == SpaceRole::Other(0x06)) {
            if let Some(child) = pool.children(container.id()).find(|c| !c.extents.is_empty()) {
                // Read through the child's own layout: opening it as a
                // SpaceReader would look for tracking of its own, which
                // corrupt metadata can make circular.
                let mapped = Mapped::new(pool, child)?;
                let base = mapped.layout.base;
                let size = child
                    .info
                    .range
                    .map_or_else(|| mapped.layout.mapped_size(), |(_, len)| len);
                return Self::drt_pages(&mapped, base, size, notes);
            }
        }
        Ok(None)
    }

    /// The dirty region log from its two header pages (at the start and 8
    /// KiB before the end of its space, the newer generation counting),
    /// each from a valid copy of the mirror.
    fn drt_pages(
        mapped: &Mapped<'p, D>,
        base: u64,
        size: u64,
        notes: &mut Vec<HeaderNote>,
    ) -> Result<Option<DirtyRegions>> {
        const PAGE: usize = 0x1000;
        let generation = |b: &[u8]| DirtyRegions::parse(b).map_or(0, |h| h.generation);
        let mut pages = Vec::new();
        let mut failed = Vec::new();
        let mut zero_note = None;
        for at in [0, size.saturating_sub(0x2000)] {
            // Copies of a page may differ after a crash (a write reached
            // one): the newest valid one counts, as among the two pages.
            let alike = |a: &[u8], b: &[u8]| a == b;
            match mapped.header(Part::DirtyRegions, base + at, PAGE, drt_page_state, alike) {
                Ok((page, mut note)) => {
                    if let Some(n) = note.as_mut().filter(|n| n.status == Status::Suspect && page.is_some()) {
                        n.status = Status::Info;
                        n.summary =
                            "the copies of a header page of the dirty region log differ; the newest is read".into();
                    }
                    let page = page.map(|first| {
                        mapped
                            .copies_at(base + at, PAGE)
                            .ok()
                            .and_then(|copies| {
                                copies
                                    .into_iter()
                                    .map(|(_, b)| b)
                                    .filter(|b| drt_page_state(b) == HeaderCopy::Valid)
                                    .max_by_key(|b| generation(b))
                            })
                            .unwrap_or(first)
                    });
                    if page.is_none() {
                        // Zero on every copy: said once below, for both.
                        failed.push((at, None));
                        zero_note = zero_note.or(note);
                    } else {
                        notes.extend(note);
                    }
                    pages.push((at, page));
                }
                Err(e) => {
                    failed.push((at, Some(e.to_string())));
                    pages.push((at, None));
                }
            }
        }
        match failed.len() {
            0 => {}
            // One page lost on every copy: the other is read, and runs only
            // the lost one listed would not be compared.
            1 => notes.push(HeaderNote {
                part: Part::DirtyRegions,
                status: Status::Degraded,
                summary: "one of the two header pages of the dirty region log does not check out on any copy; \
                          the other is read"
                    .into(),
                offset: base + failed[0].0,
                copies: Vec::new(),
            }),
            // Both zero: as the header of a cache or journal that is zero
            // on every copy (never written, or not known with a copy away).
            _ if failed.iter().all(|f| f.1.is_none()) => {
                notes.extend(zero_note);
                return Ok(None);
            }
            _ => {
                let why = failed.iter().find_map(|f| f.1.clone()).unwrap_or_default();
                return Err(format_err!(
                    "neither header page of the dirty region log checks out: {why}"
                ));
            }
        }
        let read = |off: u64, buf: &mut [u8]| -> Result<()> {
            match pages.iter().find(|(at, _)| *at == off) {
                Some((_, Some(page))) => buf.copy_from_slice(&page[..buf.len()]),
                // A page without a valid copy reads as nothing.
                Some((_, None)) => buf.fill(0),
                None => mapped.read_exact(base + off, buf)?,
            }
            Ok(())
        };
        DirtyRegions::load(size, read)
    }

    /// Number of mirror extent runs the dirty region log lists: those written
    /// since the space was last disconnected (a restart does not clear them).
    /// Their copies are compared on read.
    pub fn listed_mirror_runs(&self) -> usize {
        self.base.drt.as_ref().map_or(0, |d| d.dirty_runs())
    }

    /// Number of extent runs whose parity journal marks stripes that may be
    /// inconsistent (0 after a clean shutdown).
    pub fn unclean_parity_runs(&self) -> usize {
        self.base.journal.as_ref().map_or(0, |j| j.dirty_runs())
    }

    /// Size of the space in bytes.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Redundancy state of the space (its tiers and cache included) with
    /// the disks at hand; see [`Condition`].
    pub fn condition(&self) -> Condition {
        let pool = self.base.pool;
        let present = |disk: u64| pool.disks.get(&disk).is_some_and(|d| d.member.is_some());
        let mut all: Vec<&Mapped<'p, D>> = vec![&self.base];
        all.extend(&self.tiers);
        all.extend(self.cache.as_ref().map(|(m, _)| m));
        all.iter()
            .map(|m| m.layout.condition(present))
            .max()
            .unwrap_or(Condition::Healthy)
    }

    pub fn layout(&self) -> &Layout {
        &self.base.layout
    }

    pub fn cache(&self) -> Option<&CacheIndex> {
        self.cache.as_ref().map(|(_, index)| index)
    }

    /// Dirty region tracking of a mirror space.
    pub fn dirty_regions(&self) -> Option<&DirtyRegions> {
        self.base.drt.as_deref()
    }

    /// Parity journal of a parity space.
    pub fn journal(&self) -> Option<&ParityJournal> {
        self.base.journal.as_deref()
    }

    /// Checks the stripes the parity journal does not record as consistent
    /// (Windows lists stripes never written as well as those written when
    /// the space was last in use) against their parity, reading at most
    /// `budget` bytes (`None`: no limit). Stripes the write-back cache
    /// holds whole are not checked: their data is read from the cache.
    /// `None` for a space without a parity journal.
    pub fn check_unclean_parity(&self, budget: Option<u64>) -> Result<Option<ParityCheck>> {
        let Some(journal) = self.base.journal.as_deref() else {
            return Ok(None);
        };
        let mut out = ParityCheck::default();
        // Stripes looked at one by one, at most: the journal lists ranges
        // of up to 2^51 stripes (a run of 2^32 rows), which are counted
        // whole; the rest is left, as past the read budget.
        let mut visits: u64 = 1 << 26;
        let parity = std::iter::once(&self.base)
            .chain(&self.tiers)
            .filter(|m| m.layout.resiliency == Resiliency::Parity);
        for m in parity {
            let l = &m.layout;
            let width = l.data_columns * l.interleave;
            let per_row = SLAB_SIZE / l.interleave;
            let cost = l.columns * l.interleave;
            let Some(runs) = l.runs().get(&(0, 0)) else {
                continue;
            };
            for run in runs {
                let start = l.run_start_offset(run.first_row);
                for (from, to) in journal.dirty_ranges(start, run.rows * per_row) {
                    out.listed += to - from;
                    for stripe in from..to {
                        if out.incomplete || visits == 0 {
                            out.incomplete = true;
                            break;
                        }
                        visits -= 1;
                        let offset = start + stripe * width;
                        if let Some((_, index)) = &self.cache
                            && let Lookup::Hit { len, .. } = index.lookup(offset)
                            && len >= width
                        {
                            out.cached += 1;
                            continue;
                        }
                        if budget.is_some_and(|b| out.bytes + cost > b) {
                            out.incomplete = true;
                            break;
                        }
                        out.bytes += cost;
                        match m.stripe_check(&l.locate(offset), l.interleave as usize)? {
                            Some(true) => out.checked += 1,
                            Some(false) => {
                                out.checked += 1;
                                out.mismatches.push(offset);
                            }
                            None => out.unreadable += 1,
                        }
                    }
                }
            }
        }
        Ok(Some(out))
    }

    /// Where byte `offset` of the space is kept, for evidence: the block of
    /// the write-back cache that holds it, or the virtual slab, column and
    /// row and for every copy the physical slab, with the device (named by
    /// `names`, by its index otherwise) and the byte offset on it.
    pub fn describe(&self, offset: u64, names: &[String]) -> String {
        let head = format!("space byte {offset:#x}");
        if let Some((mapped, index)) = &self.cache
            && let Lookup::Hit { cache_offset, .. } = index.lookup(offset)
        {
            return format!(
                "{head}, in the write-back cache at {cache_offset:#x}: {}",
                mapped.describe(cache_offset, names)
            );
        }
        let holder = std::iter::once(&self.base)
            .chain(&self.tiers)
            .find(|m| {
                offset >= m.layout.base && {
                    let loc = m.layout.locate(offset);
                    (0..m.layout.copies).any(|c| m.layout.physical(loc.column, c, loc.row).is_some())
                }
            })
            .unwrap_or(&self.base);
        format!(
            "{head} (virtual slab {}), {}",
            offset / SLAB_SIZE,
            holder.describe(offset, names)
        )
    }

    /// Fills `buf` from `offset`.
    pub fn read_exact_at(&self, mut buf: &mut [u8], mut offset: u64) -> Result<()> {
        let end = offset.checked_add(buf.len() as u64).filter(|&e| e <= self.size);
        if end.is_none() {
            return Err(Error::Io(io::ErrorKind::UnexpectedEof.into()));
        }
        while !buf.is_empty() {
            if let Some((_, index)) = &self.cache
                && index.is_ambiguous(offset)
                && self.base.unclean == UncleanParity::Refuse
            {
                return Err(Error::Pool(format!(
                    "the copies of the write-back cache disagree about offset {offset:#x} after an unclean shutdown"
                )));
            }
            let n = match &self.cache {
                Some((mapped, index)) => match index.lookup(offset) {
                    Lookup::Hit { cache_offset, len } => {
                        let n = buf.len().min(len as usize);
                        mapped.read_exact(cache_offset, &mut buf[..n])?;
                        n
                    }
                    Lookup::Miss { len } => {
                        let n = buf.len().min(len as usize);
                        self.read_space(offset, &mut buf[..n])?
                    }
                },
                None => self.read_space(offset, buf)?,
            };
            offset += n as u64;
            buf = &mut buf[n..];
        }
        Ok(())
    }

    /// Reads the space's own extents only, ignoring the write-back cache
    /// (diagnostics and recovery; this is not the current content).
    pub fn read_uncached_at(&self, mut buf: &mut [u8], mut offset: u64) -> Result<()> {
        while !buf.is_empty() {
            let n = self.read_space(offset, buf)?;
            offset += n as u64;
            buf = &mut buf[n..];
        }
        Ok(())
    }

    /// Reads the space's own extents or those of the tier that holds the
    /// range; zeros if nothing is allocated. Returns the bytes read.
    fn read_space(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let (mut n, allocated) = self.base.read_some(offset, buf)?;
        if allocated {
            return Ok(n);
        }
        for tier in &self.tiers {
            let (m, allocated) = tier.read_some(offset, &mut buf[..n])?;
            if allocated {
                return Ok(m);
            }
            n = n.min(m);
        }
        buf[..n].fill(0);
        Ok(n)
    }

    /// The headers of the space's hidden structures (cache, parity
    /// journal, dirty region log) whose copies are not all valid and alike.
    pub fn header_notes(&self) -> &[HeaderNote] {
        &self.notes
    }

    /// Whether the space stores its data in storage tiers.
    pub fn is_tiered(&self) -> bool {
        !self.tiers.is_empty()
    }

    /// The number of its storage tiers that hold data.
    pub fn tier_count(&self) -> usize {
        self.tiers.len()
    }

    /// Reads what the write-back cache holds for `offset`, if anything.
    pub fn read_cached_at(&self, buf: &mut [u8], offset: u64) -> Result<bool> {
        let Some((mapped, index)) = &self.cache else {
            return Ok(false);
        };
        match index.lookup(offset) {
            Lookup::Hit { cache_offset, len } if len as usize >= buf.len() => {
                mapped.read_exact(cache_offset, buf)?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// Wraps the reader into a `Read + Seek` stream.
    pub fn stream(self) -> SpaceStream<'p, D> {
        SpaceStream { reader: self, pos: 0 }
    }
}

impl<D: ReadAt> ReadAt for SpaceReader<'_, D> {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        SpaceReader::read_exact_at(self, buf, offset).map_err(into_io)
    }

    fn size(&self) -> io::Result<u64> {
        Ok(self.size)
    }
}

/// Sequential view of a [`SpaceReader`].
pub struct SpaceStream<'p, D> {
    reader: SpaceReader<'p, D>,
    pos: u64,
}

impl<D: ReadAt> Read for SpaceStream<'_, D> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = (buf.len() as u64).min(self.reader.size.saturating_sub(self.pos)) as usize;
        self.reader.read_exact_at(&mut buf[..n], self.pos).map_err(into_io)?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl<D: ReadAt> Seek for SpaceStream<'_, D> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let new = match pos {
            SeekFrom::Start(p) => Some(p),
            SeekFrom::End(d) => self.reader.size.checked_add_signed(d),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
        };
        self.pos = new.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek before start"))?;
        Ok(self.pos)
    }
}

pub(crate) fn into_io(e: Error) -> io::Error {
    match e {
        Error::Io(e) => e,
        other => io::Error::other(other),
    }
}
