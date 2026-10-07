//! A ReFS volume mounted through FUSE: read-only, or with `--rw` for
//! writing through `refs::write`.
//!
//! Inodes stand for paths (the write API takes paths), except that the
//! names of a file whose record is kept in its home directory (moved, or
//! hard-linked) share one inode. Symbolic links and junctions are
//! links: a relative target keeps its path, an absolute one
//! ("\\??\\E:\\dir") becomes the path inside the mount. Named streams are
//! extended attributes "user.<name>".
//!
//! Writing: every change is one `refs::write` transaction (copy on write,
//! then a new checkpoint). A file opened for writing is read into memory
//! and written back whole when it is flushed (closed or synced), up to the
//! size `refs write` takes; the listings and records read before are
//! dropped after every change.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use fuser::{
    BsdFileFlags, Config, CopyFileRangeFlags, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, INodeNo,
    LockOwner, MountOption, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, ReplyXattr, Request, TimeOrNow, WriteFlags,
};
use refs::{Entry, File as RefsFile, Target, Times, Volume};
use storage_spaces::io::WriteAt;

/// The device of a mounted volume (written only when mounted with --rw).
pub type Rw = Box<dyn WriteAt>;

const TTL: Duration = Duration::from_secs(1);

/// The largest file the mount writes (a file is rewritten whole; the
/// volume may take less: its extent map must fit one page).
const MAX_WRITTEN: u64 = 64 << 30;

/// Linux's open(2) and setxattr(2) flags.
const O_TRUNC: i32 = 0o1000;
const XATTR_CREATE: i32 = 1;
const XATTR_REPLACE: i32 = 2;

/// A directory's entries, read once: a lookup or a listing in a large
/// directory would otherwise read the whole directory again.
struct Dir {
    entries: Vec<Entry>,
    by_name: HashMap<String, usize>,
}

/// Directories kept read (the cache is emptied when full), and the entries
/// they may hold together (a hostile directory can have millions).
const DIRECTORIES_KEPT: usize = 256;
const DIR_ENTRIES_KEPT: usize = 200_000;
/// Files whose decoded record is kept (each holds its whole extent list).
const FILES_KEPT: usize = 1024;
/// Inodes kept that the kernel has not looked up (a listing makes them; the
/// kernel looks them up one by one, or never, and tells us when it forgets
/// the ones it did).
const INODES_KEPT: usize = 1 << 16;

/// The guard of a lock whose holder panicked: one failing handler must not
/// make every later one fail with it (the data is consistent: handlers
/// only change it in single steps).
trait Unpoison<T> {
    type Guard<'a>
    where
        Self: 'a;
    fn lock_ok(&self) -> Self::Guard<'_>;
}
impl<T> Unpoison<T> for Mutex<T> {
    type Guard<'a>
        = std::sync::MutexGuard<'a, T>
    where
        Self: 'a;
    fn lock_ok(&self) -> Self::Guard<'_> {
        self.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
trait UnpoisonRw<T> {
    fn read_ok(&self) -> std::sync::RwLockReadGuard<'_, T>;
    fn write_ok(&self) -> std::sync::RwLockWriteGuard<'_, T>;
}
impl<T> UnpoisonRw<T> for RwLock<T> {
    fn read_ok(&self) -> std::sync::RwLockReadGuard<'_, T> {
        self.read().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn write_ok(&self) -> std::sync::RwLockWriteGuard<'_, T> {
        self.write().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// A file opened for writing: its whole content in a temporary file
/// (unlinked; in $REFS_TMPDIR, else the system's), written back on flush.
struct Pending {
    file: std::fs::File,
    len: u64,
    dirty: bool,
    handles: usize,
    /// The byte ranges written (refs::write changes them in place), or
    /// `whole`: the file is written anew.
    ranges: Vec<(u64, u64)>,
    whole: bool,
}

impl Pending {
    fn new() -> Result<Self, Errno> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::var_os("REFS_TMPDIR").map_or_else(std::env::temp_dir, PathBuf::from);
        let path = dir.join(format!(
            "refs-mount-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| {
                eprintln!("{}: {e}", path.display());
                Errno::EIO
            })?;
        let _ = std::fs::remove_file(&path);
        Ok(Pending {
            file,
            len: 0,
            dirty: false,
            handles: 1,
            ranges: Vec::new(),
            whole: false,
        })
    }

    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<(), Errno> {
        std::os::unix::fs::FileExt::write_all_at(&self.file, data, offset).map_err(|_| Errno::EIO)?;
        self.len = self.len.max(offset + data.len() as u64);
        let (a, b) = (offset, offset + data.len() as u64);
        match self.ranges.last_mut() {
            Some(last) if a <= last.1 && b >= last.0 => *last = (last.0.min(a), last.1.max(b)),
            _ => self.ranges.push((a, b)),
        }
        Ok(())
    }

    fn set_len(&mut self, len: u64) -> Result<(), Errno> {
        self.file.set_len(len).map_err(|_| Errno::EIO)?;
        if len > self.len {
            self.ranges.push((self.len, len));
        }
        self.len = len;
        Ok(())
    }

    fn read_at(&self, offset: u64, size: u32) -> Result<Vec<u8>, Errno> {
        let from = offset.min(self.len);
        let to = (from + u64::from(size)).min(self.len);
        let mut buf = vec![0u8; (to - from) as usize];
        std::os::unix::fs::FileExt::read_exact_at(&self.file, &mut buf, from).map_err(|_| Errno::EIO)?;
        Ok(buf)
    }
}

/// A pending file as the data `refs::write` writes from.
struct PendingData<'a>(&'a std::fs::File, u64);

impl refs::write::Source for PendingData<'_> {
    fn len(&self) -> u64 {
        self.1
    }
    fn read_into(&self, offset: u64, buf: &mut [u8]) -> refs::Result<()> {
        Ok(std::os::unix::fs::FileExt::read_exact_at(self.0, buf, offset)?)
    }
}

