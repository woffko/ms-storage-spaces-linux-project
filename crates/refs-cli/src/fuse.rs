//! A ReFS volume mounted read-only through FUSE.
//!
//! Inode numbers are handed out as names are looked up: a directory keeps
//! one per object id, a file whose record lives in its home directory
//! (moved, or with several names) one per (home directory, ordinal), so all
//! its hard links share it, and an embedded file one per (directory, name).
//! Symbolic links and junctions are links: a relative target keeps its
//! path, an absolute one ("\\??\\E:\\dir") becomes the path inside the mount.
//! Named streams are extended attributes "user.<name>".

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use fuser::{
    Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, INodeNo, LockOwner, MountOption, OpenFlags,
    ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry, ReplyOpen, ReplyXattr, Request,
};
use refs::{Entry, File as RefsFile, Target, Volume};

use crate::Device;

const TTL: Duration = Duration::from_secs(3600);

#[derive(Clone, PartialEq, Eq, Hash)]
enum Key {
    Directory(u64),
    Record { home: u64, ordinal: u64 },
    Embedded { dir: u64, name: String },
}

struct Node {
    entry: Option<Entry>,
    key: Key,
    file: Option<Arc<RefsFile>>,
}

struct Inodes {
    by_key: HashMap<Key, u64>,
    nodes: HashMap<u64, Node>,
    next: u64,
}

/// A directory's entries, read once: a lookup or a listing in a large
/// directory would otherwise read the whole directory again.
struct Dir {
    entries: Vec<Entry>,
    by_name: HashMap<String, usize>,
}

/// Directories kept read (the cache is emptied when full).
const DIRECTORIES_KEPT: usize = 256;

struct RefsFs {
    vol: Volume<Device>,
    mountpoint: PathBuf,
    inodes: Mutex<Inodes>,
    dirs: Mutex<HashMap<u64, Arc<Dir>>>,
}

/// FILETIME to SystemTime.
fn system_time(t: u64) -> SystemTime {
    let unix_100ns = t.saturating_sub(116_444_736_000_000_000);
    UNIX_EPOCH + Duration::from_nanos(unix_100ns.saturating_mul(100))
}

fn key_of(dir: u64, e: &Entry) -> Key {
    match &e.target {
        Target::Directory(oid) => Key::Directory(*oid),
        Target::Split { home, ordinal } => Key::Record {
            home: *home,
            ordinal: *ordinal,
        },
        Target::Embedded(_) => Key::Embedded {
            dir,
            name: e.name.clone(),
        },
    }
}

impl RefsFs {
    /// The inode of an entry of directory `dir`, created on first sight.
    fn inode(&self, dir: u64, e: &Entry) -> u64 {
        let key = key_of(dir, e);
        let mut inodes = self.inodes.lock().unwrap();
        if let Some(&ino) = inodes.by_key.get(&key) {
            return ino;
        }
        let ino = inodes.next;
        inodes.next += 1;
        inodes.by_key.insert(key.clone(), ino);
        inodes.nodes.insert(
            ino,
            Node {
                entry: Some(e.clone()),
                key,
                file: None,
            },
        );
        ino
    }

    fn entry(&self, ino: u64) -> Option<(Option<Entry>, Key)> {
        let inodes = self.inodes.lock().unwrap();
        inodes.nodes.get(&ino).map(|n| (n.entry.clone(), n.key.clone()))
    }

    /// The decoded record of a file inode (cached).
    fn file(&self, ino: u64) -> Result<Arc<RefsFile>, Errno> {
        if let Some(f) = self.inodes.lock().unwrap().nodes.get(&ino).and_then(|n| n.file.clone()) {
            return Ok(f);
        }
        let (entry, _) = self.entry(ino).ok_or(Errno::ENOENT)?;
        let entry = entry.ok_or(Errno::EISDIR)?;
        let file = Arc::new(self.vol.open_file(&entry).map_err(|e| {
            eprintln!("{}: {e}", entry.name);
            Errno::EIO
        })?);
        if let Some(n) = self.inodes.lock().unwrap().nodes.get_mut(&ino) {
            n.file = Some(file.clone());
        }
        Ok(file)
    }

