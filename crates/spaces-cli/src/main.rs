use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

#[cfg(target_os = "linux")]
mod attach;
mod dump;
#[cfg(all(target_os = "linux", feature = "fuse"))]
mod fuse;
mod nbd;
#[cfg(target_os = "linux")]
mod scan;
#[cfg(unix)]
mod snapshot;
#[cfg(all(target_os = "linux", feature = "ublk"))]
mod ublk;
use clap::{Parser, Subcommand};
use storage_spaces::format::{SLAB_SIZE, SpaceRole};
use storage_spaces::io::ReadAt;
use storage_spaces::segments::SegmentKind;
use storage_spaces::{Condition, Pool, Space, UncleanParity, testpattern};

/// Inspect and read Microsoft Storage Spaces pools.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// What to return after an unclean shutdown for parity stripes whose
    /// parity does not match their data and for mirror copies that differ:
    /// fail the read, or the on-disk data (the highest mirror copy).
    #[arg(long, global = true, value_enum, default_value = "refuse")]
    unclean_parity: UncleanArg,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum UncleanArg {
    Refuse,
    Data,
}

static OPEN_OPTIONS: std::sync::OnceLock<storage_spaces::OpenOptions> = std::sync::OnceLock::new();

/// Opens a space with the options given on the command line.
fn open_space<'p>(pool: &'p Pool<File>, id: u64) -> storage_spaces::Result<storage_spaces::SpaceReader<'p, File>> {
    pool.open_space_with(id, OPEN_OPTIONS.get().copied().unwrap_or_default())
}

/// Command-line arguments that serving processes inherit from `attach`.
pub(crate) fn inherited_args() -> Vec<String> {
    match OPEN_OPTIONS.get().map(|o| o.unclean_parity) {
        Some(UncleanParity::PreferData) => vec!["--unclean-parity".into(), "data".into()],
        _ => Vec::new(),
    }
}

