//! Writes to spaces (Stage 2 of docs/plan.md).
//!
//! A space opens for writing only when everything about its state is
//! understood, and only for what the writes can keep consistent the way
//! Windows would: so far in-place writes to allocated rows of simple,
//! mirror and single parity spaces. Everything else is refused with the
//! reason.
//!
//! Single parity writes go to the space itself (the write-back cache stays
//! empty) under the parity journal: a stripe recorded as consistent is first
//! recorded as not consistent (slot written and flushed), then its changed
//! data units and its parity, computed from all its data units, are
//! written and flushed, and the stripe is recorded as consistent again.
//! Stripes never written count as not consistent already.
//!
//! Mirror writes follow the dirty region log model of the format document
//! (`DrtWriter`): before the first write into an extent run that is not
//! listed, the next header is written into every copy of the tracking space
//! and flushed; then the data goes to every copy. Runs this writer wrote are
//! removed from the log at the next header write once idle for 30 s, as
//! Windows does; runs listed when the space was opened stay listed (their
//! copies may differ after a crash of Windows, and readers keep comparing
//! them).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::drt::DrtWriter;
use crate::error::{Error, Result};
use crate::format::{Resiliency, SLAB_SIZE, SpaceRole};
use crate::io::WriteAt;
use crate::journal::JournalWriter;
use crate::layout::{Condition, Layout};
use crate::pool::Pool;
use crate::reader::{OpenOptions, SpaceReader};

/// Idle time after which Windows removes a run from the dirty region log
/// (runs idle for 29 s stayed, 35 s went).
const DRT_CLEAN_AFTER: Duration = Duration::from_secs(30);

/// Writes to a space; reads go through [`SpaceWriter::reader`].
pub struct SpaceWriter<'p, D> {
    reader: SpaceReader<'p, D>,
    drt: Option<DrtState>,
    journal: Option<JournalState>,
}

/// The parity journal of a parity space being written.
struct JournalState {
    /// Layout of the journal space.
    layout: Layout,
    slot_offset: u64,
    slot_size: u64,
    /// Also serializes the writes to the space (read-modify-write of whole
    /// stripes).
    writer: Mutex<JournalWriter>,
}

