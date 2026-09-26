//! Positional read access to pool member devices.

use std::fs::File;
use std::io::{self, Seek, SeekFrom};
use std::sync::Arc;

/// A device or image that supports reads at absolute offsets.
pub trait ReadAt: Send + Sync {
    /// Fills `buf` completely from `offset`, failing on a short read.
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()>;

    /// Total size in bytes.
    fn size(&self) -> io::Result<u64>;
}

impl ReadAt for File {
    #[cfg(unix)]
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        std::os::unix::fs::FileExt::read_exact_at(self, buf, offset)
    }

    #[cfg(windows)]
    fn read_exact_at(&self, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
        use std::os::windows::fs::FileExt;
        while !buf.is_empty() {
            match self.seek_read(buf, offset) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => {
                    buf = &mut buf[n..];
                    offset += n as u64;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn size(&self) -> io::Result<u64> {
        // Block devices report a zero length in metadata, seeking works for both.
        let mut file = self;
        file.seek(SeekFrom::End(0))
    }
}

/// An in-memory device, mainly for tests.
#[derive(Debug, Clone, Default)]
pub struct MemDevice(pub Vec<u8>);

impl ReadAt for MemDevice {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        let start = usize::try_from(offset).map_err(|_| io::ErrorKind::UnexpectedEof)?;
        let end = start.checked_add(buf.len()).ok_or(io::ErrorKind::UnexpectedEof)?;
        let src = self.0.get(start..end).ok_or(io::ErrorKind::UnexpectedEof)?;
        buf.copy_from_slice(src);
        Ok(())
    }

    fn size(&self) -> io::Result<u64> {
        Ok(self.0.len() as u64)
    }
}

impl<T: ReadAt + ?Sized> ReadAt for &T {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        (**self).read_exact_at(buf, offset)
    }
    fn size(&self) -> io::Result<u64> {
        (**self).size()
    }
}

impl<T: ReadAt + ?Sized> ReadAt for Box<T> {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        (**self).read_exact_at(buf, offset)
    }
    fn size(&self) -> io::Result<u64> {
        (**self).size()
    }
}

impl<T: ReadAt + ?Sized> ReadAt for Arc<T> {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        (**self).read_exact_at(buf, offset)
    }
    fn size(&self) -> io::Result<u64> {
        (**self).size()
    }
}

/// Reads `len` bytes at `offset` into a new buffer.
pub(crate) fn read_vec<D: ReadAt + ?Sized>(dev: &D, offset: u64, len: usize) -> io::Result<Vec<u8>> {
    let mut buf = vec![0; len];
    dev.read_exact_at(&mut buf, offset)?;
    Ok(buf)
}

/// Wraps a device and records every range read from it (used to capture
/// the metadata a pool needs into a small fixture).
pub struct Recording<D> {
    inner: D,
    reads: std::sync::Mutex<Vec<(u64, usize)>>,
}

impl<D: ReadAt> Recording<D> {
    pub fn new(inner: D) -> Self {
        Recording {
            inner,
            reads: Default::default(),
        }
    }

    /// Recorded ranges as (offset, length), in read order.
    pub fn reads(&self) -> Vec<(u64, usize)> {
        self.reads.lock().unwrap().clone()
    }

    pub fn inner(&self) -> &D {
        &self.inner
    }
}

impl<D: ReadAt> ReadAt for Recording<D> {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.reads.lock().unwrap().push((offset, buf.len()));
        self.inner.read_exact_at(buf, offset)
    }

    fn size(&self) -> io::Result<u64> {
        self.inner.size()
    }
}

/// A sparse in-memory device: stored ranges hold data, everything else
/// reads as zeros. Serialized as "SSFIXT01", u64 size, u32 count and
/// `count` × (u64 offset, u32 length, data), little-endian.
#[derive(Debug, Clone, Default)]
pub struct SparseImage {
    pub size: u64,
    ranges: std::collections::BTreeMap<u64, Vec<u8>>,
}

const SPARSE_MAGIC: &[u8; 8] = b"SSFIXT01";

impl SparseImage {
    pub fn new(size: u64) -> Self {
        SparseImage {
            size,
            ranges: Default::default(),
        }
    }

