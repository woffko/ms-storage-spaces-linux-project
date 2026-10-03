//! Directories and files.
//!
//! A directory is an object (an object table entry) whose tree holds a
//! row per name (type 0x30: the name in the key). A subdirectory's row is
//! an index entry naming the subdirectory's object id. A file's row either
//! embeds the file's record (key flags 1) or, once the file was moved or
//! got a second name, points at the record (type 0x40) in the directory it
//! was created in (key flags 2: that directory's object id and the file's
//! ordinal there).
//!
//! A file record is a small B+-tree of attributes: the default data stream
//! ($DATA, inline or as an extent map), named streams, the reparse point.
//!
//! A stream in extents is a chain of levels: each level maps the clusters
//! written while it was the live one and names its parent level; a stream
//! snapshot freezes the live level and starts a new one on top of it. The
//! stream reads as its levels laid over each other, the newest on top.

use storage_spaces::io::ReadAt;

use crate::checksum::{crc32c, crc64};
use crate::error::{Error, Result, format_err};
use crate::node::{Node, Row};
use crate::util::{le16, le32, le64, utf16};
use crate::volume::{ROOT_DIRECTORY, Volume};

/// The directory bit of ReFS's attribute word (Windows shows 0x10).
const REFS_DIRECTORY: u32 = 0x1000_0000;
pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
pub const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

const ROW_OWN: u16 = 0x10;
const ROW_NAME: u16 = 0x30;
const ROW_RECORD: u16 = 0x40;

const SINGLE: u32 = 0x8000_0001;
const MULTI: u32 = 0x8000_0002;
/// Attribute types (the low half of the descriptor).
const DATA: u32 = 0x80;
const NAMED: u32 = 0xb0;
const REPARSE: u32 = 0xc0;
/// Rows of a stream set (the data of named streams in extents).
const STREAM_SET: u32 = 3;
/// The level id of a live stream; levels kept by snapshots count up from
/// it, smaller ids name the header of the level set.
pub(crate) const LIVE_STREAM: u64 = 0x1000;
/// Named stream rows: an alternate data stream, a snapshot.
const NAMED_STREAM: u16 = 0;
const NAMED_SNAPSHOT: u16 = 2;
/// Levels a stream may stack (deeper chains are refused).
const MAX_LEVELS: usize = 1024;

/// Times as FILETIME (100 ns since 1601).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Times {
    pub created: u64,
    pub modified: u64,
    pub changed: u64,
    pub accessed: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A subdirectory: its object id.
    Directory(u64),
    /// A file whose record is in the name row.
    Embedded(Vec<u8>),
    /// A file whose record is a type 0x40 row of directory `home`.
    Split { home: u64, ordinal: u64 },
}

/// A name in a directory.
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    /// Windows' attribute bits (0x10 for directories).
    pub attributes: u32,
    pub times: Times,
    pub size: u64,
    pub allocated: u64,
    pub target: Target,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        matches!(self.target, Target::Directory(_))
    }

    fn parse(key: &[u8], value: &[u8]) -> Result<Self> {
        let flags = le16(key, 2);
        let name = utf16(&key[4..]);
        let windows = |a: u32| {
            if a & REFS_DIRECTORY != 0 {
                (a & !REFS_DIRECTORY) | FILE_ATTRIBUTE_DIRECTORY
            } else {
                a
            }
        };
        match flags {
            1 => {
                if value.len() < 0x68 {
                    return Err(format_err!("record of {name:?} of {} bytes", value.len()));
                }
                Ok(Entry {
                    name,
                    attributes: windows(le32(value, 0x48)),
                    times: Times {
                        created: le64(value, 0x28),
                        modified: le64(value, 0x30),
                        changed: le64(value, 0x38),
                        accessed: le64(value, 0x40),
                    },
                    size: le64(value, 0x58),
                    allocated: le64(value, 0x60),
                    target: Target::Embedded(value.to_vec()),
                })
            }
            2 => {
                if value.len() < 0x44 {
                    return Err(format_err!("index entry of {name:?} of {} bytes", value.len()));
                }
                let raw = le32(value, 0x40);
                let (ordinal, home) = (le64(value, 0), le64(value, 8));
                Ok(Entry {
                    name,
                    attributes: windows(raw),
                    times: Times {
                        created: le64(value, 0x10),
                        modified: le64(value, 0x18),
                        changed: le64(value, 0x20),
                        accessed: le64(value, 0x28),
                    },
                    size: le64(value, 0x38),
                    allocated: le64(value, 0x30),
                    target: if raw & REFS_DIRECTORY != 0 {
                        Target::Directory(home)
                    } else {
                        Target::Split { home, ordinal }
                    },
                })
            }
            f => Err(Error::Unsupported(format!(
                "name row of {name:?} with key flags {f:#x}"
            ))),
        }
    }
}