struct State {
    /// The paths of each inode (the first is used).
    paths: HashMap<u64, Vec<String>>,
    inodes: HashMap<String, u64>,
    /// Inodes of files whose record is kept in their home directory, by
    /// (home, ordinal): all their names.
    records: HashMap<(u64, u64), u64>,
    next: u64,
    dirs: HashMap<u64, Arc<Dir>>,
    /// The entries the cached listings hold together.
    dir_entries: usize,
    files: HashMap<String, Arc<RefsFile>>,
    pending: HashMap<u64, Pending>,
    /// The lookups the kernel holds per inode (`forget` gives them back).
    lookups: HashMap<u64, u64>,
}

impl State {
    fn new() -> Self {
        let mut s = State {
            paths: HashMap::new(),
            inodes: HashMap::new(),
            records: HashMap::new(),
            next: 2,
            dirs: HashMap::new(),
            dir_entries: 0,
            files: HashMap::new(),
            pending: HashMap::new(),
            lookups: HashMap::new(),
        };
        s.paths.insert(INodeNo::ROOT.0, vec!["/".to_owned()]);
        s.inodes.insert("/".to_owned(), INodeNo::ROOT.0);
        s
    }

    /// The inode of a path, made on first sight; the names of one record
    /// (`record`: its home and ordinal) share it.
    fn inode(&mut self, path: &str, record: Option<(u64, u64)>) -> u64 {
        if let Some(&ino) = self.inodes.get(path) {
            if let Some(r) = record {
                self.records.entry(r).or_insert(ino);
            }
            return ino;
        }
        if self.inodes.len() > INODES_KEPT {
            self.trim();
        }
        let ino = match record.and_then(|r| self.records.get(&r).copied()) {
            Some(ino) => ino,
            None => {
                let ino = self.next;
                self.next += 1;
                if let Some(r) = record {
                    self.records.insert(r, ino);
                }
                ino
            }
        };
        self.inodes.insert(path.to_owned(), ino);
        self.paths.entry(ino).or_default().push(path.to_owned());
        ino
    }

    /// The kernel was told about the inode (a reply with an entry).
    fn looked_up(&mut self, ino: u64) {
        *self.lookups.entry(ino).or_default() += 1;
    }

    /// The kernel gives `n` lookups back; with none left it forgets the
    /// inode, and so do we (the root stays).
    fn forget(&mut self, ino: u64, n: u64) {
        let Some(held) = self.lookups.get_mut(&ino) else {
            return;
        };
        *held = held.saturating_sub(n);
        if *held == 0 {
            self.lookups.remove(&ino);
            self.drop_inode(ino);
        }
    }

    fn drop_inode(&mut self, ino: u64) {
        if ino == INodeNo::ROOT.0 || self.pending.contains_key(&ino) {
            return;
        }
        for name in self.paths.remove(&ino).unwrap_or_default() {
            self.inodes.remove(&name);
        }
        self.records.retain(|_, i| *i != ino);
    }

    /// Drops the inodes the kernel never looked up (a later lookup makes
    /// them again, under a new number the kernel has not seen).
    fn trim(&mut self) {
        let idle: Vec<u64> = self
            .paths
            .keys()
            .copied()
            .filter(|i| !self.lookups.contains_key(i))
            .collect();
        for ino in idle {
            self.drop_inode(ino);
        }
    }

    /// Keeps a listing, within the budget of entries.
    fn cache_dir(&mut self, oid: u64, d: Arc<Dir>) {
        let n = d.entries.len();
        if n > DIR_ENTRIES_KEPT {
            return;
        }
        if self.dirs.len() >= DIRECTORIES_KEPT || self.dir_entries + n > DIR_ENTRIES_KEPT {
            self.dirs.clear();
            self.dir_entries = 0;
        }
        if let Some(old) = self.dirs.insert(oid, d) {
            self.dir_entries = self.dir_entries.saturating_sub(old.entries.len());
        }
        self.dir_entries += n;
    }

    fn cache_file(&mut self, path: String, f: Arc<RefsFile>) {
        if self.files.len() >= FILES_KEPT {
            self.files.clear();
        }
        self.files.insert(path, f);
    }
}

struct RefsFs {
    vol: RwLock<Volume<Rw>>,
    writable: bool,
    mountpoint: PathBuf,
    owner: (u32, u32),
    state: Mutex<State>,
}

/// FILETIME to SystemTime.
fn system_time(t: u64) -> SystemTime {
    let unix_100ns = t.saturating_sub(116_444_736_000_000_000);
    UNIX_EPOCH + Duration::from_nanos(unix_100ns.saturating_mul(100))
}

/// SystemTime to FILETIME.
fn filetime(t: SystemTime) -> u64 {
    let since = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    (since.as_nanos() / 100) as u64 + 116_444_736_000_000_000
}

fn now() -> u64 {
    filetime(SystemTime::now())
}

fn child(parent: &str, name: &str) -> String {
    if parent == "/" {
        format!("/{name}")
    } else {
        format!("{parent}/{name}")
    }
}

fn split(path: &str) -> (&str, &str) {
    match path.rsplit_once('/') {
        Some(("", name)) => ("/", name),
        Some((parent, name)) => (parent, name),
        None => ("/", path),
    }
}

/// A library error as an errno (the message goes to stderr).
fn errno(what: &str, e: refs::Error) -> Errno {
    eprintln!("{what}: {e}");
    match e {
        refs::Error::NotFound(_) => Errno::ENOENT,
        refs::Error::Unsupported(m) if m.contains("not empty") => Errno::ENOTEMPTY,
        refs::Error::Unsupported(m) if m.contains("exists") => Errno::EEXIST,
        refs::Error::Unsupported(_) => Errno::EOPNOTSUPP,
        _ => Errno::EIO,
    }
}

