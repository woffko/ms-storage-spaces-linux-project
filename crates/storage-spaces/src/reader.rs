//! Reading the contents of a space.

use std::io::{self, Read, Seek, SeekFrom};

use crate::cache::{CacheHeader, CacheIndex, Lookup};
use crate::error::{Error, Result, format_err};
use crate::format::{Resiliency, SLAB_SIZE, SpaceRole};
use crate::gf16;
use crate::io::ReadAt;
use crate::journal::ParityJournal;
use crate::layout::{Layout, Location};
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

/// A space layout bound to the pool it reads from.
struct Mapped<'p, D> {
    pool: &'p Pool<D>,
    layout: Layout,
    journal: Option<Arc<ParityJournal>>,
    unclean: UncleanParity,
}

impl<D: ReadAt> Mapped<'_, D> {
    fn new<'p>(pool: &'p Pool<D>, space: &Space) -> Result<Mapped<'p, D>> {
        let policy = space
            .info
            .policy
            .ok_or_else(|| format_err!("space {} has no recognised placement policy", space.id()))?;
        let base = space.info.range.map_or(0, |(start, _)| start);
        Ok(Mapped {
            pool,
            layout: Layout::with_base(&policy, &space.extents, base)?,
            journal: None,
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

    /// Whether the XOR parity of the stripe holding `loc` matches its data
    /// over `len` bytes. Missing columns count as consistent (nothing to check).
    fn stripe_consistent(&self, loc: &Location, len: usize) -> Result<bool> {
        let l = &self.layout;
        let p = l.parity_column(l.stripe_of(loc));
        let q = (l.parity_units == 2).then(|| (p + 1) % l.columns);
        let mut acc = vec![0u8; len];
        let mut unit = vec![0u8; len];
        for column in (0..l.columns).filter(|&c| Some(c) != q) {
            let Some((disk, slab)) = l.physical(column, 0, loc.row) else {
                return Ok(true);
            };
            match self.pool.read_slab(disk, slab, loc.offset_in_slab, &mut unit) {
                Ok(true) => acc.iter_mut().zip(&unit).for_each(|(a, u)| *a ^= u),
                Ok(false) | Err(Error::Io(_)) => return Ok(true),
                Err(e) => return Err(e),
            }
        }
        Ok(acc.iter().all(|&b| b == 0))
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
    /// the other data units of its stripe: from P (XOR) when one column is
    /// lost, from P and Q (GF(16), see `gf16`) when two are.
    fn reconstruct(&self, loc: &Location, buf: &mut [u8]) -> Result<()> {
        let l = &self.layout;
        let lost = || Error::Pool(format!("row {} lost more columns than its parity covers", loc.row));
        let p = l.parity_column(l.stripe_of(loc));
        let q = (l.parity_units == 2).then(|| (p + 1) % l.columns);
        let data: Vec<u64> = (0..l.data_columns)
            .map(|i| (p + l.parity_units + i) % l.columns)
            .collect();
        let target = data.iter().position(|&c| c == loc.column).ok_or_else(lost)?;

        // Work on whole 512-byte chunks of the unit (the Q code's granularity).
        let within = loc.offset_in_slab % l.interleave;
        let unit_start = loc.offset_in_slab - within;
        let from = within / gf16::CHUNK as u64 * gf16::CHUNK as u64;
        let to = (within + buf.len() as u64).div_ceil(gf16::CHUNK as u64) * gf16::CHUNK as u64;
        let (at, len) = (unit_start + from, (to.min(l.interleave) - from) as usize);

        let mut units: Vec<Option<Vec<u8>>> = Vec::with_capacity(data.len());
        for (k, &c) in data.iter().enumerate() {
            units.push(if k == target {
                None
            } else {
                self.read_column(c, loc.row, at, len)?
            });
        }
        let missing: Vec<usize> = (0..data.len()).filter(|&k| k != target && units[k].is_none()).collect();
        let pu = self.read_column(p, loc.row, at, len)?;
        let qu = match q {
            Some(q) if missing.len() == 1 || pu.is_none() => self.read_column(q, loc.row, at, len)?,
            _ => None,
        };
        let coef = gf16::coefficients(l.data_columns);
        let xor_into = |acc: &mut [u8], x: &[u8]| acc.iter_mut().zip(x).for_each(|(a, b)| *a ^= b);

        let rebuilt = match (missing.as_slice(), &pu, &qu) {
            ([], Some(pu), _) => {
                let mut acc = pu.clone();
                units.iter().flatten().for_each(|u| xor_into(&mut acc, u));
                acc
            }
            ([], None, Some(qu)) => {
                let coef = coef.ok_or_else(|| Error::Unsupported("dual parity Q for this column count".into()))?;
                let mut acc = qu.clone();
                for (k, u) in units.iter().enumerate() {
                    if let Some(u) = u {
                        gf16::mul_region_xor(coef[k], u, &mut acc);
                    }
                }
                let mut out = vec![0u8; len];
                gf16::mul_region_xor(gf16::inv(coef[target]).ok_or_else(lost)?, &acc, &mut out);
                out
            }
            (&[other], Some(pu), Some(qu)) => {
                let coef = coef.ok_or_else(|| Error::Unsupported("dual parity Q for this column count".into()))?;
                // P' = D_t + D_o and Q' = e_t D_t + e_o D_o after removing the known units.
                let (mut p2, mut q2) = (pu.clone(), qu.clone());
                for (k, u) in units.iter().enumerate() {
                    if let Some(u) = u {
                        xor_into(&mut p2, u);
                        gf16::mul_region_xor(coef[k], u, &mut q2);
                    }
                }
                gf16::mul_region_xor(coef[other], &p2, &mut q2); // e_t D_t + e_o D_t = (e_t + e_o) D_t
                let mut out = vec![0u8; len];
                gf16::mul_region_xor(gf16::inv(coef[target] ^ coef[other]).ok_or_else(lost)?, &q2, &mut out);
                out
            }
            _ => return Err(lost()),
        };
        let skip = (within - from) as usize;
        buf.copy_from_slice(&rebuilt[skip..skip + buf.len()]);
        Ok(())
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
        let space = pool
            .spaces
            .get(&id)
            .ok_or_else(|| Error::Pool(format!("no space with id {id}")))?;
        let mut base = Mapped::new(pool, space)?;
        let size = match space.info.size {
            Some(size) => size,
            None => {
                let rows = base
                    .layout
                    .runs()
                    .values()
                    .flatten()
                    .map(|r| r.first_row + r.rows)
                    .max()
                    .unwrap_or(0);
                rows * SLAB_SIZE * base.layout.data_columns
            }
        };
        let cache = Self::open_cache(pool, space)?;
        let mut tiers = pool
            .children(space.id())
            .filter(|c| c.info.is_child && !c.extents.is_empty())
            .map(|c| Mapped::new(pool, c))
            .collect::<Result<Vec<_>>>()?;
        let journal = Self::open_journal(pool, space)?.map(Arc::new);
        for m in std::iter::once(&mut base).chain(tiers.iter_mut()) {
            m.journal = journal.clone();
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
        let index = CacheIndex::load(header, |off, buf| mapped.read_exact(off, buf))?;
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
                return ParityJournal::load(space.info.guid, |off, buf| mapped.read_exact(off, buf));
            }
        }
        Ok(None)
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

    pub fn layout(&self) -> &Layout {
        &self.base.layout
    }

    pub fn cache(&self) -> Option<&CacheIndex> {
        self.cache.as_ref().map(|(_, index)| index)
    }

    /// Fills `buf` from `offset`.
    pub fn read_exact_at(&self, mut buf: &mut [u8], mut offset: u64) -> Result<()> {
        let end = offset.checked_add(buf.len() as u64).filter(|&e| e <= self.size);
        if end.is_none() {
            return Err(Error::Io(io::ErrorKind::UnexpectedEof.into()));
        }
        while !buf.is_empty() {
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

fn into_io(e: Error) -> io::Error {
    match e {
        Error::Io(e) => e,
        other => io::Error::other(other),
    }
}