/// One run of a stream's extent map, in clusters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extent {
    pub vcn: u64,
    pub vlcn: u64,
    pub clusters: u64,
    /// Holds data (written); otherwise it reads as zeros (a sparse hole,
    /// or allocated and not written yet).
    pub written: bool,
    /// Integrity stream checksums of the run's clusters.
    pub checksums: Option<DataChecksums>,
}

/// The checksums of an integrity stream's run: `per_cluster` values for
/// each cluster, each over an equal part of it (one per 4 KiB cluster
/// with CRC32-C, one per 16 KiB of 64 KiB clusters with CRC-64).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataChecksums {
    /// 1 CRC32-C, 2 CRC-64/NVME (the codes of page references).
    pub kind: u16,
    pub per_cluster: usize,
    pub values: std::sync::Arc<[u64]>,
}

/// Where a stream's bytes are.
#[derive(Debug, Clone)]
pub enum Content {
    Inline(Vec<u8>),
    Extents(Vec<Extent>),
}

#[derive(Debug, Clone)]
pub struct Stream {
    pub size: u64,
    pub content: Content,
}

/// A file's record, decoded.
#[derive(Debug, Clone, Default)]
pub struct File {
    pub data: Option<Stream>,
    /// Named (alternate) streams.
    pub streams: Vec<(String, Stream)>,
    /// Stream snapshots (`refsutil streamsnapshot`) of the default stream:
    /// the data at the time each was taken.
    pub snapshots: Vec<(String, Stream)>,
    /// The reparse point (symbolic links, junctions).
    pub reparse: Option<Reparse>,
}

pub const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xa000_0003;
pub const IO_REPARSE_TAG_SYMLINK: u32 = 0xa000_000c;

/// A reparse point: its tag and the tag's data (REPARSE_DATA_BUFFER
/// without the tag and length).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reparse {
    pub tag: u32,
    pub data: Vec<u8>,
}

/// Where a symbolic link or junction points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkTarget {
    /// The substitute name ("\\??\\E:\\deep" for absolute targets).
    pub substitute: String,
    /// The name Windows shows (may be empty on junctions).
    pub print: String,
    /// A symbolic link relative to its directory.
    pub relative: bool,
}

impl Reparse {
    /// The target of a symbolic link or a junction.
    pub fn link_target(&self) -> Option<LinkTarget> {
        let d = &self.data;
        let (paths, relative) = match self.tag {
            IO_REPARSE_TAG_SYMLINK => (12, le32(d, 8) & 1 != 0),
            IO_REPARSE_TAG_MOUNT_POINT => (8, false),
            _ => return None,
        };
        let name = |off: usize, len: usize| d.get(paths + off..paths + off + len).map(utf16);
        Some(LinkTarget {
            substitute: name(le16(d, 0) as usize, le16(d, 2) as usize)?,
            print: name(le16(d, 4) as usize, le16(d, 6) as usize)?,
            relative,
        })
    }
}