    fn is_link(e: &Entry) -> bool {
        e.attributes & refs::file::FILE_ATTRIBUTE_REPARSE_POINT != 0
    }

    fn attr(&self, ino: u64) -> Option<FileAttr> {
        let (entry, _) = self.entry(ino)?;
        let (kind, size, perm, times) = match &entry {
            None => (FileType::Directory, 0, 0o555, refs::Times::default()),
            Some(e) if Self::is_link(e) => (
                FileType::Symlink,
                self.link(ino).map_or(0, |l| l.len() as u64),
                0o777,
                e.times,
            ),
            Some(e) if e.is_dir() => (FileType::Directory, 0, 0o555, e.times),
            Some(e) => (FileType::RegularFile, e.size, 0o444, e.times),
        };
        Some(FileAttr {
            ino: INodeNo(ino),
            size,
            blocks: size.div_ceil(512),
            atime: system_time(times.accessed),
            mtime: system_time(times.modified),
            ctime: system_time(times.changed),
            crtime: system_time(times.created),
            kind,
            perm,
            nlink: if kind == FileType::Directory { 2 } else { 1 },
            uid: 0,
            gid: 0,
            rdev: 0,
            flags: 0,
            blksize: self.vol.cluster as u32,
        })
    }

    /// Where a link inode points, as a Linux path.
    fn link(&self, ino: u64) -> Option<String> {
        let target = self.file(ino).ok()?.reparse.as_ref()?.link_target()?;
        let path = target.substitute.replace('\\', "/");
        if target.relative {
            return Some(path);
        }
        // "\??\E:\dir": the path inside this volume.
        let inside = path.trim_start_matches("/??/");
        let inside = match inside.as_bytes() {
            [_, b':', ..] => &inside[2..],
            _ => inside,
        };
        Some(format!(
            "{}/{}",
            self.mountpoint.display(),
            inside.trim_start_matches('/')
        ))
    }

    /// The entries of directory `dir` (cached).
    fn listing(&self, dir: u64) -> Result<Arc<Dir>, Errno> {
        if let Some(d) = self.dirs.lock().unwrap().get(&dir) {
            return Ok(d.clone());
        }
        let entries = self.vol.read_dir(dir).map_err(|e| {
            eprintln!("directory {dir:#x}: {e}");
            Errno::EIO
        })?;
        let by_name = entries.iter().enumerate().map(|(i, e)| (e.name.clone(), i)).collect();
        let d = Arc::new(Dir { entries, by_name });
        let mut dirs = self.dirs.lock().unwrap();
        if dirs.len() >= DIRECTORIES_KEPT {
            dirs.clear();
        }
        dirs.insert(dir, d.clone());
        Ok(d)
    }

    fn directory(&self, ino: u64) -> Result<u64, Errno> {
        match self.entry(ino).ok_or(Errno::ENOENT)? {
            (_, Key::Directory(oid)) => Ok(oid),
            _ => Err(Errno::ENOTDIR),
        }
    }
}

