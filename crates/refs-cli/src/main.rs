//! `refs`: read Microsoft ReFS volumes (images, disks, partitions, or a
//! space of a Storage Spaces pool).

#[cfg(all(target_os = "linux", feature = "fuse"))]
mod fuse;

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use refs::{Content, Entry, Target, Volume};
use storage_spaces::io::ReadAt;

#[derive(Parser)]
#[command(version, about = "Read Microsoft ReFS volumes")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// Where the volume is.
#[derive(Args, Clone)]
struct Source {
    /// An image, disk or partition (the first ReFS partition of a disk is
    /// found by its partition table); with --space, the pool's disks.
    #[arg(required = true)]
    devices: Vec<PathBuf>,
    /// Read the volume inside this space of the Storage Spaces pool whose
    /// disks are given.
    #[arg(long)]
    space: Option<String>,
    /// Byte offset of the volume (default: found).
    #[arg(long)]
    offset: Option<u64>,
}

#[derive(Subcommand)]
enum Command {
    /// The volume: version, clusters, checkpoint, objects.
    Info {
        #[command(flatten)]
        source: Source,
    },
    /// List a directory.
    Ls {
        #[command(flatten)]
        source: Source,
        /// The directory ("/" is the root).
        #[arg(long, default_value = "/")]
        path: String,
        /// Long listing: attributes, size, modification time.
        #[arg(short, long)]
        long: bool,
        /// Recurse into subdirectories.
        #[arg(short = 'R', long)]
        recursive: bool,
    },
    /// Write a file's data (or a named stream) to standard output.
    Cat {
        #[command(flatten)]
        source: Source,
        #[arg(long)]
        path: String,
        /// A named (alternate) data stream.
        #[arg(long)]
        stream: Option<String>,
        /// A stream snapshot of the file's data (`refs stat` lists them).
        #[arg(long, conflicts_with = "stream")]
        snapshot: Option<String>,
    },
    /// Everything about a file or directory.
    Stat {
        #[command(flatten)]
        source: Source,
        #[arg(long)]
        path: String,
    },
    /// Mount the volume read-only through FUSE (foreground; unmount with
    /// fusermount -u).
    #[cfg(all(target_os = "linux", feature = "fuse"))]
    Mount {
        #[command(flatten)]
        source: Source,
        mountpoint: PathBuf,
        /// Allow other users to read the mount (needs user_allow_other).
        #[arg(long)]
        allow_other: bool,
        /// Mount for writing (experimental: every change is a transaction
        /// as `refs write` makes; a file is written back whole; not
        /// --space).
        #[arg(long)]
        rw: bool,
    },
    /// Change a file's times or attributes (experimental: writes the
    /// volume, only with --yes). Times are UTC, "YYYY-MM-DD hh:mm:ss" or a
    /// FILETIME number.
    Set {
        /// The image, disk or partition (not --space: pools are not
        /// written by refs).
        device: PathBuf,
        #[arg(long)]
        offset: Option<u64>,
        #[arg(long)]
        path: String,
        #[arg(long, value_parser = parse_time)]
        created: Option<u64>,
        #[arg(long, value_parser = parse_time)]
        modified: Option<u64>,
        #[arg(long, value_parser = parse_time)]
        changed: Option<u64>,
        #[arg(long, value_parser = parse_time)]
        accessed: Option<u64>,
        /// Attribute bits (read-only 1, hidden 2, system 4, archive 0x20,
        /// ...); the ones Windows does not let users set stay.
        #[arg(long, value_parser = parse_number)]
        attributes: Option<u64>,
        /// Turn integrity streams on or off (empty files only).
        #[arg(long, value_parser = ["on", "off"])]
        integrity: Option<String>,
        /// Write (without it, only print what would change).
        #[arg(long)]
        yes: bool,
    },
    /// Overwrite bytes of a file with the content of a local file
    /// (experimental: writes the volume, only with --yes; within the file's
    /// size, not into sparse ranges; an integrity stream's clusters it
    /// touches are copied on write). Sets the modification and change
    /// times to now.
    Overwrite {
        /// The image, disk or partition.
        device: PathBuf,
        #[arg(long)]
        offset: Option<u64>,
        #[arg(long)]
        path: String,
        /// Byte offset in the file.
        #[arg(long, value_parser = parse_number)]
        at: u64,
        /// The bytes to write.
        #[arg(long)]
        from: PathBuf,
        #[arg(long)]
        yes: bool,
    },
    /// Create a file holding the content of a local file (experimental:
    /// writes the volume, only with --yes; up to 64 GiB; a printable ASCII
    /// name).
    Create {
        /// The image, disk or partition.
        device: PathBuf,
        #[arg(long)]
        offset: Option<u64>,
        #[arg(long)]
        path: String,
        /// The content (default: empty).
        #[arg(long)]
        from: Option<PathBuf>,
        #[arg(long)]
        yes: bool,
    },
    /// Delete a file, one name of a hard-linked file, or an empty directory
    /// (experimental: writes the volume, only with --yes).
    Delete {
        /// The image, disk or partition.
        device: PathBuf,
        #[arg(long)]
        offset: Option<u64>,
        #[arg(long)]
        path: String,
        /// Delete this named stream instead.
        #[arg(long)]
        stream: Option<String>,
        #[arg(long)]
        yes: bool,
    },
    /// Rename a file or directory within its directory (experimental:
    /// writes the volume, only with --yes; a printable ASCII name).
    Rename {
        /// The image, disk or partition.
        device: PathBuf,
        #[arg(long)]
        offset: Option<u64>,
        #[arg(long)]
        path: String,
        /// The new name (in the same directory).
        #[arg(long)]
        to: String,
        #[arg(long)]
        yes: bool,
    },
    /// Create a directory (experimental: writes the volume, only with
    /// --yes; a printable ASCII name).
    Mkdir {
        /// The image, disk or partition.
        device: PathBuf,
        #[arg(long)]
        offset: Option<u64>,
        #[arg(long)]
        path: String,
        #[arg(long)]
        yes: bool,
    },
    /// Replace a file's content (or a named stream's) with a local file's,
    /// or append it (experimental: writes the volume, only with --yes; up
    /// to 64 GiB, 2 GiB for integrity streams; not files with
    /// snapshots).
    Write {
        /// The image, disk or partition.
        device: PathBuf,
        #[arg(long)]
        offset: Option<u64>,
        #[arg(long)]
        path: String,
        #[arg(long)]
        from: PathBuf,
        /// Add to the end instead of replacing.
        #[arg(long)]
        append: bool,
        /// Write this named stream instead of the file's data.
        #[arg(long)]
        stream: Option<String>,
        #[arg(long)]
        yes: bool,
    },
    /// Move a file or directory into another directory (experimental:
    /// writes the volume, only with --yes).
    Move {
        /// The image, disk or partition.
        device: PathBuf,
        #[arg(long)]
        offset: Option<u64>,
        #[arg(long)]
        path: String,
        /// The new path (directory and name).
        #[arg(long)]
        to: String,
        #[arg(long)]
        yes: bool,
    },
    /// Give a file another name, a hard link (experimental: writes the
    /// volume, only with --yes).
    Link {
        /// The image, disk or partition.
        device: PathBuf,
        #[arg(long)]
        offset: Option<u64>,
        /// The file.
        #[arg(long)]
        path: String,
        /// The new name's path.
        #[arg(long)]
        to: String,
        #[arg(long)]
        yes: bool,
    },
    /// Copy a file as a block clone, as Copy-Item does on a Dev Drive: the
    /// copy shares the file's clusters, counted in the block reference
    /// count table (experimental: writes the volume, only with --yes; the
    /// data only, not named streams; small files are copied)
    Clone {
        /// The image, disk or partition.
        device: PathBuf,
        #[arg(long)]
        offset: Option<u64>,
        /// The file.
        #[arg(long)]
        path: String,
        /// The copy's path (a new name, or an empty file).
        #[arg(long)]
        to: String,
        #[arg(long)]
        yes: bool,
    },
    /// Check the volume's consistency (read-only): every page's checksum
    /// and structure, every page and file cluster used in its allocator,
    /// clusters several files map counted as shared. Exits with 1 on a
    /// problem.
    Check {
        #[command(flatten)]
        source: Source,
    },
    /// Every cluster the volume uses, by physical cluster: superblocks,
    /// checkpoints, the pages of each tree, the data runs of each file
    /// (for format work: what changed between two images).
    #[command(hide = true)]
    Map {
        #[command(flatten)]
        source: Source,
    },
    /// Every row of a checkpoint root's tree or of an object's tree, in hex
    /// (for format work).
    #[command(hide = true)]
    Tree {
        #[command(flatten)]
        source: Source,
        /// The checkpoint root (0 to 12).
        #[arg(long, conflicts_with = "object")]
        root: Option<usize>,
        /// The object id (hex with 0x, or decimal).
        #[arg(long, value_parser = parse_number)]
        object: Option<u64>,
    },
    /// The raw attribute rows of a file's record, in hex (for format work).
    #[command(hide = true)]
    Rows {
        #[command(flatten)]
        source: Source,
        #[arg(long)]
        path: String,
        /// Bytes of each value shown (default 256).
        #[arg(long, default_value_t = 256)]
        bytes: usize,
    },
    /// Capture what reading a corpus volume needs into a small fixture:
    /// the metadata of every file and the data of the small ones.
    #[command(hide = true)]
    Fixture {
        /// Volume directory with disk.img and manifest.json.
        volume_dir: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
        /// Keep the data of files and streams up to this size.
        #[arg(long, default_value_t = 65537)]
        data_limit: u64,
        /// Leave out what is under this directory (its own entry stays).
        #[arg(long)]
        exclude: Vec<String>,
    },
}