#[derive(Subcommand)]
enum Command {
    /// Find Storage Spaces pool members among the block devices.
    #[cfg(target_os = "linux")]
    Scan,
    /// Expose the spaces of complete pools as read-only block devices
    /// (/dev/mapper/ss-<pool>-<space> and -p<N> for partitions).
    #[cfg(target_os = "linux")]
    Attach {
        /// Only this pool (name or GUID).
        #[arg(long)]
        pool: Option<String>,
        /// Only this space (name or GUID).
        #[arg(short, long)]
        space: Option<String>,
        #[arg(long, value_enum, default_value = "auto")]
        backend: attach::Backend,
        /// Attach pools with missing disks as long as every space is readable.
        #[arg(long)]
        degraded: bool,
        /// Attach even when fewer than half of the pool's disks are present
        /// (their metadata may describe an old state of the pool).
        #[arg(long)]
        force: bool,
        /// Use these member devices instead of scanning.
        devices: Vec<PathBuf>,
    },
    /// Remove attached spaces.
    #[cfg(target_os = "linux")]
    Detach {
        /// Space name, GUID or device-mapper name; all spaces if omitted.
        space: Option<String>,
    },
    /// List attached spaces.
    #[cfg(target_os = "linux")]
    Status,
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
    /// Serve a space over NBD on a Unix socket, read-only unless --rw
    /// (foreground; SIGINT/SIGTERM flush and stop it).
    ///
    /// Attach it with `nbd-client -unix SOCKET /dev/nbdN -b SECTOR -readonly`
    /// (without `-readonly` for --rw).
    ServeNbd {
        #[arg(required = true)]
        devices: Vec<PathBuf>,
        #[arg(short, long)]
        space: String,
        #[arg(long)]
        socket: PathBuf,
        /// Serve the space writable (only spaces whose state is fully
        /// understood; the member devices are written).
        #[arg(long)]
        rw: bool,
        /// Write the socket path here once listening.
        #[arg(long)]
        ready_file: Option<PathBuf>,
    },
    /// Expose a space as a ublk block device, read-only unless --rw
    /// (foreground; stop with SIGINT/SIGTERM). Needs root and the ublk_drv
    /// kernel module.
    #[cfg(all(target_os = "linux", feature = "ublk"))]
    ServeUblk {
        #[arg(required = true)]
        devices: Vec<PathBuf>,
        #[arg(short, long)]
        space: String,
        /// Serve the space writable (only spaces whose state is fully
        /// understood; the member devices are written).
        #[arg(long)]
        rw: bool,
        /// Write the block device path here once it exists.
        #[arg(long)]
        ready_file: Option<PathBuf>,
    },
    /// Expose a space as the read-only file MOUNTPOINT/space.img through FUSE
    /// (foreground; stop by unmounting). Attach it with `losetup -r -b SECTOR`.
    #[cfg(all(target_os = "linux", feature = "fuse"))]
    ServeFuse {
        #[arg(required = true)]
        devices: Vec<PathBuf>,
        #[arg(short, long)]
        space: String,
        #[arg(long)]
        mountpoint: PathBuf,
        /// Write the image file path here once mounted.
        #[arg(long)]
        ready_file: Option<PathBuf>,
    },
    /// Print the pool metadata (database copies and records; cache, parity
    /// journal and dirty region logs of each space), one fact per line.
    Dump {
        #[arg(required = true)]
        devices: Vec<PathBuf>,
    },
    /// Compare the metadata of two states of a pool: lines only in the old
    /// state start with "-", lines only in the new one with "+".
    Diff {
        /// Member disks or images of the old state.
        #[arg(long, required = true, num_args = 1..)]
        old: Vec<PathBuf>,
        /// Member disks or images of the new state.
        #[arg(long, required = true, num_args = 1..)]
        new: Vec<PathBuf>,
    },
    /// Print a device-mapper table for the space (simple and mirror spaces).
    DmTable {
        #[arg(required = true)]
        devices: Vec<PathBuf>,
        #[arg(short, long)]
        space: String,
    },
    /// Verify the test pattern on any file or block device holding a space.
    #[command(hide = true)]
    VerifyPattern {
        path: PathBuf,
        /// Pattern tag (the space name).
        #[arg(long)]
        tag: String,
        #[arg(long)]
        length: Option<u64>,
        /// Instead of a sequential pass, do this many random reads of random
        /// length (4 KiB to 1 MiB, 4 KiB aligned).
        #[arg(long)]
        random: Option<u64>,
        /// Random reads from this many threads at once.
        #[arg(long, default_value_t = 1, requires = "random")]
        jobs: u64,
        /// Read with O_DIRECT, bypassing the page cache.
        #[arg(long)]
        direct: bool,
    },
    /// Capture the metadata a test pool needs into small fixture files.
    #[command(hide = true)]
    Fixture {
        /// Pool directory with disk<N>.img and manifest.json.
        pool_dir: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
        /// Leave out the slots of write-back caches and parity journals (a
        /// full log is megabytes; the fixture then describes an empty cache
        /// and a clean journal).
        #[arg(long)]
        without_cache_slots: bool,
    },
    /// Turn member disk snapshots of tools/vm/Invoke-Scenario.ps1 into raw
    /// images (disk<N>.snap -> disk<N>.img in the same directory).
    #[cfg(unix)]
    #[command(hide = true)]
    SnapshotToRaw {
        #[arg(required = true)]
        snapshots: Vec<PathBuf>,
    },
    /// Write the verification pattern with a tag into a space (tests of
    /// write support: the member devices are written).
    #[command(hide = true)]
    WritePattern {
        #[arg(required = true)]
        devices: Vec<PathBuf>,
        #[arg(short, long)]
        space: String,
        /// Byte offset (a multiple of 4096).
        #[arg(long)]
        offset: u64,
        /// Bytes to write (a multiple of 4096).
        #[arg(long)]
        length: u64,
        #[arg(long)]
        tag: String,
        /// Stop the process before the Nth write reaches a member, as a
        /// power loss would (for crash tests); without a crash, the number
        /// of member writes is printed.
        #[arg(long)]
        crash_after_writes: Option<usize>,
        /// Destage the write-back cache after the writes.
        #[arg(long)]
        destage: bool,
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
    // Output piped into a program that stops reading (`spaces dump | head`)
    // ends the process quietly, as SIGPIPE would, instead of a panic.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = info
            .payload()
            .downcast_ref::<String>()
            .map(String::as_str)
            .unwrap_or_default();
        if message.starts_with("failed printing to stdout") && message.contains("Broken pipe") {
            std::process::exit(141);
        }
        default_hook(info);
    }));
    let cli = Cli::parse();
    let unclean_parity = match cli.unclean_parity {
        UncleanArg::Refuse => UncleanParity::Refuse,
        UncleanArg::Data => UncleanParity::PreferData,
    };
    let _ = OPEN_OPTIONS.set(storage_spaces::OpenOptions { unclean_parity });
    match cli.command {
        Command::Info { devices, all } => info(&open_pool(&devices)?, all),
        Command::Extents { devices, space } => {
            let pool = open_pool(&devices)?;
            extents(&pool, find_space(&pool, &space)?)
        }
        Command::Export { devices, space, output } => {
            let pool = open_pool(&devices)?;
            export(&pool, find_space(&pool, &space)?, &output)
        }
        Command::ServeNbd {
            devices,
            space,
            socket,
            rw,
            ready_file,
        } => {
            let pool = if rw {
                open_pool_rw(&devices)?
            } else {
                open_pool_exclusive(&devices)?
            };
            serve_nbd(&pool, find_space(&pool, &space)?, &socket, ready_file.as_deref(), rw)
        }
        #[cfg(all(target_os = "linux", feature = "ublk"))]
        Command::ServeUblk {
            devices,
            space,
            rw,
            ready_file,
        } => {
            let pool: &'static Pool<File> = Box::leak(Box::new(if rw {
                open_pool_rw(&devices)?
            } else {
                open_pool_exclusive(&devices)?
            }));
            let id = find_space(pool, &space)?.id();
            let writer: Option<&'static storage_spaces::SpaceWriter<'static, File>> = if rw {
                Some(Box::leak(Box::new(pool.open_space_rw(id)?)))
            } else {
                None
            };
            let source: &'static dyn storage_spaces::io::ReadAt = match writer {
                Some(w) => w,
                None => Box::leak(Box::new(open_space(pool, id)?)),
            };
            ublk::serve(pool, source, writer, move |dev| {
                println!("{dev}");
                if let Some(path) = &ready_file
                    && let Err(e) = std::fs::write(path, dev)
                {
                    eprintln!("cannot write {}: {e}", path.display());
                }
            })
        }
        #[cfg(all(target_os = "linux", feature = "fuse"))]
        Command::ServeFuse {
            devices,
            space,
            mountpoint,
            ready_file,
        } => {
            let pool: &'static Pool<File> = Box::leak(Box::new(open_pool_exclusive(&devices)?));
            let reader = Box::leak(Box::new(open_space(pool, find_space(pool, &space)?.id())?));
            fuse::serve(reader, pool.logical_sector_size, &mountpoint, ready_file.as_deref())
        }
        #[cfg(target_os = "linux")]
        Command::Scan => cmd_scan(),
        #[cfg(target_os = "linux")]
        Command::Attach {
            pool,
            space,
            backend,
            degraded,
            force,
            devices,
        } => cmd_attach(
            pool.as_deref(),
            space.as_deref(),
            backend,
            degraded || force,
            force,
            &devices,
        ),
        #[cfg(target_os = "linux")]
        Command::Detach { space } => cmd_detach(space.as_deref()),
        #[cfg(target_os = "linux")]
        Command::Status => cmd_status(),
        Command::Dump { devices } => {
            for line in dump::dump(&open_pool(&devices)?) {
                println!("{line}");
            }
            Ok(())
        }
        Command::Diff { old, new } => {
            let old = dump::dump(&open_pool(&old)?);
            let new = dump::dump(&open_pool(&new)?);
            for line in dump::diff(&old, &new) {
                println!("{line}");
            }
            Ok(())
        }
        Command::DmTable { devices, space } => {
            let pool = open_pool(&devices)?;
            dm_table(&pool, find_space(&pool, &space)?, &devices)
        }
        Command::VerifyPattern {
            path,
            tag,
            length,
            random,
            jobs,
            direct,
        } => verify_pattern(&path, &tag, length, random, jobs, direct),
        Command::Fixture {
            pool_dir,
            output,
            without_cache_slots,
        } => fixture(&pool_dir, &output, without_cache_slots),
        #[cfg(unix)]
        Command::SnapshotToRaw { snapshots } => {
            for snap in snapshots {
                let out = snap.with_extension("img");
                let (stored, pattern) = snapshot::to_raw(&snap, &out)?;
                println!("{}: {stored} stored and {pattern} pattern pages", out.display());
            }
            Ok(())
        }
        Command::WritePattern {
            devices,
            space,
            offset,
            length,
            tag,
            crash_after_writes,
            destage,
        } => write_pattern(&devices, &space, offset, length, &tag, crash_after_writes, destage),
        Command::CheckPattern { devices, space, length } => {
            let pool = open_pool(&devices)?;
            check_pattern(&pool, find_space(&pool, &space)?, length)
        }
    }
}

