//! Development aid: dumps the write-back cache slots of a space.
//! Usage: cargo run --example dump_cache -- SPACE DEVICE...
use std::fs::File;

use storage_spaces::Pool;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let files = args[2..].iter().map(|p| File::open(p).unwrap()).collect();
    let pool = Pool::open(files).unwrap();
    let space = pool.find_space(&args[1]).expect("space");
    for container in pool.children(space.id()) {
        for child in pool.children(container.id()) {
            println!(
                "space {} child {} role {:?}",
                container.id(),
                child.id(),
                container.info.role
            );
        }
    }
    match pool.open_space(space.id()) {
        Ok(r) => println!("opened, cache {:?}", r.cache().map(|c| c.cached_chunks())),
        Err(e) => println!("open failed: {e}"),
    }
}