impl<D: ReadAt> Volume<D> {
    /// The names of a directory, in key order.
    pub fn read_dir(&self, oid: u64) -> Result<Vec<Entry>> {
        let mut out = Vec::new();
        for (key, value) in self.object_rows(oid)? {
            if le16(&key, 0) == ROW_NAME {
                out.push(Entry::parse(&key, &value)?);
            }
        }
        Ok(out)
    }

    /// The entry of `path` ("/"-separated, from the root; names compared
    /// case-insensitively as Windows does for ASCII).
    pub fn lookup(&self, path: &str) -> Result<Entry> {
        let mut dir = ROOT_DIRECTORY;
        let mut found: Option<Entry> = None;
        for part in path.split('/').filter(|p| !p.is_empty()) {
            if let Some(e) = &found {
                dir = match e.target {
                    Target::Directory(oid) => oid,
                    _ => return Err(Error::NotFound(format!("{path}: {} is no directory", e.name))),
                };
            }
            let entries = self.read_dir(dir)?;
            found = Some(
                entries
                    .iter()
                    .find(|e| e.name == part)
                    .or_else(|| entries.iter().find(|e| e.name.eq_ignore_ascii_case(part)))
                    .cloned()
                    .ok_or_else(|| Error::NotFound(path.to_owned()))?,
            );
        }
        found.ok_or_else(|| Error::NotFound(format!("{path}: the root has no entry")))
    }

    /// The record of a file entry.
    pub fn record(&self, entry: &Entry) -> Result<Vec<u8>> {
        match &entry.target {
            Target::Embedded(v) => Ok(v.clone()),
            Target::Split { home, ordinal } => {
                for (key, value) in self.object_rows(*home)? {
                    if le16(&key, 0) == ROW_RECORD && le64(&key, 8) == *ordinal && le64(&key, 0x10) == *home {
                        return Ok(value);
                    }
                }
                Err(format_err!(
                    "{}: no record {ordinal} in directory {home:#x}",
                    entry.name
                ))
            }
            Target::Directory(_) => Err(Error::Unsupported(format!("{} is a directory", entry.name))),
        }
    }

    /// Decodes an entry's attributes: a file's record, or a directory's
    /// own row (type 0x10, where a junction keeps its reparse point).
    pub fn open_file(&self, entry: &Entry) -> Result<File> {
        let rows = self.attribute_rows(entry)?;
        let levels = Levels::of(&rows);
        let mut file = File::default();
        for (k, v) in &rows {
            self.attribute(&mut file, k, v, &levels)?;
        }
        Ok(file)
    }