fn open_pool(paths: &[PathBuf]) -> Result<Pool<File>> {
    open_pool_with(paths, false)
}

/// Opens the members exclusively (O_EXCL on block devices), so that nothing
/// else can mount or assemble them while a serving process uses them.
fn open_pool_exclusive(paths: &[PathBuf]) -> Result<Pool<File>> {
    open_pool_with(paths, true)
}

/// Opens the members for reading and writing, exclusively (for `--rw`).
fn open_pool_rw(paths: &[PathBuf]) -> Result<Pool<File>> {
    open_pool_mode(paths, true, true)
}

fn open_pool_with(paths: &[PathBuf], exclusive: bool) -> Result<Pool<File>> {
    open_pool_mode(paths, exclusive, false)
}

fn open_pool_mode(paths: &[PathBuf], exclusive: bool, write: bool) -> Result<Pool<File>> {
    let files = paths
        .iter()
        .map(|p| {
            let mut options = std::fs::OpenOptions::new();
            options.read(true).write(write);
            #[cfg(target_os = "linux")]
            if exclusive && p.starts_with("/dev") {
                use std::os::unix::fs::OpenOptionsExt;
                const O_EXCL: i32 = 0o200;
                options.custom_flags(O_EXCL);
            }
            #[cfg(not(target_os = "linux"))]
            let _ = exclusive;
            options.open(p).with_context(|| format!("cannot open {}", p.display()))
        })
        .collect::<Result<Vec<_>>>()?;
    let pool = Pool::open(files)?;
    for w in &pool.warnings {
        eprintln!("warning: {w}");
    }
    Ok(pool)
}

