//! Development aid: copies any space of a pool (by id, hidden ones
//! included) into a file. Usage: dump_space ID OUT DEVICE...
use std::fs::File;
use std::io::Write;

use storage_spaces::Pool;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let id: u64 = args[1].parse().unwrap();
    let files = args[3..].iter().map(|p| File::open(p).unwrap()).collect();
    let pool = Pool::open(files).unwrap();
    let r = pool.open_space(id).unwrap();
    let mut buf = vec![0u8; r.size().min(1 << 30) as usize];
    r.read_exact_at(&mut buf, 0).unwrap();
    File::create(&args[2]).unwrap().write_all(&buf).unwrap();
    println!("space {id}: wrote {} bytes", buf.len());
}