    /// The raw attribute rows (key, value) of an entry's record, or of a
    /// directory's own row.
    pub fn attribute_rows(&self, entry: &Entry) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let record = match entry.target {
            Target::Directory(oid) => self.own_row(oid)?,
            _ => self.record(entry)?,
        };
        let node = Node::at(&record, 0)?;
        let mut rows = Vec::new();
        self.walk_node(&node, false, 0, &mut |row: Row<'_>| {
            rows.push((row.key.to_vec(), row.value.to_vec()));
            Ok(())
        })?;
        Ok(rows)
    }

    /// A directory's own times (its own row keeps them where a file's
    /// record does: created, modified, changed, accessed from 0x28); the
    /// root directory, which has no entry, has them only there.
    pub fn directory_times(&self, oid: u64) -> Result<Times> {
        let own = self.own_row(oid)?;
        if own.len() < 0x48 {
            return Err(format_err!("directory {oid:#x}: own row of {} bytes", own.len()));
        }
        Ok(Times {
            created: le64(&own, 0x28),
            modified: le64(&own, 0x30),
            changed: le64(&own, 0x38),
            accessed: le64(&own, 0x40),
        })
    }

    /// A directory's own row. Rows sort by their type, and the own row's
    /// is the lowest, so the walk stops at the first row of another type
    /// (the whole tree of a large directory is megabytes); a tree in
    /// another order is searched in full.
    pub(crate) fn own_row(&self, oid: u64) -> Result<Vec<u8>> {
        let mut own = None;
        self.walk_while(&self.object(oid)?.clone(), false, &mut |row| {
            let kind = le16(row.key, 0);
            if kind == ROW_OWN {
                own = Some(row.value.to_vec());
            }
            Ok(kind < ROW_OWN)
        })?;
        match own {
            Some(v) => Ok(v),
            None => self
                .object_rows(oid)?
                .into_iter()
                .find(|(k, _)| le16(k, 0) == ROW_OWN)
                .map(|(_, v)| v)
                .ok_or_else(|| format_err!("directory {oid:#x} without its own row")),
        }
    }

    fn attribute(&self, file: &mut File, k: &[u8], v: &[u8], levels: &Levels<'_>) -> Result<()> {
        let (marker, descriptor) = (le32(k, 8), le32(k, 12));
        match (marker, descriptor & 0xffff) {
            (SINGLE, DATA) => {
                let size = le64(v, 0x20);
                let content = v
                    .get(0x3c..0x3c + size as usize)
                    .ok_or_else(|| format_err!("inline data of {size} bytes in a value of {}", v.len()))?;
                file.data = Some(Stream {
                    size,
                    content: Content::Inline(content.to_vec()),
                });
            }
            (MULTI, DATA) if le64(k, 0x10) == LIVE_STREAM => {
                file.data = Some(Stream {
                    size: le64(v, 0x38),
                    content: Content::Extents(self.level_extents(levels, OWN_SET, LIVE_STREAM)?),
                });
            }
            (MULTI, NAMED) if matches!(le16(v, 0x10), NAMED_STREAM | NAMED_SNAPSHOT) => {
                let name = utf16(&k[0x10..]);
                let size = le64(v, 0x20);
                let content = if le16(v, 2) & 0x1000 != 0 {
                    // In extents: the level the value names, in the
                    // file's own $DATA levels (set 0: snapshots of the
                    // default stream) or in a stream set.
                    let (set, id) = (le64(v, 0x3c), le64(v, 0x44));
                    Content::Extents(
                        self.level_extents(levels, set, id)
                            .map_err(|e| format_err!("stream {name:?}: {e}"))?,
                    )
                } else {
                    Content::Inline(
                        v.get(0x3c..0x3c + size as usize)
                            .ok_or_else(|| format_err!("stream {name:?} of {size} bytes in a value of {}", v.len()))?
                            .to_vec(),
                    )
                };
                let stream = (name, Stream { size, content });
                if le16(v, 0x10) == NAMED_SNAPSHOT {
                    file.snapshots.push(stream);
                } else {
                    file.streams.push(stream);
                }
            }
            (SINGLE, REPARSE) => {
                let len = le16(v, 0x10) as usize;
                file.reparse = Some(Reparse {
                    tag: le32(v, 0x0c),
                    data: v
                        .get(0x14..0x14 + len)
                        .ok_or_else(|| format_err!("reparse data of {len} bytes in a value of {}", v.len()))?
                        .to_vec(),
                });
            }
            _ => {}
        }
        Ok(())
    }

    /// The extents of level `id` of a level set: its chain of levels
    /// down to the set's header, the older ones overlaid by the newer.
    fn level_extents(&self, levels: &Levels<'_>, set: u64, id: u64) -> Result<Vec<Extent>> {
        let mut chain = Vec::new();
        let mut at = id;
        while at >= LIVE_STREAM {
            let &(parent, value) = levels
                .0
                .get(&(set, at))
                .ok_or_else(|| format_err!("no data level {set:#x}/{at:#x}"))?;
            if chain.len() == MAX_LEVELS {
                return Err(format_err!("more than {MAX_LEVELS} data levels"));
            }
            chain.push(value);
            at = parent;
        }
        let mut out = Vec::new();
        for value in chain.iter().rev() {
            out = overlay(out, self.extents(value)?);
        }
        Ok(out)
    }

    fn data_checksums(&self, kind: u16, clusters: u64, bytes: &[u8]) -> Result<DataChecksums> {
        let values: std::sync::Arc<[u64]> = match kind {
            1 => bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|c| u32::from_le_bytes(*c) as u64)
                .collect(),
            2 => bytes
                .as_chunks::<8>()
                .0
                .iter()
                .map(|c| u64::from_le_bytes(*c))
                .collect(),
            _ => return Err(Error::Unsupported(format!("integrity checksums of kind {kind}"))),
        };
        let per_cluster = values.len().checked_div(clusters as usize).unwrap_or(0);
        if per_cluster == 0
            || values.len() as u64 != per_cluster as u64 * clusters
            || !self.cluster.is_multiple_of(per_cluster as u64)
        {
            return Err(format_err!(
                "{} checksums for a run of {clusters} clusters",
                values.len()
            ));
        }
        Ok(DataChecksums {
            kind,
            per_cluster,
            values,
        })
    }

    /// The extent map of a $DATA value: a node at the value's start whose
    /// leaf rows are 24-byte extent records (more with per-cluster
    /// checksums); index rows point at pages of the same.
    pub(crate) fn extents(&self, value: &[u8]) -> Result<Vec<Extent>> {
        let mut out = Vec::new();
        // The checksum of integrity streams' data.
        let kind = le16(value, 0x16);
        self.extent_node(&Node::at(value, 0)?, 0, kind, &mut out)?;
        out.sort_by_key(|e| e.vcn);
        Ok(out)
    }

    /// The physical clusters of the pages an extent map keeps outside its
    /// value (large maps: the value's node is an index over pages).
    pub(crate) fn extent_map_pages(&self, value: &[u8]) -> Result<Vec<u64>> {
        let mut out = Vec::new();
        self.map_pages(&Node::at(value, 0)?, 0, &mut out)?;
        Ok(out)
    }

    fn map_pages(&self, node: &Node<'_>, depth: usize, out: &mut Vec<u64>) -> Result<()> {
        if depth > 16 {
            return Err(format_err!("extent map deeper than 16 levels"));
        }
        if node.is_leaf() {
            return Ok(());
        }
        let per_page = (self.page_size / self.cluster) as usize;
        for row in node.rows() {
            let child = crate::page::PageRef::parse(row?.value)?;
            for &l in &child.lcns[..per_page] {
                out.push(self.translate(l)?);
            }
            let page = self.read_page(&child, false)?;
            self.map_pages(&Node::at(&page, crate::page::PAGE_HEADER_SIZE)?, depth + 1, out)?;
        }
        Ok(())
    }

    fn extent_node(&self, node: &Node<'_>, depth: usize, kind: u16, out: &mut Vec<Extent>) -> Result<()> {
        if depth > 16 {
            return Err(format_err!("extent map deeper than 16 levels"));
        }
        if node.is_leaf() {
            for record in node.records(|r| (le16(r, 0x0a) as usize).max(24)) {
                let r = record?;
                let flags = le16(r, 8);
                let clusters = le32(r, 0x14) as u64;
                // Integrity streams: the checksums follow the record.
                let checksums = if flags & 0x80 != 0 {
                    Some(self.data_checksums(kind, clusters, &r[24..])?)
                } else {
                    None
                };
                out.push(Extent {
                    vcn: le32(r, 0x0c) as u64,
                    vlcn: le64(r, 0),
                    clusters,
                    written: flags & 0x10 != 0 && flags & 0x20 == 0,
                    checksums,
                });
            }
        } else {
            for row in node.rows() {
                let child = crate::page::PageRef::parse(row?.value)?;
                let page = self.read_page(&child, false)?;
                self.extent_node(&Node::at(&page, crate::page::PAGE_HEADER_SIZE)?, depth + 1, kind, out)?;
            }
        }
        Ok(())
    }

    /// Reads a stream from `offset` into `buf`; returns the bytes read
    /// (fewer at the end of the stream).
    pub fn read_stream(&self, stream: &Stream, offset: u64, buf: &mut [u8]) -> Result<usize> {
        if offset >= stream.size {
            return Ok(0);
        }
        let n = buf.len().min((stream.size - offset) as usize);
        let buf = &mut buf[..n];
        match &stream.content {
            Content::Inline(bytes) => {
                let end = (offset as usize + n).min(bytes.len());
                let have = end.saturating_sub(offset as usize);
                buf[..have].copy_from_slice(&bytes[offset as usize..end]);
                buf[have..].fill(0);
            }
            Content::Extents(extents) => {
                buf.fill(0);
                let cluster = self.cluster;
                // Extents are sorted and do not overlap.
                let first = extents.partition_point(|e| (e.vcn + e.clusters) * cluster <= offset);
                for e in extents[first..]
                    .iter()
                    .take_while(|e| e.vcn * cluster < offset + n as u64)
                {
                    let (start, end) = (e.vcn * cluster, (e.vcn + e.clusters) * cluster);
                    let (from, to) = (offset.max(start), (offset + n as u64).min(end));
                    if !e.written || from >= to {
                        continue;
                    }
                    let skip = from - start;
                    let vlcn = e
                        .vlcn
                        .checked_add(skip / cluster)
                        .ok_or_else(|| format_err!("extent at cluster {:#x} beyond any device", e.vlcn))?;
                    let out = &mut buf[(from - offset) as usize..(to - offset) as usize];
                    match &e.checksums {
                        None => self.read_virtual(vlcn, skip % cluster, out)?,
                        Some(sums) => {
                            // The checked parts the read touches, each
                            // checked before use.
                            let part = cluster / sums.per_cluster as u64;
                            let first = skip / part;
                            let count = (skip % part + out.len() as u64).div_ceil(part);
                            let mut parts = vec![0u8; (count * part) as usize];
                            let at = first * part;
                            self.read_virtual(e.vlcn + at / cluster, at % cluster, &mut parts)?;
                            for (i, c) in parts.chunks(part as usize).enumerate() {
                                let sum = match sums.kind {
                                    1 => crc32c(c) as u64,
                                    _ => crc64(c),
                                };
                                if sum != sums.values[first as usize + i] {
                                    let at = (first + i as u64) * part;
                                    return Err(Error::Checksum(format!(
                                        "bytes {:#x}..{:#x} of the stream (virtual cluster {:#x})",
                                        start + at,
                                        start + at + part,
                                        e.vlcn + at / cluster
                                    )));
                                }
                            }
                            let skip = (skip % part) as usize;
                            out.copy_from_slice(&parts[skip..skip + out.len()]);
                        }
                    }
                }
            }
        }
        Ok(n)
    }
}