impl RefsFs {
    /// The inode of a path (`entry`: what it names, if known), made on
    /// first sight; the names of one record share it.
    fn inode(&self, path: &str, entry: Option<&Entry>) -> u64 {
        let record = match entry.map(|e| &e.target) {
            Some(Target::Split { home, ordinal }) => Some((*home, *ordinal)),
            _ => None,
        };
        self.state.lock_ok().inode(path, record)
    }

    fn path(&self, ino: u64) -> Result<String, Errno> {
        self.state
            .lock()
            .unwrap()
            .paths
            .get(&ino)
            .and_then(|p| p.first().cloned())
            .ok_or(Errno::ENOENT)
    }

    /// Forgets what was read before a change (listings and records), and
    /// the paths below `gone` when it went away.
    fn changed(&self, gone: Option<&str>) {
        let mut s = self.state.lock_ok();
        s.dirs.clear();
        s.dir_entries = 0;
        s.files.clear();
        if let Some(gone) = gone {
            let below = format!("{gone}/");
            let lost: Vec<(String, u64)> = s
                .inodes
                .iter()
                .filter(|(p, _)| *p == gone || p.starts_with(&below))
                .map(|(p, &i)| (p.clone(), i))
                .collect();
            for (p, i) in lost {
                s.inodes.remove(&p);
                let empty = s.paths.get_mut(&i).is_some_and(|names| {
                    names.retain(|n| *n != p);
                    names.is_empty()
                });
                if empty {
                    s.paths.remove(&i);
                    s.records.retain(|_, ino| *ino != i);
                }
            }
        }
    }

    /// Moves the paths of `from` (and below it) to `to`.
    fn moved(&self, from: &str, to: &str) {
        self.changed(Some(to));
        let mut s = self.state.lock_ok();
        let below = format!("{from}/");
        let moved: Vec<(String, u64)> = s
            .inodes
            .iter()
            .filter(|(p, _)| *p == from || p.starts_with(&below))
            .map(|(p, &i)| (p.clone(), i))
            .collect();
        for (p, i) in moved {
            let new = format!("{to}{}", &p[from.len()..]);
            s.inodes.remove(&p);
            s.inodes.insert(new.clone(), i);
            if let Some(names) = s.paths.get_mut(&i) {
                for n in names.iter_mut().filter(|n| **n == p) {
                    n.clone_from(&new);
                }
            }
        }
    }

    /// The entries of directory `oid` (cached).
    fn listing(&self, vol: &Volume<Rw>, oid: u64) -> Result<Arc<Dir>, Errno> {
        if let Some(d) = self.state.lock_ok().dirs.get(&oid) {
            return Ok(d.clone());
        }
        let entries = vol
            .read_dir(oid)
            .map_err(|e| errno(&format!("directory {oid:#x}"), e))?;
        let by_name = entries.iter().enumerate().map(|(i, e)| (e.name.clone(), i)).collect();
        let d = Arc::new(Dir { entries, by_name });
        self.state.lock_ok().cache_dir(oid, d.clone());
        Ok(d)
    }

    /// The entry of a path (None: the root).
    fn resolve(&self, vol: &Volume<Rw>, path: &str) -> Result<Option<Entry>, Errno> {
        let mut oid = refs::volume::ROOT_DIRECTORY;
        let mut found = None;
        for part in path.split('/').filter(|p| !p.is_empty()) {
            if found.is_some()
                && !matches!(
                    found,
                    Some(Entry {
                        target: Target::Directory(_),
                        ..
                    })
                )
            {
                return Err(Errno::ENOTDIR);
            }
            let listing = self.listing(vol, oid)?;
            let e = listing
                .by_name
                .get(part)
                .map(|&i| &listing.entries[i])
                .or_else(|| listing.entries.iter().find(|e| e.name.eq_ignore_ascii_case(part)))
                .ok_or(Errno::ENOENT)?
                .clone();
            if let Target::Directory(child) = e.target {
                oid = child;
            }
            found = Some(e);
        }
        Ok(found)
    }

    /// The directory object a path names.
    fn directory(&self, vol: &Volume<Rw>, path: &str) -> Result<u64, Errno> {
        match self.resolve(vol, path)? {
            None => Ok(refs::volume::ROOT_DIRECTORY),
            Some(Entry {
                target: Target::Directory(oid),
                ..
            }) => Ok(oid),
            Some(_) => Err(Errno::ENOTDIR),
        }
    }

    /// The decoded record of a file (cached).
    fn file(&self, vol: &Volume<Rw>, path: &str) -> Result<Arc<RefsFile>, Errno> {
        if let Some(f) = self.state.lock_ok().files.get(path) {
            return Ok(f.clone());
        }
        let entry = self.resolve(vol, path)?.ok_or(Errno::EISDIR)?;
        let file = Arc::new(vol.open_file(&entry).map_err(|e| errno(path, e))?);
        self.state.lock_ok().cache_file(path.to_owned(), file.clone());
        Ok(file)
    }

    fn is_link(e: &Entry) -> bool {
        e.attributes & refs::file::FILE_ATTRIBUTE_REPARSE_POINT != 0
    }

