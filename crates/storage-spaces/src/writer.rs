//! Writes to spaces (Stage 2 of docs/plan.md).
//!
//! A space opens for writing only when everything about its state is
//! understood, and only for what the writes can keep consistent the way
//! Windows would: allocated rows of simple, mirror and single parity
//! spaces. Everything else is refused with the reason. Reads of a space
//! opened for writing go through its writer ([`SpaceWriter::read_exact_at`]).
//!
//! A write-back cache that holds data when the space is opened is destaged
//! first (see below); simple and mirror writes then go to the space itself.
//!
//! Mirror writes follow the dirty region log model of the format document
//! (`DrtWriter`): before the first write into an extent run that is not
//! listed, the next header is written into every copy of the tracking space
//! and flushed; then the data goes to every copy. Runs this writer wrote are
//! removed from the log at the next header write once idle for 30 s, as
//! Windows does; runs listed when the space was opened stay listed (their
//! copies may differ after a crash of Windows, and readers keep comparing
//! them).
//!
//! Single parity writes follow Windows (format document, "Parity journal"):
//! a request of whole stripes that the journal records as not consistent
//! and the cache does not hold goes to the space directly and is recorded
//! as consistent afterwards; everything else goes to the write-back cache,
//! so a stripe recorded as consistent is never rewritten while its new
//! content exists nowhere else. A cached write puts its data into the
//! chunk's cache block (a chunk is one stripe); the log slots that map new
//! sectors are written at the next flush, once that data is durable. When
//! the log or the blocks run out, every cached chunk is destaged: chunks
//! with sectors missing are made whole in the cache first (the rest of the
//! stripe is read from the space and logged), then written as whole stripes
//! (recorded as not consistent, flushed, written with their parity,
//! flushed, recorded as consistent) and removed from the cache with
//! tombstone entries, flushed before their blocks are handed out again. At
//! every point either a stripe on disk matches its parity or the cache holds
//! all of its data: the write hole stays closed.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};

use crate::cache::CacheWriter;
use crate::drt::DrtWriter;
use crate::error::{Error, Result};
use crate::format::{Resiliency, SLAB_SIZE, SpaceRole};
use crate::io::{ReadAt, WriteAt};
use crate::journal::JournalWriter;
use crate::layout::{Condition, Layout};
use crate::pool::Pool;
use crate::reader::{OpenOptions, SpaceReader};

/// Idle time after which Windows removes a run from the dirty region log
/// (runs idle for 29 s stayed, 35 s went).
const DRT_CLEAN_AFTER: Duration = Duration::from_secs(30);

/// Data destaged at a time.
const DESTAGE_BATCH: u64 = 32 << 20;

/// Writes to a space, and reads what was written.
pub struct SpaceWriter<'p, D> {
    reader: SpaceReader<'p, D>,
    drt: Option<DrtState>,
    journal: Option<JournalState>,
    cache: Option<CacheState>,
}

/// The parity journal of a parity space being written.
struct JournalState {
    /// Layout of the journal space.
    layout: Layout,
    slot_offset: u64,
    slot_size: u64,
    writer: Mutex<JournalWriter>,
}

/// The dirty region log of a mirror space being written.
struct DrtState {
    /// Layout and size of the tracking space.
    layout: Layout,
    size: u64,
    log: Mutex<(DrtWriter, HashMap<u64, Instant>)>,
}

/// The write-back cache of a space being written.
struct CacheState {
    /// Layout of the cache space.
    layout: Layout,
    slot_offset: u64,
    slot_size: u64,
    /// Held for writing by every write of a parity space and while
    /// destaging, for reading by reads (a cache block may be handed to
    /// another chunk once its chunk is destaged).
    log: RwLock<CacheLog>,
}

struct CacheLog {
    writer: CacheWriter,
    /// Slots of cached writes whose data may not be durable yet, written
    /// at the next flush.
    pending: Vec<(usize, Vec<u8>)>,
}

impl<'p, D: WriteAt> SpaceWriter<'p, D> {
    pub(crate) fn new(pool: &'p Pool<D>, id: u64) -> Result<Self> {
        let refuse = |why: String| {
            Err(Error::Unsupported(format!(
                "space {id} cannot be opened for writing: {why}"
            )))
        };
        if !pool.warnings.is_empty() {
            return refuse(format!(
                "the pool is not in a clean state ({})",
                pool.warnings.join("; ")
            ));
        }
        let reader = pool.open_space_with(id, OpenOptions::default())?;
        if reader.condition() != Condition::Healthy {
            return refuse(format!("it is {:?}", reader.condition()).to_lowercase());
        }
        if pool.children(id).any(|c| c.info.is_child && !c.extents.is_empty()) {
            return refuse("writes to tiered spaces are not supported yet".into());
        }
        let layout = reader.layout();
        let (mut drt, mut journal) = (None, None);
        match layout.resiliency {
            Resiliency::Simple => {}
            Resiliency::Mirror => {
                let Some(log) = reader.dirty_regions() else {
                    return refuse("the mirror space has no readable dirty region log".into());
                };
                let (layout, size) = Self::child_space(pool, id, SpaceRole::Other(0x06))?;
                drt = Some(DrtState {
                    layout,
                    size,
                    log: Mutex::new((log.writer(), HashMap::new())),
                });
            }
            Resiliency::Parity if layout.parity_units == 1 => {
                let Some(pj) = reader.journal() else {
                    return refuse("the parity space has no readable parity journal".into());
                };
                let (jlayout, _) = Self::child_space(pool, id, SpaceRole::Other(0x0a))?;
                let (slot_offset, slot_size, _) = pj.geometry();
                journal = Some(JournalState {
                    slot_offset: jlayout.base + slot_offset,
                    layout: jlayout,
                    slot_size: slot_size as u64,
                    writer: Mutex::new(pj.writer(reader.space.info.guid)),
                });
            }
            Resiliency::Parity => return refuse("writes to dual parity spaces are not supported yet".into()),
            other => return refuse(format!("writes to {other:?} spaces are not supported yet").to_lowercase()),
        }
        let mut cache = None;
        match reader.cache() {
            Some(index) => {
                if index.conflicting_chunks() > 0 {
                    return refuse(format!(
                        "the copies of its write-back cache disagree about {} chunks after an unclean shutdown",
                        index.conflicting_chunks()
                    ));
                }
                if let Some((offset, _, _)) = index
                    .mappings()
                    .into_iter()
                    .find(|m| !row_allocated(layout, layout.locate(layout.base + m.0).row))
                {
                    return refuse(format!(
                        "its write-back cache holds data at {offset:#x}, in a row the thin space has not allocated \
                         (allocation is not supported yet)"
                    ));
                }
                let (clayout, _) = Self::child_space(pool, id, SpaceRole::Cache)?;
                // Parity caches hand out blocks from 64, mirror caches from 0.
                let first_block = if journal.is_some() { 64 } else { 0 };
                cache = Some(CacheState {
                    layout: clayout,
                    slot_offset: index.header.slot_offset,
                    slot_size: index.header.slot_size as u64,
                    log: RwLock::new(CacheLog {
                        writer: index.writer(first_block),
                        pending: Vec::new(),
                    }),
                });
            }
            None if journal.is_some() => {
                return refuse("the parity space has no write-back cache, which its writes need".into());
            }
            None => {}
        }
        let mut writer = SpaceWriter {
            reader,
            drt,
            journal,
            cache,
        };
        if writer.reader.cache().is_some_and(|c| c.cached_chunks() > 0) {
            writer.destage()?;
            // The reader's view of the cache is out of date now.
            writer.reader = pool.open_space_with(id, OpenOptions::default())?;
        }
        Ok(writer)
    }