/// The default stream's levels are set 0 (the $DATA rows of the record).
const OWN_SET: u64 = 0;

/// The data levels of a record by (set, id): the parent level's id and
/// the level's value. $DATA rows (set 0) have the id at key 0x10 and the
/// parent at 0x18; stream set rows the set at 0x30, the id at 0x38 and the
/// parent at 0x40.
struct Levels<'a>(std::collections::HashMap<(u64, u64), (u64, &'a [u8])>);

impl<'a> Levels<'a> {
    fn of(rows: &'a [(Vec<u8>, Vec<u8>)]) -> Self {
        let mut levels = std::collections::HashMap::new();
        for (k, v) in rows {
            let (marker, descriptor) = (le32(k, 8), le32(k, 12));
            if (marker, descriptor & 0xffff) == (MULTI, DATA) {
                levels.insert((OWN_SET, le64(k, 0x10)), (le64(k, 0x18), v.as_slice()));
            } else if marker == STREAM_SET {
                levels.insert((le64(k, 0x30), le64(k, 0x38)), (le64(k, 0x40), v.as_slice()));
            }
        }
        Levels(levels)
    }
}

/// `base` with `top` laid over it: where a run of `top` lies, its clusters
/// (or its zeros) replace those of `base`. Both are sorted by vcn, without
/// overlaps; so is the result.
fn overlay(base: Vec<Extent>, top: Vec<Extent>) -> Vec<Extent> {
    if base.is_empty() {
        return top;
    }
    let mut out = Vec::with_capacity(base.len() + top.len());
    for b in base {
        let end = b.vcn + b.clusters;
        let mut at = b.vcn;
        let first = top.partition_point(|t| t.vcn + t.clusters <= b.vcn);
        for t in top[first..].iter().take_while(|t| t.vcn < end) {
            if t.vcn > at {
                out.push(part(&b, at, t.vcn));
            }
            at = at.max(t.vcn + t.clusters);
        }
        if at < end {
            out.push(part(&b, at, end));
        }
    }
    out.extend(top);
    out.sort_by_key(|e| e.vcn);
    out
}

