//! Development aid: shows base, cached and effective content at an offset,
//! checked against the test pattern. Usage: probe_offset SPACE OFFSET DEVICE...
use std::fs::File;

use storage_spaces::{Pool, testpattern};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let offset = u64::from_str_radix(args[2].trim_start_matches("0x"), 16).unwrap();
    let files = args[3..].iter().map(|p| File::open(p).unwrap()).collect();
    let pool = Pool::open(files).unwrap();
    let space = pool.find_space(&args[1]).unwrap();
    let r = pool.open_space(space.id()).unwrap();
    let mut b = vec![0u8; 4096];
    let describe = |b: &[u8]| {
        let ok = testpattern::verify(b, offset, space.name()).is_none();
        let tag = if &b[0..8] == testpattern::MAGIC {
            format!("pattern of {:#x}", u64::from_le_bytes(b[8..16].try_into().unwrap()))
        } else if b.iter().all(|&x| x == 0) {
            "zeros".into()
        } else {
            "other".into()
        };
        format!("{} ({tag})", if ok { "OK " } else { "BAD" })
    };
    r.read_uncached_at(&mut b, offset).unwrap();
    println!("base:      {}", describe(&b));
    if r.read_cached_at(&mut b, offset).unwrap() {
        println!("cache:     {}", describe(&b));
    } else {
        println!("cache:     no entry");
    }
    r.read_exact_at(&mut b, offset).unwrap();
    println!("effective: {}", describe(&b));
}
