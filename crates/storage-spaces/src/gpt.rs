//! Locating the Storage Spaces partition on a member disk.

use crate::error::Result;
use crate::format::SPACEDB_SIGNATURE;
use crate::guid::Guid;
use crate::io::{ReadAt, read_vec};

/// GPT partition type of a Storage Spaces pool member.
pub const STORAGE_SPACES_PARTITION_TYPE: &str = "e75caf8f-f680-4cee-afa3-b001e56efc2d";

/// Byte range of the Storage Spaces partition on a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PartitionLocation {
    pub offset: u64,
    pub length: u64,
}

/// Finds the Storage Spaces partition on a whole disk, or accepts a device
/// that is the partition itself (starts with the SPACEDB header).
pub fn find_spaces_partition<D: ReadAt + ?Sized>(dev: &D) -> Result<Option<PartitionLocation>> {
    let size = dev.size()?;
    if size >= 8 && read_vec(dev, 0, 8)? == SPACEDB_SIGNATURE {
        return Ok(Some(PartitionLocation {
            offset: 0,
            length: size,
        }));
    }
    for sector in [512u64, 4096] {
        if size < sector * 2 {
            continue;
        }
        let header = read_vec(dev, sector, 92)?;
        if &header[0..8] != b"EFI PART" {
            continue;
        }
        let entries_lba = u64::from_le_bytes(header[72..80].try_into().unwrap());
        let count = u32::from_le_bytes(header[80..84].try_into().unwrap()) as usize;
        let entry_size = u32::from_le_bytes(header[84..88].try_into().unwrap()) as usize;
        if !(128..=4096).contains(&entry_size) || count > 4096 {
            continue;
        }
        let table = read_vec(dev, entries_lba * sector, count * entry_size)?;
        for entry in table.chunks_exact(entry_size) {
            let type_guid = Guid::from_mixed_endian(entry[0..16].try_into().unwrap());
            if type_guid.to_string() != STORAGE_SPACES_PARTITION_TYPE {
                continue;
            }
            let first = u64::from_le_bytes(entry[32..40].try_into().unwrap());
            let last = u64::from_le_bytes(entry[40..48].try_into().unwrap());
            if last < first {
                continue;
            }
            return Ok(Some(PartitionLocation {
                offset: first * sector,
                length: (last - first + 1) * sector,
            }));
        }
        return Ok(None);
    }
    Ok(None)
}
