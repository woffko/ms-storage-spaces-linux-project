use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use storage_spaces::format::{SLAB_SIZE, SpaceRole};
use storage_spaces::{Pool, Space, testpattern};

/// Inspect and read Microsoft Storage Spaces pools.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show the pool, its disks and spaces.
    Info {
        /// Pool member disks, partitions or images.
        #[arg(required = true)]
        devices: Vec<PathBuf>,
        /// Also list internal spaces (metadata, caches).
        #[arg(short, long)]
        all: bool,
    },
    /// List the slab allocation of a space.
    Extents {
        #[arg(required = true)]
        devices: Vec<PathBuf>,
        /// Space name, GUID or numeric id.
        #[arg(short, long)]
        space: String,
    },
    /// Copy the contents of a space into a (sparse) image file.
    Export {
        #[arg(required = true)]
        devices: Vec<PathBuf>,
        #[arg(short, long)]
        space: String,
        #[arg(short, long)]
        output: PathBuf,
    },
    /// Verify the test pattern written by tools/vm/New-TestPool.ps1.
    #[command(hide = true)]
    CheckPattern {
        #[arg(required = true)]
        devices: Vec<PathBuf>,
        #[arg(short, long)]
        space: String,
        /// Number of bytes to check (default: whole space).
        #[arg(long)]
        length: Option<u64>,
    },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Info { devices, all } => info(&open_pool(&devices)?, all),
        Command::Extents { devices, space } => {
            let pool = open_pool(&devices)?;
            extents(&pool, find_space(&pool, &space)?)
        }
        Command::Export { devices, space, output } => {
            let pool = open_pool(&devices)?;
            export(&pool, find_space(&pool, &space)?, &output)
        }
        Command::CheckPattern { devices, space, length } => {
            let pool = open_pool(&devices)?;
            check_pattern(&pool, find_space(&pool, &space)?, length)
        }
    }
}

fn open_pool(paths: &[PathBuf]) -> Result<Pool<File>> {
    let files = paths
        .iter()
        .map(|p| File::open(p).with_context(|| format!("cannot open {}", p.display())))
        .collect::<Result<Vec<_>>>()?;
    let pool = Pool::open(files)?;
    for w in &pool.warnings {
        eprintln!("warning: {w}");
    }
    Ok(pool)
}

fn find_space<'p>(pool: &'p Pool<File>, key: &str) -> Result<&'p Space> {
    if let Some(s) = pool.find_space(key) {
        return Ok(s);
    }
    if let Some(s) = key.parse().ok().and_then(|id: u64| pool.spaces.get(&id)) {
        return Ok(s);
    }
    bail!("no space named {key:?}")
}

fn size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit + 1 < UNITS.len() {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.2} {}", UNITS[unit])
    }
}

fn info(pool: &Pool<File>, all: bool) -> Result<()> {
    println!("Pool {:?} {}", pool.name, pool.guid);
    println!("  database sequence {}", pool.database.sequence);
    println!("Disks:");
    for d in pool.disks.values() {
        let state = match d.member {
            Some(m) => format!("device {}", pool.members[m].device),
            None => "MISSING".into(),
        };
        println!("  {:>3}  {}  {:<32} {state}", d.id, d.guid, d.name);
    }
    println!("Spaces:");
    for s in pool.spaces.values().filter(|s| all || s.is_user()) {
        let role = match s.info.role {
            SpaceRole::User => "user".to_string(),
            SpaceRole::Metadata => "metadata".into(),
            SpaceRole::Cache => "cache".into(),
            SpaceRole::Other(b) => format!("role {b:#x}"),
        };
        let kind = if s.info.is_child { "child" } else { &role };
        println!("  {:>3}  {}  {:?}  [{kind}]", s.id(), s.info.guid, s.name());
        if let Some(size_bytes) = s.info.size {
            println!("       size {}, allocated {}", size(size_bytes), size(s.allocated()));
        } else {
            println!("       allocated {}", size(s.allocated()));
        }
        if let Some(p) = s.info.policy {
            println!(
                "       {:?}, {} column(s), {} copies, redundancy {}, interleave {}",
                p.resiliency,
                p.columns,
                p.copies,
                p.redundancy,
                size(p.interleave)
            );
        }
        if let Some(parent) = s.info.parent.filter(|&p| p != 0) {
            println!("       parent {parent}");
        }
        if s.is_user() {
            match pool.open_space(s.id()) {
                Ok(r) => {
                    if let Some(cache) = r.cache() {
                        println!(
                            "       write-back cache: {} of {} chunks of {} in use",
                            cache.cached_chunks(),
                            cache.header.chunk_count,
                            size(cache.header.chunk_size as u64)
                        );
                    }
                }
                Err(e) => println!("       cannot open: {e}"),
            }
        }
    }
    Ok(())
}

fn extents(pool: &Pool<File>, space: &Space) -> Result<()> {
    let mut list = space.extents.clone();
    list.sort_by_key(|e| (e.virtual_slab, e.column, e.copy));
    println!(
        "{:>8} {:>6} {:>4} {:>6} {:>5} {:>12}",
        "vslab", "column", "copy", "slabs", "disk", "phys offset"
    );
    for e in list {
        println!(
            "{:>8} {:>6} {:>4} {:>6} {:>5} {:>#12x}",
            e.virtual_slab,
            e.column,
            e.copy,
            e.slab_count,
            e.disk_id,
            e.physical_slab * SLAB_SIZE
        );
    }
    for child in pool.children(space.id()) {
        println!("child space {} ({} extents)", child.id(), child.extents.len());
    }
    Ok(())
}

fn export(pool: &Pool<File>, space: &Space, output: &PathBuf) -> Result<()> {
    let reader = pool.open_space(space.id())?;
    let mut out = OpenOptions::new().write(true).create_new(true).open(output)?;
    const CHUNK: usize = 1 << 20;
    let mut buf = vec![0u8; CHUNK];
    let mut offset = 0;
    while offset < reader.size() {
        let n = CHUNK.min((reader.size() - offset) as usize);
        reader.read_exact_at(&mut buf[..n], offset)?;
        if buf[..n].iter().all(|&b| b == 0) {
            out.seek(SeekFrom::Current(n as i64))?;
        } else {
            out.write_all(&buf[..n])?;
        }
        offset += n as u64;
    }
    out.set_len(reader.size())?;
    println!("exported {} to {}", size(reader.size()), output.display());
    Ok(())
}

fn check_pattern(pool: &Pool<File>, space: &Space, length: Option<u64>) -> Result<()> {
    let reader = pool.open_space(space.id())?;
    let total = length.unwrap_or(reader.size()).min(reader.size());
    const CHUNK: usize = 1 << 20;
    let mut buf = vec![0u8; CHUNK];
    let mut offset = 0;
    while offset < total {
        let n = CHUNK.min((total - offset) as usize);
        reader.read_exact_at(&mut buf[..n], offset)?;
        if let Some(bad) = testpattern::verify(&buf[..n], offset, space.name()) {
            bail!("pattern mismatch at offset {bad:#x}");
        }
        offset += n as u64;
    }
    println!("pattern OK over {}", size(total));
    Ok(())
}
