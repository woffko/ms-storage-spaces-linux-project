//! Compressed data: ReFS compresses whole containers, not files. The
//! dedup engine (`Enable-ReFSDedup -Type DedupAndCompress`) compacts a
//! data container: its clusters in use, in order, become one stream cut
//! into units (64 KiB) that are compressed one by one and stored together
//! elsewhere. Files keep their extents; reading a cluster of a compacted
//! container means finding its place in that stream and decompressing the
//! unit that holds it.
//!
//! * The container table row: class 0xa (u32 at 0x14), the compression
//!   format at 0x30 (1 LZ4 blocks, 2 ZSTD frames, 3 LZ4 on QuickAssist
//!   hardware), the unit size at 0x34, and in place of the physical start
//!   the *virtual* cluster where the compressed bytes are, and their
//!   clusters.
//! * Root 10 rows keyed (container id u64, sequence u32, type u32): type 3
//!   from 0x30 a bitmap of the container's clusters kept (a cluster's place
//!   in the stream is the number of kept clusters before it); type 7 a
//!   range of the stream: 0x10 its start and 0x18 its length (bytes of the
//!   stream), 0x20 the start of its compressed bytes and 0x28 their length,
//!   0x30 flags (2: a checksum per unit), 0x34 the units, 0x38 the offset of
//!   a u32 per unit (where the unit's compressed bytes end), 0x3c the
//!   checksum kind (1: CRC32-C of the compressed bytes, a u32 per unit after
//!   the ends). A unit whose compressed bytes are not shorter than it is
//!   stored as it is.

use std::collections::BTreeMap;

use crate::checksum::crc32c;
use crate::error::{Error, Result, format_err};
use crate::util::{le16, le32, le64};

/// The container table's class of a compacted container.
pub(crate) const COMPACTED: u32 = 0xa;

/// A compacted container, as the container table describes it.
#[derive(Debug, Clone)]
pub(crate) struct Compacted {
    /// The virtual cluster where its compressed bytes start, and their
    /// clusters.
    pub(crate) data: u64,
    pub(crate) clusters: u64,
    pub(crate) format: u32,
    /// Bytes of the stream a unit holds.
    pub(crate) unit: u64,
    /// Its clusters kept (the stream's order) and the ranges of the stream.
    pub(crate) kept: Vec<u8>,
    pub(crate) ranges: Vec<Range>,
}

/// A range of a compacted container's stream.
#[derive(Debug, Clone)]
pub(crate) struct Range {
    pub(crate) start: u64,
    pub(crate) len: u64,
    pub(crate) packed: u64,
    /// Where each unit's compressed bytes end (from the container's
    /// compressed bytes' start), and its checksum.
    pub(crate) ends: Vec<u32>,
    pub(crate) sums: Option<Vec<u32>>,
}

impl Compacted {
    /// A container table row of class 0xa.
    pub(crate) fn from_row(v: &[u8]) -> Result<Self> {
        if v.len() < 0x48 {
            return Err(format_err!("compacted container row of {} bytes", v.len()));
        }
        Ok(Compacted {
            data: le64(v, v.len() - 16),
            clusters: le64(v, v.len() - 8),
            format: le32(v, 0x30),
            unit: u64::from(le32(v, 0x34)),
            kept: Vec::new(),
            ranges: Vec::new(),
        })
    }

