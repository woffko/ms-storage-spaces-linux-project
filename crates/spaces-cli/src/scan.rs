//! Finding pool members among the block devices of the system.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use storage_spaces::Guid;
use storage_spaces::format::DiskHeader;
use storage_spaces::gpt::find_spaces_partition;
use storage_spaces::io::ReadAt;

/// A device holding a Storage Spaces disk header.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub path: PathBuf,
    pub pool_guid: Guid,
    pub disk_guid: Guid,
    /// True if the device is the Storage Spaces partition itself.
    pub is_partition: bool,
}

/// Devices never scanned: our own outputs and devices without media.
fn skipped(name: &str) -> bool {
    ["dm-", "ublkb", "nbd", "zram", "ram", "sr", "fd", "md"]
        .iter()
        .any(|p| name.starts_with(p))
}

/// Reads the disk header of a device, whole disk or partition; an error
/// if the device cannot be opened.
pub fn probe(path: &Path) -> std::io::Result<Option<Candidate>> {
    let file = File::open(path)?;
    Ok(read_header(path, &file))
}

fn read_header(path: &Path, file: &File) -> Option<Candidate> {
    if file.size().ok()? == 0 {
        return None;
    }
    let location = find_spaces_partition(file).ok()??;
    let mut buf = vec![0u8; DiskHeader::SIZE];
    file.read_exact_at(&mut buf, location.offset).ok()?;
    let header = DiskHeader::parse(&buf).ok()?;
    Some(Candidate {
        path: path.to_path_buf(),
        pool_guid: header.pool_guid,
        disk_guid: header.disk_guid,
        is_partition: location.offset == 0,
    })
}

/// What a scan found: the members by pool, and how many devices could not
/// be opened for want of permission (all of them without root).
pub struct Scan {
    pub pools: BTreeMap<Guid, Vec<Candidate>>,
    pub denied: usize,
}

/// Scans all block devices and groups the members by pool. When both a
/// disk and its Storage Spaces partition are found, the partition is used.
pub fn scan() -> Scan {
    let mut found: Vec<Candidate> = Vec::new();
    let mut denied = 0;
    let Ok(entries) = fs::read_dir("/sys/class/block") else {
        return Scan {
            pools: BTreeMap::new(),
            denied,
        };
    };
    let mut names: Vec<String> = entries.filter_map(|e| e.ok()?.file_name().into_string().ok()).collect();
    names.sort();
    for name in names.into_iter().filter(|n| !skipped(n)) {
        match probe(&Path::new("/dev").join(&name)) {
            Ok(Some(c)) => found.push(c),
            Ok(None) => {}
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => denied += 1,
            Err(_) => {}
        }
    }
    let mut pools: BTreeMap<Guid, Vec<Candidate>> = BTreeMap::new();
    for c in found {
        let members = pools.entry(c.pool_guid).or_default();
        match members.iter_mut().find(|m| m.disk_guid == c.disk_guid) {
            Some(existing) if c.is_partition && !existing.is_partition => *existing = c,
            Some(_) => {}
            None => members.push(c),
        }
    }
    Scan { pools, denied }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A device this user may not open is an error (the scan counts them),
    /// not "no pool member"; one without a disk header is no member.
    #[test]
    fn probe_tells_unreadable_from_no_member() {
        let dir = std::env::temp_dir().join(format!("spaces-probe-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("disk.img");
        fs::write(&path, vec![0u8; 1 << 20]).unwrap();
        assert!(probe(&path).unwrap().is_none());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        match probe(&path) {
            Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied),
            Ok(_) => eprintln!("permissions do not apply to this user (root): skipped"),
        }
        fs::remove_dir_all(&dir).unwrap();
    }
}
