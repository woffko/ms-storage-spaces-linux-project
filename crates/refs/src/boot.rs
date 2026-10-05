//! The boot sector (VBR) at the start of a ReFS volume.

use crate::checksum::boot_sum;
use crate::error::{Result, format_err};
use crate::util::{le16, le32, le64};

/// Bytes of the boot sector.
pub const BOOT_SECTOR_SIZE: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootSector {
    pub sectors: u64,
    pub bytes_per_sector: u32,
    pub sectors_per_cluster: u32,
    pub major: u8,
    pub minor: u8,
    /// The checksum algorithm the volume was formatted with (0 none, 2
    /// CRC64, 4 SHA-256); the checkpoint's page reference size decides
    /// what is on disk.
    pub checksum_kind: u16,
    pub flags: u32,
    pub serial: u64,
    /// 64 MiB on every volume known.
    pub container_size: u64,
}

impl BootSector {
    /// Whether `sector` (at least 512 bytes) looks like a ReFS boot sector.
    pub fn is_refs(sector: &[u8]) -> bool {
        sector.len() >= BOOT_SECTOR_SIZE && &sector[3..11] == b"ReFS\0\0\0\0" && &sector[0x10..0x14] == b"FSRS"
    }

    pub fn parse(sector: &[u8]) -> Result<Self> {
        if !Self::is_refs(sector) {
            return Err(format_err!("no ReFS boot sector"));
        }
        let stored = le16(sector, 0x16);
        if stored != boot_sum(sector) && !cfg!(fuzzing) {
            return Err(format_err!("boot sector checksum {stored:#06x} does not match"));
        }
        let boot = BootSector {
            sectors: le64(sector, 0x18),
            bytes_per_sector: le32(sector, 0x20),
            sectors_per_cluster: le32(sector, 0x24),
            major: sector[0x28],
            minor: sector[0x29],
            checksum_kind: le16(sector, 0x2a),
            flags: le32(sector, 0x2c),
            serial: le64(sector, 0x38),
            container_size: le64(sector, 0x40),
        };
        let cluster = boot.cluster_size();
        if !matches!(boot.bytes_per_sector, 512 | 4096) || !matches!(cluster, 4096 | 65536) {
            return Err(crate::Error::Unsupported(format!(
                "{} bytes per sector, {cluster}-byte clusters",
                boot.bytes_per_sector
            )));
        }
        match boot.major {
            // ReFS 1.x (Windows 8.1, Server 2012 R2): no containers.
            // (64 KiB clusters only: the blocks of 16 KiB its runs count
            // are the unit, and Windows formats no other.)
            1 if cluster == 65536 => {}
            1 => {
                return Err(crate::Error::Unsupported(format!(
                    "ReFS 1.x with {cluster}-byte clusters"
                )));
            }
            // 0 before 3.4 (3.1 of Server 2016: the container table tells).
            3 if boot.container_size.is_multiple_of(cluster) => {}
            3 => return Err(format_err!("container size {:#x}", boot.container_size)),
            _ => return Err(crate::Error::Unsupported(format!("ReFS {}.{}", boot.major, boot.minor))),
        }
        Ok(boot)
    }

    pub fn cluster_size(&self) -> u64 {
        self.bytes_per_sector as u64 * self.sectors_per_cluster as u64
    }

    /// Bytes of the volume.
    pub fn volume_size(&self) -> u64 {
        self.sectors.saturating_mul(self.bytes_per_sector as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The boot sector of a ReFS 1.2 volume with `sectors_per_cluster`,
    /// its checksum recomputed.
    fn sector_1x(sectors_per_cluster: u32) -> Vec<u8> {
        let mut s = vec![0u8; BOOT_SECTOR_SIZE];
        s[3..11].copy_from_slice(b"ReFS\0\0\0\0");
        s[0x10..0x14].copy_from_slice(b"FSRS");
        s[0x18..0x20].copy_from_slice(&(1u64 << 20).to_le_bytes());
        s[0x20..0x24].copy_from_slice(&512u32.to_le_bytes());
        s[0x24..0x28].copy_from_slice(&sectors_per_cluster.to_le_bytes());
        s[0x28] = 1;
        s[0x29] = 2;
        let sum = boot_sum(&s);
        s[0x16..0x18].copy_from_slice(&sum.to_le_bytes());
        s
    }

    #[test]
    fn refs_1x_has_clusters_of_64_kib_only() {
        // Its runs count blocks of 16 KiB: a cluster below that is no volume.
        assert!(BootSector::parse(&sector_1x(128)).is_ok());
        for spc in [1, 8, 16, 32] {
            let err = BootSector::parse(&sector_1x(spc)).unwrap_err();
            assert!(matches!(err, crate::Error::Unsupported(_)), "{spc}: {err}");
        }
    }
}