    fn attr(&self, vol: &Volume<Rw>, ino: u64) -> Result<FileAttr, Errno> {
        let path = self.path(ino)?;
        let mut entry = self.resolve(vol, &path)?;
        // A moved or linked file: its record has the current sizes and
        // times (the entry of a name is updated only when written through).
        let mut links = 1;
        if let Some(e) = entry.as_mut()
            && matches!(e.target, Target::Split { .. })
            && let Ok(r) = vol.record(e)
            && r.len() >= 0xa0
        {
            let u64_at = |at: usize| u64::from_le_bytes(r[at..at + 8].try_into().unwrap());
            e.times = Times {
                created: u64_at(0x28),
                modified: u64_at(0x30),
                changed: u64_at(0x38),
                accessed: u64_at(0x40),
            };
            e.size = u64_at(0x58);
            e.attributes = (e.attributes & !0xffff) | u32::from_le_bytes(r[0x48..0x4c].try_into().unwrap()) & 0xffff;
            links = u32::from_le_bytes(r[0x98..0x9c].try_into().unwrap()).max(1);
        }
        let (rw, ro) = if self.writable { (0o200, 0o555) } else { (0, 0o555) };
        let (kind, mut size, perm, times) = match &entry {
            None => (
                FileType::Directory,
                0,
                ro | rw,
                vol.directory_times(refs::volume::ROOT_DIRECTORY).unwrap_or_default(),
            ),
            Some(e) if Self::is_link(e) => (
                FileType::Symlink,
                self.link(vol, &path).map_or(0, |l| l.len() as u64),
                0o777,
                e.times,
            ),
            Some(e) if e.is_dir() => (FileType::Directory, 0, ro | rw, e.times),
            Some(e) => {
                let writable = if e.attributes & 1 == 0 { rw } else { 0 };
                (FileType::RegularFile, e.size, 0o444 | writable, e.times)
            }
        };
        if let Some(p) = self.state.lock_ok().pending.get(&ino) {
            size = p.len;
        }
        let (uid, gid) = if self.writable { self.owner } else { (0, 0) };
        Ok(FileAttr {
            ino: INodeNo(ino),
            size,
            blocks: size.div_ceil(512),
            atime: system_time(times.accessed),
            mtime: system_time(times.modified),
            ctime: system_time(times.changed),
            crtime: system_time(times.created),
            kind,
            perm,
            nlink: if kind == FileType::Directory { 2 } else { links },
            uid,
            gid,
            rdev: 0,
            flags: 0,
            blksize: vol.cluster as u32,
        })
    }