/// The dirty region log of a mirror space being written.
struct DrtState {
    /// Layout and size of the tracking space.
    layout: Layout,
    size: u64,
    log: Mutex<(DrtWriter, HashMap<u64, Instant>)>,
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
        if let Some(cache) = reader.cache()
            && (cache.cached_chunks() > 0 || cache.conflicting_chunks() > 0)
        {
            return refuse(format!(
                "its write-back cache holds {} chunks (writing through the cache is not supported yet)",
                cache.cached_chunks()
            ));
        }
        Ok(SpaceWriter { reader, drt, journal })
    }

    /// Layout and size of the hidden child of space `id` under a container
    /// of role `role` (0x06: dirty region tracking, 0x0a: parity journal).
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

    pub fn reader(&self) -> &SpaceReader<'p, D> {
        &self.reader
    }

    pub fn size(&self) -> u64 {
        self.reader.size()
    }

    /// Writes all of `buf` at `offset` of the space. A write into a row a
    /// thin space has not allocated is refused (allocation is not
    /// supported yet).
    pub fn write_all_at(&self, mut buf: &[u8], mut offset: u64) -> Result<()> {
        let size = self.size();
        if offset.checked_add(buf.len() as u64).is_none_or(|end| end > size) {
            return Err(Error::Pool(format!(
                "write of {} bytes at {offset:#x} beyond the end of the space ({size:#x})",
                buf.len()
            )));
        }
        if let Some(journal) = &self.journal {
            return self.write_parity(journal, buf, offset);
        }
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

    /// Writes to a single parity space, stripe by stripe (see the module
    /// documentation).
    fn write_parity(&self, journal: &JournalState, buf: &[u8], offset: u64) -> Result<()> {
        let layout = self.reader.layout();
        let pool = self.reader.pool();
        let unit = layout.interleave;
        let stripe_bytes = layout.data_columns * unit;
        let per_row = SLAB_SIZE / unit;
        let mut writer = journal.writer.lock().unwrap();
        // The stripes touched: (stripe number in the space, start of the
        // touched part, the bytes).
        let first = offset / stripe_bytes;
        let last = (offset + buf.len() as u64 - 1) / stripe_bytes;
        let mut stripes = Vec::new();
        for s in first..=last {
            let start = s * stripe_bytes;
            let loc = layout.locate(layout.base + start);
            let run_start = layout.run_start_offset(loc.row);
            let run_rows = self.run_rows(loc.row)?;
            stripes.push((start, loc, run_start, run_rows * per_row, layout.stripe_of(&loc)));
        }
        // The touched stripes per extent run: (stripes of the run, first, last).
        let mut by_run: std::collections::BTreeMap<u64, (u64, u64, u64)> = Default::default();
        for &(_, _, run_start, run_stripes, stripe) in &stripes {
            let e = by_run.entry(run_start).or_insert((run_stripes, stripe, stripe));
            e.1 = e.1.min(stripe);
            e.2 = e.2.max(stripe);
        }
        // Where some of them are recorded as consistent, record them all as
        // not consistent first, one slot per run.
        let mut marked = false;
        for (&run_start, &(run_stripes, lo, hi)) in &by_run {
            if (lo..=hi).any(|s| writer.is_consistent(run_start, run_stripes, s)) {
                let (index, page) = writer.mark(run_start, run_stripes, lo, hi - lo + 1, false);
                self.write_journal_slot(journal, index, &page)?;
                marked = true;
            }
        }
        if marked {
            pool.flush_members()?;
        }
        for &(start, loc, _, _, stripe) in &stripes {
            let p = layout.parity_column(stripe);
            // All data units of the stripe, with the new bytes applied.
            let mut units: Vec<(u64, Vec<u8>, bool)> = Vec::with_capacity(layout.data_columns as usize);
            for k in 0..layout.data_columns {
                let at = start + k * unit;
                let column = layout.locate(layout.base + at).column;
                let (lo, hi) = (at.max(offset), (at + unit).min(offset + buf.len() as u64));
                let mut data = vec![0u8; unit as usize];
                if lo >= hi || hi - lo < unit {
                    self.read_unit(column, loc.row, loc.offset_in_slab, &mut data)?;
                }
                let changed = lo < hi;
                if changed {
                    data[(lo - at) as usize..(hi - at) as usize]
                        .copy_from_slice(&buf[(lo - offset) as usize..(hi - offset) as usize]);
                }
                units.push((column, data, changed));
            }
            let mut parity = vec![0u8; unit as usize];
            for (_, data, _) in &units {
                parity.iter_mut().zip(data).for_each(|(p, d)| *p ^= d);
            }
            for (column, data, _) in units.iter().filter(|u| u.2) {
                self.write_unit(*column, loc.row, loc.offset_in_slab, data)?;
            }
            self.write_unit(p, loc.row, loc.offset_in_slab, &parity)?;
        }
        pool.flush_members()?;
        // Record them as consistent, one slot per extent run.
        for (run_start, (run_stripes, lo, hi)) in by_run {
            let (index, page) = writer.mark(run_start, run_stripes, lo, hi - lo + 1, true);
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

    fn read_unit(&self, column: u64, row: u64, offset_in_slab: u64, buf: &mut [u8]) -> Result<()> {
        let (disk, slab) = self.reader.layout().physical(column, 0, row).ok_or_else(|| {
            Error::Unsupported("a stripe with an unallocated column (thin allocation is not supported yet)".into())
        })?;
        if !self.reader.pool().read_slab(disk, slab, offset_in_slab, buf)? {
            return Err(Error::Pool(format!("disk {disk} of the space is not present")));
        }
        Ok(())
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

    /// Makes the writes so far durable on every member.
    pub fn flush(&self) -> Result<()> {
        self.reader.pool().flush_members()
    }
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

    /// Single parity writes: a partial and a whole stripe and one across a
    /// stripe boundary read back, every touched stripe has the XOR of its
    /// data as parity, and the journal records them as consistent.
    #[test]
    fn parity_writes_keep_stripes_consistent_and_journaled() {
        let pool = fixture("parity3_26100");
        let id = pool.find_space("parity3_26100").unwrap().id();
        let w = pool.open_space_rw(id).unwrap();
        let layout = w.reader().layout().clone();
        let stripe = layout.data_columns * layout.interleave;
        let mut writes = Vec::new();
        for (at, len) in [
            (8192u64, 3 * BLOCK as u64),
            (4 * stripe, stripe),
            (7 * stripe - 4096, 3 * 4096),
        ] {
            let mut data = vec![0u8; len as usize];
            for (i, b) in data.chunks_mut(BLOCK).enumerate() {
                fill_block(b, at + (i * BLOCK) as u64, "parity");
            }
            w.write_all_at(&data, at).unwrap();
            writes.push((at, data));
        }
        w.flush().unwrap();
        let r = pool.open_space(id).unwrap();
        for (at, data) in &writes {
            let mut back = vec![0u8; data.len()];
            r.read_exact_at(&mut back, *at).unwrap();
            assert!(back == *data, "at {at:#x}");
        }
        // Parity of every touched stripe.
        let journal = r.journal().unwrap();
        for s in [0, 4, 6, 7] {
            let loc = layout.locate(s * stripe);
            let mut acc = vec![0u8; layout.interleave as usize];
            for column in 0..layout.columns {
                let (disk, slab) = layout.physical(column, 0, loc.row).unwrap();
                let mut unit = vec![0u8; acc.len()];
                pool.read_slab(disk, slab, loc.offset_in_slab, &mut unit).unwrap();
                acc.iter_mut().zip(&unit).for_each(|(a, u)| *a ^= u);
            }
            assert!(acc.iter().all(|&b| b == 0), "stripe {s}");
            assert!(
                !journal.is_dirty(layout.run_start_offset(loc.row), layout.stripe_of(&loc)),
                "stripe {s}"
            );
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
        assert!(
            pool.open_space_rw(id)
                .err()
                .unwrap()
                .to_string()
                .contains("not in a clean state")
        );
    }
}
