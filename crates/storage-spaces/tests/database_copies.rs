//! Five members whose copies of the pool database are 64 MiB each (the most
//! a header may claim) and all differ: opening the pool keeps one of them
//! in memory, not all (1.2.1 kept every version until one decoded).

use std::alloc::{GlobalAlloc, Layout, System};
use std::fs::File;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use storage_spaces::Pool;
use storage_spaces::format::{RawRecord, read_database};
use storage_spaces::gpt::find_spaces_partition;
use storage_spaces::io::SparseImage;

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

/// Entries of 4 KiB, the largest a header may have.
const ENTRY: usize = 0x1000;

/// A database of `records` at `sequence`, each record in consecutive
/// entries after the 8 of the header, its id its first entry's number.
fn database(owner: &[u8], sequence: u64, records: &[RawRecord]) -> Vec<u8> {
    let payload = ENTRY - 16;
    let mut entries: Vec<Vec<u8>> = vec![vec![0u8; ENTRY]; 8];
    for r in records {
        let first = entries.len();
        let mut data = vec![r.kind, r.version, 0, 0];
        data.extend_from_slice(&(r.body.len() as u32).to_be_bytes());
        data.extend_from_slice(&r.body);
        let n = data.len().div_ceil(payload);
        data.resize(n * payload, 0);
        for (k, part) in data.chunks(payload).enumerate() {
            let mut e = vec![0u8; ENTRY];
            e[..4].copy_from_slice(b"SDBB");
            e[4..8].copy_from_slice(&((first + k) as u32).to_be_bytes());
            e[8..12].copy_from_slice(&(first as u32).to_be_bytes());
            e[12..14].copy_from_slice(&(k as u16).to_be_bytes());
            e[14..16].copy_from_slice(&(n as u16).to_be_bytes());
            e[16..].copy_from_slice(part);
            entries.push(e);
        }
    }
    let used = entries.len();
    let mut bytes = entries.concat();
    bytes[..8].copy_from_slice(b"SDBC    ");
    bytes[8..12].copy_from_slice(&[0, 1, 2, 0]);
    bytes[0x10..0x20].copy_from_slice(owner);
    bytes[0x24..0x28].copy_from_slice(&(ENTRY as u32).to_be_bytes());
    bytes[0x28..0x2c].copy_from_slice(&(used as u32).to_be_bytes());
    bytes[0x38..0x40].copy_from_slice(&sequence.to_be_bytes());
    bytes[0x40..0x48].copy_from_slice(&sequence.to_be_bytes());
    let mut head = bytes[..0x200].to_vec();
    head[0x0c..0x10].fill(0);
    let crc = crc32(&head);
    bytes[0x0c..0x10].copy_from_slice(&crc.to_be_bytes());
    bytes
}

#[test]
fn differing_copies_of_a_large_database_are_not_all_kept() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mirror3");
    let mut disks: Vec<SparseImage> = (0..)
        .map_while(|i| File::open(dir.join(format!("disk{i}.fixture"))).ok())
        .map(|f| SparseImage::read_from(f).unwrap())
        .collect();
    let (mut size, mut copies) = (0, 0);
    for (i, disk) in disks.iter_mut().enumerate() {
        let at = find_spaces_partition(&*disk).unwrap().unwrap().offset + 0x1000;
        let Ok(Some((header, mut records))) = read_database(&*disk, at) else {
            continue;
        };
        // Records of a type nothing reads, 4000 bytes each, other on each
        // disk: the copies of one sequence all differ.
        records.extend((0..16_000u32).map(|n| RawRecord {
            id: 0,
            kind: 0x63,
            version: 1,
            body: vec![i as u8 ^ n as u8; 4000],
        }));
        let bytes = database(&header.owner_guid.0, header.sequence + 1, &records);
        size = bytes.len();
        copies += 1;
        disk.insert(at, &bytes);
    }
    assert!(size > 60 << 20, "{size}");
    let before = NOW.load(Ordering::Relaxed);
    PEAK.store(before, Ordering::Relaxed);
    let pool = Pool::open(disks).unwrap();
    let took = PEAK.load(Ordering::Relaxed) - before;
    eprintln!(
        "{} copies of {} MiB: opening took {} MiB at most",
        pool.members.len(),
        size >> 20,
        took >> 20
    );
    let torn = pool
        .issues
        .iter()
        .filter(|i| matches!(i, storage_spaces::Issue::TornCopy { .. }));
    assert_eq!(torn.count(), copies - 1);
    assert!(took <= 3 * size, "{} MiB", took >> 20);
}
