//! Assembly of SDBB entries into records, then decoding every record. The
//! input is the database from the SDBC header on, 64-byte entries. The
//! update model then inserts a record, frees another and commits: the
//! result must read back with the new record and a valid header. Where the
//! record does not fit, the database grows by a page first, and it must
//! read back whole with its new page. An extent record made from the input
//! must decode to itself, and the first free slab of a disk must be free.
#![no_main]

use libfuzzer_sys::fuzz_target;
use storage_spaces::database::Database;
use storage_spaces::format::{ExtentRecord, RawRecord, Record, assemble_records, read_database};
use storage_spaces::io::MemDevice;

const ENTRY: usize = 0x40;

fuzz_target!(|data: &[u8]| {
    let mut raw = data.to_vec();
    raw.truncate(raw.len() / ENTRY * ENTRY);
    // Most inputs should reach the fragment logic: entries whose first byte
    // is odd get the SDBB signature and their own slot number.
    for (slot, entry) in raw.chunks_exact_mut(ENTRY).enumerate().skip(8) {
        if entry[0] & 1 == 1 {
            entry[0..4].copy_from_slice(b"SDBB");
            entry[4..8].copy_from_slice(&(slot as u32).to_be_bytes());
        }
    }
    let Ok(records) = assemble_records(&raw, ENTRY) else {
        return;
    };
    for r in &records {
        let _ = Record::decode(r);
    }
    if raw.len() < 16 * ENTRY {
        return;
    }
    raw[..8].copy_from_slice(b"SDBC    ");
    raw[0x24..0x28].copy_from_slice(&(ENTRY as u32).to_be_bytes());
    let slots = raw.len() / ENTRY;
    let Ok(mut db) = Database::read(&MemDevice(raw.clone()), 0, slots) else {
        return;
    };
    extent_round_trip(data);
    let disk = u64::from(data[0] % 4);
    let free = db.first_free_slab(disk);
    for r in &records {
        if let Ok(Record::Extent(e)) = Record::decode(r)
            && e.disk_id == disk
        {
            assert!(free < e.physical_slab || free >= e.physical_slab.saturating_add(e.slab_count));
        }
    }
    let body = &data[..data.len().min(300)];
    let frees: Vec<u32> = records.iter().take(1).map(|r| r.id).collect();
    let mut next = db.clone();
    let mut grown = 0;
    let ids = loop {
        if let Some(ids) = next.update(&[(3, 16, body)], &frees) {
            break ids;
        }
        if grown == 2 {
            return;
        }
        db.grow();
        grown += 1;
        next = db.clone();
    };
    let mut db = next;
    db.commit(7, 0x1234);
    let (header, back) = read_database(&MemDevice(db.bytes().to_vec()), 0).unwrap().unwrap();
    assert_eq!(header.sequence, 7);
    let new = back.iter().find(|r| r.id == ids[0]).unwrap();
    assert_eq!(new.body, body);
    assert!(frees.iter().all(|f| back.iter().all(|r| r.id != *f)));
    if grown > 0 && slots.is_multiple_of(64) {
        // Whole pages of formatted slots read back as they are (followed
        // by more of the partition, as on a disk).
        let mut disk = db.bytes().to_vec();
        disk.resize(disk.len() + 4096, 0);
        let again = Database::read_formatted(&MemDevice(disk), 0).unwrap();
        assert_eq!(again.bytes(), db.bytes());
    }
});

/// An extent record with fields from the input encodes and decodes to
/// itself.
fn extent_round_trip(data: &[u8]) {
    let mut words = data.chunks(8).map(|c| {
        let mut b = [0u8; 8];
        b[..c.len()].copy_from_slice(c);
        u64::from_le_bytes(b)
    });
    let mut next = || words.next().unwrap_or(0);
    let e = ExtentRecord {
        space_id: next() % (1 << 32),
        virtual_slab: next() % (1 << 32),
        column: next() % 64,
        copy: next() % 4,
        slab_count: next() % (1 << 20) + 1,
        disk_id: next() % (1 << 16),
        physical_slab: next() % (1 << 32),
        flags: (next() % 256) as u8,
        stale_marker: if next() & 1 == 0 {
            0xffff_ffff
        } else {
            next() % (1 << 32)
        },
    };
    let raw = RawRecord {
        id: 8,
        kind: 4,
        version: 6,
        body: e.encode(next() % (1 << 32)),
    };
    match Record::decode(&raw) {
        Ok(Record::Extent(back)) => assert_eq!(format!("{back:?}"), format!("{e:?}")),
        other => panic!("{e:?} decoded as {other:?}"),
    }
}