/// A member that ends the process before a given write, as a power loss
/// would leave the disks.
struct CrashAfter {
    file: File,
    writes: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    limit: usize,
}

impl storage_spaces::io::ReadAt for CrashAfter {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
        self.file.read_exact_at(buf, offset)
    }
    fn size(&self) -> std::io::Result<u64> {
        storage_spaces::io::ReadAt::size(&self.file)
    }
}

impl storage_spaces::io::WriteAt for CrashAfter {
    fn write_all_at(&self, buf: &[u8], offset: u64) -> std::io::Result<()> {
        if self.writes.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1 >= self.limit {
            eprintln!("crash before member write {}", self.limit);
            std::process::exit(99);
        }
        storage_spaces::io::WriteAt::write_all_at(&self.file, buf, offset)
    }
    fn flush(&self) -> std::io::Result<()> {
        storage_spaces::io::WriteAt::flush(&self.file)
    }
}

fn write_pattern(
    devices: &[PathBuf],
    space: &str,
    offset: u64,
    length: u64,
    tag: &str,
    crash_after_writes: Option<usize>,
    destage: bool,
) -> Result<()> {
    let block = testpattern::BLOCK as u64;
    if !offset.is_multiple_of(block) || !length.is_multiple_of(block) {
        bail!("offset and length must be multiples of {block}");
    }
    if let Some(limit) = crash_after_writes {
        let writes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let files = devices
            .iter()
            .map(|p| {
                let file = std::fs::OpenOptions::new().read(true).write(true).open(p)?;
                Ok(CrashAfter {
                    file,
                    writes: writes.clone(),
                    limit,
                })
            })
            .collect::<std::io::Result<Vec<_>>>()?;
        let pool = Pool::open(files)?;
        let id = pool
            .find_space(space)
            .map(|s| s.id())
            .ok_or_else(|| anyhow::anyhow!("no space {space:?}"))?;
        let writer = pool.open_space_rw(id)?;
        write_pattern_with(&writer, offset, length, tag, destage)?;
        eprintln!("member writes: {}", writes.load(std::sync::atomic::Ordering::SeqCst));
        return Ok(());
    }
    let pool = open_pool_rw(devices)?;
    let writer = pool.open_space_rw(find_space(&pool, space)?.id())?;
    write_pattern_with(&writer, offset, length, tag, destage)
}

