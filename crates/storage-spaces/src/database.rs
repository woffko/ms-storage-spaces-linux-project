//! How Windows updates a metadata database (SDBC header, SDBB entry slots):
//! the model the scenario tests check against the pools Windows wrote.
//!
//! A record occupies consecutive slots; its id is the number of its first
//! slot. An update writes the new version of each record into the first run
//! of free slots long enough for it, while the old versions still occupy
//! theirs, and then frees the old versions: a free slot keeps "SDBB" and its
//! own number, the rest is zero. Old versions are freed only after all new
//! records of the update are written. The header counts the slots up to the
//! last one in use and carries the update sequence (twice) and a timestamp.

use crate::crc::crc32_excluding;
use crate::error::{Result, format_err};
use crate::format::{RawRecord, SDBB_SIGNATURE, SDBC_SIGNATURE};
use crate::guid::Guid;
use crate::io::{ReadAt, read_vec};

/// The eight header slots of 0x40 bytes.
const HEADER_LEN: usize = 0x200;
/// Slots are formatted a page at a time.
const PAGE: usize = 0x1000;
/// Pages a database is read up to (4 MiB).
const MAX_PAGES: usize = 1024;
const HEADER_SLOTS: usize = 8;
/// Record header in the first fragment: type, version, two zero bytes and
/// the body length (u32 BE).
const RECORD_HEAD: usize = 8;
const FRAGMENT_HEAD: usize = 16;

/// A database as its raw slots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Database {
    bytes: Vec<u8>,
    entry_size: usize,
}

/// The value at 0x20 of a new pool database header (meaning unknown: a
/// size limit of about 8 MiB?).
pub const POOL_DATABASE_LIMIT: u32 = 0x7f_ff80;
/// The same for the per-space databases in the metadata space.
pub const SPACE_DATABASE_LIMIT: u32 = 0x1_0000;

impl Database {
    /// A new, empty database of `owner` as Windows creates one: a page of
    /// 64 slots, the 8 header slots and 56 free ones.
    pub fn new(owner: Guid, limit: u32) -> Self {
        let entry_size = 0x40;
        let mut bytes = vec![0u8; PAGE];
        bytes[..8].copy_from_slice(SDBC_SIGNATURE);
        bytes[8..12].copy_from_slice(&[0, 1, 2, 0]);
        bytes[0x10..0x20].copy_from_slice(&owner.0);
        bytes[0x20..0x24].copy_from_slice(&limit.to_be_bytes());
        bytes[0x24..0x28].copy_from_slice(&(entry_size as u32).to_be_bytes());
        bytes[0x30..0x34].copy_from_slice(&0x1_0000u32.to_be_bytes());
        let mut db = Database { bytes, entry_size };
        for i in HEADER_SLOTS..db.slots() {
            let e = db.slot_mut(i);
            e[..4].copy_from_slice(SDBB_SIGNATURE);
            e[4..8].copy_from_slice(&(i as u32).to_be_bytes());
        }
        db
    }

    /// Reads the database whose header is at `offset`, with all `slots`
    /// formatted slots (the pool database has 64).
    pub fn read<D: ReadAt + ?Sized>(dev: &D, offset: u64, slots: usize) -> Result<Self> {
        let head = read_vec(dev, offset, HEADER_LEN)?;
        if &head[..8] != SDBC_SIGNATURE {
            return Err(format_err!("missing SDBC signature at {offset:#x}"));
        }
        let entry_size = u32::from_be_bytes(head[0x24..0x28].try_into().unwrap()) as usize;
        if !(FRAGMENT_HEAD + RECORD_HEAD..=0x1000).contains(&entry_size) || slots > 1 << 16 {
            return Err(format_err!("implausible database geometry"));
        }
        let bytes = read_vec(dev, offset, entry_size * slots)?;
        Ok(Database { bytes, entry_size })
    }

