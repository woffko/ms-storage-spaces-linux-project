//! Development aid: prints the raw records of the pool database of one
//! member (id, type, record version, body in hex).
//! Usage: dump_records DEVICE [TYPE...]
use std::fs::File;

use storage_spaces::format::{POOL_DB_OFFSET, read_database};
use storage_spaces::gpt::find_spaces_partition;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dev = File::open(&args[1]).unwrap();
    let kinds: Vec<u8> = args[2..].iter().map(|k| k.parse().unwrap()).collect();
    let part = find_spaces_partition(&dev)
        .unwrap()
        .expect("no Storage Spaces partition");
    let (header, records) = read_database(&dev, part.offset + POOL_DB_OFFSET)
        .unwrap()
        .expect("empty database");
    println!("{header:?}");
    for r in records.iter().filter(|r| kinds.is_empty() || kinds.contains(&r.kind)) {
        let hex: Vec<String> = r.body.iter().map(|b| format!("{b:02x}")).collect();
        println!(
            "id {} type {} v{} len {}: {}",
            r.id,
            r.kind,
            r.version,
            r.body.len(),
            hex.join(" ")
        );
    }
}
