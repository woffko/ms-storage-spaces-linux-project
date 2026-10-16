//! The index of a write-back cache whose slot area (64 MiB, the most a
//! header may claim) is full of entries of one-sector runs: what loading it
//! takes in memory stays a small multiple of the area (1.2.1 kept each run
//! in 16 bytes, its word on disk is 2), and finding a sector of a chunk
//! does not walk its runs.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use storage_spaces::Guid;
use storage_spaces::cache::{CacheHeader, CacheIndex, Lookup};

/// Counts the bytes allocated now and at most.
struct Counting;

static NOW: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            let now = NOW.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(now, Ordering::Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        NOW.fetch_sub(layout.size(), Ordering::Relaxed);
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (i, t) in table.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
        }
        *t = c;
    }
    !data
        .iter()
        .fold(!0u32, |c, &b| table[((c ^ u32::from(b)) & 0xff) as usize] ^ (c >> 8))
}

const OWNER: [u8; 16] = [9; 16];
const SLOT: usize = 64 << 10;
const SLOTS: usize = 1024;
const CHUNK: u64 = 128 << 10;
const SECTOR: u64 = 512;

/// A slot (sequence `sequence`) of entries for chunks `first..`, each
/// partly valid in runs of one sector, valid and not in turn.
fn slot(sequence: u64, first: u64) -> (Vec<u8>, u64) {
    let mut s = vec![0u8; SLOT];
    s[0..8].copy_from_slice(b"SPSLOT\0\0");
    s[8..24].copy_from_slice(&OWNER);
    s[0x18..0x1c].copy_from_slice(&1u32.to_le_bytes());
    s[0x1c..0x20].copy_from_slice(&(SLOT as u32).to_le_bytes());
    s[0x28..0x30].copy_from_slice(&sequence.to_le_bytes());
    let runs = (CHUNK / SECTOR) as usize;
    let mut pos = 0x38;
    let mut n = 0u64;
    while pos + 16 + 2 * runs <= SLOT {
        let chunk = first + n;
        s[pos..pos + 8].copy_from_slice(&(chunk * CHUNK).to_le_bytes());
        s[pos + 8..pos + 12].copy_from_slice(&(chunk as u32).to_le_bytes());
        s[pos + 12..pos + 14].copy_from_slice(&2u16.to_le_bytes());
        s[pos + 14..pos + 16].copy_from_slice(&((2 * runs) as u16).to_le_bytes());
        for r in 0..runs {
            let word: u16 = if r % 2 == 0 { 0x8001 } else { 0x0001 };
            s[pos + 16 + 2 * r..pos + 18 + 2 * r].copy_from_slice(&word.to_le_bytes());
        }
        pos += (16 + 2 * runs).next_multiple_of(8);
        n += 1;
    }
    s[0x30..0x34].copy_from_slice(&(n as u32).to_le_bytes());
    let crc = {
        let mut c = s.clone();
        c[0x24..0x28].fill(0);
        crc32(&c)
    };
    s[0x24..0x28].copy_from_slice(&crc.to_le_bytes());
    (s, n)
}

#[test]
fn a_cache_index_of_many_runs_stays_small_and_quick() {
    let mut area = Vec::with_capacity(SLOT * SLOTS);
    let mut chunks = 0;
    for i in 0..SLOTS as u64 {
        let (s, n) = slot(i + 1, chunks);
        area.extend_from_slice(&s);
        chunks += n;
    }
    let header = CacheHeader {
        owner_guid: Guid::from_mixed_endian(&OWNER),
        sequence: 1,
        slot_offset: 0,
        slot_size: SLOT as u32,
        slot_count: SLOTS as u32,
        checkpoint_offset: 0,
        checkpoint_size: 0,
        checkpoint_count: 0,
        data_offset: (SLOT * SLOTS) as u64,
        chunk_size: CHUNK as u32,
        chunk_count: chunks as u32 + 1,
    };
    let before = NOW.load(Ordering::Relaxed);
    PEAK.store(before, Ordering::Relaxed);
    let read = |offset: u64, buf: &mut [u8]| -> storage_spaces::Result<()> {
        let at = offset as usize;
        buf.copy_from_slice(&area[at..at + buf.len()]);
        Ok(())
    };
    let index = CacheIndex::load(header, SECTOR as u32, read).unwrap();
    let took = PEAK.load(Ordering::Relaxed) - before;
    eprintln!(
        "{chunks} chunks of {} runs each: loading took {} MiB at most, for an area of {} MiB",
        CHUNK / SECTOR,
        took >> 20,
        area.len() >> 20
    );
    assert_eq!(index.cached_chunks() as u64, chunks);
    assert!(took <= 4 * area.len(), "{} MiB", took >> 20);
    // Every sector of the first 2000 chunks: valid and not in turn.
    let start = Instant::now();
    for chunk in 0..2000 {
        for s in 0..CHUNK / SECTOR {
            let at = chunk * CHUNK + s * SECTOR;
            let hit = matches!(index.lookup(at), Lookup::Hit { len, .. } if len == SECTOR);
            assert_eq!(hit, s % 2 == 0, "chunk {chunk} sector {s}");
        }
    }
    eprintln!("512 000 lookups: {:?}", start.elapsed());
}
