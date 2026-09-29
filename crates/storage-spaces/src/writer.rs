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
//! sectors are written at the next flush, once that data is durable, with a
//! checkpoint of the whole map before the log comes round to the slot the
//! last checkpoint continues at (flushed after everything before it and
//! before anything after it, see `cache::Checkpoint`). When
//! the log or the blocks run out, every cached chunk is destaged: chunks
//! with sectors missing are made whole in the cache first (the rest of the
//! stripe is read from the space and logged), then written as whole stripes
//! (recorded as not consistent, flushed, written with their parity,
//! flushed, recorded as consistent) and removed from the cache with
//! tombstone entries, flushed before their blocks are handed out again. At
//! every point either a stripe on disk matches its parity or the cache holds
//! all of its data: the write hole stays closed.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Mutex, RwLock, RwLockReadGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::cache::{CacheWriter, LogWrite};
use crate::database::Database;
use crate::drt::DrtWriter;
use crate::error::{Error, Result};
use crate::format::{
    DATA_AREA_OFFSET, DiskUsage, ExtentRecord, Policy, Provisioning, Resiliency, SLAB_SIZE, SpaceRole,
};
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
    /// The layout of the space with the rows allocated since it was opened
    /// (the reader knows only those allocated before).
    layout: RwLock<Layout>,
    /// Thin spaces: allocates rows as writes first reach them.
    alloc: Option<Mutex<Allocator>>,
    drt: Option<DrtState>,
    journal: Option<JournalState>,
    cache: Option<CacheState>,
}

/// Allocates rows of a thin space as Windows does (format document, "Slab
/// allocation"): one update of the pool database per row, with one extent
/// record per column and copy at the first free slab of a disk the row
/// does not use yet, written to every member (each flushed before the
/// next) before the row is written. New slabs are not cleared.
struct Allocator {
    db: Database,
    policy: Policy,
    base: u64,
    extents: Vec<ExtentRecord>,
}

/// Where the log of a write-back cache or parity journal lives: the layout
/// of its space, its slots and its checkpoint areas.
struct LogArea {
    layout: Layout,
    slot_offset: u64,
    slot_size: u64,
    checkpoint_offset: u64,
    checkpoint_size: u64,
}

/// The parity journal of a parity space being written.
struct JournalState {
    area: LogArea,
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
    /// The cache space (its layout also locates the data blocks).
    area: LogArea,
    /// Held for writing by every write of a parity space and while
    /// destaging, for reading by reads (a cache block may be handed to
    /// another chunk once its chunk is destaged).
    log: RwLock<CacheLog>,
}

struct CacheLog {
    writer: CacheWriter,
    /// Log writes of cached writes whose data may not be durable yet,
    /// written at the next flush.
    pending: Vec<LogWrite>,
}