    /// Adds a root 10 row of the container (types 3 and 7; others are not
    /// needed to read).
    pub(crate) fn add_row(&mut self, kind: u32, v: &[u8], clusters: u64) -> Result<()> {
        match kind {
            3 => {
                let bytes = clusters.div_ceil(8) as usize;
                self.kept = v
                    .get(0x30..0x30 + bytes)
                    .ok_or_else(|| format_err!("kept-cluster bitmap of {} bytes", v.len()))?
                    .to_vec();
            }
            7 => {
                if v.len() < 0x40 {
                    return Err(format_err!("compressed range row of {} bytes", v.len()));
                }
                let units = le32(v, 0x34) as usize;
                let at = le32(v, 0x38) as usize;
                let u32s = |from: usize| -> Result<Vec<u32>> {
                    let bytes = v
                        .get(from..from.saturating_add(4 * units))
                        .ok_or_else(|| format_err!("compressed range row of {} bytes", v.len()))?;
                    Ok(bytes
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .map(|c| u32::from_le_bytes(*c))
                        .collect())
                };
                let ends = u32s(at)?;
                let sums = match (le32(v, 0x30) & 2 != 0, le16(v, 0x3c)) {
                    (true, 1) => Some(u32s(at + 4 * units)?),
                    (true, kind) => return Err(Error::Unsupported(format!("unit checksums of kind {kind}"))),
                    (false, _) => None,
                };
                self.ranges.push(Range {
                    start: le64(v, 0x10),
                    len: le64(v, 0x18),
                    packed: le64(v, 0x20),
                    ends,
                    sums,
                });
                self.ranges.sort_by_key(|r| r.start);
            }
            _ => {}
        }
        Ok(())
    }

    /// Where cluster `c` of the container is in the stream (bytes), or
    /// None when the container did not keep it (it reads as zeros).
    pub(crate) fn place(&self, c: u64, cluster: u64) -> Option<u64> {
        let (byte, bit) = ((c / 8) as usize, c % 8);
        if self.kept.get(byte)? >> bit & 1 == 0 {
            return None;
        }
        let before: u64 = self.kept[..byte].iter().map(|b| u64::from(b.count_ones())).sum::<u64>()
            + u64::from((self.kept[byte] & ((1u8 << bit) - 1)).count_ones());
        Some(before * cluster)
    }

    /// The unit holding byte `at` of the stream: (range, unit, the unit's
    /// first byte in the stream).
    pub(crate) fn unit_of(&self, at: u64) -> Result<(usize, usize, u64)> {
        let i = self.ranges.partition_point(|r| r.start <= at);
        let r = i
            .checked_sub(1)
            .map(|i| (i, &self.ranges[i]))
            .filter(|(_, r)| at < r.start.saturating_add(r.len))
            .ok_or_else(|| format_err!("byte {at:#x} of a compacted container in no range"))?;
        if self.unit == 0 {
            return Err(format_err!("compressed units of 0 bytes"));
        }
        let u = (at - r.1.start) / self.unit;
        Ok((r.0, u as usize, r.1.start + u * self.unit))
    }

    /// The bounds of unit `u` of range `r` within the compressed bytes and
    /// the bytes it decompresses to.
    pub(crate) fn unit_bounds(&self, r: usize, u: usize) -> Result<(u64, u64, usize)> {
        let range = &self.ranges[r];
        let end = u64::from(
            *range
                .ends
                .get(u)
                .ok_or_else(|| format_err!("unit {u} of a range of {} units", range.ends.len()))?,
        );
        let start = match u {
            0 => range.packed,
            _ => u64::from(range.ends[u - 1]),
        };
        if end < start || end - start > 4 * self.unit {
            return Err(format_err!("compressed unit of bytes {start:#x}..{end:#x}"));
        }
        let size = self.unit.min(range.len - u as u64 * self.unit) as usize;
        Ok((start, end, size))
    }

    /// Unit `u` of range `r` from its compressed bytes.
    pub(crate) fn decode(&self, r: usize, u: usize, packed: &[u8], size: usize) -> Result<Vec<u8>> {
        if let Some(sums) = &self.ranges[r].sums
            && sums.get(u).is_some_and(|&s| s != crc32c(packed))
            && !cfg!(fuzzing)
        {
            return Err(Error::Checksum(format!("compressed unit {u} of a compacted container")));
        }
        if packed.len() >= size {
            return Ok(packed[..size].to_vec());
        }
        match self.format {
            1 | 3 => lz4_block(packed, size),
            2 => zstd_frame(packed, size),
            f => Err(Error::Unsupported(format!("compression format {f}"))),
        }
    }
}

