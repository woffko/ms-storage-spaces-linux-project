//! Metadata pages and the references that point at them.

use crate::checksum::{crc32c, crc64, sha256};
use crate::error::{Result, format_err};
use crate::util::{le32, le64};

/// Bytes of the header every metadata page (SUPB, CHKP, MSB+) starts with.
pub const PAGE_HEADER_SIZE: usize = 0x50;
/// ReFS 1.x: metadata pages are blocks of 16 KiB, numbered from the start
/// of the volume, whose 48-byte header starts with their own number.
pub const V1_BLOCK: u64 = 0x4000;
pub const V1_HEADER_SIZE: usize = 0x30;

/// A page reference: the (up to four) clusters of a page and the checksum
/// of the page's content. 48 bytes with CRC64, 72 with SHA-256, 104 on
/// volumes formatted before ReFS 3.10.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageRef {
    pub lcns: [u64; 4],
    /// 0 none, 1 CRC32-C, 2 CRC64, 4 SHA-256.
    pub checksum_kind: u8,
    pub checksum: Vec<u8>,
}

impl PageRef {
    pub fn parse(b: &[u8]) -> Result<Self> {
        if b.len() < 0x28 {
            return Err(format_err!("page reference of {} bytes", b.len()));
        }
        let kind = b[0x22];
        let offset = b[0x23] as usize;
        let len = le32(b, 0x24) as usize;
        let checksum = match kind {
            0 => Vec::new(),
            _ => b
                .get(0x20 + offset..0x20 + offset + len)
                .ok_or_else(|| format_err!("page reference checksum outside its {} bytes", b.len()))?
                .to_vec(),
        };
        Ok(PageRef {
            lcns: [le64(b, 0), le64(b, 8), le64(b, 16), le64(b, 24)],
            checksum_kind: kind,
            checksum,
        })
    }

    /// A ReFS 1.x reference (24 bytes): the block, then the checksum's
    /// descriptor (kind at 0x0a, its offset from 8 at 0x0b, length at
    /// 0x0c) and the checksum.
    pub fn parse_v1(b: &[u8]) -> Result<Self> {
        if b.len() < 0x10 {
            return Err(format_err!("page reference of {} bytes", b.len()));
        }
        let kind = b[0x0a];
        let offset = b[0x0b] as usize;
        let len = crate::util::le16(b, 0x0c) as usize;
        let checksum = match kind {
            0 => Vec::new(),
            _ => b
                .get(8 + offset..8 + offset + len)
                .ok_or_else(|| format_err!("page reference checksum outside its {} bytes", b.len()))?
                .to_vec(),
        };
        Ok(PageRef {
            lcns: [le64(b, 0), 0, 0, 0],
            checksum_kind: kind,
            checksum,
        })
    }

    /// Whether `page` (the whole page, all its clusters) has the checksum
    /// the reference records. A reference without a checksum accepts any
    /// page; an unknown kind none. (Fuzzing builds accept every page, so
    /// that mutated pages reach the parsers.)
    pub fn verifies(&self, page: &[u8]) -> bool {
        if cfg!(fuzzing) {
            return true;
        }
        match self.checksum_kind {
            0 => true,
            1 => self.checksum.len() == 4 && crc32c(page).to_le_bytes()[..] == self.checksum[..],
            2 => self.checksum.len() == 8 && crc64(page).to_le_bytes()[..] == self.checksum[..],
            4 => self.checksum.len() == 32 && sha256(page)[..] == self.checksum[..],
            _ => false,
        }
    }
}

/// Points the page reference in `buf` (which keeps its checksum kind and
/// layout) at `lcns` and stores the checksum of `page` in it.
pub fn store_reference(buf: &mut [u8], lcns: &[u64], page: &[u8]) -> Result<()> {
    if buf.len() < 0x28 || lcns.len() > 4 {
        return Err(format_err!("page reference of {} bytes", buf.len()));
    }
    for i in 0..4 {
        let lcn = lcns.get(i).copied().unwrap_or(0);
        buf[8 * i..8 * i + 8].copy_from_slice(&lcn.to_le_bytes());
    }
    let (kind, at, len) = (buf[0x22], 0x20 + buf[0x23] as usize, le32(buf, 0x24) as usize);
    let sum: Vec<u8> = match kind {
        0 => return Ok(()),
        1 => crc32c(page).to_le_bytes().to_vec(),
        2 => crc64(page).to_le_bytes().to_vec(),
        4 => sha256(page).to_vec(),
        _ => return Err(format_err!("checksum kind {kind}")),
    };
    if sum.len() != len {
        return Err(format_err!("checksum of {} bytes in a field of {len}", sum.len()));
    }
    buf.get_mut(at..at + len)
        .ok_or_else(|| format_err!("checksum outside the page reference"))?
        .copy_from_slice(&sum);
    Ok(())
}

/// The common header of a metadata page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageHeader {
    pub signature: [u8; 4],
    pub volume_signature: u32,
    pub virtual_clock: u64,
    pub tree_clock: u64,
    pub lcns: [u64; 4],
    /// The owning table (MSB+ pages only).
    pub table: u64,
}

impl PageHeader {
    pub fn parse(b: &[u8]) -> Result<Self> {
        if b.len() < PAGE_HEADER_SIZE {
            return Err(format_err!("metadata page of {} bytes", b.len()));
        }
        Ok(PageHeader {
            signature: b[0..4].try_into().unwrap(),
            volume_signature: le32(b, 0x0c),
            virtual_clock: le64(b, 0x10),
            tree_clock: le64(b, 0x18),
            lcns: [le64(b, 0x20), le64(b, 0x28), le64(b, 0x30), le64(b, 0x38)],
            table: le64(b, 0x48),
        })
    }
}