/// A device for the volume: a file, or a space of a pool.
type Device = Box<dyn ReadAt>;

/// A local file as the data `refs::write` writes from (its length fixed
/// when opened).
/// The process's effective user id (from /proc/self/status).
#[cfg(feature = "fuse")]
fn effective_uid() -> Option<u32> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("Uid:"))?;
    line.split_whitespace().nth(2)?.parse().ok()
}

struct FileSource(File, u64);

impl FileSource {
    fn open(path: &std::path::Path) -> Result<Self> {
        let f = File::open(path).with_context(|| format!("cannot read {}", path.display()))?;
        let len = f.metadata()?.len();
        Ok(FileSource(f, len))
    }

    fn empty() -> Result<Self> {
        Ok(FileSource(Self::temporary()?.0, 0))
    }

    /// An unlinked temporary file (in $REFS_TMPDIR, else the system's).
    fn temporary() -> Result<Self> {
        let dir = std::env::var_os("REFS_TMPDIR").map_or_else(std::env::temp_dir, PathBuf::from);
        let path = dir.join(format!("refs-{}", std::process::id()));
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| format!("cannot make {}", path.display()))?;
        let _ = std::fs::remove_file(&path);
        Ok(FileSource(f, 0))
    }
}

impl refs::write::Source for FileSource {
    fn len(&self) -> u64 {
        self.1
    }
    fn read_into(&self, offset: u64, buf: &mut [u8]) -> refs::Result<()> {
        Ok(std::os::unix::fs::FileExt::read_exact_at(&self.0, buf, offset)?)
    }
}

