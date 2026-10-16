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
    // Every block device is read: a partition table that makes the parser
    // panic is no member, not the end of the scan.
    Ok(storage_spaces::isolate::caught(|| read_header(path, &file)).unwrap_or(None))
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
    Scan {
        pools: group(found, partition_of),
        denied,
    }
}

/// Whether block device `part` is a partition of the disk `disk` (sysfs).
fn partition_of(part: &Path, disk: &Path) -> bool {
    let (Some(p), Some(d)) = (part.file_name(), disk.file_name()) else {
        return false;
    };
    let sys = Path::new("/sys/class/block").join(p);
    sys.join("partition").exists()
        && fs::canonicalize(&sys)
            .ok()
            .is_some_and(|real| real.parent().and_then(|x| x.file_name()) == Some(d))
}

/// The members by pool. A disk found whole and as its own Storage Spaces
/// partition is the partition; two devices with one disk GUID otherwise (a
/// clone) are both kept, for the pool to report.
fn group(found: Vec<Candidate>, partition_of: impl Fn(&Path, &Path) -> bool) -> BTreeMap<Guid, Vec<Candidate>> {
    let mut pools: BTreeMap<Guid, Vec<Candidate>> = BTreeMap::new();
    for c in found {
        let members = pools.entry(c.pool_guid).or_default();
        let same = members.iter().position(|m| {
            m.disk_guid == c.disk_guid
                && (c.is_partition && !m.is_partition && partition_of(&c.path, &m.path)
                    || m.is_partition && !c.is_partition && partition_of(&m.path, &c.path))
        });
        match same {
            Some(i) if c.is_partition => members[i] = c,
            Some(_) => {}
            None => members.push(c),
        }
    }
    pools
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A disk and its own partition: the partition. Two devices with one
    /// disk GUID that are not: both (in 1.2.1 the first by name).
    #[test]
    fn a_clone_is_kept_beside_its_original() {
        let guid = |b: u8| Guid([b; 16]);
        let c = |path: &str, disk: u8, is_partition: bool| Candidate {
            path: PathBuf::from(path),
            pool_guid: guid(1),
            disk_guid: guid(disk),
            is_partition,
        };
        let partition_of = |p: &Path, d: &Path| p.to_str().unwrap().starts_with(d.to_str().unwrap());
        let pools = group(
            vec![
                c("/dev/sdb", 2, false),
                c("/dev/sdb1", 2, true),
                c("/dev/sdc1", 3, true),
            ],
            partition_of,
        );
        let paths: Vec<_> = pools[&guid(1)].iter().map(|m| m.path.to_str().unwrap()).collect();
        assert_eq!(paths, ["/dev/sdb1", "/dev/sdc1"]);
        let pools = group(vec![c("/dev/sdb1", 2, true), c("/dev/sdd1", 2, true)], partition_of);
        assert_eq!(pools[&guid(1)].len(), 2);
    }

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