/// The clusters `from..to` of extent `e`.
fn part(e: &Extent, from: u64, to: u64) -> Extent {
    let (a, b) = ((from - e.vcn) as usize, (to - e.vcn) as usize);
    Extent {
        vcn: from,
        vlcn: e.vlcn.wrapping_add(from - e.vcn),
        clusters: to - from,
        written: e.written,
        checksums: e.checksums.as_ref().map(|c| DataChecksums {
            values: c.values[a * c.per_cluster..b * c.per_cluster].into(),
            ..c.clone()
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn x(vcn: u64, vlcn: u64, clusters: u64) -> Extent {
        Extent {
            vcn,
            vlcn,
            clusters,
            written: true,
            checksums: None,
        }
    }

    #[test]
    fn newer_levels_replace_the_clusters_they_cover() {
        // A snapshot's whole file, then runs written after it.
        let base = vec![x(0, 100, 256)];
        let top = vec![x(32, 500, 16), x(256, 600, 16)];
        assert_eq!(
            overlay(base, top),
            [x(0, 100, 32), x(32, 500, 16), x(48, 148, 208), x(256, 600, 16)]
        );
        // A run covering several base runs and their gaps, and one at the
        // very start.
        let base = vec![x(0, 10, 4), x(6, 20, 4), x(12, 30, 4)];
        let top = vec![x(0, 90, 1), x(3, 70, 11)];
        assert_eq!(
            overlay(base, top),
            [x(0, 90, 1), x(1, 11, 2), x(3, 70, 11), x(14, 32, 2)]
        );
        // Zeros written over data hide it.
        let hole = Extent {
            written: false,
            ..x(2, 0, 2)
        };
        assert_eq!(
            overlay(vec![x(0, 10, 8)], vec![hole.clone()]),
            [x(0, 10, 2), hole, x(4, 14, 4)]
        );
        // Checksums follow their clusters.
        let sums = |s: &[u64]| Extent {
            checksums: Some(DataChecksums {
                kind: 2,
                per_cluster: 2,
                values: s.into(),
            }),
            ..x(0, 10, s.len() as u64 / 2)
        };
        let cut = overlay(vec![sums(&[1, 2, 3, 4, 5, 6, 7, 8])], vec![x(1, 50, 2)]);
        assert_eq!(cut[0].checksums.as_ref().map(|c| &c.values[..]), Some(&[1, 2][..]));
        assert_eq!(cut[2].checksums.as_ref().map(|c| &c.values[..]), Some(&[7, 8][..]));
        assert_eq!(overlay(Vec::new(), vec![x(5, 1, 1)]), [x(5, 1, 1)]);
    }
}