    /// Stores `data` at `offset`, merging with overlapping or adjacent ranges.
    pub fn insert(&mut self, offset: u64, data: &[u8]) {
        let mut start = offset;
        let mut bytes = data.to_vec();
        let overlapping: Vec<u64> = self
            .ranges
            .range(..=offset + data.len() as u64)
            .filter(|(o, d)| **o + d.len() as u64 >= offset)
            .map(|(o, _)| *o)
            .collect();
        for o in overlapping {
            let old = self.ranges.remove(&o).unwrap();
            let new_start = start.min(o);
            let new_end = (start + bytes.len() as u64).max(o + old.len() as u64);
            let mut merged = vec![0u8; (new_end - new_start) as usize];
            merged[(o - new_start) as usize..][..old.len()].copy_from_slice(&old);
            merged[(start - new_start) as usize..][..bytes.len()].copy_from_slice(&bytes);
            start = new_start;
            bytes = merged;
        }
        self.ranges.insert(start, bytes);
    }

    /// Stored ranges as (offset, length).
    pub fn ranges(&self) -> Vec<(u64, usize)> {
        self.ranges.iter().map(|(o, d)| (*o, d.len())).collect()
    }

    /// Stored bytes (without holes).
    pub fn stored(&self) -> usize {
        self.ranges.values().map(Vec::len).sum()
    }

    pub fn write_to<W: io::Write>(&self, mut w: W) -> io::Result<()> {
        w.write_all(SPARSE_MAGIC)?;
        w.write_all(&self.size.to_le_bytes())?;
        w.write_all(&(self.ranges.len() as u32).to_le_bytes())?;
        for (offset, data) in &self.ranges {
            w.write_all(&offset.to_le_bytes())?;
            w.write_all(&(data.len() as u32).to_le_bytes())?;
            w.write_all(data)?;
        }
        Ok(())
    }

    pub fn read_from<R: io::Read>(mut r: R) -> io::Result<Self> {
        let invalid = |m: &str| io::Error::new(io::ErrorKind::InvalidData, m.to_string());
        let mut head = [0u8; 20];
        r.read_exact(&mut head)?;
        if &head[0..8] != SPARSE_MAGIC {
            return Err(invalid("not a sparse image"));
        }
        let size = u64::from_le_bytes(head[8..16].try_into().unwrap());
        let count = u32::from_le_bytes(head[16..20].try_into().unwrap());
        let mut image = SparseImage::new(size);
        for _ in 0..count {
            let mut e = [0u8; 12];
            r.read_exact(&mut e)?;
            let offset = u64::from_le_bytes(e[0..8].try_into().unwrap());
            let len = u32::from_le_bytes(e[8..12].try_into().unwrap()) as usize;
            if len > 1 << 30 || offset.saturating_add(len as u64) > size {
                return Err(invalid("range outside the image"));
            }
            let mut data = vec![0u8; len];
            r.read_exact(&mut data)?;
            image.ranges.insert(offset, data);
        }
        Ok(image)
    }
}

impl ReadAt for SparseImage {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        let end = offset.checked_add(buf.len() as u64).filter(|&e| e <= self.size);
        let end = end.ok_or(io::ErrorKind::UnexpectedEof)?;
        buf.fill(0);
        let first = self
            .ranges
            .range(..=offset)
            .next_back()
            .map(|(o, _)| *o)
            .unwrap_or(offset);
        for (o, data) in self.ranges.range(first..end) {
            let (o, e) = (*o, *o + data.len() as u64);
            if e <= offset {
                continue;
            }
            let from = offset.max(o);
            let to = end.min(e);
            buf[(from - offset) as usize..(to - offset) as usize]
                .copy_from_slice(&data[(from - o) as usize..(to - o) as usize]);
        }
        Ok(())
    }

    fn size(&self) -> io::Result<u64> {
        Ok(self.size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparse_image_round_trip() {
        let mut img = SparseImage::new(1 << 20);
        img.insert(100, &[1, 2, 3]);
        img.insert(102, &[9, 9]); // overlaps and extends
        img.insert(4096, &[7; 10]);
        let mut buf = [0u8; 8];
        img.read_exact_at(&mut buf, 98).unwrap();
        assert_eq!(buf, [0, 0, 1, 2, 9, 9, 0, 0]);
        let mut bytes = Vec::new();
        img.write_to(&mut bytes).unwrap();
        let back = SparseImage::read_from(&bytes[..]).unwrap();
        assert_eq!(back.stored(), 4 + 10);
        let mut buf = [0u8; 12];
        back.read_exact_at(&mut buf, 4094).unwrap();
        assert_eq!(buf, [0, 0, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7]);
        assert!(back.read_exact_at(&mut buf, (1 << 20) - 4).is_err());
    }
}