fn write_pattern_with<D: storage_spaces::io::WriteAt>(
    writer: &storage_spaces::SpaceWriter<'_, D>,
    offset: u64,
    length: u64,
    tag: &str,
    destage: bool,
) -> Result<()> {
    let mut buf = vec![0u8; 1 << 20];
    let mut at = offset;
    while at < offset + length {
        let n = (offset + length - at).min(buf.len() as u64) as usize;
        for (i, b) in buf[..n].chunks_mut(testpattern::BLOCK).enumerate() {
            testpattern::fill_block(b, at + (i * testpattern::BLOCK) as u64, tag);
        }
        writer.write_all_at(&buf[..n], at)?;
        at += n as u64;
    }
    writer.flush()?;
    if destage {
        writer.destage()?;
    }
    println!("wrote {} at {offset:#x} with tag {tag:?}", size(length));
    Ok(())
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
    println!(
        "  version {}, sectors {} logical / {} physical",
        pool.version, pool.logical_sector_size, pool.physical_sector_size
    );
    println!("  database sequence {}", pool.database.sequence);
    if !pool.has_quorum() {
        println!("  WARNING: fewer than half of the disks are present; the metadata may be out of date");
    }
    println!("Disks:");
    for d in pool.disks.values() {
        let state = match d.member {
            Some(m) => format!("device {}", pool.members[m].device),
            None => "MISSING".into(),
        };
        println!(
            "  {:>3}  {}  {:<13} {:<11} {state}",
            d.id,
            d.guid,
            d.usage.name(),
            d.media.name()
        );
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
            match open_space(pool, s.id()) {
                Ok(r) => {
                    println!(
                        "       {}, provisioning {:?}",
                        match r.condition() {
                            Condition::Healthy => "healthy",
                            Condition::Degraded => "degraded: redundancy reduced by missing or out-of-date disks",
                            Condition::Failed => "failed: some data is only on missing disks",
                        },
                        s.info.provisioning
                    );
                    if r.unclean_parity_runs() > 0 {
                        println!(
                            "       parity journal: {} extent run(s) not cleanly shut down; mismatching stripes are {}",
                            r.unclean_parity_runs(),
                            if OPEN_OPTIONS
                                .get()
                                .is_some_and(|o| o.unclean_parity == UncleanParity::PreferData)
                            {
                                "read as on disk"
                            } else {
                                "refused (--unclean-parity data reads them as on disk)"
                            }
                        );
                    }
                    if r.listed_mirror_runs() > 0 {
                        println!(
                            "       dirty region log: {} extent run(s) written since the space was last disconnected; their copies are compared on read, and differing copies are {}",
                            r.listed_mirror_runs(),
                            if OPEN_OPTIONS
                                .get()
                                .is_some_and(|o| o.unclean_parity == UncleanParity::PreferData)
                            {
                                "read from the highest copy"
                            } else {
                                "refused (--unclean-parity data reads the highest copy)"
                            }
                        );
                    }
                    if let Some(cache) = r.cache() {
                        println!(
                            "       write-back cache: {} of {} chunks of {} in use",
                            cache.cached_chunks(),
                            cache.header.chunk_count,
                            size(cache.header.chunk_size as u64)
                        );
                        if cache.conflicting_chunks() > 0 {
                            println!(
                                "       write-back cache: its copies disagree about {} chunk(s) after an unclean shutdown; those are {}",
                                cache.conflicting_chunks(),
                                if OPEN_OPTIONS
                                    .get()
                                    .is_some_and(|o| o.unclean_parity == UncleanParity::PreferData)
                                {
                                    "read from the newest copy"
                                } else {
                                    "refused (--unclean-parity data reads the newest)"
                                }
                            );
                        }
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
    let reader = open_space(pool, space.id())?;
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
    println!(
        "exported {} to {} (logical sector size {}; use `losetup -b {}`)",
        size(reader.size()),
        output.display(),
        pool.logical_sector_size,
        pool.logical_sector_size
    );
    Ok(())
}

fn check_pattern(pool: &Pool<File>, space: &Space, length: Option<u64>) -> Result<()> {
    let reader = open_space(pool, space.id())?;
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

fn dm_table(pool: &Pool<File>, space: &Space, paths: &[PathBuf]) -> Result<()> {
    let reader = open_space(pool, space.id())?;
    for seg in reader.segments()? {
        let (start, length) = (seg.start / 512, seg.length / 512);
        match seg.kind {
            SegmentKind::Zero => println!("{start} {length} zero"),
            SegmentKind::Striped { stripes, .. } if stripes.len() == 1 => {
                println!(
                    "{start} {length} linear {} {}",
                    paths[stripes[0].device].display(),
                    stripes[0].offset / 512
                )
            }
            SegmentKind::Striped { chunk, stripes } => {
                let devs: Vec<String> = stripes
                    .iter()
                    .map(|s| format!("{} {}", paths[s.device].display(), s.offset / 512))
                    .collect();
                println!(
                    "{start} {length} striped {} {} {}",
                    stripes.len(),
                    chunk / 512,
                    devs.join(" ")
                );
            }
        }
    }
    Ok(())
}

fn verify_pattern(
    path: &PathBuf,
    tag: &str,
    length: Option<u64>,
    random: Option<u64>,
    jobs: u64,
    direct: bool,
) -> Result<()> {
    let mut options = OpenOptions::new();
    options.read(true);
    if direct {
        #[cfg(target_os = "linux")]
        std::os::unix::fs::OpenOptionsExt::custom_flags(&mut options, 0o40000); // O_DIRECT
    }
    let file = options
        .open(path)
        .with_context(|| format!("cannot open {}", path.display()))?;
    let total = length.map_or_else(|| file.size(), Ok)?;
    // O_DIRECT needs buffers aligned to the logical block size.
    const ALIGN: usize = 4096;
    let aligned = |len: usize| {
        let v = vec![0u8; len + ALIGN];
        let skip = v.as_ptr().align_offset(ALIGN);
        (v, skip)
    };
    if let Some(count) = random {
        let jobs = jobs.max(1);
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos() as u64;
        let blocks = total / 4096;
        std::thread::scope(|scope| -> Result<()> {
            let workers: Vec<_> = (0..jobs)
                .map(|job| {
                    let file = &file;
                    scope.spawn(move || -> Result<()> {
                        // xorshift64*, seeded from the clock; failures print the offset.
                        let mut state = seed.wrapping_add(job.wrapping_mul(0x9E37_79B9_7F4A_7C15)) | 1;
                        let mut next = move || {
                            state ^= state >> 12;
                            state ^= state << 25;
                            state ^= state >> 27;
                            state.wrapping_mul(0x2545_F491_4F6C_DD1D)
                        };
                        let (mut v, skip) = aligned(1 << 20);
                        let buf = &mut v[skip..skip + (1 << 20)];
                        for _ in 0..count.div_ceil(jobs) {
                            let len = ((next() % 256 + 1) * 4096).min(total) as usize;
                            let offset = (next() % (blocks - len as u64 / 4096 + 1)) * 4096;
                            file.read_exact_at(&mut buf[..len], offset)?;
                            if let Some(bad) = testpattern::verify(&buf[..len], offset, tag) {
                                bail!("pattern mismatch at offset {bad:#x}");
                            }
                        }
                        Ok(())
                    })
                })
                .collect();
            for w in workers {
                w.join().map_err(|_| anyhow::anyhow!("reader thread panicked"))??;
            }
            Ok(())
        })?;
        println!(
            "pattern OK for {} random reads ({jobs} thread(s){}) over {}",
            count.div_ceil(jobs) * jobs,
            if direct { ", O_DIRECT" } else { "" },
            size(total)
        );
        return Ok(());
    }
    const CHUNK: usize = 1 << 20;
    let (mut v, skip) = aligned(CHUNK);
    let buf = &mut v[skip..skip + CHUNK];
    let mut offset = 0;
    while offset < total {
        let n = CHUNK.min((total - offset) as usize);
        file.read_exact_at(&mut buf[..n], offset)?;
        if let Some(bad) = testpattern::verify(&buf[..n], offset, tag) {
            bail!("pattern mismatch at offset {bad:#x}");
        }
        offset += n as u64;
    }
    println!("pattern OK over {}", size(total));
    Ok(())
}

/// Writes through a space writer (`--rw`).
impl nbd::Sink for storage_spaces::SpaceWriter<'_, File> {
    fn write_all_at(&self, buf: &[u8], offset: u64) -> std::io::Result<()> {
        storage_spaces::SpaceWriter::write_all_at(self, buf, offset).map_err(std::io::Error::other)
    }
    fn flush(&self) -> std::io::Result<()> {
        storage_spaces::SpaceWriter::flush(self).map_err(std::io::Error::other)
    }
}

fn serve_nbd(
    pool: &Pool<File>,
    space: &Space,
    socket: &PathBuf,
    ready_file: Option<&std::path::Path>,
    rw: bool,
) -> Result<()> {
    let writer = if rw {
        Some(pool.open_space_rw(space.id())?)
    } else {
        None
    };
    let reader_ro;
    let source: &dyn storage_spaces::io::ReadAt = match &writer {
        Some(w) => w,
        None => {
            reader_ro = open_space(pool, space.id())?;
            &reader_ro
        }
    };
    let listener = std::os::unix::net::UnixListener::bind(socket)
        .with_context(|| format!("cannot listen on {}", socket.display()))?;
    let space_size = source.size()?;
    let export = nbd::Export {
        name: space.name(),
        source,
        sink: writer.as_ref().map(|w| w as &dyn nbd::Sink),
        size: space_size,
        block_size: pool.logical_sector_size,
    };
    eprintln!(
        "serving {:?} ({}{}) on {}",
        space.name(),
        size(space_size),
        if rw { ", writable" } else { "" },
        socket.display()
    );
    if let Some(path) = ready_file {
        std::fs::write(path, socket.display().to_string())?;
    }
    // SIGINT/SIGTERM: what was written becomes durable before the process
    // ends (the log slots of cached writes wait for a flush).
    let mut signals = signal_hook::iterator::Signals::new([signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM])?;
    std::thread::scope(|scope| {
        let writer = writer.as_ref();
        scope.spawn(move || {
            if signals.forever().next().is_some() {
                let code = match writer.map_or(Ok(()), |w| w.flush()) {
                    Ok(()) => 0,
                    Err(e) => {
                        eprintln!("flush failed: {e}");
                        1
                    }
                };
                std::process::exit(code);
            }
        });
        for conn in listener.incoming() {
            let conn = conn?;
            let export = &export;
            scope.spawn(move || {
                if let Err(e) = nbd::serve(export, conn) {
                    eprintln!("connection failed: {e}");
                }
            });
        }
        Ok(())
    })
}

#[cfg(target_os = "linux")]
fn cmd_scan() -> Result<()> {
    let pools = scan::scan();
    if pools.is_empty() {
        println!("no Storage Spaces pool members found");
    }
    for (guid, members) in pools {
        let paths: Vec<PathBuf> = members.iter().map(|m| m.path.clone()).collect();
        match open_pool(&paths) {
            Ok(pool) => {
                let present = pool.disks.values().filter(|d| d.member.is_some()).count();
                println!("pool {:?} {guid}: {present} of {} disks", pool.name, pool.disks.len());
                for m in &members {
                    println!("  {}", m.path.display());
                }
                for s in pool.user_spaces() {
                    println!(
                        "  space {:?} {} ({})",
                        s.name(),
                        s.info.guid,
                        size(s.info.size.unwrap_or(0))
                    );
                }
            }
            Err(e) => println!("pool {guid}: cannot open: {e}"),
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn cmd_attach(
    pool_sel: Option<&str>,
    space_sel: Option<&str>,
    backend: attach::Backend,
    degraded: bool,
    force: bool,
    devices: &[PathBuf],
) -> Result<()> {
    let _lock = attach::lock()?;
    let groups: Vec<Vec<PathBuf>> = if devices.is_empty() {
        scan::scan()
            .into_values()
            .map(|m| m.into_iter().map(|c| c.path).collect())
            .collect()
    } else {
        vec![devices.to_vec()]
    };
    let mut failures = 0;
    for paths in groups {
        let pool = open_pool(&paths)?;
        if pool_sel.is_some_and(|p| p != pool.name && !p.eq_ignore_ascii_case(&pool.guid.to_string())) {
            continue;
        }
        if !pool.has_quorum() && !force {
            eprintln!(
                "pool {:?}: fewer than half of its disks are present; their metadata may be out of date (use --force)",
                pool.name
            );
            failures += 1;
            continue;
        }
        let missing = pool.disks.values().filter(|d| d.member.is_none()).count();
        if missing > 0 && !degraded {
            eprintln!(
                "pool {:?}: {missing} disk(s) missing, skipping (use --degraded)",
                pool.name
            );
            failures += 1;
            continue;
        }
        for space in pool.user_spaces() {
            if space_sel.is_some_and(|s| s != space.name() && !s.eq_ignore_ascii_case(&space.info.guid.to_string())) {
                continue;
            }
            if attach::is_attached(space) {
                println!("{:?} is already attached", space.name());
                continue;
            }
            match attach::attach_space(&pool, space, &paths, backend) {
                Ok(state) => {
                    println!("attached {:?} with {}:", space.name(), state.backend);
                    for d in &state.dm {
                        println!("  /dev/mapper/{d}");
                    }
                }
                Err(e) => {
                    eprintln!("cannot attach {:?}: {e:#}", space.name());
                    failures += 1;
                }
            }
        }
    }
    if failures > 0 {
        bail!("{failures} space(s) or pool(s) not attached");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn cmd_detach(sel: Option<&str>) -> Result<()> {
    let _lock = attach::lock()?;
    let mut failures = 0;
    for state in attach::State::load_all() {
        let matches = sel.is_none_or(|s| {
            s.eq_ignore_ascii_case(&state.space_guid)
                || state
                    .dm
                    .first()
                    .is_some_and(|d| d == s || d.ends_with(&format!("-{s}")))
        });
        if !matches {
            continue;
        }
        match attach::teardown(&state) {
            Ok(()) => println!("detached {}", state.dm.first().map_or(state.space_guid.as_str(), |d| d)),
            Err(e) => {
                eprintln!("cannot fully detach {}: {e:#}", state.space_guid);
                failures += 1;
            }
        }
    }
    if failures > 0 {
        bail!("{failures} space(s) not fully detached");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn cmd_status() -> Result<()> {
    let states = attach::State::load_all();
    if states.is_empty() {
        println!("no spaces attached");
    }
    for s in states {
        let main = s.dm.first().cloned().unwrap_or_default();
        println!("/dev/mapper/{main}  space {}  backend {}", s.space_guid, s.backend);
        if let Some(d) = &s.device {
            println!("  backend device {d}");
        }
        if let Some(u) = &s.unit {
            println!("  unit {u}");
        }
        for p in s.dm.iter().skip(1) {
            println!("  partition /dev/mapper/{p}");
        }
    }
    Ok(())
}

fn fixture(dir: &std::path::Path, out: &std::path::Path, without_cache_slots: bool) -> Result<()> {
    use storage_spaces::io::{Recording, SparseImage};
    let mut paths = Vec::new();
    while dir.join(format!("disk{}.img", paths.len())).exists() {
        paths.push(dir.join(format!("disk{}.img", paths.len())));
    }
    let devices: Vec<Recording<File>> = paths
        .iter()
        .map(|p| File::open(p).map(Recording::new))
        .collect::<std::io::Result<_>>()?;
    let pool = Pool::open(devices.iter().collect::<Vec<_>>())?;
    for s in pool.user_spaces() {
        pool.open_space(s.id())?;
    }
    std::fs::create_dir_all(out)?;
    let mut total = 0;
    for (i, dev) in devices.iter().enumerate() {
        let mut image = SparseImage::new(dev.size()?);
        for (offset, len) in dev.reads() {
            // Keep only non-zero 4 KiB pages of what was read.
            let mut buf = vec![0u8; len];
            dev.inner().read_exact_at(&mut buf, offset)?;
            for (k, page) in buf.chunks(4096).enumerate() {
                if page.iter().any(|&b| b != 0) && !(without_cache_slots && page.starts_with(b"SPSLOT")) {
                    image.insert(offset + (k * 4096) as u64, page);
                }
            }
        }
        total += image.stored();
        image.write_to(std::io::BufWriter::new(File::create(
            out.join(format!("disk{i}.fixture")),
        )?))?;
    }
    std::fs::copy(dir.join("manifest.json"), out.join("manifest.json"))?;
    println!("{}: {} disks, {} bytes of metadata", out.display(), paths.len(), total);
    Ok(())
}
