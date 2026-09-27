//! Development aid: for every 4 KiB block of one MiB of a space, shows what
//! the base layout and the write-back cache hold (pattern tag and offset),
//! and the CRC-32 of the MiB read with and without the cache.
//! Usage: mib_blocks SPACE MIB DEVICE...
use std::fs::File;

use storage_spaces::Pool;

fn describe(b: &[u8]) -> String {
    if &b[0..8] == b"SSPATTRN" {
        let off = u64::from_le_bytes(b[8..16].try_into().unwrap());
        let tag = String::from_utf8_lossy(&b[16..32]).trim_end_matches('\0').to_string();
        format!("{tag}@{off:#x}")
    } else if b.iter().all(|&x| x == 0) {
        "zeros".into()
    } else {
        "other".into()
    }
}

fn crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (i, t) in table.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
        *t = c;
    }
    !data
        .iter()
        .fold(!0u32, |c, &b| table[((c ^ b as u32) & 0xff) as usize] ^ (c >> 8))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mib: u64 = args[2].parse().unwrap();
    let files = args[3..].iter().map(|p| File::open(p).unwrap()).collect();
    let pool = Pool::open(files).unwrap();
    let space = pool.find_space(&args[1]).unwrap();
    let r = pool
        .open_space_with(
            space.id(),
            storage_spaces::OpenOptions {
                unclean_parity: storage_spaces::UncleanParity::PreferData,
            },
        )
        .unwrap();
    let base_off = mib << 20;
    let mut whole = vec![0u8; 1 << 20];
    let mut uncached = vec![0u8; 1 << 20];
    r.read_exact_at(&mut whole, base_off).unwrap();
    r.read_uncached_at(&mut uncached, base_off).unwrap();
    println!(
        "MiB {mib}: effective {:08x}, base only {:08x}",
        crc32(&whole),
        crc32(&uncached)
    );
    let mut b = vec![0u8; 4096];
    for k in 0..256u64 {
        let off = base_off + k * 4096;
        let base = describe(&uncached[(k * 4096) as usize..((k + 1) * 4096) as usize]);
        let cache = if r.read_cached_at(&mut b, off).unwrap() {
            describe(&b)
        } else {
            "-".into()
        };
        println!("{k:3} {off:#x} base {base} cache {cache}");
    }
}