impl<'p, D: WriteAt> SpaceWriter<'p, D> {
    pub(crate) fn new(pool: &'p Pool<D>, id: u64) -> Result<Self> {
        if let Some(why) = pool.write_refusal(id)? {
            return Err(Error::Unsupported(format!(
                "space {id} cannot be opened for writing: {why}"
            )));
        }
        let reader = pool.open_space_with(id, OpenOptions::default())?;
        let layout = reader.layout();
        let (mut drt, mut journal) = (None, None);
        if let Some(log) = reader
            .dirty_regions()
            .filter(|_| layout.resiliency == Resiliency::Mirror)
        {
            let (layout, size) = Self::child_space(pool, id, SpaceRole::Other(0x06))?;
            drt = Some(DrtState {
                layout,
                size,
                log: Mutex::new((log.writer(), HashMap::new())),
            });
        }
        if let Some(pj) = reader.journal().filter(|_| layout.resiliency == Resiliency::Parity) {
            let (jlayout, _) = Self::child_space(pool, id, SpaceRole::Other(0x0a))?;
            let (slot_offset, slot_size, _) = pj.geometry();
            journal = Some(JournalState {
                area: LogArea {
                    slot_offset: jlayout.base + slot_offset,
                    slot_size: slot_size as u64,
                    checkpoint_offset: jlayout.base + pj.checkpoint_geometry().0,
                    checkpoint_size: pj.checkpoint_geometry().1 as u64,
                    layout: jlayout,
                },
                writer: Mutex::new(pj.writer(reader.space.info.guid)),
            });
        }
        let mut cache = None;
        if let Some(index) = reader.cache() {
            let (clayout, _) = Self::child_space(pool, id, SpaceRole::Cache)?;
            // Parity caches hand out blocks from 64, mirror caches from 0.
            let first_block = if journal.is_some() { 64 } else { 0 };
            cache = Some(CacheState {
                area: LogArea {
                    layout: clayout,
                    slot_offset: index.header.slot_offset,
                    slot_size: index.header.slot_size as u64,
                    checkpoint_offset: index.header.checkpoint_offset,
                    checkpoint_size: index.header.checkpoint_size as u64,
                },
                log: RwLock::new(CacheLog {
                    writer: index.writer(first_block),
                    pending: Vec::new(),
                }),
            });
        }
        let space = reader.space;
        let alloc = match (space.info.provisioning, space.info.policy) {
            (Provisioning::Thin, Some(policy)) if space.info.allocation_unit == SLAB_SIZE => {
                Some(Mutex::new(Allocator {
                    db: pool.database_model()?,
                    policy,
                    base: space.info.range.map_or(0, |(start, _)| start),
                    extents: space.extents.clone(),
                }))
            }
            _ => None,
        };
        let mut writer = SpaceWriter {
            layout: RwLock::new(reader.layout().clone()),
            reader,
            alloc,
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

    /// The layout of the space, with the rows allocated since it was opened.
    fn layout(&self) -> RwLockReadGuard<'_, Layout> {
        self.layout.read().unwrap()
    }

    /// Allocates `row` of a thin space if it is not allocated yet (see
    /// [`Allocator`]).
    fn allocate(&self, row: u64) -> Result<()> {
        let Some(alloc) = &self.alloc else {
            return Err(Error::Unsupported(format!(
                "write into row {row}, which the space has not allocated (allocation is supported for thin spaces \
                 with 256 MiB allocation units)"
            )));
        };
        let mut a = alloc.lock().unwrap();
        if row_allocated(&self.layout(), row) {
            return Ok(()); // allocated while waiting for the lock
        }
        let pool = self.reader.pool();
        let layout = self.layout().clone();
        let mut used = BTreeSet::new();
        let mut records = Vec::new();
        for column in 0..a.policy.columns {
            for copy in 0..a.policy.copies.max(1) {
                let disk_id = pick_disk(pool, &a.db, &layout, column, copy, row, &used)?;
                used.insert(disk_id);
                records.push(ExtentRecord {
                    space_id: self.reader.space.id(),
                    virtual_slab: a.base / SLAB_SIZE + row * layout.data_columns,
                    column,
                    copy,
                    slab_count: 1,
                    disk_id,
                    physical_slab: a.db.first_free_slab(disk_id),
                    flags: 0,
                    stale_marker: 0xffff_ffff,
                });
            }
        }
        let sequence = a.db.sequence() + 1;
        let bodies: Vec<Vec<u8>> = records.iter().map(|r| r.encode(sequence)).collect();
        let writes: Vec<(u8, u8, &[u8])> = bodies.iter().map(|b| (4, 6, b.as_slice())).collect();
        let mut db = a.db.clone();
        while db.update(&writes, &[]).is_none() {
            // No run of free slots is long enough: format another page.
            if a.db.bytes().len() >= 4 << 20 {
                return Err(Error::Pool("the pool database is full".into()));
            }
            a.db.grow();
            db = a.db.clone();
        }
        db.commit(sequence, filetime_now());
        pool.write_database(&db)?;
        a.db = db;
        a.extents.extend(records);
        *self.layout.write().unwrap() = Layout::with_base(&a.policy, &a.extents, a.base)?;
        Ok(())
    }

    /// Allocates the rows of `offset..offset + len` a thin space has not
    /// allocated yet, in order.
    fn allocate_range(&self, offset: u64, len: u64) -> Result<()> {
        let row_bytes = SLAB_SIZE * self.layout().data_columns;
        let base = self.layout().base;
        for row in (offset - base) / row_bytes..(offset - base + len).div_ceil(row_bytes) {
            if !row_allocated(&self.layout(), row) {
                self.allocate(row)?;
            }
        }
        Ok(())
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
            None if offset.checked_add(buf.len() as u64).is_none_or(|end| end > self.size()) => {
                Err(Error::Io(std::io::ErrorKind::UnexpectedEof.into()))
            }
            None => self.read_space(buf, offset),
        }
    }

