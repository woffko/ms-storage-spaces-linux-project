//! Exposing a space as a single read-only file through FUSE.
//!
//! The mount point holds one file, `space.img`, which can be attached with
//! `losetup -r -b <sector size>`. This is the fallback for systems without
//! ublk, NBD or device-mapper.

use std::ffi::OsStr;
use std::fs::File;
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{Context, Result};
use fuser::{
    Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, INodeNo, LockOwner, MountOption, OpenFlags,
    ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry, ReplyOpen, Request,
};
use storage_spaces::SpaceReader;

pub const FILE_NAME: &str = "space.img";
const FILE_INO: INodeNo = INodeNo(2);
const TTL: Duration = Duration::from_secs(3600);

struct SpaceFs {
    reader: &'static SpaceReader<'static, File>,
    block_size: u32,
}

impl SpaceFs {
    fn attr(&self, ino: INodeNo) -> Option<FileAttr> {
        let (kind, size, perm, nlink) = match ino {
            INodeNo::ROOT => (FileType::Directory, 0, 0o555, 2),
            FILE_INO => (FileType::RegularFile, self.reader.size(), 0o444, 1),
            _ => return None,
        };
        Some(FileAttr {
            ino,
            size,
            blocks: size.div_ceil(512),
            atime: UNIX_EPOCH,
            mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH,
            kind,
            perm,
            nlink,
            uid: 0,
            gid: 0,
            rdev: 0,
            flags: 0,
            blksize: self.block_size,
        })
    }
}

impl Filesystem for SpaceFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        match self.attr(FILE_INO) {
            Some(attr) if parent == INodeNo::ROOT && name == FILE_NAME => {
                reply.entry(&TTL, &attr, fuser::Generation(0))
            }
            _ => reply.error(Errno::ENOENT),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.attr(ino) {
            Some(attr) => reply.attr(&TTL, &attr),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        if ino != FILE_INO {
            reply.error(Errno::EISDIR);
        } else if flags.0 & 3 != 0 {
            reply.error(Errno::EROFS); // O_WRONLY or O_RDWR
        } else {
            reply.opened(FileHandle(0), FopenFlags::FOPEN_KEEP_CACHE);
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
        if ino != FILE_INO {
            return reply.error(Errno::ENOENT);
        }
        let len = (size as u64).min(self.reader.size().saturating_sub(offset)) as usize;
        let mut buf = vec![0u8; len];
        match self.reader.read_exact_at(&mut buf, offset) {
            Ok(()) => reply.data(&buf),
            Err(e) => {
                eprintln!("read of {len} bytes at {offset:#x} failed: {e}");
                reply.error(Errno::EIO)
            }
        }
    }

    fn readdir(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
        if ino != INodeNo::ROOT {
            return reply.error(Errno::ENOTDIR);
        }
        let entries = [
            (INodeNo::ROOT, FileType::Directory, "."),
            (INodeNo::ROOT, FileType::Directory, ".."),
            (FILE_INO, FileType::RegularFile, FILE_NAME),
        ];
        for (i, (ino, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            if reply.add(ino, (i + 1) as u64, kind, name) {
                break;
            }
        }
        reply.ok();
    }
}

/// Mounts the space at `mountpoint` and serves it until unmounted.
pub fn serve(
    reader: &'static SpaceReader<'static, File>,
    block_size: u32,
    mountpoint: &Path,
    ready_file: Option<&Path>,
) -> Result<()> {
    let mut config = Config::default();
    config.mount_options.extend([
        MountOption::RO,
        MountOption::FSName("storage-spaces".into()),
        MountOption::Subtype("spaces".into()),
        MountOption::DefaultPermissions,
    ]);
    config.n_threads = Some(4);
    let session = fuser::spawn_mount(SpaceFs { reader, block_size }, mountpoint, &config)
        .with_context(|| format!("cannot mount on {}", mountpoint.display()))?;
    if let Some(path) = ready_file {
        std::fs::write(path, mountpoint.join(FILE_NAME).display().to_string())?;
    }
    session.join().context("FUSE session failed")?;
    Ok(())
}