impl Filesystem for RefsFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Ok(dir) = self.directory(parent.0) else {
            return reply.error(Errno::ENOTDIR);
        };
        let listing = match self.listing(dir) {
            Ok(l) => l,
            Err(e) => return reply.error(e),
        };
        match listing.by_name.get(name.to_string_lossy().as_ref()) {
            Some(&i) => {
                let e = &listing.entries[i];
                let ino = self.inode(dir, e);
                match self.attr(ino) {
                    Some(attr) => reply.entry(&TTL, &attr, fuser::Generation(0)),
                    None => reply.error(Errno::ENOENT),
                }
            }
            None => reply.error(Errno::ENOENT),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.attr(ino.0) {
            Some(attr) => reply.attr(&TTL, &attr),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        match self.link(ino.0) {
            Some(target) => reply.data(target.as_bytes()),
            None => reply.error(Errno::EINVAL),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        if flags.0 & 3 != 0 {
            return reply.error(Errno::EROFS);
        }
        match self.file(ino.0) {
            Ok(_) => reply.opened(FileHandle(0), FopenFlags::FOPEN_KEEP_CACHE),
            Err(e) => reply.error(e),
        }
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let file = match self.file(ino.0) {
            Ok(f) => f,
            Err(e) => return reply.error(e),
        };
        let Some(stream) = &file.data else {
            return reply.data(&[]);
        };
        let mut buf = vec![0u8; size as usize];
        match self.vol.read_stream(stream, offset, &mut buf) {
            Ok(n) => reply.data(&buf[..n]),
            Err(e) => {
                eprintln!("read of {size} bytes at {offset:#x} failed: {e}");
                reply.error(Errno::EIO)
            }
        }
    }

    fn readdir(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
        let dir = match self.directory(ino.0) {
            Ok(d) => d,
            Err(e) => return reply.error(e),
        };
        let listing = match self.listing(dir) {
            Ok(l) => l,
            Err(e) => return reply.error(e),
        };
        let mut all = vec![
            (ino.0, FileType::Directory, ".".to_owned()),
            (ino.0, FileType::Directory, "..".to_owned()),
        ];
        for e in &listing.entries {
            let kind = if Self::is_link(e) {
                FileType::Symlink
            } else if e.is_dir() {
                FileType::Directory
            } else {
                FileType::RegularFile
            };
            all.push((self.inode(dir, e), kind, e.name.clone()));
        }
        for (i, (child, kind, name)) in all.into_iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(child), (i + 1) as u64, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn listxattr(&self, _req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        let names: Vec<u8> = match self.file(ino.0) {
            Ok(f) => f
                .streams
                .iter()
                .flat_map(|(n, _)| format!("user.{n}\0").into_bytes())
                .collect(),
            Err(_) => Vec::new(),
        };
        if size == 0 {
            reply.size(names.len() as u32)
        } else if names.len() > size as usize {
            reply.error(Errno::ERANGE)
        } else {
            reply.data(&names)
        }
    }

    fn getxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        let name = name.to_string_lossy();
        let Some(stream_name) = name.strip_prefix("user.") else {
            return reply.error(Errno::NO_XATTR);
        };
        let Ok(file) = self.file(ino.0) else {
            return reply.error(Errno::NO_XATTR);
        };
        let Some((_, stream)) = file.streams.iter().find(|(n, _)| n == stream_name) else {
            return reply.error(Errno::NO_XATTR);
        };
        // Linux takes extended attributes up to 64 KiB; larger streams are
        // read with `refs cat --stream`.
        if stream.size > 65536 {
            return reply.error(Errno::E2BIG);
        }
        if size == 0 {
            return reply.size(stream.size as u32);
        }
        if stream.size > size as u64 {
            return reply.error(Errno::ERANGE);
        }
        let mut buf = vec![0u8; stream.size as usize];
        match self.vol.read_stream(stream, 0, &mut buf) {
            Ok(n) => reply.data(&buf[..n]),
            Err(_) => reply.error(Errno::EIO),
        }
    }
}

/// Mounts the volume at `mountpoint` and serves it until unmounted.
pub fn serve(vol: Volume<Device>, mountpoint: &Path, allow_other: bool) -> Result<()> {
    let mut config = Config::default();
    config.mount_options.extend([
        MountOption::RO,
        MountOption::FSName("refs".into()),
        MountOption::Subtype("refs".into()),
        MountOption::DefaultPermissions,
    ]);
    if allow_other {
        config.acl = fuser::SessionACL::All;
    }
    config.n_threads = Some(4);
    let root = Node {
        entry: None,
        key: Key::Directory(refs::volume::ROOT_DIRECTORY),
        file: None,
    };
    let mut inodes = Inodes {
        by_key: HashMap::new(),
        nodes: HashMap::new(),
        next: 2,
    };
    inodes
        .by_key
        .insert(Key::Directory(refs::volume::ROOT_DIRECTORY), INodeNo::ROOT.0);
    inodes.nodes.insert(INodeNo::ROOT.0, root);
    let mountpoint = std::fs::canonicalize(mountpoint).unwrap_or_else(|_| mountpoint.to_path_buf());
    let fs = RefsFs {
        vol,
        mountpoint: mountpoint.clone(),
        inodes: Mutex::new(inodes),
        dirs: Mutex::new(HashMap::new()),
    };
    let session = fuser::spawn_mount(fs, &mountpoint, &config)
        .with_context(|| format!("cannot mount on {}", mountpoint.display()))?;
    session.join().context("FUSE session failed")?;
    Ok(())
}
