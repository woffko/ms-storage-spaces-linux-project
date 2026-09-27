//! Development aid: shows the write-back cache of a space and optionally
//! copies the header and slot area of the cache space into a file (and the
//! start of the parity journal or dirty region tracking into FILE.journal
//! or FILE.drt).
//! Usage: cargo run --example dump_cache -- SPACE [--out FILE] DEVICE...
use std::fs::File;
use std::io::Write;

use storage_spaces::Pool;
use storage_spaces::cache::CacheHeader;
use storage_spaces::format::SpaceRole;

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let out = args.iter().position(|a| a == "--out").map(|i| {
        let path = args.remove(i + 1);
        args.remove(i);
        path
    });
    let files = args[1..].iter().map(|p| File::open(p).unwrap()).collect();
    let pool = Pool::open(files).unwrap();
    let space = pool.find_space(&args[0]).expect("space");
    for container in pool.children(space.id()) {
        for child in pool.children(container.id()) {
            println!(
                "space {} child {} role {:?}, {} extents",
                container.id(),
                child.id(),
                container.info.role,
                child.extents.len()
            );
            if let (SpaceRole::Other(role @ (0x06 | 0x0a)), Some(path)) = (container.info.role, &out) {
                let ext = if role == 0x06 { "drt" } else { "journal" };
                // The whole parity journal or dirty region tracking space.
                let r = pool.open_space(child.id()).unwrap();
                let mut buf = vec![0u8; r.size() as usize];
                r.read_exact_at(&mut buf, 0).unwrap();
                File::create(format!("{path}.{ext}")).unwrap().write_all(&buf).unwrap();
                println!("wrote {path}.{ext}");
            }
            if container.info.role != SpaceRole::Cache || child.extents.is_empty() {
                continue;
            }
            let r = pool.open_space(child.id()).unwrap();
            let mut head = vec![0u8; CacheHeader::SIZE];
            r.read_exact_at(&mut head, 0).unwrap();
            let header = CacheHeader::parse(&head).unwrap().expect("no cache header");
            println!("{header:?}");
            if let Some(path) = &out {
                let len = header.slot_offset + header.slot_size as u64 * header.slot_count as u64;
                let mut buf = vec![0u8; len as usize];
                r.read_exact_at(&mut buf, 0).unwrap();
                File::create(path).unwrap().write_all(&buf).unwrap();
                println!("wrote {len} bytes to {path}");
            }
        }
    }
    match pool.open_space(space.id()) {
        Ok(r) => println!("opened, cache {:?}", r.cache().map(|c| c.cached_chunks())),
        Err(e) => println!("open failed: {e}"),
    }
}