/// An LZ4 block (no frame) that decompresses to exactly `size` bytes.
pub fn lz4_block(src: &[u8], size: usize) -> Result<Vec<u8>> {
    let bad = || format_err!("a damaged LZ4 block");
    let mut out: Vec<u8> = Vec::with_capacity(size);
    let mut i = 0;
    let length = |i: &mut usize, mut n: usize| -> Result<usize> {
        if n == 15 {
            loop {
                let b = *src.get(*i).ok_or_else(bad)?;
                *i += 1;
                n = n.checked_add(usize::from(b)).ok_or_else(bad)?;
                if b != 255 {
                    break;
                }
            }
        }
        Ok(n)
    };
    while i < src.len() {
        let token = src[i];
        i += 1;
        let literals = length(&mut i, usize::from(token >> 4))?;
        let end = i.checked_add(literals).filter(|&e| e <= src.len()).ok_or_else(bad)?;
        if out.len() + literals > size {
            return Err(bad());
        }
        out.extend_from_slice(&src[i..end]);
        i = end;
        if i == src.len() {
            break;
        }
        let offset = usize::from(*src.get(i).ok_or_else(bad)?) | usize::from(*src.get(i + 1).ok_or_else(bad)?) << 8;
        i += 2;
        let matched = length(&mut i, usize::from(token & 15))?
            .checked_add(4)
            .ok_or_else(bad)?;
        if offset == 0 || offset > out.len() || out.len() + matched > size {
            return Err(bad());
        }
        let from = out.len() - offset;
        for k in 0..matched {
            let b = out[from + k];
            out.push(b);
        }
    }
    if out.len() != size {
        return Err(format_err!("an LZ4 block of {} bytes, not {size}", out.len()));
    }
    Ok(out)
}

/// A ZSTD frame that decompresses to exactly `size` bytes (each unit of a
/// ZSTD-compressed container is one, with its content size).
pub fn zstd_frame(src: &[u8], size: usize) -> Result<Vec<u8>> {
    use std::io::Read;
    let bad = |e: &dyn std::fmt::Display| format_err!("a damaged ZSTD frame: {e}");
    // The frame header first: a window (what the decoder allocates) of at
    // most 8 MiB, and a content size, when it has one, of `size`.
    let header = src.get(..14).unwrap_or(src);
    if header.get(..4) != Some(&[0x28, 0xb5, 0x2f, 0xfd][..]) || header.len() < 6 {
        return Err(bad(&"no frame magic"));
    }
    let fhd = header[4];
    let single = fhd & 0x20 != 0;
    let mut at = 5;
    if !single {
        let wd = header[5];
        let base = 1u64 << (10 + u32::from(wd >> 3));
        if base + base / 8 * u64::from(wd & 7) > 8 << 20 {
            return Err(bad(&"a window over 8 MiB"));
        }
        at += 1;
    }
    at += [0, 1, 2, 4][usize::from(fhd & 3)];
    let fcs_len = match fhd >> 6 {
        0 if single => 1,
        0 => 0,
        1 => 2,
        2 => 4,
        _ => 8,
    };
    if fcs_len > 0 {
        let field = header
            .get(at..at + fcs_len)
            .ok_or_else(|| bad(&"a short frame header"))?;
        let mut fcs = field.iter().rev().fold(0u64, |a, &b| a << 8 | u64::from(b));
        if fcs_len == 2 {
            fcs += 256;
        }
        if fcs != size as u64 {
            return Err(bad(&format!("content of {fcs} bytes, not {size}")));
        }
    } else if single {
        return Err(bad(&"a single-segment frame without its size"));
    }
    let decoder = ruzstd::decoding::StreamingDecoder::new(src).map_err(|e| bad(&e))?;
    let mut out = Vec::with_capacity(size);
    decoder
        .take(size as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|e| bad(&e))?;
    if out.len() != size {
        return Err(format_err!("a ZSTD frame of {} bytes, not {size}", out.len()));
    }
    Ok(out)
}

/// The compacted containers of a volume, by id, with their root 10 rows.
pub(crate) type Map = BTreeMap<u64, Compacted>;