    /// Where a link points, as a Linux path.
    fn link(&self, vol: &Volume<Rw>, path: &str) -> Option<String> {
        let target = self.file(vol, path).ok()?.reparse.as_ref()?.link_target()?;
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

    /// A file's whole content, in a pending buffer.
    fn content(&self, vol: &Volume<Rw>, path: &str) -> Result<Pending, Errno> {
        let mut p = Pending::new()?;
        let file = self.file(vol, path)?;
        let Some(stream) = &file.data else {
            return Ok(p);
        };
        if stream.size > MAX_WRITTEN {
            return Err(Errno::EFBIG);
        }
        let mut buf = vec![0u8; 1 << 20];
        let mut at = 0;
        while at < stream.size {
            let n = vol.read_stream(stream, at, &mut buf).map_err(|e| errno(path, e))?;
            if n == 0 {
                break;
            }
            p.write_at(at, &buf[..n])?;
            at += n as u64;
        }
        p.set_len(stream.size)?;
        // What it holds is the file's own: nothing written yet.
        p.ranges.clear();
        Ok(p)
    }

    /// Runs a change on the volume (refused unless mounted for writing).
    fn change<T>(&self, what: &str, f: impl FnOnce(&mut Volume<Rw>) -> refs::Result<T>) -> Result<T, Errno> {
        if !self.writable {
            return Err(Errno::EROFS);
        }
        let mut vol = self.vol.write_ok();
        let r = f(&mut vol).map_err(|e| errno(what, e));
        drop(vol);
        self.changed(None);
        r
    }

    /// Writes back a file opened for writing, if it changed: in place
    /// what was written (`update_file`), or the file anew.
    fn write_back(&self, ino: u64) -> Result<(), Errno> {
        let (file, len, ranges, whole) = {
            let mut s = self.state.lock_ok();
            match s.pending.get_mut(&ino) {
                Some(p) if p.dirty => {
                    p.dirty = false;
                    let ranges = std::mem::take(&mut p.ranges);
                    let whole = std::mem::replace(&mut p.whole, false);
                    (p.file.try_clone().map_err(|_| Errno::EIO)?, p.len, ranges, whole)
                }
                _ => return Ok(()),
            }
        };
        let path = self.path(ino)?;
        let source = PendingData(&file, len);
        if whole {
            self.change(&path, |v| v.write_file_from(&path, &source, now()))
        } else {
            self.change(&path, |v| v.update_file(&path, &source, &ranges, now()))
        }
    }

    /// The entry reply for a path just made.
    fn entry_reply(&self, path: &str, reply: ReplyEntry) {
        let vol = self.vol.read_ok();
        let entry = self.resolve(&vol, path).ok().flatten();
        let ino = self.inode(path, entry.as_ref());
        match self.attr(&vol, ino) {
            Ok(attr) => {
                self.state.lock_ok().looked_up(ino);
                reply.entry(&TTL, &attr, fuser::Generation(0))
            }
            Err(e) => reply.error(e),
        }
    }

    fn name_path(&self, parent: INodeNo, name: &OsStr) -> Result<String, Errno> {
        let name = name.to_str().ok_or(Errno::EINVAL)?;
        Ok(child(&self.path(parent.0)?, name))
    }
}

impl Filesystem for RefsFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let path = match self.name_path(parent, name) {
            Ok(p) => p,
            Err(e) => return reply.error(e),
        };
        let vol = self.vol.read_ok();
        if let Err(e) = self.directory(&vol, split(&path).0) {
            return reply.error(e);
        }
        // The name as the volume spells it (lookups ignore case).
        let (path, entry) = match self.resolve(&vol, &path) {
            Ok(Some(e)) => (child(split(&path).0, &e.name), Some(e)),
            Ok(None) => (path, None),
            Err(e) => return reply.error(e),
        };
        let ino = self.inode(&path, entry.as_ref());
        match self.attr(&vol, ino) {
            Ok(attr) => {
                self.state.lock_ok().looked_up(ino);
                reply.entry(&TTL, &attr, fuser::Generation(0))
            }
            Err(e) => reply.error(e),
        }
    }

    fn forget(&self, _req: &Request, ino: INodeNo, nlookup: u64) {
        self.state.lock_ok().forget(ino.0, nlookup);
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let vol = self.vol.read_ok();
        match self.attr(&vol, ino.0) {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(e) => reply.error(e),
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let r = (|| -> Result<(), Errno> {
            let path = self.path(ino.0)?;
            if let Some(size) = size {
                if size > MAX_WRITTEN {
                    return Err(Errno::EFBIG);
                }
                let pending = self.state.lock_ok().pending.contains_key(&ino.0);
                if pending {
                    let mut s = self.state.lock_ok();
                    let p = s.pending.get_mut(&ino.0).unwrap();
                    p.set_len(size)?;
                    p.dirty = true;
                } else {
                    let mut p = self.content(&self.vol.read_ok(), &path)?;
                    p.set_len(size)?;
                    let ranges = std::mem::take(&mut p.ranges);
                    let source = PendingData(&p.file, p.len);
                    self.change(&path, |v| v.update_file(&path, &source, &ranges, now()))?;
                }
            }
            if atime.is_some() || mtime.is_some() || crtime.is_some() {
                let mut times = self.resolve(&self.vol.read_ok(), &path)?.ok_or(Errno::EPERM)?.times;
                let pick = |t: TimeOrNow| match t {
                    TimeOrNow::Now => now(),
                    TimeOrNow::SpecificTime(t) => filetime(t),
                };
                if let Some(t) = atime {
                    times.accessed = pick(t);
                }
                if let Some(t) = mtime {
                    times.modified = pick(t);
                }
                if let Some(t) = crtime {
                    times.created = filetime(t);
                }
                times.changed = now();
                self.change(&path, |v| v.set_times(&path, &times))?;
            }
            if let Some(mode) = mode {
                // The write bits: Windows' read-only attribute.
                let entry = self.resolve(&self.vol.read_ok(), &path)?.ok_or(Errno::EPERM)?;
                if !entry.is_dir() && !Self::is_link(&entry) {
                    let ro = mode & 0o222 == 0;
                    if ro != (entry.attributes & 1 != 0) {
                        let a = (entry.attributes & !1) | u32::from(ro);
                        self.change(&path, |v| v.set_attributes(&path, a))?;
                    }
                }
            }
            Ok(())
        })();
        match r {
            Ok(()) => self.getattr(_req, ino, None, reply),
            Err(e) => reply.error(e),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        let vol = self.vol.read_ok();
        match self.path(ino.0).ok().and_then(|p| self.link(&vol, &p)) {
            Some(target) => reply.data(target.as_bytes()),
            None => reply.error(Errno::EINVAL),
        }
    }

    fn mkdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, _mode: u32, _umask: u32, reply: ReplyEntry) {
        let path = match self.name_path(parent, name) {
            Ok(p) => p,
            Err(e) => return reply.error(e),
        };
        match self.change(&path, |v| v.create_directory(&path, now())) {
            Ok(()) => self.entry_reply(&path, reply),
            Err(e) => reply.error(e),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let path = match self.name_path(parent, name) {
            Ok(p) => p,
            Err(e) => return reply.error(e),
        };
        match self.change(&path, |v| v.delete_file(&path, now())) {
            Ok(()) => {
                self.changed(Some(&path));
                reply.ok()
            }
            Err(e) => reply.error(e),
        }
    }

    fn rmdir(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.unlink(req, parent, name, reply)
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let r = (|| -> Result<(), Errno> {
            let from = self.name_path(parent, name)?;
            let to = self.name_path(newparent, newname)?;
            if flags.contains(RenameFlags::RENAME_EXCHANGE) {
                return Err(Errno::EINVAL);
            }
            let exists = match self.resolve(&self.vol.read_ok(), &to) {
                Err(Errno::ENOENT) => None,
                r => r?,
            };
            if let Some(target) = exists {
                if flags.contains(RenameFlags::RENAME_NOREPLACE) {
                    return Err(Errno::EEXIST);
                }
                // The name only changes case: Windows' names ignore case.
                if !from.eq_ignore_ascii_case(&to) {
                    let source = self.resolve(&self.vol.read_ok(), &from)?.ok_or(Errno::EBUSY)?;
                    if source.is_dir() != target.is_dir() {
                        return Err(if target.is_dir() { Errno::EISDIR } else { Errno::ENOTDIR });
                    }
                    self.change(&to, |v| v.delete_file(&to, now()))?;
                    self.changed(Some(&to));
                }
            }
            let (from_dir, _) = split(&from);
            let (to_dir, to_name) = split(&to);
            if from_dir == to_dir {
                self.change(&from, |v| v.rename(&from, to_name, now()))?;
            } else {
                self.change(&from, |v| v.move_file(&from, &to, now()))?;
            }
            self.moved(&from, &to);
            Ok(())
        })();
        match r {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn link(&self, _req: &Request, ino: INodeNo, newparent: INodeNo, newname: &OsStr, reply: ReplyEntry) {
        let r = (|| -> Result<String, Errno> {
            let path = self.path(ino.0)?;
            let to = self.name_path(newparent, newname)?;
            self.change(&to, |v| v.link_file(&path, &to, now()))?;
            // The new name is the same inode.
            let mut s = self.state.lock_ok();
            s.inodes.insert(to.clone(), ino.0);
            s.paths.entry(ino.0).or_default().push(to.clone());
            drop(s);
            let vol = self.vol.read_ok();
            if let Ok(Some(e)) = self.resolve(&vol, &to) {
                self.inode(&to, Some(&e));
            }
            Ok(to)
        })();
        match r {
            Ok(to) => self.entry_reply(&to, reply),
            Err(e) => reply.error(e),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let r = (|| -> Result<(), Errno> {
            let path = self.path(ino.0)?;
            let vol = self.vol.read_ok();
            self.file(&vol, &path)?;
            if flags.0 & 3 == 0 {
                return Ok(());
            }
            if !self.writable {
                return Err(Errno::EROFS);
            }
            let truncate = flags.0 & O_TRUNC != 0;
            let mut s = self.state.lock_ok();
            if let Some(p) = s.pending.get_mut(&ino.0) {
                p.handles += 1;
                if truncate {
                    p.set_len(0)?;
                    p.dirty = true;
                    p.whole = true;
                }
                return Ok(());
            }
            drop(s);
            let mut p = if truncate {
                Pending::new()?
            } else {
                self.content(&vol, &path)?
            };
            p.dirty = truncate;
            p.whole = truncate;
            self.state.lock_ok().pending.insert(ino.0, p);
            Ok(())
        })();
        match r {
            Ok(()) => reply.opened(FileHandle(0), FopenFlags::empty()),
            Err(e) => reply.error(e),
        }
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let r = (|| -> Result<FileAttr, Errno> {
            let path = self.name_path(parent, name)?;
            self.change(&path, |v| v.create_file(&path, b"", now()))?;
            let ino = self.inode(&path, None);
            let mut p = Pending::new()?;
            p.whole = true;
            self.state.lock_ok().pending.insert(ino, p);
            self.attr(&self.vol.read_ok(), ino)
        })();
        match r {
            Ok(attr) => {
                self.state.lock_ok().looked_up(attr.ino.0);
                reply.created(&TTL, &attr, fuser::Generation(0), FileHandle(0), FopenFlags::empty())
            }
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
        if let Some(p) = self.state.lock_ok().pending.get(&ino.0) {
            return match p.read_at(offset, size) {
                Ok(b) => reply.data(&b),
                Err(e) => reply.error(e),
            };
        }
        let vol = self.vol.read_ok();
        let file = match self.path(ino.0).and_then(|p| self.file(&vol, &p)) {
            Ok(f) => f,
            Err(e) => return reply.error(e),
        };
        let Some(stream) = &file.data else {
            return reply.data(&[]);
        };
        let mut buf = vec![0u8; size as usize];
        match vol.read_stream(stream, offset, &mut buf) {
            Ok(n) => reply.data(&buf[..n]),
            Err(e) => {
                eprintln!("read of {size} bytes at {offset:#x} failed: {e}");
                reply.error(Errno::EIO)
            }
        }
    }

    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let mut s = self.state.lock_ok();
        let Some(p) = s.pending.get_mut(&ino.0) else {
            return reply.error(Errno::EBADF);
        };
        if offset + data.len() as u64 > MAX_WRITTEN {
            return reply.error(Errno::EFBIG);
        }
        if let Err(e) = p.write_at(offset, data) {
            return reply.error(e);
        }
        p.dirty = true;
        reply.written(data.len() as u32)
    }

    /// A whole file copied into an empty one (as `cp` and file managers
    /// copy with copy_file_range) becomes a block clone, as Windows'
    /// Copy-Item makes on a Dev Drive: the copy shares the clusters until
    /// one of them changes. Other ranges are copied through the files'
    /// buffers, a MiB at a time.
    fn copy_file_range(
        &self,
        _req: &Request,
        ino_in: INodeNo,
        _fh_in: FileHandle,
        offset_in: u64,
        ino_out: INodeNo,
        _fh_out: FileHandle,
        offset_out: u64,
        len: u64,
        _flags: CopyFileRangeFlags,
        reply: ReplyWrite,
    ) {
        let r = (|| -> Result<u32, Errno> {
            if !self.writable {
                return Err(Errno::EROFS);
            }
            let (src, dst) = (self.path(ino_in.0)?, self.path(ino_out.0)?);
            let (buffered, src_dirty, dst_len) = {
                let s = self.state.lock_ok();
                let dst = s.pending.get(&ino_out.0).ok_or(Errno::EBADF)?;
                let src = s.pending.get(&ino_in.0);
                (src.map(|p| p.len), src.is_some_and(|p| p.dirty), dst.len)
            };
            let size = match buffered {
                Some(len) => len,
                None => {
                    let vol = self.vol.read_ok();
                    self.file(&vol, &src)?.data.as_ref().map_or(0, |s| s.size)
                }
            };
            if ino_in != ino_out
                && (offset_in, offset_out, dst_len) == (0, 0, 0)
                && len >= size
                && size > 0
                && size <= u64::from(u32::MAX)
                && !src_dirty
            {
                self.write_back(ino_out.0)?;
                if self.change(&dst, |v| v.clone_file(&src, &dst, now())).is_ok() {
                    // The copy's buffer: its new content, nothing to write.
                    let vol = self.vol.read_ok();
                    let mut p = self.content(&vol, &dst)?;
                    drop(vol);
                    let mut s = self.state.lock_ok();
                    p.handles = s.pending.get(&ino_out.0).map_or(1, |old| old.handles);
                    s.pending.insert(ino_out.0, p);
                    return Ok(size as u32);
                }
            }
            let n = len.min(1 << 20).min(size.saturating_sub(offset_in));
            if offset_out.saturating_add(n) > MAX_WRITTEN {
                return Err(Errno::EFBIG);
            }
            let data = match self.state.lock_ok().pending.get(&ino_in.0) {
                Some(p) => Some(p.read_at(offset_in, n as u32)?),
                None => None,
            };
            let data = match data {
                Some(d) => d,
                None => {
                    let vol = self.vol.read_ok();
                    let file = self.file(&vol, &src)?;
                    let mut buf = vec![0u8; n as usize];
                    if let Some(stream) = &file.data {
                        let got = vol
                            .read_stream(stream, offset_in, &mut buf)
                            .map_err(|e| errno(&src, e))?;
                        buf.truncate(got);
                    }
                    buf
                }
            };
            let mut s = self.state.lock_ok();
            let p = s.pending.get_mut(&ino_out.0).ok_or(Errno::EBADF)?;
            p.write_at(offset_out, &data)?;
            p.dirty = true;
            Ok(data.len() as u32)
        })();
        match r {
            Ok(n) => reply.written(n),
            Err(e) => reply.error(e),
        }
    }

    fn flush(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, _lock_owner: LockOwner, reply: ReplyEmpty) {
        match self.write_back(ino.0) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn fsync(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, _datasync: bool, reply: ReplyEmpty) {
        match self.write_back(ino.0) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn release(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let r = self.write_back(ino.0);
        let mut s = self.state.lock_ok();
        if let Some(p) = s.pending.get_mut(&ino.0) {
            p.handles = p.handles.saturating_sub(1);
            if p.handles == 0 && !p.dirty {
                s.pending.remove(&ino.0);
            }
        }
        match r {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn readdir(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
        let vol = self.vol.read_ok();
        let path = match self.path(ino.0) {
            Ok(p) => p,
            Err(e) => return reply.error(e),
        };
        let listing = match self.directory(&vol, &path).and_then(|d| self.listing(&vol, d)) {
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
            all.push((self.inode(&child(&path, &e.name), Some(e)), kind, e.name.clone()));
        }
        for (i, (child, kind, name)) in all.into_iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(child), (i + 1) as u64, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        let vol = self.vol.read_ok();
        let blocks = vol.boot.volume_size() / vol.cluster;
        let free = vol.free_clusters().unwrap_or(0);
        reply.statfs(blocks, free, free, 0, 0, vol.cluster as u32, 255, vol.cluster as u32);
    }

    fn listxattr(&self, _req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        let vol = self.vol.read_ok();
        let names: Vec<u8> = match self.path(ino.0).and_then(|p| self.file(&vol, &p)) {
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
        let vol = self.vol.read_ok();
        let Ok(file) = self.path(ino.0).and_then(|p| self.file(&vol, &p)) else {
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
        match vol.read_stream(stream, 0, &mut buf) {
            Ok(n) => reply.data(&buf[..n]),
            Err(_) => reply.error(Errno::EIO),
        }
    }

    fn setxattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        name: &OsStr,
        value: &[u8],
        flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        let r = (|| -> Result<(), Errno> {
            let name = name.to_str().ok_or(Errno::EINVAL)?;
            let stream = name.strip_prefix("user.").ok_or(Errno::EOPNOTSUPP)?;
            let path = self.path(ino.0)?;
            let file = self.file(&self.vol.read_ok(), &path)?;
            let exists = file.streams.iter().any(|(n, _)| n.eq_ignore_ascii_case(stream));
            if exists && flags & XATTR_CREATE != 0 {
                return Err(Errno::EEXIST);
            }
            if !exists && flags & XATTR_REPLACE != 0 {
                return Err(Errno::NO_XATTR);
            }
            self.change(&path, |v| v.write_stream(&path, stream, value, now()))
        })();
        match r {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn removexattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let r = (|| -> Result<(), Errno> {
            let name = name.to_str().ok_or(Errno::EINVAL)?;
            let stream = name.strip_prefix("user.").ok_or(Errno::NO_XATTR)?;
            let path = self.path(ino.0)?;
            self.change(&path, |v| v.delete_stream(&path, stream, now()))
                .map_err(|e| if e == Errno::ENOENT { Errno::NO_XATTR } else { e })
        })();
        match r {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }
}

/// What `refs mount --status-file PATH` tells whoever started it
/// (`mount.refs`): `state=opening` with `read=` the bytes read from the
/// device so far, renewed every second while the volume is opened; then
/// `state=mounted` once the mount is in place, or `state=failed` with
/// `error=`. Each state is written whole into PATH.tmp and renamed over
/// PATH, so a reader never sees half of one. So the one waiting needs no
/// timeout: a slow device shows as a growing count.
pub struct MountStatus {
    path: Option<PathBuf>,
    read: Arc<std::sync::atomic::AtomicU64>,
    /// Set once mounted or failed; the lock orders the writes.
    done: Arc<Mutex<bool>>,
}

impl MountStatus {
    /// Starts reporting into `path` (nothing without one).
    pub fn start(path: Option<PathBuf>) -> Self {
        let status = MountStatus {
            path,
            read: Default::default(),
            done: Default::default(),
        };
        if let Some(path) = status.path.clone() {
            let (read, done) = (status.read.clone(), status.done.clone());
            std::thread::spawn(move || {
                loop {
                    {
                        let done = done.lock().unwrap_or_else(|e| e.into_inner());
                        if *done {
                            break;
                        }
                        let n = read.load(std::sync::atomic::Ordering::Relaxed);
                        let _ = Self::write(&path, &format!("state=opening\nread={n}\n"));
                    }
                    std::thread::sleep(Duration::from_secs(1));
                }
            });
        }
        status
    }

    /// The counter of bytes read that the progress shows.
    pub fn counter(&self) -> Arc<std::sync::atomic::AtomicU64> {
        self.read.clone()
    }

    pub fn mounted(&self) {
        self.finish("state=mounted\n");
    }

    pub fn failed(&self, error: &str) {
        self.finish(&format!("state=failed\nerror={}\n", error.replace('\n', " | ")));
    }

    fn finish(&self, text: &str) {
        let mut done = self.done.lock().unwrap_or_else(|e| e.into_inner());
        if let (false, Some(path)) = (*done, &self.path) {
            let _ = Self::write(path, text);
        }
        *done = true;
    }

    fn write(path: &Path, text: &str) -> std::io::Result<()> {
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path)
    }
}

/// Mounts the volume at `mountpoint` and serves it until unmounted; with
/// `writable` changes go to the volume. `source` names the mount (the
/// device, as mount(8) and udisks2 expect to find it in the mount table);
/// with `blkdev` (root, a block device) the mount is of type fuseblk on
/// that device, as ntfs-3g's are, so that udisks2 counts it as the
/// device's mount. `status` learns when the mount is in place.
pub fn serve(
    vol: Volume<Rw>,
    source: &str,
    mountpoint: &Path,
    allow_other: bool,
    writable: bool,
    blkdev: bool,
    status: &MountStatus,
) -> Result<()> {
    let mut config = Config::default();
    config.mount_options.extend([
        MountOption::FSName(source.into()),
        MountOption::Subtype("refs".into()),
        MountOption::DefaultPermissions,
    ]);
    if !writable {
        config.mount_options.push(MountOption::RO);
    }
    if allow_other {
        config.acl = fuser::SessionACL::All;
    }
    if blkdev {
        // fusermount3 makes a fuseblk mount of "blkdev" (fuser mounts
        // type fuse itself, as root, unless asked to unmount through
        // fusermount3 when the process ends; that needs allow_other, which
        // fuser narrows to root and the owner itself).
        config
            .mount_options
            .extend([MountOption::CUSTOM("blkdev".into()), MountOption::AutoUnmount]);
        if config.acl == fuser::SessionACL::Owner {
            config.acl = fuser::SessionACL::RootAndOwner;
        }
    }
    // Changes are made one at a time; reads may run beside each other.
    config.n_threads = Some(if writable { 1 } else { 4 });
    let mountpoint = std::fs::canonicalize(mountpoint).unwrap_or_else(|_| mountpoint.to_path_buf());
    let owner = std::fs::metadata(&mountpoint)
        .map(|m| (m.uid(), m.gid()))
        .unwrap_or((0, 0));
    let state = State::new();
    let fs = RefsFs {
        vol: RwLock::new(vol),
        writable,
        mountpoint: mountpoint.clone(),
        owner,
        state: Mutex::new(state),
    };
    let session = fuser::spawn_mount(fs, &mountpoint, &config)
        .with_context(|| format!("cannot mount on {}", mountpoint.display()))?;
    status.mounted();
    session.join().context("FUSE session failed")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// The status file of `refs mount --status-file`: opening with the
    /// bytes read while the volume opens, then mounted or failed, each
    /// state whole; nothing changes it after that.
    #[test]
    fn mount_status_reports_progress_then_the_outcome() {
        let dir = std::env::temp_dir().join(format!("refs-status-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let read = |p: &Path| std::fs::read_to_string(p).unwrap_or_default();
        let path = dir.join("status");
        let status = MountStatus::start(Some(path.clone()));
        status.counter().fetch_add(4096, std::sync::atomic::Ordering::Relaxed);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while read(&path) != "state=opening\nread=4096\n" {
            assert!(std::time::Instant::now() < deadline, "{:?}", read(&path));
            std::thread::sleep(Duration::from_millis(50));
        }
        status.mounted();
        assert_eq!(read(&path), "state=mounted\n");
        std::thread::sleep(Duration::from_millis(1200));
        status.failed("too late");
        assert_eq!(read(&path), "state=mounted\n");
        let failed = dir.join("failed");
        MountStatus::start(Some(failed.clone())).failed("no ReFS volume\non the device");
        assert_eq!(read(&failed), "state=failed\nerror=no ReFS volume | on the device\n");
        assert!(!dir.join("status.tmp").exists() && !dir.join("failed.tmp").exists());
        MountStatus::start(None).mounted();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn entry(name: &str) -> Entry {
        Entry {
            name: name.to_owned(),
            attributes: 0,
            times: Default::default(),
            size: 0,
            allocated: 0,
            target: Target::Embedded(Vec::new()),
        }
    }

    #[test]
    fn inodes_the_kernel_forgot_are_dropped() {
        let mut s = State::new();
        let a = s.inode("/a", None);
        s.looked_up(a);
        s.looked_up(a);
        s.forget(a, 1);
        assert!(s.paths.contains_key(&a), "one lookup is still held");
        s.forget(a, 1);
        assert!(!s.paths.contains_key(&a) && !s.inodes.contains_key("/a"));
        // The root is not the kernel's to forget.
        s.forget(INodeNo::ROOT.0, 1000);
        assert!(s.paths.contains_key(&INodeNo::ROOT.0));
    }

    #[test]
    fn inodes_nothing_looked_up_are_bounded() {
        // Listing a directory makes inodes the kernel then looks up one by
        // one, or never; a walk over a big volume must not keep them all.
        let mut s = State::new();
        let held = s.inode("/held", None);
        s.looked_up(held);
        for i in 0..INODES_KEPT + 100 {
            s.inode(&format!("/f{i}"), None);
        }
        assert!(s.inodes.len() <= INODES_KEPT + 1, "{} inodes", s.inodes.len());
        assert!(s.paths.contains_key(&held) && s.paths.contains_key(&INodeNo::ROOT.0));
    }

    #[test]
    fn cached_listings_and_files_are_bounded() {
        let mut s = State::new();
        let big = Arc::new(Dir {
            entries: (0..DIR_ENTRIES_KEPT / 2 + 1).map(|i| entry(&i.to_string())).collect(),
            by_name: HashMap::new(),
        });
        for oid in 0..5 {
            s.cache_dir(oid, big.clone());
            assert!(s.dir_entries <= DIR_ENTRIES_KEPT, "{} entries", s.dir_entries);
        }
        let huge = Arc::new(Dir {
            entries: (0..DIR_ENTRIES_KEPT + 1).map(|i| entry(&i.to_string())).collect(),
            by_name: HashMap::new(),
        });
        s.cache_dir(9, huge);
        assert!(!s.dirs.contains_key(&9), "a listing over the budget is not kept");
        let file = Arc::new(RefsFile::default());
        for i in 0..FILES_KEPT + 10 {
            s.cache_file(format!("/f{i}"), file.clone());
        }
        assert!(s.files.len() <= FILES_KEPT);
    }

    #[test]
    fn a_panic_in_one_handler_does_not_end_the_mount() {
        let m = Arc::new(Mutex::new(1u32));
        let l = Arc::new(RwLock::new(1u32));
        let (m2, l2) = (m.clone(), l.clone());
        let _ = std::thread::spawn(move || {
            let _a = m2.lock_ok();
            let _b = l2.write_ok();
            panic!("a handler fails");
        })
        .join();
        assert!(m.is_poisoned() && l.is_poisoned());
        assert_eq!(*m.lock_ok(), 1);
        assert_eq!(*l.read_ok(), 1);
        *l.write_ok() = 2;
    }
}
