//! `spaces pool`, `spaces space` and `spaces disk`: pool management. Every
//! command computes its plan first and prints it; it writes only with
//! `--yes`. Devices are opened exclusively (O_EXCL on block devices).

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use storage_spaces::io::ReadAt;
use storage_spaces::ops::{self, BlankDisk, SpaceSpec};
use storage_spaces::plan::{Action, Plan, Target};
use storage_spaces::{Guid, Pool};

#[derive(Subcommand)]
pub enum PoolCommand {
    /// Create a pool of blank disks (Windows 11 24H2 layout, pool version 28).
    Create {
        /// The pool's name.
        #[arg(long)]
        name: String,
        /// Logical sector size of the pool's spaces (default: the largest of
        /// the disks', 512 or 4096).
        #[arg(long)]
        logical_sector: Option<u32>,
        /// Overwrite disks that are not blank (partition tables, file systems:
        /// everything on them is lost).
        #[arg(long)]
        wipe: bool,
        /// Write; without it the plan is only printed.
        #[arg(long)]
        yes: bool,
        #[arg(required = true)]
        devices: Vec<PathBuf>,
    },
}

#[derive(Subcommand)]
pub enum SpaceCommand {
    /// Create a space.
    Create {
        #[arg(long)]
        name: String,
        /// simple, mirror or parity (single parity).
        #[arg(long)]
        resiliency: String,
        /// Size, e.g. 10G (rounded up to whole rows).
        #[arg(long, value_parser = parse_size)]
        size: u64,
        /// Thin provisioning (rows allocated as they are written).
        #[arg(long)]
        thin: bool,
        /// Mirror: 2 (default) or 3 copies.
        #[arg(long)]
        copies: Option<u64>,
        #[arg(long)]
        columns: Option<u64>,
        /// Interleave, e.g. 256K (default).
        #[arg(long, value_parser = parse_size)]
        interleave: Option<u64>,
        /// Parity: size of the write-back cache (default 1G, at least 512M).
        #[arg(long, value_parser = parse_size)]
        write_cache: Option<u64>,
        #[arg(long)]
        yes: bool,
        #[arg(required = true)]
        devices: Vec<PathBuf>,
    },
}

/// Parses a size with an optional binary suffix (K, M, G, T).
pub fn parse_size(s: &str) -> std::result::Result<u64, String> {
    let s = s.trim();
    let (digits, shift) = match s.char_indices().last() {
        Some((i, c)) if c.is_ascii_alphabetic() => (
            &s[..i],
            match c.to_ascii_uppercase() {
                'K' => 10,
                'M' => 20,
                'G' => 30,
                'T' => 40,
                _ => return Err(format!("unknown size suffix in {s}")),
            },
        ),
        _ => (s, 0),
    };
    let n: u64 = digits.parse().map_err(|_| format!("not a size: {s}"))?;
    n.checked_mul(1 << shift).ok_or_else(|| format!("size too large: {s}"))
}

/// Random (version 4) GUIDs from the kernel.
fn random_guids() -> impl FnMut() -> Guid {
    let mut urandom = File::open("/dev/urandom").ok();
    move || {
        let mut g = [0u8; 16];
        match urandom.as_mut() {
            Some(f) => std::io::Read::read_exact(f, &mut g).expect("reading /dev/urandom"),
            None => {
                let t = ops::filetime_now().to_le_bytes();
                g[..8].copy_from_slice(&t);
                g[8..].copy_from_slice(&(std::process::id() as u64).to_le_bytes());
            }
        }
        g[6] = (g[6] & 0x0f) | 0x40;
        g[8] = (g[8] & 0x3f) | 0x80;
        Guid(g)
    }
}

fn open_rw(paths: &[PathBuf]) -> Result<Vec<File>> {
    paths
        .iter()
        .map(|p| {
            let mut options = OpenOptions::new();
            options.read(true).write(true);
            #[cfg(target_os = "linux")]
            if p.starts_with("/dev") {
                use std::os::unix::fs::OpenOptionsExt;
                const O_EXCL: i32 = 0o200;
                options.custom_flags(O_EXCL);
            }
            options.open(p).with_context(|| format!("cannot open {}", p.display()))
        })
        .collect()
}

/// A sysfs attribute of a block device (through its parent for partitions).
fn sysfs(dev: &Path, attr: &str) -> Option<String> {
    let dev = std::fs::canonicalize(dev).ok()?;
    let name = dev.file_name()?.to_str()?;
    let sys = std::fs::canonicalize(Path::new("/sys/class/block").join(name)).ok()?;
    let base = if sys.join("partition").exists() {
        sys.parent()?.to_path_buf()
    } else {
        sys
    };
    std::fs::read_to_string(base.join(attr))
        .ok()
        .map(|s| s.trim().to_string())
}