/// A device that refuses writes (a volume mounted read-only).
#[cfg(feature = "fuse")]
struct ReadOnly(Device);

#[cfg(feature = "fuse")]
impl ReadAt for ReadOnly {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
        self.0.read_exact_at(buf, offset)
    }
    fn size(&self) -> std::io::Result<u64> {
        self.0.size()
    }
}

#[cfg(feature = "fuse")]
impl storage_spaces::io::WriteAt for ReadOnly {
    fn write_all_at(&self, _buf: &[u8], _offset: u64) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "mounted read-only",
        ))
    }
    fn flush(&self) -> std::io::Result<()> {
        Ok(())
    }
}

fn open_device(source: &Source) -> Result<Device> {
    let files = source
        .devices
        .iter()
        .map(|p| File::open(p).with_context(|| format!("cannot open {}", p.display())))
        .collect::<Result<Vec<_>>>()?;
    match &source.space {
        None => {
            if files.len() != 1 {
                bail!("one device, or --space with the disks of a pool");
            }
            Ok(Box::new(files.into_iter().next().unwrap()))
        }
        Some(name) => {
            let pool: &'static storage_spaces::Pool<File> = Box::leak(Box::new(storage_spaces::Pool::open(files)?));
            let space = pool
                .find_space(name)
                .with_context(|| format!("no space {name:?} in pool {:?}", pool.name))?;
            Ok(Box::new(pool.open_space(space.id())?))
        }
    }
}

/// Byte offsets of the ReFS volumes on a device: the device itself, or its
/// partitions.
fn find_volumes(dev: &dyn ReadAt) -> Result<Vec<u64>> {
    let mut sector = vec![0u8; 512];
    dev.read_exact_at(&mut sector, 0)?;
    if refs::boot::BootSector::is_refs(&sector) {
        return Ok(vec![0]);
    }
    let mut found = Vec::new();
    for size in [512u64, 4096] {
        for p in storage_spaces::gpt::read_partitions(dev, size).unwrap_or_default() {
            if dev.read_exact_at(&mut sector, p.offset).is_ok() && refs::boot::BootSector::is_refs(&sector) {
                found.push(p.offset);
            }
        }
        if !found.is_empty() {
            break;
        }
    }
    Ok(found)
}

fn open_volume(source: &Source) -> Result<Volume<Device>> {
    let dev = open_device(source)?;
    let offset = match source.offset {
        Some(o) => o,
        None => *find_volumes(dev.as_ref())?
            .first()
            .context("no ReFS volume on the device (or its partitions)")?,
    };
    Ok(Volume::open(dev, offset)?)
}

/// FILETIME as "YYYY-MM-DD hh:mm:ss" (UTC).
fn time(filetime: u64) -> String {
    let secs = (filetime / 10_000_000) as i64 - 11_644_473_600;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil from days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Prints "LCN COUNT WHAT" for every cluster the volume uses.
fn map(vol: &Volume<Device>, out: &mut impl Write) -> Result<()> {
    let total = vol.boot.volume_size() / vol.cluster;
    let per_page = vol.page_size / vol.cluster;
    for (lcn, what) in [
        (refs::volume::SUPERBLOCK_LCN, "superblock"),
        (total - 2, "superblock copy"),
        (total - 3, "superblock copy"),
    ] {
        writeln!(out, "{lcn:#x} 1 {what}")?;
    }
    for &lcn in &vol.checkpoint_lcns {
        let current = if lcn == vol.checkpoint.lcn { " (current)" } else { "" };
        writeln!(out, "{lcn:#x} {per_page} checkpoint{current}")?;
    }
    for (i, r) in vol.checkpoint.roots.iter().enumerate() {
        tree_pages(vol, r, matches!(i, 7 | 8 | 12), &format!("root {i}"), 0, out)?;
    }
    for oid in vol.object_ids().collect::<Vec<_>>() {
        let r = vol.object(oid)?.clone();
        tree_pages(vol, &r, false, &format!("object {oid:#x}"), 0, out)?;
    }
    let mut dirs = vec![(refs::volume::ROOT_DIRECTORY, String::from("/"))];
    while let Some((oid, prefix)) = dirs.pop() {
        for e in vol.read_dir(oid)? {
            let path = format!("{prefix}{}", e.name);
            if let Target::Directory(child) = e.target
                && e.attributes & 0x400 == 0
            {
                dirs.push((child, format!("{path}/")));
            }
            let Ok(file) = vol.open_file(&e) else {
                continue;
            };
            let named = file
                .streams
                .iter()
                .chain(&file.snapshots)
                .map(|(n, s)| (format!(":{n}"), s));
            for (name, s) in
                std::iter::once((String::new(), file.data.as_ref())).chain(named.map(|(n, s)| (n, Some(s))))
            {
                let Some(refs::Stream {
                    content: Content::Extents(extents),
                    ..
                }) = s
                else {
                    continue;
                };
                for x in extents.iter().filter(|x| x.written) {
                    let lcn = vol.translate(x.vlcn)?;
                    writeln!(out, "{lcn:#x} {} data {path}{name} vcn {}", x.clusters, x.vcn)?;
                }
            }
        }
    }
    Ok(())
}

/// The pages of the tree below `r`, each cluster on its own line.
fn tree_pages<D: ReadAt>(
    vol: &Volume<D>,
    r: &refs::page::PageRef,
    physical: bool,
    what: &str,
    level: usize,
    out: &mut impl Write,
) -> Result<()> {
    if level > 16 {
        bail!("{what}: deeper than 16 levels");
    }
    for &lcn in r.lcns.iter().take((vol.page_size / vol.cluster) as usize) {
        let at = if physical { lcn } else { vol.translate(lcn)? };
        writeln!(out, "{at:#x} 1 page {what} depth {level}")?;
    }
    let page = vol.read_page(r, physical)?;
    let node = refs::node::Node::at(&page, refs::page::PAGE_HEADER_SIZE)?;
    if !node.is_leaf() {
        for row in node.rows() {
            let child = refs::page::PageRef::parse(row?.value)?;
            tree_pages(vol, &child, physical, what, level + 1, out)?;
        }
    }
    Ok(())
}

/// A volume on a device opened for writing when `write` (read-only
/// otherwise, for a dry run).
fn open_writable(device: &std::path::Path, offset: Option<u64>, write: bool) -> Result<Volume<File>> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(write)
        .open(device)
        .with_context(|| format!("cannot open {}", device.display()))?;
    let offset = match offset {
        Some(o) => o,
        None => *find_volumes(&file)?.first().context("no ReFS volume on the device")?,
    };
    Ok(Volume::open(file, offset)?)
}