    /// Reads the database whose header is at `offset` with all its
    /// formatted slots: pages of 4 KiB whose slots carry "SDBB" and their
    /// own number (the pool database starts with one page and grows by
    /// pages, see [`Database::grow`]).
    pub fn read_formatted<D: ReadAt + ?Sized>(dev: &D, offset: u64) -> Result<Self> {
        let head = read_vec(dev, offset, HEADER_LEN)?;
        if &head[..8] != SDBC_SIGNATURE {
            return Err(format_err!("missing SDBC signature at {offset:#x}"));
        }
        let entry_size = u32::from_be_bytes(head[0x24..0x28].try_into().unwrap()) as usize;
        if !(FRAGMENT_HEAD + RECORD_HEAD..=PAGE).contains(&entry_size) || !PAGE.is_multiple_of(entry_size) {
            return Err(format_err!("implausible database geometry"));
        }
        let per_page = PAGE / entry_size;
        let mut pages = 1;
        while pages < MAX_PAGES {
            let page = read_vec(dev, offset + (pages * PAGE) as u64, PAGE)?;
            let formatted = page.chunks_exact(entry_size).enumerate().all(|(k, e)| {
                &e[..4] == SDBB_SIGNATURE
                    && u32::from_be_bytes(e[4..8].try_into().unwrap()) as usize == pages * per_page + k
            });
            if !formatted {
                break;
            }
            pages += 1;
        }
        Self::read(dev, offset, pages * per_page)
    }

