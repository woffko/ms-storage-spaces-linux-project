//! Assembly of SDBB entries into records, then decoding every record. The
//! input is the database from the SDBC header on, 64-byte entries. The
//! update model then inserts a record, frees another and commits: the
//! result must read back with the new record and a valid header.
#![no_main]

use libfuzzer_sys::fuzz_target;
use storage_spaces::database::Database;
use storage_spaces::format::{Record, assemble_records, read_database};
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
    let body = &data[..data.len().min(300)];
    let frees: Vec<u32> = records.iter().take(1).map(|r| r.id).collect();
    if let Some(ids) = db.update(&[(3, 16, body)], &frees) {
        db.commit(7, 0x1234);
        let (header, back) = read_database(&MemDevice(db.bytes().to_vec()), 0).unwrap().unwrap();
        assert_eq!(header.sequence, 7);
        let new = back.iter().find(|r| r.id == ids[0]).unwrap();
        assert_eq!(new.body, body);
        assert!(frees.iter().all(|f| back.iter().all(|r| r.id != *f)));
    }
});