    /// Reads the space itself (not its cache): through the reader, except
    /// rows allocated since the space was opened, which the reader does not
    /// know (read from their first copy).
    fn read_space(&self, mut buf: &mut [u8], mut offset: u64) -> Result<()> {
        if self.alloc.is_none() {
            return self.reader.read_uncached_at(buf, offset);
        }
        while !buf.is_empty() {
            let layout = self.layout();
            let loc = layout.locate(offset);
            let n = buf.len().min(loc.contiguous as usize);
            if row_allocated(&layout, loc.row) && !row_allocated(self.reader.layout(), loc.row) {
                read_first_copy(self.reader.pool(), &layout, offset, &mut buf[..n])?;
            } else {
                drop(layout);
                self.reader.read_uncached_at(&mut buf[..n], offset)?;
            }
            offset += n as u64;
            buf = &mut buf[n..];
        }
        Ok(())
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
                None => self.read_space(&mut buf[..n], offset)?,
            }
            offset += n as u64;
            buf = &mut buf[n..];
        }
        Ok(())
    }

    /// Reads from the cache space (its first copy at hand).
    fn read_cache(&self, cache: &CacheState, offset: u64, buf: &mut [u8]) -> Result<()> {
        read_first_copy(self.reader.pool(), &cache.area.layout, offset, buf)
    }

    /// Writes all of `buf` at `offset` of the space. A thin space allocates
    /// the rows the write reaches first ([`Allocator`]).
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
        let pool = self.reader.pool();
        while !buf.is_empty() {
            let row = self.layout().locate(offset).row;
            if !row_allocated(&self.layout(), row) {
                self.allocate(row)?;
            }
            let layout = self.layout();
            let loc = layout.locate(offset);
            let n = buf.len().min(loc.contiguous as usize);
            if let Some(drt) = &self.drt {
                self.mark_dirty(drt, layout.run_start_offset(loc.row) / SLAB_SIZE)?;
            }
            write_copies(pool, &layout, offset, &buf[..n])?;
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
        let layout = self.layout();
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
            // Rows not allocated yet go to the cache, allocated when it
            // destages (as Windows does, m5thinwbc).
            let direct = row_allocated(&layout, loc.row)
                && at == start
                && next == start + stripe
                && log.writer.chunk_runs(start).is_none()
                && !journal.writer.lock().unwrap().is_consistent(
                    layout.run_start_offset(loc.row),
                    run_rows(&layout, loc.row)? * (SLAB_SIZE / layout.interleave),
                    layout.stripe_of(&loc),
                );
            match parts.last_mut() {
                Some((d, _, e)) if *d == direct => *e = next,
                _ => parts.push((direct, at, next)),
            }
            at = next;
        }
        drop(layout);
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
            write_copies(
                pool,
                &cache.area.layout,
                target,
                &buf[(at - start) as usize..][..n as usize],
            )?;
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
                        self.read_space(part, offset + at)?;
                        if self.journal.is_some() {
                            write_copies(pool, &cache.area.layout, base + at, part)?;
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
                        let records = log.writer.fill(&partial);
                        pool.flush_members()?;
                        self.write_log(&cache.area, &records)?;
                        pool.flush_members()?;
                    }
                    // Rows of a thin space are allocated as they are destaged.
                    for &(offset, _) in &parts {
                        self.allocate_range(offset, chunk)?;
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
            let records = log.writer.destage(&offsets);
            self.write_log(&cache.area, &records)?;
            pool.flush_members()?;
        }
        Ok(())
    }

    /// Writes the log records of cached writes once their data is durable.
    fn write_pending(&self, cache: &CacheState, log: &mut CacheLog) -> Result<()> {
        if log.pending.is_empty() {
            return Ok(());
        }
        self.reader.pool().flush_members()?;
        let records = std::mem::take(&mut log.pending);
        self.write_log(&cache.area, &records)
    }

    /// Writes log records in order; a checkpoint reaches the disks after
    /// everything before it and before anything after it.
    fn write_log(&self, log: &LogArea, records: &[LogWrite]) -> Result<()> {
        let pool = self.reader.pool();
        for record in records {
            match record {
                LogWrite::Slot(index, page) => {
                    let at = log.slot_offset + *index as u64 * log.slot_size;
                    write_copies(pool, &log.layout, at, page)?;
                }
                LogWrite::Checkpoint(area, page) => {
                    pool.flush_members()?;
                    let at = log.checkpoint_offset + *area as u64 * log.checkpoint_size;
                    write_copies(pool, &log.layout, at, page)?;
                    pool.flush_members()?;
                }
            }
        }
        Ok(())
    }

    /// Writes whole stripes of a single parity space in place: `parts` are
    /// (offset, data) of whole stripes. Those the journal records as
    /// consistent (or all, in a run the journal has no entry for) are
    /// recorded as not consistent first; afterwards all are recorded as
    /// consistent.
    fn write_stripes(&self, journal: &JournalState, parts: &[(u64, &[u8])]) -> Result<()> {
        let layout = self.layout();
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
                let run_stripes = run_rows(&layout, loc.row)? * per_row;
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
                let records = writer.mark_stripes(run_start, *run_stripes, list, false);
                self.write_log(&journal.area, &records)?;
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
                self.write_unit(&layout, column, loc.row, loc.offset_in_slab, d)?;
            }
            self.write_unit(
                &layout,
                layout.parity_column(stripe),
                loc.row,
                loc.offset_in_slab,
                &parity,
            )?;
        }
        pool.flush_members()?;
        for (run_start, (run_stripes, list)) in by_run {
            let records = writer.mark_stripes(run_start, run_stripes, &list, true);
            self.write_log(&journal.area, &records)?;
        }
        Ok(())
    }

    fn write_unit(&self, layout: &Layout, column: u64, row: u64, offset_in_slab: u64, buf: &[u8]) -> Result<()> {
        let (disk, slab) = layout
            .physical(column, 0, row)
            .ok_or_else(|| Error::Pool(format!("row {row} of column {column} is not allocated")))?;
        if !self.reader.pool().write_slab(disk, slab, offset_in_slab, buf)? {
            return Err(Error::Pool(format!("disk {disk} of the space is not present")));
        }
        Ok(())
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

/// Why the space `reader` reads cannot be opened for writing, if it cannot
/// (its pool being clean): everything about its state has to be understood
/// and kept consistent the way Windows would. Nothing is written.
pub(crate) fn refusal<D: ReadAt>(pool: &Pool<D>, reader: &SpaceReader<'_, D>) -> Option<String> {
    let id = reader.space.id();
    if reader.condition() != Condition::Healthy {
        return Some(format!("it is {:?}", reader.condition()).to_lowercase());
    }
    if pool.children(id).any(|c| c.info.is_child && !c.extents.is_empty()) {
        return Some("writes to tiered spaces are not supported yet".into());
    }
    let layout = reader.layout();
    match layout.resiliency {
        Resiliency::Simple => {}
        Resiliency::Mirror if reader.dirty_regions().is_none() => {
            return Some("the mirror space has no readable dirty region log".into());
        }
        Resiliency::Mirror => {}
        Resiliency::Parity if layout.parity_units > 1 => {
            return Some("writes to dual parity spaces are not supported yet".into());
        }
        Resiliency::Parity if reader.journal().is_none() => {
            return Some("the parity space has no readable parity journal".into());
        }
        Resiliency::Parity if reader.cache().is_none() => {
            return Some("the parity space has no write-back cache, which its writes need".into());
        }
        Resiliency::Parity if reader.journal().is_some_and(|j| j.checkpoint_geometry().2 == 0) => {
            return Some("its parity journal has no checkpoint areas".into());
        }
        Resiliency::Parity => {}
        other => return Some(format!("writes to {other:?} spaces are not supported yet").to_lowercase()),
    }
    if let Some(index) = reader.cache() {
        if index.conflicting_chunks() > 0 {
            return Some(format!(
                "the copies of its write-back cache disagree about {} chunks after an unclean shutdown",
                index.conflicting_chunks()
            ));
        }
        let allocates =
            reader.space.info.provisioning == Provisioning::Thin && reader.space.info.allocation_unit == SLAB_SIZE;
        if let Some((offset, _, _)) = index
            .mappings()
            .into_iter()
            .find(|m| !allocates && !row_allocated(layout, layout.locate(layout.base + m.0).row))
        {
            return Some(format!(
                "its write-back cache holds data at {offset:#x}, in a row the thin space has not allocated \
                 (allocation is not supported yet)"
            ));
        }
        if index.header.checkpoint_count == 0 {
            return Some("its write-back cache has no checkpoint areas".into());
        }
    }
    None
}

/// The disk for the slab of `column` and `copy` of a new `row`: the one
/// that column and copy use in the nearest allocated row if it has a free
/// slab (Windows' choice does not follow from the metadata), else the
/// present disk with the most free slabs, never one of `used` (the row's
/// other slabs).
fn pick_disk<D: ReadAt>(
    pool: &Pool<D>,
    db: &Database,
    layout: &Layout,
    column: u64,
    copy: u64,
    row: u64,
    used: &BTreeSet<u64>,
) -> Result<u64> {
    let free = |disk_id: u64| -> u64 {
        let Some(m) = pool
            .disks
            .get(&disk_id)
            .and_then(|d| d.member)
            .map(|m| &pool.members[m])
        else {
            return 0;
        };
        let capacity = m.partition.length.saturating_sub(DATA_AREA_OFFSET) / SLAB_SIZE;
        capacity.saturating_sub(db.first_free_slab(disk_id))
    };
    let preferred = layout
        .runs()
        .get(&(column, copy))
        .and_then(|runs| runs.iter().min_by_key(|r| r.first_row.abs_diff(row)))
        .map(|r| r.disk_id);
    let mut candidates: Vec<(bool, u64, u64)> = pool
        .disks
        .values()
        .filter(|d| d.member.is_some() && matches!(d.usage, DiskUsage::AutoSelect | DiskUsage::ManualSelect))
        .map(|d| (Some(d.id) != preferred, u64::MAX - free(d.id), d.id))
        .collect();
    candidates.sort();
    candidates
        .into_iter()
        .map(|c| c.2)
        .find(|&d| !used.contains(&d) && free(d) > 0)
        .ok_or_else(|| Error::Pool(format!("no disk has a free slab for row {row} (the pool is full)")))
}

/// The current time as a FILETIME (100 ns since 1601), as the pool database
/// stores it.
fn filetime_now() -> u64 {
    let since_1970 = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    (since_1970.as_nanos() / 100) as u64 + 11_644_473_600 * 10_000_000
}

/// Rows of the extent run holding `row`.
fn run_rows(layout: &Layout, row: u64) -> Result<u64> {
    layout
        .runs()
        .get(&(0, 0))
        .and_then(|runs| runs.iter().find(|r| r.first_row <= row && row < r.first_row + r.rows))
        .map(|r| r.rows)
        .ok_or_else(|| Error::Pool(format!("row {row} is not allocated")))
}

/// Reads `buf` at `offset` of a space from the first copy at hand.
fn read_first_copy<D: ReadAt>(pool: &Pool<D>, layout: &Layout, mut offset: u64, mut buf: &mut [u8]) -> Result<()> {
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
            return Err(Error::Pool(format!("offset {offset:#x} is not readable")));
        }
        offset += n as u64;
        buf = &mut buf[n..];
    }
    Ok(())
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

    use crate::format::SLAB_SIZE;
    use crate::io::{Overlay, SparseImage};
    use crate::pool::Pool;
    use crate::testpattern::{BLOCK, fill_block};
    use std::collections::BTreeSet;

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
        let records = jw.mark(0, 16384, 100, 4, false);
        let (index, page) = records[0].slot().unwrap();
        let (jl, _) = super::SpaceWriter::child_space(&pool, id, SpaceRole::Other(0x0a)).unwrap();
        super::write_copies(
            &pool,
            &jl,
            jl.base + slot_offset + index as u64 * slot_size as u64,
            page,
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

    /// A cache log that wraps with chunks still mapped by the slots it is
    /// about to overwrite gets a checkpoint first (Windows reads the slots
    /// before the newest only up to a checkpoint). 1100 writes of 4 KiB
    /// into distinct chunks, flushed every 50, fill the log, destage once
    /// and wrap; the crash states around the checkpoint and after every
    /// flush are replayed: each chunk reads as before or as written, and
    /// as written once flushed.
    #[test]
    fn a_wrapping_cache_log_writes_a_checkpoint_first() {
        use crate::io::{DeviceEvent, Recorder, WriteAt};
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
        let chunk = w.reader().cache().unwrap().header.chunk_size as u64;
        // (offset, old, new, event count after which new is durable)
        let mut writes = Vec::new();
        for i in 0..1100u64 {
            let at = (16 + 7 * i) * chunk;
            let mut old = vec![0u8; BLOCK];
            w.read_exact_at(&mut old, at).unwrap();
            let new = pattern(at, BLOCK as u64, &format!("w{i}"));
            w.write_all_at(&new, at).unwrap();
            writes.push((at, old, new, usize::MAX));
            if i % 50 == 49 || i == 1099 {
                w.flush().unwrap();
                let n = log.lock().unwrap().len();
                writes.iter_mut().filter(|w| w.3 == usize::MAX).for_each(|w| w.3 = n);
            }
        }
        drop(w);
        drop(pool);
        let events = log.lock().unwrap().clone();
        let checkpoint = events
            .iter()
            .position(|e| matches!(e, DeviceEvent::Write { data, .. } if data.starts_with(b"SPCHECK")))
            .expect("the log wrapped without a checkpoint");

        let check = |k: usize| {
            let replay: Vec<Overlay<&SparseImage>> = images.iter().map(Overlay::new).collect();
            for e in &events[..k] {
                if let DeviceEvent::Write { device, offset, data } = e {
                    replay[*device].write_all_at(data, *offset).unwrap();
                }
            }
            let pool = Pool::open(replay.iter().collect::<Vec<_>>()).unwrap();
            let mut r = pool.open_space(id).unwrap();
            if r.cache().unwrap().conflicting_chunks() > 0 {
                // A log write that reached one copy only: the older version
                // is the state before it, checked as well.
                let newest = crate::reader::OpenOptions {
                    unclean_parity: crate::reader::UncleanParity::PreferData,
                };
                r = pool.open_space_with(id, newest).unwrap();
            }
            for (at, old, new, durable) in &writes {
                let mut b = vec![0u8; BLOCK];
                r.read_exact_at(&mut b, *at)
                    .unwrap_or_else(|e| panic!("state {k}: {e}"));
                if k >= *durable {
                    assert!(b == *new, "state {k}: flushed write at {at:#x} lost");
                } else {
                    assert!(b == *old || b == *new, "state {k}: {at:#x} holds neither version");
                }
            }
            r.cache().unwrap().checkpoint().map(|c| c.sequence)
        };
        let flushed: Vec<usize> = (1..=events.len())
            .filter(|&k| matches!(events[k - 1], DeviceEvent::Flush { .. }))
            .collect();
        let mut states: Vec<usize> = (checkpoint.saturating_sub(4)..(checkpoint + 12).min(events.len())).collect();
        states.extend(flushed.iter().copied().step_by(3));
        states.push(events.len());
        states.sort();
        states.dedup();
        for &k in &states {
            check(k);
        }
        assert!(check(events.len()).is_some(), "no checkpoint in the final state");
    }

    fn scenario(name: &str, label: &str) -> Vec<SparseImage> {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/scenarios")
            .join(name)
            .join(label);
        (0..)
            .map_while(|i| File::open(dir.join(format!("disk{i}.fixture"))).ok())
            .map(|f| SparseImage::read_from(f).unwrap())
            .collect()
    }

    /// The pool database of every member, as the model reads it.
    fn member_databases<D: crate::io::ReadAt>(pool: &Pool<D>) -> Vec<crate::database::Database> {
        pool.members
            .iter()
            .filter(|m| m.db_sequence.is_some())
            .map(|m| {
                crate::database::Database::read_formatted(
                    &pool.devices[m.device],
                    m.partition.offset + crate::format::POOL_DB_OFFSET,
                )
                .unwrap()
            })
            .collect()
    }

    /// A write into a row a thin simple space has not allocated (m5thin
    /// s0, 4 KiB at 2 GiB) allocates it first: one update of every
    /// member's pool database, byte for byte the model of Windows' updates
    /// (one extent at the first free slab of the disk chosen), then the
    /// data; the pool reads it back.
    #[test]
    fn thin_rows_are_allocated_on_their_first_write() {
        let images = scenario("m5thin", "s0");
        let disks: Vec<Overlay<&SparseImage>> = images.iter().map(Overlay::new).collect();
        let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
        let id = pool.find_space("m5thin").unwrap().id();
        let before = pool.database_model().unwrap();
        let w = pool.open_space_rw(id).unwrap();
        let data = pattern(2 << 30, BLOCK as u64, "a");
        w.write_all_at(&data, 2 << 30).unwrap();
        let mut back = vec![0u8; BLOCK];
        w.read_exact_at(&mut back, 2 << 30).unwrap();
        assert!(back == data);
        w.flush().unwrap();
        drop(w);
        drop(pool);

        let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
        assert!(pool.warnings.is_empty(), "{:?}", pool.warnings);
        assert_eq!(pool.database.sequence, before.sequence() + 1);
        let r = pool.open_space(id).unwrap();
        r.read_exact_at(&mut back, 2 << 30).unwrap();
        assert!(back == data);
        let (disk, slab) = r.layout().physical(0, 0, 8).unwrap();
        assert_eq!(slab, before.first_free_slab(disk));
        let mut model = before.clone();
        let record = crate::format::ExtentRecord {
            space_id: id,
            virtual_slab: 8,
            column: 0,
            copy: 0,
            slab_count: 1,
            disk_id: disk,
            physical_slab: slab,
            flags: 0,
            stale_marker: 0xffff_ffff,
        }
        .encode(before.sequence() + 1);
        model.update(&[(4, 6, &record)], &[]).unwrap();
        model.commit(before.sequence() + 1, pool.database.timestamp);
        for db in member_databases(&pool) {
            assert!(db.bytes() == model.bytes());
        }
    }

    /// A thin two-way mirror (m5thinm s0) allocates both copies of a new
    /// row on different disks, lists the run in the dirty region log
    /// before writing it, and writes both copies.
    #[test]
    fn thin_mirror_rows_get_both_copies() {
        let images = scenario("m5thinm", "s0");
        let disks: Vec<Overlay<&SparseImage>> = images.iter().map(Overlay::new).collect();
        let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
        let id = pool.find_space("m5thinm").unwrap().id();
        let w = pool.open_space_rw(id).unwrap();
        let data = pattern(2 << 30, BLOCK as u64, "m");
        w.write_all_at(&data, 2 << 30).unwrap();
        w.flush().unwrap();
        drop(w);
        drop(pool);
        let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
        let r = pool.open_space(id).unwrap();
        let layout = r.layout();
        let copies: Vec<(u64, u64)> = (0..2).map(|c| layout.physical(0, c, 8).unwrap()).collect();
        assert_ne!(copies[0].0, copies[1].0);
        for (disk, slab) in copies {
            let mut b = vec![0u8; BLOCK];
            pool.read_slab(disk, slab, 0, &mut b).unwrap();
            assert!(b == data);
        }
        assert!(r.dirty_regions().unwrap().is_dirty(8));
    }

    /// A thin parity space with a cache (m5thinwbc s0): a write into an
    /// unallocated row stays in the cache without allocating, as Windows
    /// does; destaging allocates the row (one extent per column) and writes
    /// the stripe with its parity.
    #[test]
    fn thin_parity_rows_are_allocated_when_destaged() {
        let images = scenario("m5thinwbc", "s0");
        let disks: Vec<Overlay<&SparseImage>> = images.iter().map(Overlay::new).collect();
        let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
        let id = pool.find_space("m5thinwbc").unwrap().id();
        let sequence = pool.database.sequence;
        let w = pool.open_space_rw(id).unwrap();
        let data = pattern(2 << 30, BLOCK as u64, "p");
        w.write_all_at(&data, 2 << 30).unwrap();
        w.flush().unwrap();
        assert_eq!(
            member_databases(&pool)[0].sequence(),
            sequence,
            "the cached write allocated"
        );
        w.destage().unwrap();
        drop(w);
        drop(pool);
        let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
        assert_eq!(pool.database.sequence, sequence + 1);
        let r = pool.open_space(id).unwrap();
        assert_eq!(r.cache().unwrap().cached_chunks(), 0);
        let layout = r.layout().clone();
        let row = (2u64 << 30) / (SLAB_SIZE * layout.data_columns);
        let disks: BTreeSet<u64> = (0..layout.columns)
            .map(|c| layout.physical(c, 0, row).unwrap().0)
            .collect();
        assert_eq!(disks.len(), 3);
        let mut back = vec![0u8; BLOCK];
        r.read_uncached_at(&mut back, 2 << 30).unwrap();
        assert!(back == data);
        assert!(stripe_matches(&pool, &layout, 2 << 30));
        assert!(!r.journal().unwrap().is_dirty(2 << 30, 0));
    }

    /// Every crash state of a write that allocates a thin row: the database
    /// copies are written one member after the other, each whole, so every
    /// state opens (at most with a stale copy) and reads zeros or the data,
    /// and the data once flushed.
    #[test]
    fn a_crash_while_allocating_leaves_a_readable_pool() {
        use crate::io::{DeviceEvent, Recorder, WriteAt};
        use std::sync::{Arc, Mutex};
        let images = scenario("m5thin", "s0");
        let log = Arc::new(Mutex::new(Vec::new()));
        let disks: Vec<Recorder<Overlay<&SparseImage>>> = images
            .iter()
            .enumerate()
            .map(|(i, img)| Recorder::new(Overlay::new(img), i, log.clone()))
            .collect();
        let pool = Pool::open(disks).unwrap();
        let id = pool.find_space("m5thin").unwrap().id();
        let w = pool.open_space_rw(id).unwrap();
        let data = pattern(3 << 30, BLOCK as u64, "c");
        w.write_all_at(&data, 3 << 30).unwrap();
        w.flush().unwrap();
        drop(w);
        drop(pool);
        let events = log.lock().unwrap().clone();
        let database_writes = events
            .iter()
            .filter(|e| matches!(e, DeviceEvent::Write { data, .. } if data.starts_with(b"SDBC")))
            .count();
        assert_eq!(database_writes, 3);
        for k in 0..=events.len() {
            let replay: Vec<Overlay<&SparseImage>> = images.iter().map(Overlay::new).collect();
            for e in &events[..k] {
                if let DeviceEvent::Write { device, offset, data } = e {
                    replay[*device].write_all_at(data, *offset).unwrap();
                }
            }
            let pool = Pool::open(replay.iter().collect::<Vec<_>>()).unwrap();
            assert!(
                pool.warnings.iter().all(|w| w.contains("stale pool database")),
                "state {k}: {:?}",
                pool.warnings
            );
            let r = pool.open_space(id).unwrap();
            let mut back = vec![0u8; BLOCK];
            r.read_exact_at(&mut back, 3 << 30).unwrap();
            if k == events.len() {
                assert!(back == data);
            } else {
                assert!(back == data || back.iter().all(|&b| b == 0), "state {k}");
            }
        }
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
        let err = pool.open_space_rw(id).err().unwrap().to_string();
        assert!(err.contains("not in a clean state"), "{err}");
        assert_eq!(
            pool.write_refusal(id)
                .unwrap()
                .map(|w| w.contains("not in a clean state")),
            Some(true)
        );
    }
}
