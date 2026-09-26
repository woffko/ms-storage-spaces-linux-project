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

/// Reads the disk header of a device, whole disk or partition.
pub fn probe(path: &Path) -> Option<Candidate> {
    let file = File::open(path).ok()?;
    if file.size().ok()? == 0 {
        return None;
    }
    let location = find_spaces_partition(&file).ok()??;
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

/// Scans all block devices and groups the members by pool. When both a
/// disk and its Storage Spaces partition are found, the partition is used.
pub fn scan() -> BTreeMap<Guid, Vec<Candidate>> {
    let mut found: Vec<Candidate> = Vec::new();
    let Ok(entries) = fs::read_dir("/sys/class/block") else {
        return BTreeMap::new();
    };
    let mut names: Vec<String> = entries.filter_map(|e| e.ok()?.file_name().into_string().ok()).collect();
    names.sort();
    for name in names.into_iter().filter(|n| !skipped(n)) {
        if let Some(c) = probe(&Path::new("/dev").join(&name)) {
            found.push(c);
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
    pools
}
