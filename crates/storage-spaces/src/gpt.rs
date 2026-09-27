//! Partition tables: locating the Storage Spaces partition on member disks
//! and listing the partitions inside a space.
//!
//! Windows does not always write a protective MBR in front of the GPT of a
//! space (the kernel then ignores the GPT), so the GPT is read directly.

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

/// A partition found in a GPT or MBR partition table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Partition {
    /// 1-based partition number as the kernel would assign it.
    pub number: u32,
    pub offset: u64,
    pub length: u64,
    /// GPT type GUID, or the MBR type byte formatted as "mbr:0x07".
    pub kind: String,
    pub name: String,
}

/// Reads the GPT of a device with the given logical sector size.
pub fn read_gpt<D: ReadAt + ?Sized>(dev: &D, sector: u64) -> Result<Option<Vec<Partition>>> {
    let size = dev.size()?;
    if size < sector * 2 {
        return Ok(None);
    }
    let header = read_vec(dev, sector, 92)?;
    if &header[0..8] != b"EFI PART" {
        return Ok(None);
    }
    let entries_lba = u64::from_le_bytes(header[72..80].try_into().unwrap());
    let count = u32::from_le_bytes(header[80..84].try_into().unwrap()) as usize;
    let entry_size = u32::from_le_bytes(header[84..88].try_into().unwrap()) as usize;
    if !(128..=4096).contains(&entry_size) || count > 4096 {
        return Ok(None);
    }
    let table_start = entries_lba.checked_mul(sector).filter(|&s| {
        s.checked_add((count * entry_size) as u64)
            .is_some_and(|end| end <= size)
    });
    let Some(table_start) = table_start else {
        return Ok(None);
    };
    let table = read_vec(dev, table_start, count * entry_size)?;
    let mut parts = Vec::new();
    for (i, entry) in table.chunks_exact(entry_size).enumerate() {
        let type_guid = Guid::from_mixed_endian(entry[0..16].try_into().unwrap());
        if type_guid.is_nil() {
            continue;
        }
        let first = u64::from_le_bytes(entry[32..40].try_into().unwrap());
        let last = u64::from_le_bytes(entry[40..48].try_into().unwrap());
        if last < first
            || last
                .checked_add(1)
                .and_then(|n| n.checked_mul(sector))
                .is_none_or(|end| end > size)
        {
            continue;
        }
        let name: Vec<u16> = entry[56..128]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&c| u16::from_le_bytes(c))
            .collect();
        parts.push(Partition {
            number: i as u32 + 1,
            offset: first * sector,
            length: (last - first + 1) * sector,
            kind: type_guid.to_string(),
            name: String::from_utf16_lossy(&name).trim_end_matches('\0').to_string(),
        });
    }
    Ok(Some(parts))
}

/// Reads the primary entries of an MBR partition table (no extended partitions).
pub fn read_mbr<D: ReadAt + ?Sized>(dev: &D, sector: u64) -> Result<Option<Vec<Partition>>> {
    let size = dev.size()?;
    if size < 512 {
        return Ok(None);
    }
    let mbr = read_vec(dev, 0, 512)?;
    if mbr[510..512] != [0x55, 0xaa] {
        return Ok(None);
    }
    let mut parts = Vec::new();
    for i in 0..4 {
        let e = &mbr[446 + i * 16..462 + i * 16];
        let kind = e[4];
        let start = u32::from_le_bytes(e[8..12].try_into().unwrap()) as u64;
        let count = u32::from_le_bytes(e[12..16].try_into().unwrap()) as u64;
        if kind == 0 || count == 0 || kind == 0xee || (start + count) * sector > size {
            continue;
        }
        parts.push(Partition {
            number: i as u32 + 1,
            offset: start * sector,
            length: count * sector,
            kind: format!("mbr:{kind:#04x}"),
            name: String::new(),
        });
    }
    Ok((!parts.is_empty()).then_some(parts))
}

/// Lists the partitions of a device: GPT first, then MBR.
pub fn read_partitions<D: ReadAt + ?Sized>(dev: &D, sector: u64) -> Result<Vec<Partition>> {
    if let Some(parts) = read_gpt(dev, sector)? {
        return Ok(parts);
    }
    Ok(read_mbr(dev, sector)?.unwrap_or_default())
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
        if let Some(parts) = read_gpt(dev, sector)? {
            return Ok(parts
                .into_iter()
                .find(|p| p.kind == STORAGE_SPACES_PARTITION_TYPE)
                .map(|p| PartitionLocation {
                    offset: p.offset,
                    length: p.length,
                }));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::MemDevice;

    /// A 4 KiB-sector disk with a GPT but no protective MBR, as Windows
    /// writes it inside a space.
    fn gpt_4k() -> MemDevice {
        let sector = 4096usize;
        let mut d = vec![0u8; sector * 64];
        let h = &mut d[sector..sector + 92];
        h[0..8].copy_from_slice(b"EFI PART");
        h[72..80].copy_from_slice(&2u64.to_le_bytes());
        h[80..84].copy_from_slice(&128u32.to_le_bytes());
        h[84..88].copy_from_slice(&128u32.to_le_bytes());
        let e = &mut d[2 * sector..2 * sector + 128];
        // Microsoft basic data, mixed-endian
        e[0..16].copy_from_slice(&[
            0xa2, 0xa0, 0xd0, 0xeb, 0xe5, 0xb9, 0x33, 0x44, 0x87, 0xc0, 0x68, 0xb6, 0xb7, 0x26, 0x99, 0xc7,
        ]);
        e[32..40].copy_from_slice(&8u64.to_le_bytes());
        e[40..48].copy_from_slice(&39u64.to_le_bytes());
        for (i, c) in "data".encode_utf16().enumerate() {
            e[56 + 2 * i..58 + 2 * i].copy_from_slice(&c.to_le_bytes());
        }
        MemDevice(d)
    }

    #[test]
    fn reads_gpt_without_protective_mbr() {
        let parts = read_partitions(&gpt_4k(), 4096).unwrap();
        assert_eq!(
            parts,
            [Partition {
                number: 1,
                offset: 8 * 4096,
                length: 32 * 4096,
                kind: "ebd0a0a2-b9e5-4433-87c0-68b6b72699c7".into(),
                name: "data".into(),
            }]
        );
        // With the wrong sector size the table is not found.
        assert!(read_gpt(&gpt_4k(), 512).unwrap().is_none());
    }

    #[test]
    fn rejects_out_of_range_values() {
        // A table at the end of the address space.
        let mut d = gpt_4k();
        d.0[4096 + 72..4096 + 80].copy_from_slice(&(u64::MAX / 4096).to_le_bytes());
        assert!(read_gpt(&d, 4096).unwrap().is_none());
        // A partition ending at the largest LBA.
        let mut d = gpt_4k();
        d.0[2 * 4096 + 40..2 * 4096 + 48].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(read_gpt(&d, 4096).unwrap(), Some(Vec::new()));
    }

    #[test]
    fn reads_mbr() {
        let mut d = vec![0u8; 512 * 100];
        d[446 + 4] = 0x07;
        d[510] = 0x55;
        d[511] = 0xaa;
        d[446 + 8..446 + 12].copy_from_slice(&10u32.to_le_bytes());
        d[446 + 12..446 + 16].copy_from_slice(&50u32.to_le_bytes());
        let parts = read_partitions(&MemDevice(d), 512).unwrap();
        assert_eq!(
            (parts[0].offset, parts[0].length, parts[0].kind.as_str()),
            (5120, 25600, "mbr:0x07")
        );
    }
}
