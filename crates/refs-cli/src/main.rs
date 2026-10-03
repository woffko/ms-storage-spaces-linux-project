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
fn tree_pages(
    vol: &Volume<Device>,
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
    {
        let vol = Volume::open(&dev, offset)?;
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
                    && !exclude.contains(&path)
                {
                    dirs.push((child, format!("{path}/")));
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
        } => {
            let vol = open_volume(&source)?;
            fuse::serve(vol, &mountpoint, allow_other)?;
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