    /// Layout and size of the hidden child of space `id` under a container
    /// of role `role` (0x06: dirty region tracking, 0x0a: parity journal,
    /// 0x0b: write-back cache).
    fn child_space(pool: &Pool<D>, id: u64, role: SpaceRole) -> Result<(Layout, u64)> {
        let child = pool
            .children(id)
            .filter(|c| c.info.role == role)
            .find_map(|container| pool.children(container.id()).find(|c| !c.extents.is_empty()))
            .ok_or_else(|| Error::Unsupported(format!("space {id} has no {role:?} child space")))?;
        let policy = child
            .info
            .policy
            .ok_or_else(|| Error::Unsupported("tracking space without a policy".into()))?;
        let base = child.info.range.map_or(0, |(start, _)| start);
        let layout = Layout::with_base(&policy, &child.extents, base)?;
        let size = child.info.range.map_or_else(|| layout.mapped_size(), |(_, len)| len);
        Ok((layout, size))
    }

    /// The reader the writer was opened with: its layout and metadata as
    /// of the open. Its reads miss what the writer holds in the write-back
    /// cache; read through [`SpaceWriter::read_exact_at`].
    pub fn reader(&self) -> &SpaceReader<'p, D> {
        &self.reader
    }

    pub fn size(&self) -> u64 {
        self.reader.size()
    }

    /// Fills `buf` from `offset` of the space, including what is written
    /// but not flushed yet.
    pub fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> Result<()> {
        match &self.cache {
            Some(cache) => self.read_locked(cache, &cache.log.read().unwrap(), buf, offset),
            None => self.reader.read_exact_at(buf, offset),
        }
    }

    fn read_locked(&self, cache: &CacheState, log: &CacheLog, mut buf: &mut [u8], mut offset: u64) -> Result<()> {
        if offset.checked_add(buf.len() as u64).is_none_or(|end| end > self.size()) {
            return Err(Error::Io(std::io::ErrorKind::UnexpectedEof.into()));
        }
        while !buf.is_empty() {
            let (hit, len) = log.writer.lookup_run(offset);
            let n = buf.len().min(len as usize);
            match hit {
                Some(at) => self.read_cache(cache, at, &mut buf[..n])?,
                None => self.reader.read_uncached_at(&mut buf[..n], offset)?,
            }
            offset += n as u64;
            buf = &mut buf[n..];
        }
        Ok(())
    }

    /// Reads from the cache space (its first copy at hand).
    fn read_cache(&self, cache: &CacheState, mut offset: u64, mut buf: &mut [u8]) -> Result<()> {
        let pool = self.reader.pool();
        let layout = &cache.layout;
        while !buf.is_empty() {
            let loc = layout.locate(offset);
            let n = buf.len().min(loc.contiguous as usize);
            let mut read = false;
            for copy in 0..layout.copies {
                if let Some((disk, slab)) = layout.physical(loc.column, copy, loc.row)
                    && pool.read_slab(disk, slab, loc.offset_in_slab, &mut buf[..n])?
                {
                    read = true;
                    break;
                }
            }
            if !read {
                return Err(Error::Pool(format!(
                    "the write-back cache at {offset:#x} is not readable"
                )));
            }
            offset += n as u64;
            buf = &mut buf[n..];
        }
        Ok(())
    }

    /// Writes all of `buf` at `offset` of the space. A write into a row a
    /// thin space has not allocated is refused (allocation is not
    /// supported yet).
    pub fn write_all_at(&self, buf: &[u8], offset: u64) -> Result<()> {
        let size = self.size();
        if offset.checked_add(buf.len() as u64).is_none_or(|end| end > size) {
            return Err(Error::Pool(format!(
                "write of {} bytes at {offset:#x} beyond the end of the space ({size:#x})",
                buf.len()
            )));
        }
        if buf.is_empty() {
            return Ok(());
        }
        if let (Some(journal), Some(cache)) = (&self.journal, &self.cache) {
            return self.write_parity(journal, cache, &mut cache.log.write().unwrap(), buf, offset);
        }
        self.write_direct(buf, offset)
    }

    /// Writes to a simple or mirror space itself.
    fn write_direct(&self, mut buf: &[u8], mut offset: u64) -> Result<()> {
        let layout = self.reader.layout();
        let pool = self.reader.pool();
        while !buf.is_empty() {
            let loc = layout.locate(offset);
            let n = buf.len().min(loc.contiguous as usize);
            if let Some(drt) = &self.drt {
                self.mark_dirty(drt, layout.run_start_offset(loc.row) / SLAB_SIZE)?;
            }
            write_copies(pool, layout, offset, &buf[..n])?;
            buf = &buf[n..];
            offset += n as u64;
        }
        Ok(())
    }

    /// Writes to a single parity space (see the module documentation).
    fn write_parity(
        &self,
        journal: &JournalState,
        cache: &CacheState,
        log: &mut CacheLog,
        buf: &[u8],
        offset: u64,
    ) -> Result<()> {
        let layout = self.reader.layout();
        let stripe = layout.data_columns * layout.interleave;
        let end = offset + buf.len() as u64;
        // The parts that go to the space directly (true) or to the cache,
        // adjacent parts of the same kind merged.
        let mut parts: Vec<(bool, u64, u64)> = Vec::new();
        let mut at = offset;
        while at < end {
            let start = at - at % stripe;
            let next = (start + stripe).min(end);
            let loc = layout.locate(layout.base + start);
            let run_stripes = self.run_rows(loc.row)? * (SLAB_SIZE / layout.interleave);
            if !row_allocated(layout, loc.row) {
                return Err(Error::Unsupported(format!(
                    "write at {at:#x} into a row the thin space has not allocated (allocation is not supported yet)"
                )));
            }
            let direct = at == start
                && next == start + stripe
                && log.writer.chunk_runs(start).is_none()
                && !journal.writer.lock().unwrap().is_consistent(
                    layout.run_start_offset(loc.row),
                    run_stripes,
                    layout.stripe_of(&loc),
                );
            match parts.last_mut() {
                Some((d, _, e)) if *d == direct => *e = next,
                _ => parts.push((direct, at, next)),
            }
            at = next;
        }
        for (direct, start, end) in parts {
            let part = &buf[(start - offset) as usize..(end - offset) as usize];
            if direct {
                self.write_stripes(journal, &[(start, part)])?;
            } else {
                self.write_cached(cache, log, part, start)?;
            }
        }
        Ok(())
    }

    /// Puts a write into the write-back cache; its slots are written at the
    /// next flush.
    fn write_cached(&self, cache: &CacheState, log: &mut CacheLog, buf: &[u8], offset: u64) -> Result<()> {
        // The cache tracks whole 512-byte sectors.
        let (start, end) = (offset / 512 * 512, (offset + buf.len() as u64).div_ceil(512) * 512);
        let whole;
        let buf = if (start, end) == (offset, offset + buf.len() as u64) {
            buf
        } else {
            let mut b = vec![0u8; (end - start) as usize];
            let last = b.len() - 512;
            self.read_locked(cache, log, &mut b[..512], start)?;
            self.read_locked(cache, log, &mut b[last..], end - 512)?;
            b[(offset - start) as usize..][..buf.len()].copy_from_slice(buf);
            whole = b;
            &whole[..]
        };
        let len = end - start;
        if log.writer.is_full_for(start, len) {
            self.destage_locked(cache, log)?;
            if log.writer.is_full_for(start, len) {
                return Err(Error::Unsupported(format!(
                    "a write of {len} bytes does not fit into the write-back cache"
                )));
            }
        }
        let slots = log.writer.write(start, len);
        let chunk = log.writer.header().chunk_size as u64;
        let pool = self.reader.pool();
        let mut at = start;
        while at < end {
            let n = end.min((at / chunk + 1) * chunk) - at;
            let target = log.writer.lookup(at).expect("sectors just cached");
            write_copies(pool, &cache.layout, target, &buf[(at - start) as usize..][..n as usize])?;
            at += n;
        }
        log.pending.extend(slots);
        Ok(())
    }

    /// Destages everything the write-back cache holds (see the module
    /// documentation) and makes it durable.
    pub fn destage(&self) -> Result<()> {
        match &self.cache {
            Some(cache) => self.destage_locked(cache, &mut cache.log.write().unwrap()),
            None => Ok(()),
        }
    }

    fn destage_locked(&self, cache: &CacheState, log: &mut CacheLog) -> Result<()> {
        let cached = log.writer.cached();
        if cached.is_empty() {
            return Ok(());
        }
        let pool = self.reader.pool();
        self.write_pending(cache, log)?;
        let chunk = log.writer.header().chunk_size as u64;
        for batch in cached.chunks((DESTAGE_BATCH / chunk).max(1) as usize) {
            let mut data = Vec::with_capacity(batch.len());
            let mut partial = Vec::new();
            for &(offset, block) in batch {
                let (_, runs) = log.writer.chunk_runs(offset).expect("a cached chunk");
                let base = log.writer.block_offset(block);
                let mut buf = vec![0u8; chunk as usize];
                let mut at = 0;
                for &(valid, n) in &runs {
                    let part = &mut buf[at as usize..(at + n) as usize];
                    if valid {
                        self.read_cache(cache, base + at, part)?;
                    } else {
                        self.reader.read_uncached_at(part, offset + at)?;
                        if self.journal.is_some() {
                            write_copies(pool, &cache.layout, base + at, part)?;
                        }
                    }
                    at += n;
                }
                if runs.len() > 1 || !runs[0].0 {
                    partial.push(offset);
                }
                data.push((offset, buf));
            }
            let parts: Vec<(u64, &[u8])> = data.iter().map(|(o, b)| (*o, &b[..])).collect();
            match &self.journal {
                Some(journal) => {
                    if !partial.is_empty() {
                        // Whole in the cache before the stripes change.
                        let slots = log.writer.fill(&partial);
                        pool.flush_members()?;
                        self.write_cache_slots(cache, &slots)?;
                        pool.flush_members()?;
                    }
                    self.write_stripes(journal, &parts)?;
                }
                None => {
                    for &(offset, buf) in &parts {
                        self.write_direct(buf, offset)?;
                    }
                    pool.flush_members()?;
                }
            }
            let offsets: Vec<u64> = batch.iter().map(|b| b.0).collect();
            let slots = log.writer.destage(&offsets);
            self.write_cache_slots(cache, &slots)?;
            pool.flush_members()?;
        }
        Ok(())
    }

    /// Writes the slots of cached writes once their data is durable.
    fn write_pending(&self, cache: &CacheState, log: &mut CacheLog) -> Result<()> {
        if log.pending.is_empty() {
            return Ok(());
        }
        self.reader.pool().flush_members()?;
        let slots = std::mem::take(&mut log.pending);
        self.write_cache_slots(cache, &slots)
    }

    fn write_cache_slots(&self, cache: &CacheState, slots: &[(usize, Vec<u8>)]) -> Result<()> {
        for (index, page) in slots {
            let at = cache.slot_offset + *index as u64 * cache.slot_size;
            write_copies(self.reader.pool(), &cache.layout, at, page)?;
        }
        Ok(())
    }

    /// Writes whole stripes of a single parity space in place: `parts` are
    /// (offset, data) of whole stripes. Those the journal records as
    /// consistent (or all, in a run the journal has no entry for) are
    /// recorded as not consistent first; afterwards all are recorded as
    /// consistent.
    fn write_stripes(&self, journal: &JournalState, parts: &[(u64, &[u8])]) -> Result<()> {
        let layout = self.reader.layout();
        let pool = self.reader.pool();
        let unit = layout.interleave;
        let stripe_bytes = layout.data_columns * unit;
        let per_row = SLAB_SIZE / unit;
        let mut writer = journal.writer.lock().unwrap();
        // (offset, data, location, run start, stripes of the run, stripe in the run)
        let mut stripes = Vec::new();
        for &(offset, buf) in parts {
            assert!(offset.is_multiple_of(stripe_bytes) && (buf.len() as u64).is_multiple_of(stripe_bytes));
            for (i, data) in buf.chunks(stripe_bytes as usize).enumerate() {
                let start = offset + i as u64 * stripe_bytes;
                let loc = layout.locate(layout.base + start);
                let run_start = layout.run_start_offset(loc.row);
                let run_stripes = self.run_rows(loc.row)? * per_row;
                stripes.push((start, data, loc, run_start, run_stripes, layout.stripe_of(&loc)));
            }
        }
        let mut by_run: BTreeMap<u64, (u64, Vec<u64>)> = BTreeMap::new();
        for &(_, _, _, run_start, run_stripes, stripe) in &stripes {
            by_run
                .entry(run_start)
                .or_insert((run_stripes, Vec::new()))
                .1
                .push(stripe);
        }
        let mut marked = false;
        for (&run_start, (run_stripes, list)) in &by_run {
            if !writer.is_listed(run_start) || list.iter().any(|&s| writer.is_consistent(run_start, *run_stripes, s)) {
                let (index, page) = writer.mark_stripes(run_start, *run_stripes, list, false);
                self.write_journal_slot(journal, index, &page)?;
                marked = true;
            }
        }
        if marked {
            pool.flush_members()?;
        }
        for &(start, data, loc, _, _, stripe) in &stripes {
            let mut parity = vec![0u8; unit as usize];
            for (k, d) in data.chunks(unit as usize).enumerate() {
                let column = layout.locate(layout.base + start + k as u64 * unit).column;
                parity.iter_mut().zip(d).for_each(|(p, d)| *p ^= d);
                self.write_unit(column, loc.row, loc.offset_in_slab, d)?;
            }
            self.write_unit(layout.parity_column(stripe), loc.row, loc.offset_in_slab, &parity)?;
        }
        pool.flush_members()?;
        for (run_start, (run_stripes, list)) in by_run {
            let (index, page) = writer.mark_stripes(run_start, run_stripes, &list, true);
            self.write_journal_slot(journal, index, &page)?;
        }
        Ok(())
    }

    /// Rows of the extent run holding `row`.
    fn run_rows(&self, row: u64) -> Result<u64> {
        let layout = self.reader.layout();
        layout
            .runs()
            .get(&(0, 0))
            .and_then(|runs| runs.iter().find(|r| r.first_row <= row && row < r.first_row + r.rows))
            .map(|r| r.rows)
            .ok_or_else(|| {
                Error::Unsupported(format!(
                    "write into row {row}, which the thin space has not allocated (allocation is not supported yet)"
                ))
            })
    }

    fn write_unit(&self, column: u64, row: u64, offset_in_slab: u64, buf: &[u8]) -> Result<()> {
        let (disk, slab) = self.reader.layout().physical(column, 0, row).ok_or_else(|| {
            Error::Unsupported("a stripe with an unallocated column (thin allocation is not supported yet)".into())
        })?;
        if !self.reader.pool().write_slab(disk, slab, offset_in_slab, buf)? {
            return Err(Error::Pool(format!("disk {disk} of the space is not present")));
        }
        Ok(())
    }

    fn write_journal_slot(&self, journal: &JournalState, index: usize, page: &[u8]) -> Result<()> {
        let at = journal.slot_offset + index as u64 * journal.slot_size;
        write_copies(self.reader.pool(), &journal.layout, at, page)
    }

    /// Lists the extent run starting at virtual slab `run` in the dirty
    /// region log before it is written (the header durable first).
    fn mark_dirty(&self, drt: &DrtState, run: u64) -> Result<()> {
        let mut guard = drt.log.lock().unwrap();
        let (writer, last_write) = &mut *guard;
        let now = Instant::now();
        if !writer.runs().contains(&run) {
            writer.clean(|r| {
                last_write
                    .get(&r)
                    .is_some_and(|t| now.duration_since(*t) > DRT_CLEAN_AFTER)
            });
            last_write.retain(|r, _| writer.runs().contains(r));
            let Some((at_end, page)) = writer.write(run) else {
                return Err(Error::Unsupported("the dirty region log is full".into()));
            };
            let offset = if at_end { drt.size - 0x2000 } else { 0 };
            let pool = self.reader.pool();
            write_copies(pool, &drt.layout, drt.layout.base + offset, &page)?;
            pool.flush_members()?;
        }
        last_write.insert(run, now);
        Ok(())
    }

    /// Makes the writes so far durable on every member (and the log slots
    /// of cached writes with them).
    pub fn flush(&self) -> Result<()> {
        if let Some(cache) = &self.cache {
            self.write_pending(cache, &mut cache.log.write().unwrap())?;
        }
        self.reader.pool().flush_members()
    }
}