fn describe(path: &Path, file: &File) -> Result<BlankDisk> {
    let size = file.size()?;
    let block = path.starts_with("/dev");
    let number = |attr: &str, default: u64| sysfs(path, attr).and_then(|s| s.parse().ok()).unwrap_or(default);
    Ok(BlankDisk {
        size,
        logical_sector: if block {
            number("queue/logical_block_size", 512)
        } else {
            512
        },
        physical_sector: if block {
            number("queue/physical_block_size", 4096)
        } else {
            4096
        },
        manufacturer: sysfs(path, "device/vendor").unwrap_or_else(|| "Linux".into()),
        model: sysfs(path, "device/model").unwrap_or_else(|| if block { "Disk".into() } else { "Image".into() }),
    })
}

/// Whether the first and last MiB of a disk are zero.
fn is_blank(file: &File) -> Result<bool> {
    let size = file.size()?;
    let mut buf = vec![0u8; (1 << 20).min(size as usize)];
    file.read_exact_at(&mut buf, 0)?;
    if buf.iter().any(|&b| b != 0) {
        return Ok(false);
    }
    let end = size - buf.len() as u64;
    file.read_exact_at(&mut buf, end)?;
    Ok(buf.iter().all(|&b| b == 0))
}

/// Prints the plan; carries it out only with `yes`.
fn run<M: storage_spaces::io::WriteAt, N: storage_spaces::io::WriteAt>(
    plan: &Plan,
    yes: bool,
    members: &[M],
    new: &[N],
) -> Result<bool> {
    print!("{plan}");
    if !yes {
        println!("nothing written (add --yes to carry the plan out)");
        return Ok(false);
    }
    plan.apply(members, new)?;
    println!("done");
    Ok(true)
}

pub fn pool(command: PoolCommand) -> Result<()> {
    match command {
        PoolCommand::Create {
            name,
            logical_sector,
            wipe,
            yes,
            devices,
        } => {
            let files = open_rw(&devices)?;
            let mut disks = Vec::new();
            let mut dirty = Vec::new();
            for (i, (path, file)) in devices.iter().zip(&files).enumerate() {
                disks.push(describe(path, file)?);
                if !is_blank(file)? {
                    dirty.push(i);
                }
            }
            if !dirty.is_empty() && !wipe {
                bail!(
                    "not blank (partition table, file system or pool data at the start or end): {}; \
                     --wipe overwrites them",
                    dirty
                        .iter()
                        .map(|&i| devices[i].display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            let (mut plan, _) = ops::plan_create_pool(&disks, &name, logical_sector, &mut random_guids())?;
            if !dirty.is_empty() {
                // First clear what the pool's writes leave of the old start
                // and end (partition tables, file system signatures).
                let mut wipe_step = Vec::new();
                for &i in &dirty {
                    let size = disks[i].size;
                    wipe_step.push(Action::Write {
                        target: Target::New(i),
                        offset: 0,
                        bytes: vec![0; (17 << 20).min(size as usize)],
                    });
                    wipe_step.push(Action::Write {
                        target: Target::New(i),
                        offset: size - (1 << 20).min(size),
                        bytes: vec![0; (1 << 20).min(size as usize)],
                    });
                }
                plan.summary
                    .push(format!("wipe the start and end of {} disks", dirty.len()));
                plan.steps.insert(
                    0,
                    storage_spaces::plan::Step {
                        what: "wipe".into(),
                        actions: wipe_step,
                    },
                );
            }
            if run::<File, File>(&plan, yes, &[], &files)? {
                drop(files);
                let pool = Pool::open(open_rw(&devices)?)?;
                if !pool.warnings.is_empty() {
                    bail!("the new pool reports: {}", pool.warnings.join("; "));
                }
                println!("pool \"{}\" {}", pool.name, pool.guid);
            }
            Ok(())
        }
    }
}

pub fn space(command: SpaceCommand) -> Result<()> {
    match command {
        SpaceCommand::Create {
            name,
            resiliency,
            size,
            thin,
            copies,
            columns,
            interleave,
            write_cache,
            yes,
            devices,
        } => {
            let resiliency = match resiliency.as_str() {
                "simple" => 1,
                "mirror" => 2,
                "parity" => 3,
                other => bail!("resiliency {other}: simple, mirror or parity"),
            };
            let pool = Pool::open(open_rw(&devices)?)?;
            let spec = SpaceSpec {
                name,
                resiliency,
                size,
                thin,
                copies,
                columns,
                interleave,
                write_cache,
            };
            let (plan, new) = ops::plan_create_space(&pool, &spec, &mut random_guids())?;
            // (Exclusive opens: the pool's handles go first.)
            drop(pool);
            let members = open_rw(&devices)?;
            if run::<File, File>(&plan, yes, &members, &[])? {
                drop(members);
                let pool = Pool::open(open_rw(&devices)?)?;
                if !pool.warnings.is_empty() {
                    bail!("the pool reports: {}", pool.warnings.join("; "));
                }
                println!("space \"{}\" {} ({} bytes)", new.name, new.guid, new.size);
            }
            Ok(())
        }
    }
}