/// The time now as FILETIME.
fn now() -> u64 {
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    (since.as_nanos() / 100) as u64 + 116_444_736_000_000_000
}

/// "YYYY-MM-DD hh:mm:ss" (UTC; a "T" between, a trailing "Z" allowed) or
/// a FILETIME number, as FILETIME.
fn parse_time(s: &str) -> std::result::Result<u64, String> {
    if let Ok(n) = s.parse::<u64>() {
        return Ok(n);
    }
    let s = s.trim_end_matches('Z').replace('T', " ");
    let bad = || format!("{s:?}: not YYYY-MM-DD hh:mm:ss");
    let (date, clock) = s.split_once(' ').ok_or_else(bad)?;
    let d: Vec<i64> = date
        .split('-')
        .map(|x| x.parse().map_err(|_| bad()))
        .collect::<std::result::Result<_, _>>()?;
    let t: Vec<i64> = clock
        .split(':')
        .map(|x| x.parse().map_err(|_| bad()))
        .collect::<std::result::Result<_, _>>()?;
    let ([y, m, d], [hh, mm, ss]) = (
        d[..].try_into().map_err(|_| bad())?,
        t[..].try_into().map_err(|_| bad())?,
    );
    // Days from civil (Howard Hinnant).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + hh * 3600 + mm * 60 + ss + 11_644_473_600;
    u64::try_from(secs).map(|s| s * 10_000_000).map_err(|_| bad())
}

fn parse_number(s: &str) -> std::result::Result<u64, String> {
    match s.strip_prefix("0x") {
        Some(h) => u64::from_str_radix(h, 16),
        None => s.parse(),
    }
    .map_err(|e| e.to_string())
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(" ")
}

fn attributes(a: u32) -> String {
    [
        (0x10, 'd'),
        (0x400, 'l'),
        (0x1, 'r'),
        (0x2, 'h'),
        (0x4, 's'),
        (0x20, 'a'),
        (0x200, 'S'),
    ]
    .iter()
    .map(|&(bit, c)| if a & bit != 0 { c } else { '-' })
    .collect()
}

fn ls(vol: &Volume<Device>, oid: u64, prefix: &str, long: bool, recursive: bool, out: &mut impl Write) -> Result<()> {
    for e in vol.read_dir(oid)? {
        let path = format!("{prefix}{}", e.name);
        if long {
            writeln!(
                out,
                "{} {:>14} {} {path}",
                attributes(e.attributes),
                e.size,
                time(e.times.modified)
            )?;
        } else {
            writeln!(out, "{path}{}", if e.is_dir() { "/" } else { "" })?;
        }
        if recursive
            && let Target::Directory(child) = e.target
            && e.attributes & 0x400 == 0
        {
            ls(vol, child, &format!("{path}/"), long, recursive, out)?;
        }
    }
    Ok(())
}

fn directory_of(vol: &Volume<Device>, path: &str) -> Result<u64> {
    if path.trim_matches('/').is_empty() {
        return Ok(refs::volume::ROOT_DIRECTORY);
    }
    match vol.lookup(path)?.target {
        Target::Directory(oid) => Ok(oid),
        _ => bail!("{path} is no directory"),
    }
}

fn stat(vol: &Volume<Device>, e: &Entry, out: &mut impl Write) -> Result<()> {
    writeln!(out, "name        {}", e.name)?;
    writeln!(out, "attributes  {:#x} ({})", e.attributes, attributes(e.attributes))?;
    writeln!(out, "size        {} (allocated {})", e.size, e.allocated)?;
    for (what, t) in [
        ("created", e.times.created),
        ("modified", e.times.modified),
        ("changed", e.times.changed),
        ("accessed", e.times.accessed),
    ] {
        writeln!(out, "{what:<11} {} UTC", time(t))?;
    }
    match &e.target {
        Target::Directory(oid) => writeln!(out, "directory   object {oid:#x}")?,
        Target::Embedded(_) => writeln!(out, "record      in the directory entry")?,
        Target::Split { home, ordinal } => writeln!(out, "record      {ordinal} of directory {home:#x}")?,
    }
    let file = vol.open_file(e)?;
    if let Some(data) = &file.data {
        match &data.content {
            Content::Inline(_) => writeln!(out, "data        {} bytes inline", data.size)?,
            Content::Extents(extents) => {
                writeln!(out, "data        {} bytes in {} extents", data.size, extents.len())?;
                for x in extents {
                    writeln!(
                        out,
                        "  vcn {:>10} +{:<8} {} {:#x}{}",
                        x.vcn,
                        x.clusters,
                        if x.written { "at" } else { "(zeros)" },
                        x.vlcn,
                        if x.checksums.is_some() { " (checksums)" } else { "" }
                    )?;
                }
            }
        }
    }
    for (name, s) in &file.streams {
        writeln!(out, "stream      {name}: {} bytes", s.size)?;
    }
    for (name, s) in &file.snapshots {
        writeln!(out, "snapshot    {name}: {} bytes", s.size)?;
    }
    if let Some(r) = &file.reparse {
        match r.link_target() {
            Some(t) => writeln!(
                out,
                "reparse     {:#010x} -> {} ({}{})",
                r.tag,
                t.substitute,
                t.print,
                if t.relative { ", relative" } else { "" }
            )?,
            None => writeln!(out, "reparse     {:#010x}, {} bytes", r.tag, r.data.len())?,
        }
    }
    Ok(())
}

