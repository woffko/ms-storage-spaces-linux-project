//! Development aid: verifies pattern tags over ranges of a space.
//! Usage: check_ranges SPACE TAG:START_MIB:LEN_MIB[,...] DEVICE...
use std::fs::File;

use storage_spaces::{Pool, testpattern};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let files = args[3..].iter().map(|p| File::open(p).unwrap()).collect();
    let pool = Pool::open(files).unwrap();
    for w in &pool.warnings {
        println!("warning: {w}");
    }
    let r = pool.open_space(pool.find_space(&args[1]).unwrap().id()).unwrap();
    let mut buf = vec![0u8; 1 << 20];
    for spec in args[2].split(',') {
        let p: Vec<&str> = spec.split(':').collect();
        let (tag, start, len): (&str, u64, u64) = (p[0], p[1].parse().unwrap(), p[2].parse().unwrap());
        let (mut good, mut bad) = (0, Vec::new());
        for mib in start..start + len {
            r.read_exact_at(&mut buf, mib << 20).unwrap();
            if testpattern::verify(&buf, mib << 20, tag).is_none() {
                good += 1
            } else {
                bad.push(mib)
            }
        }
        println!(
            "{tag} MiB {start}..{}: {good} ok, {} wrong {:?}",
            start + len,
            bad.len(),
            &bad[..bad.len().min(8)]
        );
    }
}