    /// Formats one more page of free slots at the end (when no run of
    /// free slots is long enough for a record).
    pub fn grow(&mut self) {
        let first = self.slots();
        self.bytes.resize(self.bytes.len() + PAGE, 0);
        for i in first..self.slots() {
            let e = self.slot_mut(i);
            e[..4].copy_from_slice(SDBB_SIGNATURE);
            e[4..8].copy_from_slice(&(i as u32).to_be_bytes());
        }
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    fn slots(&self) -> usize {
        self.bytes.len() / self.entry_size
    }

    fn slot(&self, i: usize) -> &[u8] {
        &self.bytes[i * self.entry_size..(i + 1) * self.entry_size]
    }

    fn slot_mut(&mut self, i: usize) -> &mut [u8] {
        &mut self.bytes[i * self.entry_size..(i + 1) * self.entry_size]
    }

    fn record_id(&self, i: usize) -> u32 {
        u32::from_be_bytes(self.slot(i)[8..12].try_into().unwrap())
    }

    fn is_free(&self, i: usize) -> bool {
        self.record_id(i) == 0
    }

    /// Slots a record of `body_len` bytes takes.
    fn slots_for(&self, body_len: usize) -> usize {
        (RECORD_HEAD + body_len)
            .div_ceil(self.entry_size - FRAGMENT_HEAD)
            .max(1)
    }

    /// Writes a new record into the first free slots, in slot order, whether
    /// they are adjacent or not (Windows splits a record over the free slots
    /// it finds: `c9resize`); returns its id (its first slot). `None` if
    /// fewer slots are free.
    pub fn insert(&mut self, kind: u8, version: u8, body: &[u8]) -> Option<u32> {
        let n = self.slots_for(body.len());
        // The id is the first slot; a slot another record claims as its id
        // (only in damaged databases) is not taken as the first.
        let claimed = |s: usize| (HEADER_SLOTS..self.slots()).any(|i| self.record_id(i) == s as u32);
        let mut slots = Vec::with_capacity(n);
        for i in HEADER_SLOTS..self.slots() {
            if self.is_free(i) && (!slots.is_empty() || !claimed(i)) {
                slots.push(i);
                if slots.len() == n {
                    break;
                }
            }
        }
        if slots.len() < n {
            return None;
        }
        let first = slots[0];
        let mut data = vec![kind, version, 0, 0];
        data.extend_from_slice(&(body.len() as u32).to_be_bytes());
        data.extend_from_slice(body);
        let payload = self.entry_size - FRAGMENT_HEAD;
        data.resize(n * payload, 0);
        for (k, chunk) in data.chunks(payload).enumerate() {
            let i = slots[k];
            let e = self.slot_mut(i);
            e[..4].copy_from_slice(SDBB_SIGNATURE);
            e[4..8].copy_from_slice(&(i as u32).to_be_bytes());
            e[8..12].copy_from_slice(&(first as u32).to_be_bytes());
            e[12..14].copy_from_slice(&(k as u16).to_be_bytes());
            e[14..16].copy_from_slice(&(n as u16).to_be_bytes());
            e[FRAGMENT_HEAD..].copy_from_slice(chunk);
        }
        Some(first as u32)
    }

    /// Frees the slots of record `id`.
    pub fn remove(&mut self, id: u32) {
        for i in self.slots_of(id) {
            self.free(i);
        }
    }

    fn slots_of(&self, id: u32) -> Vec<usize> {
        (HEADER_SLOTS..self.slots())
            .filter(|&i| self.record_id(i) == id)
            .collect()
    }

    fn free(&mut self, i: usize) {
        self.slot_mut(i)[8..].fill(0);
    }

    /// One update: writes the new records (kind, version, body) in order,
    /// each into the first free slots, and only then frees the
    /// records `frees` (the old versions of changed records, or deleted
    /// ones). Returns the ids of the new records.
    pub fn update(&mut self, writes: &[(u8, u8, &[u8])], frees: &[u32]) -> Option<Vec<u32>> {
        let old: Vec<usize> = frees.iter().flat_map(|&id| self.slots_of(id)).collect();
        let ids = writes
            .iter()
            .map(|(kind, version, body)| self.insert(*kind, *version, body))
            .collect::<Option<Vec<u32>>>()?;
        for i in old {
            self.free(i);
        }
        Some(ids)
    }

    /// [`Database::update`] applied to a copy, formatting another page of
    /// slots whenever no run of free slots is long enough (as Windows does,
    /// see [`Database::grow`]), up to 4 MiB.
    pub fn updated(&self, writes: &[(u8, u8, &[u8])], frees: &[u32]) -> Result<(Database, Vec<u32>)> {
        let mut base = self.clone();
        loop {
            let mut db = base.clone();
            if let Some(ids) = db.update(writes, frees) {
                return Ok((db, ids));
            }
            if base.bytes.len() >= MAX_PAGES * PAGE {
                return Err(format_err!("the database is full"));
            }
            base.grow();
        }
    }

    /// The record with id `id`.
    pub fn record(&self, id: u32) -> Option<RawRecord> {
        crate::format::assemble_records(&self.bytes, self.entry_size)
            .ok()?
            .into_iter()
            .find(|r| r.id == id)
    }

    /// Finishes an update: the slot count, the sequence (at 0x38 and 0x40),
    /// the timestamp and the header checksum.
    pub fn commit(&mut self, sequence: u64, timestamp: u64) {
        let used = (HEADER_SLOTS..self.slots())
            .rev()
            .find(|&i| !self.is_free(i))
            .map_or(HEADER_SLOTS, |i| i + 1);
        let h = &mut self.bytes[..HEADER_LEN];
        h[0x28..0x2c].copy_from_slice(&(used as u32).to_be_bytes());
        h[0x38..0x40].copy_from_slice(&sequence.to_be_bytes());
        h[0x40..0x48].copy_from_slice(&sequence.to_be_bytes());
        h[0x48..0x50].copy_from_slice(&timestamp.to_be_bytes());
        let crc = crc32_excluding(h, 0x0c);
        h[0x0c..0x10].copy_from_slice(&crc.to_be_bytes());
    }

    /// The extent records with their ids.
    pub fn extents(&self) -> Vec<(u32, crate::format::ExtentRecord)> {
        crate::format::assemble_records(&self.bytes, self.entry_size)
            .unwrap_or_default()
            .iter()
            .filter_map(|r| match crate::format::Record::decode(r) {
                Ok(crate::format::Record::Extent(e)) => Some((r.id, e)),
                _ => None,
            })
            .collect()
    }

    /// The first slab of disk `disk_id` that no extent uses: where Windows
    /// puts a slab it allocates on that disk.
    pub fn first_free_slab(&self, disk_id: u64) -> u64 {
        let records = crate::format::assemble_records(&self.bytes, self.entry_size).unwrap_or_default();
        let mut used: Vec<(u64, u64)> = records
            .iter()
            .filter_map(|r| match crate::format::Record::decode(r) {
                Ok(crate::format::Record::Extent(e)) if e.disk_id == disk_id => {
                    Some((e.physical_slab, e.physical_slab.saturating_add(e.slab_count)))
                }
                _ => None,
            })
            .collect();
        used.sort();
        let mut free = 0;
        for (start, end) in used {
            if start > free {
                break;
            }
            free = free.max(end);
        }
        free
    }

    pub fn sequence(&self) -> u64 {
        u64::from_be_bytes(self.bytes[0x40..0x48].try_into().unwrap())
    }

    pub fn timestamp(&self) -> u64 {
        u64::from_be_bytes(self.bytes[0x48..0x50].try_into().unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::MemDevice;

    /// An empty database of 16 formatted slots.
    fn empty() -> Database {
        let mut b = vec![0u8; 16 * 0x40];
        b[..8].copy_from_slice(SDBC_SIGNATURE);
        b[0x24..0x28].copy_from_slice(&0x40u32.to_be_bytes());
        for i in HEADER_SLOTS..16 {
            b[i * 0x40..i * 0x40 + 4].copy_from_slice(SDBB_SIGNATURE);
            b[i * 0x40 + 4..i * 0x40 + 8].copy_from_slice(&(i as u32).to_be_bytes());
        }
        Database::read(&MemDevice(b), 0, 16).unwrap()
    }

    #[test]
    fn new_versions_go_first_fit_before_old_ones_are_freed() {
        let mut db = empty();
        assert_eq!(db.update(&[(2, 8, &[1; 60]), (4, 6, &[2; 10])], &[]), Some(vec![8, 10]));
        // A larger new version of record 8 cannot use its own slots.
        assert_eq!(db.update(&[(2, 8, &[3; 100])], &[8]), Some(vec![11]));
        assert_eq!(db.record(11).unwrap().body, [3; 100]);
        assert_eq!(db.record(8), None);
        // Slots 8 and 9 are free again, with their signature and number.
        assert_eq!(&db.bytes()[8 * 0x40..8 * 0x40 + 8], b"SDBB\0\0\0\x08");
        assert_eq!(db.update(&[(4, 6, &[4; 10])], &[]), Some(vec![8]));
        db.commit(9, 42);
        let (h, records) = crate::format::read_database(&MemDevice(db.bytes().to_vec()), 0)
            .unwrap()
            .unwrap();
        assert_eq!((h.sequence, h.timestamp, h.entry_count), (9, 42, 14));
        assert_eq!(records.len(), 3);
    }

    #[test]
    fn a_new_record_never_takes_an_id_in_use() {
        // Found by fuzzing: a damaged database with a record whose id is not
        // its first slot; the new record must not share that id, or freeing
        // the old record would free the new one too.
        let mut db = empty();
        db.insert(2, 8, &[1; 10]);
        let s = 9 * 0x40;
        db.bytes[s + 8..s + 12].copy_from_slice(&12u32.to_be_bytes());
        db.bytes[s + 12..s + 16].copy_from_slice(&[0, 0, 0, 1]);
        // Slots 10 and 11 are free, but 12 is taken as an id.
        let ids = db.update(&[(4, 6, &[5; 60])], &[12]).unwrap();
        assert_ne!(ids[0], 12);
        assert_eq!(db.record(ids[0]).unwrap().body, [5; 60]);
    }
}