/// Reads every directory, record and small stream of the volume through a
/// recording device, then writes the non-zero 4 KiB pages read as
/// `disk.fixture` and the manifest without what the fixture cannot show
/// (the hashes of larger files, the access times).
fn fixture(dir: &std::path::Path, out: &std::path::Path, data_limit: u64, exclude: &[String]) -> Result<()> {
    use storage_spaces::io::{Recording, SparseImage};
    let text = std::fs::read_to_string(dir.join("manifest.json"))?;
    let mut manifest: serde_json::Value = serde_json::from_str(text.trim_start_matches('\u{feff}'))?;
    let offset = manifest["partition_offset"]
        .as_u64()
        .context("manifest without partition_offset")?;
    let dev = Recording::new(File::open(dir.join("disk.img"))?);
    let excluded;
    {
        let vol = Volume::open(&dev, offset)?;
        // Every page of every table (writing needs the allocators and the
        // other tables), except the trees of excluded directories.
        let mut excluded_objects: Vec<u64> = Vec::new();
        // The superblock's copies too (opening needs only the first).
        let clusters = vol.boot.volume_size() / vol.cluster;
        let mut copy = vec![0u8; vol.cluster as usize];
        for lcn in [clusters - 2, clusters - 3] {
            dev.read_exact_at(&mut copy, offset + lcn * vol.cluster)?;
        }
        let mut dirs = vec![(refs::volume::ROOT_DIRECTORY, String::new())];
        let mut buf = vec![0u8; data_limit as usize];
        while let Some((oid, prefix)) = dirs.pop() {
            for e in vol.read_dir(oid)? {
                let path = format!("{prefix}{}", e.name);
                if let Target::Directory(child) = e.target
                    && e.attributes & 0x400 == 0
                {
                    if exclude.contains(&path) {
                        excluded_objects.push(child);
                    } else {
                        dirs.push((child, format!("{path}/")));
                    }
                }
                let file = vol.open_file(&e)?;
                for s in file
                    .data
                    .iter()
                    .chain(file.streams.iter().chain(&file.snapshots).map(|(_, s)| s))
                {
                    if s.size <= data_limit {
                        vol.read_stream(s, 0, &mut buf[..s.size as usize])?;
                    }
                }
            }
        }
        for (i, r) in vol.checkpoint.roots.iter().enumerate() {
            tree_pages(&vol, r, matches!(i, 7 | 8 | 12), "", 0, &mut std::io::sink())?;
        }
        for oid in vol.object_ids().collect::<Vec<_>>() {
            if !excluded_objects.contains(&oid) && oid != 7 && oid != 8 {
                tree_pages(&vol, &vol.object(oid)?.clone(), false, "", 0, &mut std::io::sink())?;
            }
        }
        // The log's control and record pages (writing checks the log),
        // and the pages only the older checkpoint references (writing
        // frees them).
        vol.log_state()?;
        vol.deferred_pages()?;
        excluded = excluded_objects;
    }
    let mut image = SparseImage::new(dev.size()?);
    for (at, len) in dev.reads() {
        let mut buf = vec![0u8; len];
        dev.inner().read_exact_at(&mut buf, at)?;
        for (k, page) in buf.chunks(4096).enumerate() {
            if page.iter().any(|&b| b != 0) {
                image.insert(at + (k * 4096) as u64, page);
            }
        }
    }
    let small = |v: &serde_json::Value| v["size"].as_u64().is_some_and(|s| s <= data_limit);
    let entries = manifest["entries"].as_array_mut().context("manifest without entries")?;
    entries.retain(|e| {
        let path = e["path"].as_str().unwrap_or_default();
        !exclude
            .iter()
            .any(|x| path.strip_prefix(x.as_str()).is_some_and(|rest| rest.starts_with('/')))
    });
    for e in manifest["entries"].as_array_mut().into_iter().flatten() {
        let keep = small(e);
        let e = e.as_object_mut().unwrap();
        e.retain(|k, _| !matches!(k.as_str(), "accessed" | "sparse_ranges" | "hard_link_of"));
        if !keep {
            e.remove("sha256");
        }
        for s in e
            .get_mut("streams")
            .and_then(|s| s.as_array_mut())
            .into_iter()
            .flatten()
        {
            if !small(s) {
                s.as_object_mut().unwrap().remove("sha256");
            }
        }
    }
    manifest.as_object_mut().unwrap().remove("refsinfo");
    manifest["fixture_data_limit"] = data_limit.into();
    manifest["fixture_excluded"] = exclude.into();
    manifest["fixture_excluded_objects"] = excluded.into();
    std::fs::create_dir_all(out)?;
    image.write_to(std::io::BufWriter::new(File::create(out.join("disk.fixture"))?))?;
    std::fs::write(out.join("manifest.json"), serde_json::to_string(&manifest)? + "\n")?;
    println!("{}: {} bytes of pages", out.display(), image.stored());
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut out = std::io::stdout().lock();
    match cli.command {
        Command::Info { source } => {
            let vol = open_volume(&source)?;
            let b = &vol.boot;
            let c = &vol.checkpoint;
            writeln!(out, "ReFS {}.{} (checkpoint {}.{})", b.major, b.minor, c.major, c.minor)?;
            writeln!(
                out,
                "size        {} bytes, {}-byte clusters",
                b.volume_size(),
                vol.cluster
            )?;
            writeln!(out, "serial      {:#018x}", b.serial)?;
            writeln!(
                out,
                "checksums   {}",
                match c.reference_size {
                    0x30 => "CRC64",
                    0x48 => "SHA-256",
                    _ => "none (pre-3.10 layout)",
                }
            )?;
            writeln!(
                out,
                "checkpoint  at cluster {:#x}, clock {}, flags {:#x}",
                c.lcn, c.clock, c.flags
            )?;
            writeln!(out, "containers  {} clusters each", vol.clusters_per_container)?;
            writeln!(out, "objects     {}", vol.object_ids().count())?;
            match vol.log_state() {
                Ok(log) if log.needs_replay() => writeln!(
                    out,
                    "log         records past the checkpoint (up to {:#x}:{:#x}): Windows replays them when it \
                     attaches the volume; refs shows the checkpoint and does not write",
                    log.newest.unwrap().high,
                    log.newest.unwrap().low
                )?,
                Ok(log) => writeln!(
                    out,
                    "log         clean at {:#x}:{:#x}",
                    log.checkpoint.high, log.checkpoint.low
                )?,
                Err(e) => writeln!(out, "log         unreadable: {e}")?,
            }
        }
        Command::Ls {
            source,
            path,
            long,
            recursive,
        } => {
            let vol = open_volume(&source)?;
            let oid = directory_of(&vol, &path)?;
            ls(&vol, oid, "", long, recursive, &mut out)?;
        }
        Command::Cat {
            source,
            path,
            stream,
            snapshot,
        } => {
            let vol = open_volume(&source)?;
            let e = vol.lookup(&path)?;
            let file = vol.open_file(&e)?;
            let named = |list: &[(String, refs::Stream)], name: &str, what: &str| {
                list.iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, s)| s.clone())
                    .with_context(|| format!("no {what} {name:?}"))
            };
            let s = match (&stream, &snapshot) {
                (Some(name), _) => named(&file.streams, name, "stream")?,
                (_, Some(name)) => named(&file.snapshots, name, "snapshot")?,
                _ => file.data.clone().context("the file has no data stream")?,
            };
            let s = &s;
            let mut buf = vec![0u8; 1 << 20];
            let mut at = 0;
            loop {
                let n = vol.read_stream(s, at, &mut buf)?;
                if n == 0 {
                    break;
                }
                out.write_all(&buf[..n])?;
                at += n as u64;
            }
        }
        Command::Stat { source, path } => {
            let vol = open_volume(&source)?;
            let e = vol.lookup(&path)?;
            stat(&vol, &e, &mut out)?;
        }
        #[cfg(all(target_os = "linux", feature = "fuse"))]
        Command::Mount {
            source,
            mountpoint,
            allow_other,
            rw,
        } => {
            let vol: Volume<fuse::Rw> = if rw {
                if source.space.is_some() || source.devices.len() != 1 {
                    bail!("--rw mounts one image, disk or partition (not --space)");
                }
                let file = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&source.devices[0])
                    .with_context(|| format!("cannot open {}", source.devices[0].display()))?;
                let offset = match source.offset {
                    Some(o) => o,
                    None => *find_volumes(&file)?.first().context("no ReFS volume on the device")?,
                };
                let vol = Volume::open(Box::new(file) as fuse::Rw, offset)?;
                if vol.log_state()?.needs_replay() {
                    bail!(
                        "the volume's log has changes its checkpoint lacks: attach it to Windows once and \
                         detach it before mounting it for writing"
                    );
                }
                vol
            } else {
                let dev = open_device(&source)?;
                let offset = match source.offset {
                    Some(o) => o,
                    None => *find_volumes(dev.as_ref())?
                        .first()
                        .context("no ReFS volume on the device (or its partitions)")?,
                };
                Volume::open(Box::new(ReadOnly(dev)) as fuse::Rw, offset)?
            };
            let name = match &source.space {
                Some(space) => format!("space:{space}"),
                None => std::fs::canonicalize(&source.devices[0])
                    .unwrap_or_else(|_| source.devices[0].clone())
                    .display()
                    .to_string(),
            };
            // Root mounting a block device: a fuseblk mount of it.
            let blkdev = source.space.is_none()
                && source.devices.len() == 1
                && std::fs::metadata(&source.devices[0])
                    .is_ok_and(|m| std::os::unix::fs::FileTypeExt::is_block_device(&m.file_type()))
                && effective_uid() == Some(0);
            fuse::serve(vol, &name, &mountpoint, allow_other, rw, blkdev)?;
        }
        Command::Set {
            device,
            offset,
            path,
            created,
            modified,
            changed,
            accessed,
            attributes,
            integrity,
            yes,
        } => {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(yes)
                .open(&device)
                .with_context(|| format!("cannot open {}", device.display()))?;
            let offset = match offset {
                Some(o) => o,
                None => *find_volumes(&file)?.first().context("no ReFS volume on the device")?,
            };
            let mut vol = Volume::open(file, offset)?;
            let e = vol.lookup(&path)?;
            let mut times = e.times;
            for (field, value) in [
                (&mut times.created, created),
                (&mut times.modified, modified),
                (&mut times.changed, changed),
                (&mut times.accessed, accessed),
            ] {
                if let Some(v) = value {
                    *field = v;
                }
            }
            writeln!(
                out,
                "{path}: times {} -> {}",
                time(e.times.modified),
                time(times.modified)
            )?;
            if let Some(a) = attributes {
                writeln!(out, "{path}: attributes {:#x} -> {a:#x} (settable bits)", e.attributes)?;
            }
            if let Some(i) = &integrity {
                writeln!(out, "{path}: integrity streams {i}")?;
            }
            if !yes {
                writeln!(out, "nothing written (--yes writes)")?;
                return Ok(());
            }
            if times != e.times {
                vol.set_times(&path, &times)?;
            }
            if let Some(i) = &integrity {
                vol.set_integrity(&path, i == "on")?;
            }
            if let Some(a) = attributes {
                vol.set_attributes(&path, a as u32)?;
            }
            writeln!(out, "written: checkpoint clock {}", vol.checkpoint.clock)?;
        }
        Command::Overwrite {
            device,
            offset,
            path,
            at,
            from,
            yes,
        } => {
            let bytes = std::fs::read(&from).with_context(|| format!("cannot read {}", from.display()))?;
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(yes)
                .open(&device)
                .with_context(|| format!("cannot open {}", device.display()))?;
            let offset = match offset {
                Some(o) => o,
                None => *find_volumes(&file)?.first().context("no ReFS volume on the device")?,
            };
            let mut vol = Volume::open(file, offset)?;
            writeln!(out, "{path}: {} bytes at {at}", bytes.len())?;
            if !yes {
                writeln!(out, "nothing written (--yes writes)")?;
                return Ok(());
            }
            vol.overwrite(&path, at, &bytes, now())?;
            writeln!(out, "written: checkpoint clock {}", vol.checkpoint.clock)?;
        }
        Command::Create {
            device,
            offset,
            path,
            from,
            yes,
        } => {
            let source = match &from {
                Some(f) => FileSource::open(f)?,
                None => FileSource::empty()?,
            };
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(yes)
                .open(&device)
                .with_context(|| format!("cannot open {}", device.display()))?;
            let offset = match offset {
                Some(o) => o,
                None => *find_volumes(&file)?.first().context("no ReFS volume on the device")?,
            };
            let mut vol = Volume::open(file, offset)?;
            writeln!(out, "{path}: {} bytes", refs::write::Source::len(&source))?;
            if !yes {
                writeln!(out, "nothing written (--yes writes)")?;
                return Ok(());
            }
            vol.create_file_from(&path, &source, now())?;
            writeln!(out, "written: checkpoint clock {}", vol.checkpoint.clock)?;
        }
        Command::Delete {
            device,
            offset,
            path,
            stream,
            yes,
        } => {
            let mut vol = open_writable(&device, offset, yes)?;
            match &stream {
                Some(s) => writeln!(out, "{path}: delete stream {s}")?,
                None => writeln!(out, "{path}: delete")?,
            }
            if !yes {
                writeln!(out, "nothing written (--yes writes)")?;
                return Ok(());
            }
            match &stream {
                Some(s) => vol.delete_stream(&path, s, now())?,
                None => vol.delete_file(&path, now())?,
            }
            writeln!(out, "written: checkpoint clock {}", vol.checkpoint.clock)?;
        }
        Command::Rename {
            device,
            offset,
            path,
            to,
            yes,
        } => {
            let mut vol = open_writable(&device, offset, yes)?;
            writeln!(out, "{path}: rename to {to}")?;
            if !yes {
                writeln!(out, "nothing written (--yes writes)")?;
                return Ok(());
            }
            vol.rename(&path, &to, now())?;
            writeln!(out, "written: checkpoint clock {}", vol.checkpoint.clock)?;
        }
        Command::Mkdir {
            device,
            offset,
            path,
            yes,
        } => {
            let mut vol = open_writable(&device, offset, yes)?;
            writeln!(out, "{path}: create directory")?;
            if !yes {
                writeln!(out, "nothing written (--yes writes)")?;
                return Ok(());
            }
            vol.create_directory(&path, now())?;
            writeln!(out, "written: checkpoint clock {}", vol.checkpoint.clock)?;
        }
        Command::Write {
            device,
            offset,
            path,
            from,
            append,
            stream,
            yes,
        } => {
            if stream.is_none() {
                let mut vol = open_writable(&device, offset, yes)?;
                // Appending changes the file in place from its old end.
                let mut old_size = None;
                let source = if append {
                    // The old content, then the new, in a temporary file.
                    let e = vol.lookup(&path)?;
                    let file = vol.open_file(&e)?;
                    let tmp = FileSource::temporary()?;
                    let mut at = 0u64;
                    if let Some(s) = &file.data {
                        let mut buf = vec![0u8; 1 << 20];
                        while at < s.size {
                            let n = vol.read_stream(s, at, &mut buf)?;
                            if n == 0 {
                                break;
                            }
                            std::os::unix::fs::FileExt::write_all_at(&tmp.0, &buf[..n], at)?;
                            at += n as u64;
                        }
                        at = s.size;
                    }
                    old_size = Some(at);
                    let mut new =
                        std::fs::File::open(&from).with_context(|| format!("cannot read {}", from.display()))?;
                    let mut buf = vec![0u8; 1 << 20];
                    loop {
                        let n = std::io::Read::read(&mut new, &mut buf)?;
                        if n == 0 {
                            break;
                        }
                        std::os::unix::fs::FileExt::write_all_at(&tmp.0, &buf[..n], at)?;
                        at += n as u64;
                    }
                    FileSource(tmp.0, at)
                } else {
                    FileSource::open(&from)?
                };
                writeln!(out, "{path}: {} bytes", refs::write::Source::len(&source))?;
                if !yes {
                    writeln!(out, "nothing written (--yes writes)")?;
                    return Ok(());
                }
                match old_size {
                    Some(old) => {
                        let len = refs::write::Source::len(&source);
                        vol.update_file(&path, &source, &[(old, len)], now())?
                    }
                    None => vol.write_file_from(&path, &source, now())?,
                }
                writeln!(out, "written: checkpoint clock {}", vol.checkpoint.clock)?;
                return Ok(());
            }
            let bytes = std::fs::read(&from).with_context(|| format!("cannot read {}", from.display()))?;
            let mut vol = open_writable(&device, offset, yes)?;
            let data = if append {
                let e = vol.lookup(&path)?;
                let file = vol.open_file(&e)?;
                let mut old = Vec::new();
                let current = match &stream {
                    Some(name) => file
                        .streams
                        .iter()
                        .find(|(n, _)| n.eq_ignore_ascii_case(name))
                        .map(|(_, s)| s),
                    None => file.data.as_ref(),
                };
                if let Some(s) = current {
                    old.resize(s.size as usize, 0);
                    let mut at = 0;
                    while at < old.len() {
                        let n = vol.read_stream(s, at as u64, &mut old[at..])?;
                        if n == 0 {
                            break;
                        }
                        at += n;
                    }
                }
                old.extend(&bytes);
                old
            } else {
                bytes
            };
            writeln!(out, "{path}: {} bytes", data.len())?;
            if !yes {
                writeln!(out, "nothing written (--yes writes)")?;
                return Ok(());
            }
            match &stream {
                Some(s) => vol.write_stream(&path, s, &data, now())?,
                None => vol.write_file(&path, &data, now())?,
            }
            writeln!(out, "written: checkpoint clock {}", vol.checkpoint.clock)?;
        }
        Command::Move {
            device,
            offset,
            path,
            to,
            yes,
        } => {
            let mut vol = open_writable(&device, offset, yes)?;
            writeln!(out, "{path}: move to {to}")?;
            if !yes {
                writeln!(out, "nothing written (--yes writes)")?;
                return Ok(());
            }
            vol.move_file(&path, &to, now())?;
            writeln!(out, "written: checkpoint clock {}", vol.checkpoint.clock)?;
        }
        Command::Link {
            device,
            offset,
            path,
            to,
            yes,
        } => {
            let mut vol = open_writable(&device, offset, yes)?;
            writeln!(out, "{path}: link as {to}")?;
            if !yes {
                writeln!(out, "nothing written (--yes writes)")?;
                return Ok(());
            }
            vol.link_file(&path, &to, now())?;
            writeln!(out, "written: checkpoint clock {}", vol.checkpoint.clock)?;
        }
        Command::Clone {
            device,
            offset,
            path,
            to,
            yes,
        } => {
            let mut vol = open_writable(&device, offset, yes)?;
            writeln!(out, "{path}: clone as {to}")?;
            if !yes {
                writeln!(out, "nothing written (--yes writes)")?;
                return Ok(());
            }
            vol.clone_file(&path, &to, now())?;
            writeln!(out, "written: checkpoint clock {}", vol.checkpoint.clock)?;
        }
        Command::Tree { source, root, object } => {
            let vol = open_volume(&source)?;
            let (r, physical) = match (root, object) {
                (Some(i), _) => (
                    vol.checkpoint.roots.get(i).context("no such root")?.clone(),
                    matches!(i, 7 | 8 | 12),
                ),
                (_, Some(oid)) => (vol.object(oid)?.clone(), false),
                _ => bail!("--root or --object"),
            };
            vol.walk(&r, physical, &mut |row| {
                writeln!(out, "key {}", hex(row.key)).map_err(refs::Error::Io)?;
                writeln!(out, "  = {}", hex(row.value)).map_err(refs::Error::Io)?;
                Ok(())
            })?;
        }
        Command::Check { source } => {
            let vol = open_volume(&source)?;
            let report = vol.check(&[])?;
            writeln!(
                out,
                "{} pages, {} directories, {} files, {} data clusters",
                report.pages, report.directories, report.files, report.data_clusters
            )?;
            if report.deferred_clusters > 0 {
                writeln!(
                    out,
                    "{} clusters of pages only the older checkpoint references, still allocated \
                     (Windows frees them with its next checkpoint, refs with its next write)",
                    report.deferred_clusters
                )?;
            }
            for p in &report.problems {
                writeln!(out, "problem: {p}")?;
            }
            if report.problem_count > report.problems.len() as u64 {
                writeln!(out, "... {} problems in all", report.problem_count)?;
            }
            if report.problem_count > 0 {
                out.flush()?;
                std::process::exit(1);
            }
            writeln!(out, "no problems found")?;
        }
        Command::Map { source } => {
            let vol = open_volume(&source)?;
            map(&vol, &mut out)?;
        }
        Command::Rows { source, path, bytes } => {
            let vol = open_volume(&source)?;
            let e = vol.lookup(&path)?;
            for (k, v) in vol.attribute_rows(&e)? {
                writeln!(out, "key {}", hex(&k))?;
                for (i, line) in v[..v.len().min(bytes)].chunks(32).enumerate() {
                    writeln!(out, "  {:04x} {}", i * 32, hex(line))?;
                }
                if v.len() > bytes {
                    writeln!(out, "  ... {} bytes", v.len())?;
                }
            }
        }
        Command::Fixture {
            volume_dir,
            output,
            data_limit,
            exclude,
        } => fixture(&volume_dir, &output, data_limit, &exclude)?,
    }
    Ok(())
}
