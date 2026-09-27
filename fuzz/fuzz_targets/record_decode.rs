//! Decoding of single database records: type, record version, body.
#![no_main]

use libfuzzer_sys::fuzz_target;
use storage_spaces::format::{RawRecord, Record};

fuzz_target!(|data: &[u8]| {
    if data.len() < 2 {
        return;
    }
    let raw = RawRecord {
        id: 1,
        kind: data[0] % 8,
        version: data[1],
        body: data[2..].to_vec(),
    };
    let _ = Record::decode(&raw);
});
