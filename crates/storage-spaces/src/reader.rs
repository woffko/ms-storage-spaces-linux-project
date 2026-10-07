//! Reading the contents of a space.

use std::io::{self, Read, Seek, SeekFrom};

use crate::cache::{CacheHeader, CacheIndex, Lookup, SlotSource};
use crate::drt::DirtyRegions;
use crate::error::{Error, Result, format_err};
use crate::format::{Resiliency, SLAB_SIZE, SpaceRole};
use crate::gf16;
use crate::io::ReadAt;
use crate::journal::ParityJournal;
use crate::layout::{Condition, Layout, Location};
use crate::pool::{Pool, Space};
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
}

/// Reads a cache or journal space, merging the slot areas of its copies.
struct Slots<'a, 'p, D>(&'a Mapped<'p, D>);

impl<D: ReadAt> SlotSource for Slots<'_, '_, D> {
    fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.0.read_exact(offset, buf)
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
        let cache = Self::open_cache(pool, space).map_err(|e| (Part::Cache, e))?;
        let mut tiers = pool
            .children(space.id())
            .filter(|c| c.info.is_child && !c.extents.is_empty())
            .map(|c| Mapped::new(pool, c))
            .collect::<Result<Vec<_>>>()
            .map_err(|e| (Part::Tiers, e))?;
        let journal = Self::open_journal(pool, space)
            .map_err(|e| (Part::Journal, e))?
            .map(Arc::new);
        let drt = Self::open_drt(pool, space)
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
        })
    }

    fn open_cache(pool: &'p Pool<D>, space: &Space) -> Result<Option<(Mapped<'p, D>, CacheIndex)>> {
        let containers: Vec<_> = pool
            .children(space.id())
            .filter(|c| c.info.role == SpaceRole::Cache)
            .collect();
        let mut found = None;
        for container in containers {
            for child in pool.children(container.id()).filter(|c| !c.extents.is_empty()) {
                if found.is_some() {
                    return Err(Error::Unsupported(format!("space {} has several caches", space.id())));
                }
                found = Some(child);
            }
        }
        let Some(child) = found else { return Ok(None) };
        let mapped = Mapped::new(pool, child)?;
        let mut head = [0u8; CacheHeader::SIZE];
        mapped.read_exact(0, &mut head)?;
        let Some(header) = CacheHeader::parse(&head)? else {
            return Ok(None);
        };
        if header.owner_guid != space.info.guid {
            return Err(format_err!(
                "cache of space {} belongs to {}",
                space.id(),
                header.owner_guid
            ));
        }
        let index = CacheIndex::load(header, pool.logical_sector_size, Slots(&mapped))?;
        Ok(Some((mapped, index)))
    }

    pub fn pool(&self) -> &'p Pool<D> {
        self.base.pool
    }

    /// Loads the parity journal of a space, if it has one.
    fn open_journal(pool: &'p Pool<D>, space: &Space) -> Result<Option<ParityJournal>> {
        let containers = pool
            .children(space.id())
            .filter(|c| c.info.role == SpaceRole::Other(0x0a));
        for container in containers {
            if let Some(child) = pool.children(container.id()).find(|c| !c.extents.is_empty()) {
                let mapped = Mapped::new(pool, child)?;
                return ParityJournal::load(space.info.guid, Slots(&mapped));
            }
        }
        Ok(None)
    }

    fn open_drt(pool: &'p Pool<D>, space: &Space) -> Result<Option<DirtyRegions>> {
        let containers = pool
            .children(space.id())
            .filter(|c| c.info.role == SpaceRole::Other(0x06));
        for container in containers {
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
                return DirtyRegions::load(size, |off, buf| mapped.read_exact(base + off, buf));
            }
        }
        Ok(None)
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
                for stripe in 0..run.rows * per_row {
                    if !journal.is_dirty(start, stripe) {
                        continue;
                    }
                    out.listed += 1;
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
                        continue;
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

    /// Whether the space stores its data in storage tiers.
    pub fn is_tiered(&self) -> bool {
        !self.tiers.is_empty()
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
