//! Assembly of SDBB entries into records, then decoding every record. The
//! input is the database from the SDBC header on, 64-byte entries.
#![no_main]

use libfuzzer_sys::fuzz_target;
use storage_spaces::format::{Record, assemble_records};

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
    if let Ok(records) = assemble_records(&raw, ENTRY) {
        for r in &records {
            let _ = Record::decode(r);
        }
    }
});