impl<D: WriteAt> ReadAt for SpaceWriter<'_, D> {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
        SpaceWriter::read_exact_at(self, buf, offset).map_err(crate::reader::into_io)
    }

    fn size(&self) -> std::io::Result<u64> {
        Ok(SpaceWriter::size(self))
    }
}

/// Whether every column of `row` is allocated.
fn row_allocated(layout: &Layout, row: u64) -> bool {
    (0..layout.columns).all(|c| layout.physical(c, 0, row).is_some())
}

/// Writes `buf` at `offset` of a space into every current copy.
fn write_copies<D: WriteAt>(pool: &Pool<D>, layout: &Layout, mut offset: u64, mut buf: &[u8]) -> Result<()> {
    while !buf.is_empty() {
        let loc = layout.locate(offset);
        let n = buf.len().min(loc.contiguous as usize);
        let mut copies = 0;
        for copy in 0..layout.copies {
            if let Some((disk, slab)) = layout.physical(loc.column, copy, loc.row) {
                if !pool.write_slab(disk, slab, loc.offset_in_slab, &buf[..n])? {
                    return Err(Error::Pool(format!("disk {disk} of the space is not present")));
                }
                copies += 1;
            }
        }
        if copies == 0 {
            return Err(Error::Unsupported(format!(
                "write at {offset:#x} into a row the thin space has not allocated (allocation is not supported yet)"
            )));
        }
        buf = &buf[n..];
        offset += n as u64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::path::Path;

    use crate::io::{Overlay, SparseImage};
    use crate::pool::Pool;
    use crate::testpattern::{BLOCK, fill_block};

    fn fixture(name: &str) -> Pool<Overlay<SparseImage>> {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
        let disks = (0..)
            .map_while(|i| File::open(dir.join(format!("disk{i}.fixture"))).ok())
            .map(|f| Overlay::new(SparseImage::read_from(f).unwrap()))
            .collect();
        Pool::open(disks).unwrap()
    }

    #[test]
    fn writes_to_a_simple_space_read_back_and_land_on_its_columns() {
        let pool = fixture("simple2c");
        let space = pool.find_space("simple2c").unwrap();
        let w = pool.open_space_rw(space.id()).unwrap();
        // Blocks across an interleave boundary and in the second row.
        let mut data = vec![0u8; 5 * BLOCK];
        for (i, block) in data.chunks_mut(BLOCK).enumerate() {
            fill_block(block, (i * BLOCK) as u64, "linux");
        }
        for offset in [0, 60 << 10, (256 << 20) + (4 << 10)] {
            w.write_all_at(&data, offset).unwrap();
            let mut back = vec![0u8; data.len()];
            w.reader().read_exact_at(&mut back, offset).unwrap();
            assert!(back == data, "at {offset:#x}");
        }
        w.flush().unwrap();
        // A reader opened afterwards sees the writes too.
        let r = pool.open_space(space.id()).unwrap();
        let mut back = vec![0u8; data.len()];
        r.read_exact_at(&mut back, 60 << 10).unwrap();
        assert!(back == data);
        assert!(w.write_all_at(&data, w.size() - BLOCK as u64).is_err());
    }

    #[test]
    fn mirror_writes_list_the_run_first_and_reach_both_copies() {
        // drtdisc: a two-way mirror whose dirty region log is empty.
        let pool = fixture("drtdisc");
        let id = pool.find_space("drtdisc").unwrap().id();
        let before = pool.open_space(id).unwrap().dirty_regions().unwrap().clone();
        assert_eq!(before.dirty_runs(), 0);
        let w = pool.open_space_rw(id).unwrap();
        let mut data = vec![0u8; 2 * BLOCK];
        for (i, block) in data.chunks_mut(BLOCK).enumerate() {
            fill_block(block, (4 << 20) + (i * BLOCK) as u64, "linux");
        }
        w.write_all_at(&data, 4 << 20).unwrap();
        w.write_all_at(&data, 4 << 20).unwrap();
        w.flush().unwrap();
        // The log now lists run 0 in generation 1, at the end copy.
        let r = pool.open_space(id).unwrap();
        let log = r.dirty_regions().unwrap();
        assert_eq!(log.dirty_runs(), 1);
        assert!(log.is_dirty(0));
        let gens: Vec<u64> = log
            .copies()
            .iter()
            .map(|c| c.header.as_ref().unwrap().generation)
            .collect();
        assert_eq!(gens, [0, 1]);
        // Run 0 is listed, so reading compares both copies: equal.
        let mut back = vec![0u8; data.len()];
        r.read_exact_at(&mut back, 4 << 20).unwrap();
        assert!(back == data);
    }

    /// Every state a crash can leave during mirror writes: the writes and
    /// flushes of all members are recorded in order, and every prefix is
    /// replayed onto fresh copies of the disks. In each, the copies of a
    /// row may differ only inside a run the dirty region log lists (the
    /// header reaches the disks before the data), and after the final
    /// flush both copies hold the data.
    #[test]
    fn mirror_writes_keep_the_log_ahead_of_the_data_in_every_crash_state() {
        use crate::io::{DeviceEvent, Recorder, WriteAt};
        use std::sync::{Arc, Mutex};

        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/drtdisc");
        let images: Vec<SparseImage> = (0..2)
            .map(|i| SparseImage::read_from(File::open(dir.join(format!("disk{i}.fixture"))).unwrap()).unwrap())
            .collect();
        let log = Arc::new(Mutex::new(Vec::new()));
        let disks: Vec<Recorder<Overlay<&SparseImage>>> = images
            .iter()
            .enumerate()
            .map(|(i, img)| Recorder::new(Overlay::new(img), i, log.clone()))
            .collect();
        let pool = Pool::open(disks).unwrap();
        let id = pool.find_space("drtdisc").unwrap().id();
        let w = pool.open_space_rw(id).unwrap();
        // Three writes into the space's one run of 1 GiB, then a flush.
        let mut data = vec![0u8; 3 * BLOCK];
        for (i, block) in data.chunks_mut(BLOCK).enumerate() {
            fill_block(block, (i * BLOCK) as u64, "crash");
        }
        let ranges = [0u64, 300 << 20, 700 << 20];
        for &at in &ranges {
            w.write_all_at(&data, at).unwrap();
        }
        w.flush().unwrap();
        drop(w);
        drop(pool);
        let events = log.lock().unwrap().clone();
        assert!(events.iter().any(|e| matches!(e, DeviceEvent::Flush { .. })));

        let mut differing = 0;
        for k in 0..=events.len() {
            let replay: Vec<Overlay<&SparseImage>> = images.iter().map(Overlay::new).collect();
            for e in &events[..k] {
                if let DeviceEvent::Write { device, offset, data } = e {
                    replay[*device].write_all_at(data, *offset).unwrap();
                }
            }
            let pool = Pool::open(replay.iter().collect::<Vec<_>>()).unwrap();
            let r = pool.open_space(id).unwrap();
            let listed = r.dirty_regions().map_or(0, |d| d.dirty_runs());
            let layout = r.layout();
            for &at in &ranges {
                let loc = layout.locate(at);
                let copies: Vec<Vec<u8>> = (0..layout.copies)
                    .map(|c| {
                        let (disk, slab) = layout.physical(loc.column, c, loc.row).unwrap();
                        let (dev, start) = pool.slab_location(disk, slab).unwrap().unwrap();
                        let mut b = vec![0u8; data.len()];
                        crate::io::ReadAt::read_exact_at(&replay[dev], &mut b, start + loc.offset_in_slab).unwrap();
                        b
                    })
                    .collect();
                let run = layout.run_start_offset(loc.row) / crate::format::SLAB_SIZE;
                if copies[0] != copies[1] {
                    differing += 1;
                    assert!(
                        r.dirty_regions().is_some_and(|d| d.is_dirty(run)),
                        "state {k}: copies differ at {at:#x} outside a listed run ({listed} listed)"
                    );
                }
                if k == events.len() {
                    assert!(
                        copies.iter().all(|c| *c == data),
                        "final state: data missing at {at:#x}"
                    );
                }
            }
        }
        assert!(differing > 0, "no state had differing copies");
    }

    /// Whether the data units of the stripe at `offset` XOR to its parity.
    fn stripe_matches<D: crate::io::ReadAt>(pool: &Pool<D>, layout: &crate::layout::Layout, offset: u64) -> bool {
        let loc = layout.locate(offset);
        let mut acc = vec![0u8; layout.interleave as usize];
        for column in 0..layout.columns {
            let (disk, slab) = layout.physical(column, 0, loc.row).unwrap();
            let mut unit = vec![0u8; acc.len()];
            pool.read_slab(disk, slab, loc.offset_in_slab, &mut unit).unwrap();
            acc.iter_mut().zip(&unit).for_each(|(a, u)| *a ^= u);
        }
        acc.iter().all(|&b| b == 0)
    }

    fn pattern(at: u64, len: u64, tag: &str) -> Vec<u8> {
        let mut data = vec![0u8; len as usize];
        for (i, b) in data.chunks_mut(BLOCK).enumerate() {
            fill_block(b, at + (i * BLOCK) as u64, tag);
        }
        data
    }

    /// Single parity writes into stripes the journal records as consistent
    /// (all of them in this pool) go to the write-back cache: a part of a
    /// stripe, a whole stripe and one across a stripe boundary. They read
    /// back at once, reach the log at the flush and leave the stripes on
    /// disk alone; destaging writes them as whole stripes that match their
    /// parity, records them as consistent and empties the cache.
    #[test]
    fn parity_writes_go_through_the_cache_and_destage_as_whole_stripes() {
        let pool = fixture("parity3_26100");
        let id = pool.find_space("parity3_26100").unwrap().id();
        let w = pool.open_space_rw(id).unwrap();
        let layout = w.reader().layout().clone();
        let stripe = layout.data_columns * layout.interleave;
        let writes: Vec<(u64, Vec<u8>)> = [
            (8192u64, 3 * BLOCK as u64),
            (4 * stripe, stripe),
            (7 * stripe - 4096, 3 * 4096),
        ]
        .into_iter()
        .map(|(at, len)| (at, pattern(at, len, "parity")))
        .collect();
        let before: Vec<Vec<u8>> = writes
            .iter()
            .map(|(at, data)| {
                let mut b = vec![0u8; data.len()];
                w.read_exact_at(&mut b, *at).unwrap();
                b
            })
            .collect();
        for (at, data) in &writes {
            w.write_all_at(data, *at).unwrap();
        }
        let read = |r: &crate::SpaceReader<'_, _>, uncached: bool, at: u64, len: usize| {
            let mut b = vec![0u8; len];
            if uncached {
                r.read_uncached_at(&mut b, at).unwrap();
            } else {
                r.read_exact_at(&mut b, at).unwrap();
            }
            b
        };
        // Through the writer at once; the log slots wait for the flush.
        for (at, data) in &writes {
            let mut back = vec![0u8; data.len()];
            w.read_exact_at(&mut back, *at).unwrap();
            assert!(back == *data, "at {at:#x}");
        }
        let r = pool.open_space(id).unwrap();
        assert_eq!(r.cache().unwrap().cached_chunks(), 0);
        for ((at, data), old) in writes.iter().zip(&before) {
            assert!(read(&r, false, *at, data.len()) == *old, "at {at:#x}");
        }
        w.flush().unwrap();
        let r = pool.open_space(id).unwrap();
        let cached: Vec<u64> = r.cache().unwrap().mappings().iter().map(|m| m.0).collect();
        assert_eq!(cached, [0, 4 * stripe, 6 * stripe, 7 * stripe]);
        for ((at, data), old) in writes.iter().zip(&before) {
            assert!(read(&r, false, *at, data.len()) == *data, "at {at:#x}");
            assert!(read(&r, true, *at, data.len()) == *old, "at {at:#x}");
        }
        assert_eq!(r.journal().unwrap().dirty_runs(), 0);

        w.destage().unwrap();
        let r = pool.open_space(id).unwrap();
        assert_eq!(r.cache().unwrap().cached_chunks(), 0);
        for (at, data) in &writes {
            assert!(read(&r, true, *at, data.len()) == *data, "at {at:#x}");
        }
        let journal = r.journal().unwrap();
        for s in [0, 4, 6, 7] {
            let loc = layout.locate(s * stripe);
            assert!(stripe_matches(&pool, &layout, s * stripe), "stripe {s}");
            assert!(
                !journal.is_dirty(layout.run_start_offset(loc.row), layout.stripe_of(&loc)),
                "stripe {s}"
            );
        }
        // The writer keeps going after the destage, with blocks from 64 again.
        let again = pattern(8192, 4096, "again");
        w.write_all_at(&again, 8192).unwrap();
        w.flush().unwrap();
        let r = pool.open_space(id).unwrap();
        assert_eq!(r.cache().unwrap().mappings()[0].1, 64);
        assert!(read(&r, false, 8192, 4096) == again);
    }

    /// Whole stripes the journal records as not consistent (here: after a
    /// slot saying so) bypass the cache and are recorded as consistent
    /// afterwards, as Windows does; a part of such a stripe is cached.
    #[test]
    fn whole_stripes_not_recorded_as_consistent_bypass_the_cache() {
        use crate::format::SpaceRole;
        let pool = fixture("parity3_26100");
        let space = pool.find_space("parity3_26100").unwrap();
        let id = space.id();
        let r = pool.open_space(id).unwrap();
        let layout = r.layout().clone();
        let stripe = layout.data_columns * layout.interleave;
        let journal = r.journal().unwrap();
        let (slot_offset, slot_size, _) = journal.geometry();
        let mut jw = journal.writer(space.info.guid);
        let (index, page) = jw.mark(0, 16384, 100, 4, false);
        let (jl, _) = super::SpaceWriter::child_space(&pool, id, SpaceRole::Other(0x0a)).unwrap();
        super::write_copies(
            &pool,
            &jl,
            jl.base + slot_offset + index as u64 * slot_size as u64,
            &page,
        )
        .unwrap();
        drop(r);

        let w = pool.open_space_rw(id).unwrap();
        let whole = pattern(100 * stripe, 2 * stripe, "direct");
        w.write_all_at(&whole, 100 * stripe).unwrap();
        let part = pattern(103 * stripe, 4096, "part");
        w.write_all_at(&part, 103 * stripe).unwrap();
        w.flush().unwrap();
        let r = pool.open_space(id).unwrap();
        let cached: Vec<u64> = r.cache().unwrap().mappings().iter().map(|m| m.0).collect();
        assert_eq!(cached, [103 * stripe]);
        let journal = r.journal().unwrap();
        let dirty: Vec<bool> = (100..104).map(|s| journal.is_dirty(0, s)).collect();
        assert_eq!(dirty, [false, false, true, true]);
        for s in [100, 101] {
            assert!(stripe_matches(&pool, &layout, s * stripe), "stripe {s}");
        }
        let mut back = vec![0u8; whole.len()];
        r.read_uncached_at(&mut back, 100 * stripe).unwrap();
        assert!(back == whole);
    }

    /// Every state a crash can leave while a single parity space is written
    /// through the cache and destaged. The writes and flushes of all members
    /// are recorded in order; a crash state keeps, per member, the writes
    /// before its last flush and any subset of the later ones (the one in
    /// order and a few random ones are replayed onto fresh copies of the
    /// disks). In each, every touched stripe either matches its parity or is
    /// held whole by the cache in every version of its log (the write hole
    /// is closed), the space reads, each sector holds one of the versions
    /// written to it, and none older than what was flushed. Where a log slot
    /// reached one copy of the cache only, the reader refuses the chunk
    /// unless told to take the newer version; the older is the state
    /// without that slot, checked as well.
    #[test]
    fn parity_writes_close_the_write_hole_in_every_crash_state() {
        use crate::cache::Validity;
        use crate::io::{DeviceEvent, Recorder, WriteAt};
        use crate::reader::{OpenOptions, UncleanParity};
        use std::sync::{Arc, Mutex};

        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/parity3_26100");
        let images: Vec<SparseImage> = (0..)
            .map_while(|i| File::open(dir.join(format!("disk{i}.fixture"))).ok())
            .map(|f| SparseImage::read_from(f).unwrap())
            .collect();
        let log = Arc::new(Mutex::new(Vec::new()));
        let disks: Vec<Recorder<Overlay<&SparseImage>>> = images
            .iter()
            .enumerate()
            .map(|(i, img)| Recorder::new(Overlay::new(img), i, log.clone()))
            .collect();
        let pool = Pool::open(disks).unwrap();
        let id = pool.find_space("parity3_26100").unwrap().id();
        let w = pool.open_space_rw(id).unwrap();
        let layout = w.reader().layout().clone();
        let stripe = layout.data_columns * layout.interleave;
        // (offset, length, tag, flush afterwards): parts of stripes, a whole
        // stripe, a rewrite of cached sectors, one across two stripes.
        let steps = [
            (4096u64, 8192u64, "a", false),
            (3 * stripe, stripe, "b", true),
            (4096, 4096, "c", false),
            (9 * stripe - 8192, 16384, "d", true),
        ];
        let span = 10 * stripe;
        let mut original = vec![0u8; span as usize];
        w.read_exact_at(&mut original, 0).unwrap();
        // Per sector: the versions written to it and the event count after
        // which each is durable.
        let mut versions: Vec<Vec<Vec<u8>>> = original.chunks(512).map(|s| vec![s.to_vec()]).collect();
        let mut durable: Vec<Vec<usize>> = vec![vec![0]; versions.len()];
        let mut touched = std::collections::BTreeSet::new();
        for (at, len, tag, flush) in steps {
            let data = pattern(at, len, tag);
            w.write_all_at(&data, at).unwrap();
            for (i, s) in data.chunks(512).enumerate() {
                versions[(at / 512) as usize + i].push(s.to_vec());
                durable[(at / 512) as usize + i].push(usize::MAX);
            }
            touched.extend(at / stripe..(at + len).div_ceil(stripe));
            if flush {
                w.flush().unwrap();
                let n = log.lock().unwrap().len();
                for d in &mut durable {
                    let last = d.last_mut().unwrap();
                    *last = (*last).min(n);
                }
            }
        }
        w.destage().unwrap();
        drop(w);
        drop(pool);
        let events = log.lock().unwrap().clone();

        let check = |k: usize, writes: &[&DeviceEvent], held_whole: &mut usize| {
            let replay: Vec<Overlay<&SparseImage>> = images.iter().map(Overlay::new).collect();
            for e in writes {
                if let DeviceEvent::Write { device, offset, data } = e {
                    replay[*device].write_all_at(data, *offset).unwrap();
                }
            }
            let pool = Pool::open(replay.iter().collect::<Vec<_>>()).unwrap();
            let r = pool.open_space(id).unwrap();
            let cache = r.cache().unwrap();
            let whole: Vec<u64> = cache
                .mappings()
                .iter()
                .filter(|m| *m.2 == Validity::Full && !cache.is_ambiguous(m.0))
                .map(|m| m.0 / stripe)
                .collect();
            for &s in &touched {
                if !stripe_matches(&pool, &layout, s * stripe) {
                    assert!(
                        whole.contains(&s),
                        "state {k}: stripe {s} neither matches its parity nor is cached whole"
                    );
                    *held_whole += 1;
                }
            }
            let mut content = vec![0u8; span as usize];
            if let Err(e) = r.read_exact_at(&mut content, 0) {
                assert!(cache.conflicting_chunks() > 0, "state {k}: {e}");
                let newest = OpenOptions {
                    unclean_parity: UncleanParity::PreferData,
                };
                pool.open_space_with(id, newest)
                    .unwrap()
                    .read_exact_at(&mut content, 0)
                    .unwrap();
            }
            for (i, sector) in content.chunks(512).enumerate() {
                let floor = durable[i].iter().rposition(|&d| d <= k).unwrap();
                assert!(
                    versions[i][floor..].iter().any(|v| v == sector),
                    "state {k}: sector {i} holds none of versions {floor}.. of {}",
                    versions[i].len()
                );
            }
            (cache.cached_chunks(), r.journal().unwrap().dirty_runs())
        };
        let mut held_whole = 0;
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        for k in 0..=events.len() {
            // Per member, the writes after its last flush may be lost.
            let mut last_flush = vec![0usize; images.len()];
            for (i, e) in events[..k].iter().enumerate() {
                if let DeviceEvent::Flush { device } = e {
                    last_flush[*device] = i;
                }
            }
            let loose: Vec<usize> = (0..k)
                .filter(|&i| matches!(&events[i], DeviceEvent::Write { device, .. } if i > last_flush[*device]))
                .collect();
            let all: Vec<&DeviceEvent> = events[..k].iter().collect();
            let (cached, dirty_runs) = check(k, &all, &mut held_whole);
            if k == events.len() {
                assert_eq!((cached, dirty_runs), (0, 0));
            }
            for _ in 0..if loose.len() > 1 { 2 } else { 0 } {
                let kept: Vec<&DeviceEvent> = (0..k)
                    .filter(|i| {
                        seed ^= seed << 13;
                        seed ^= seed >> 7;
                        seed ^= seed << 17;
                        !loose.contains(i) || seed & 1 == 0
                    })
                    .map(|i| &events[i])
                    .collect();
                check(k, &kept, &mut held_whole);
            }
        }
        assert!(held_whole > 0, "no state had a stripe held only by the cache");
    }

    #[test]
    fn spaces_that_writes_cannot_keep_consistent_are_refused() {
        for (pool, space, why) in [("dual7", "dual7", "dual parity"), ("tiered", "tiered", "tiered spaces")] {
            let pool = fixture(pool);
            let id = pool.find_space(space).unwrap().id();
            let err = pool.open_space_rw(id).err().unwrap().to_string();
            assert!(err.contains(why), "{space}: {err}");
        }
        // A missing disk makes the pool unclean.
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/simple2c");
        let one = vec![Overlay::new(
            SparseImage::read_from(File::open(dir.join("disk0.fixture")).unwrap()).unwrap(),
        )];
        let pool = Pool::open(one).unwrap();
        let id = pool.find_space("simple2c").unwrap().id();
        assert!(
            pool.open_space_rw(id)
                .err()
                .unwrap()
                .to_string()
                .contains("not in a clean state")
        );
    }
}
