//! Reading the contents of a space.

use std::io::{self, Read, Seek, SeekFrom};

use crate::cache::{CacheHeader, CacheIndex, Lookup};
use crate::error::{Error, Result, format_err};
use crate::format::{Resiliency, SLAB_SIZE, SpaceRole};
use crate::io::ReadAt;
use crate::layout::{Layout, Location};
use crate::pool::{Pool, Space};

/// A space layout bound to the pool it reads from.
struct Mapped<'p, D> {
    pool: &'p Pool<D>,
    layout: Layout,
}

impl<D: ReadAt> Mapped<'_, D> {
    fn new<'p>(pool: &'p Pool<D>, space: &Space) -> Result<Mapped<'p, D>> {
        let policy = space
            .info
            .policy
            .ok_or_else(|| format_err!("space {} has no recognised placement policy", space.id()))?;
        Ok(Mapped {
            pool,
            layout: Layout::new(&policy, &space.extents)?,
        })
    }

    /// Reads from `offset` up to the next interleave boundary; returns bytes read.
    fn read_some(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let l = &self.layout;
        let loc = l.locate(offset);
        let n = buf.len().min(loc.contiguous as usize);
        let buf = &mut buf[..n];
        let mut allocated = false;
        let mut last_error = None;
        for copy in 0..l.copies {
            if let Some((disk, slab)) = l.physical(loc.column, copy, loc.row) {
                allocated = true;
                // A read error (a disk that went away) falls through to the
                // next copy or to parity reconstruction.
                match self.pool.read_slab(disk, slab, loc.offset_in_slab, buf) {
                    Ok(true) => return Ok(n),
                    Ok(false) => {}
                    Err(Error::Io(e)) => last_error = Some(e),
                    Err(e) => return Err(e),
                }
            }
        }
        if allocated && l.resiliency == Resiliency::Parity {
            self.reconstruct(&loc, buf)?;
            return Ok(n);
        }
        if let Some(e) = last_error {
            return Err(Error::Io(e));
        }
        if allocated {
            return Err(Error::Pool(format!(
                "no copy of column {} row {} is on a present disk",
                loc.column, loc.row
            )));
        }
        buf.fill(0); // not allocated (thin provisioning)
        Ok(n)
    }

    /// Rebuilds a unit of a missing column from the XOR parity and the other
    /// data units of its stripe.
    fn reconstruct(&self, loc: &Location, buf: &mut [u8]) -> Result<()> {
        let l = &self.layout;
        let p = l.parity_column(l.stripe_of(loc));
        // The second parity unit of dual parity is not part of the XOR.
        let q = (l.parity_units == 2).then(|| (p + 1) % l.columns);
        buf.fill(0);
        let mut other = vec![0u8; buf.len()];
        for column in (0..l.columns).filter(|&c| c != loc.column && Some(c) != q) {
            let (disk, slab) = self
                .layout
                .physical(column, 0, loc.row)
                .ok_or_else(|| format_err!("parity stripe with an unallocated column {column}"))?;
            let lost = || Error::Pool(format!("row {} lost two or more columns", loc.row));
            match self.pool.read_slab(disk, slab, loc.offset_in_slab, &mut other) {
                Ok(true) => {}
                Ok(false) | Err(Error::Io(_)) => return Err(lost()),
                Err(e) => return Err(e),
            }
            buf.iter_mut().zip(&other).for_each(|(b, o)| *b ^= o);
        }
        Ok(())
    }

    fn read_exact(&self, mut offset: u64, mut buf: &mut [u8]) -> Result<()> {
        while !buf.is_empty() {
            let n = self.read_some(offset, buf)?;
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
    cache: Option<(Mapped<'p, D>, CacheIndex)>,
}

impl<'p, D: ReadAt> SpaceReader<'p, D> {
    pub(crate) fn new(pool: &'p Pool<D>, id: u64) -> Result<Self> {
        let space = pool
            .spaces
            .get(&id)
            .ok_or_else(|| Error::Pool(format!("no space with id {id}")))?;
        let base = Mapped::new(pool, space)?;
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
        Ok(SpaceReader {
            space,
            size,
            base,
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
                        self.base.read_some(offset, &mut buf[..n])?
                    }
                },
                None => self.base.read_some(offset, buf)?,
            };
            offset += n as u64;
            buf = &mut buf[n..];
        }
        Ok(())
    }

    /// Reads the space's own extents only, ignoring the write-back cache
    /// (diagnostics and recovery; this is not the current content).
    pub fn read_uncached_at(&self, buf: &mut [u8], offset: u64) -> Result<()> {
        self.base.read_exact(offset, buf)
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
