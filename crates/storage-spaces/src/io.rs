//! Positional access to pool member devices: reads, and writes for the
//! spaces a pool opens for writing.

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

/// A device that also takes writes at absolute offsets.
pub trait WriteAt: ReadAt {
    /// Writes all of `buf` at `offset`.
    fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()>;

    /// Makes the writes so far durable.
    fn flush(&self) -> io::Result<()>;
}

impl WriteAt for File {
    #[cfg(unix)]
    fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        std::os::unix::fs::FileExt::write_all_at(self, buf, offset)
    }

    #[cfg(windows)]
    fn write_all_at(&self, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
        use std::os::windows::fs::FileExt;
        while !buf.is_empty() {
            match self.seek_write(buf, offset) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => {
                    buf = &buf[n..];
                    offset += n as u64;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn flush(&self) -> io::Result<()> {
        self.sync_data()
    }
}

impl<T: WriteAt + ?Sized> WriteAt for &T {
    fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        (**self).write_all_at(buf, offset)
    }
    fn flush(&self) -> io::Result<()> {
        (**self).flush()
    }
}

impl<T: WriteAt + ?Sized> WriteAt for Box<T> {
    fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        (**self).write_all_at(buf, offset)
    }
    fn flush(&self) -> io::Result<()> {
        (**self).flush()
    }
}

impl<T: WriteAt + ?Sized> WriteAt for Arc<T> {
    fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        (**self).write_all_at(buf, offset)
    }
    fn flush(&self) -> io::Result<()> {
        (**self).flush()
    }
}

/// A writable view of a device that keeps every write in memory (4 KiB
/// pages over the device's content): for tests and for trying writes on a
/// pool without touching its disks. The device itself is never written.
pub struct Overlay<D> {
    base: D,
    pages: std::sync::Mutex<std::collections::BTreeMap<u64, Box<[u8; OVERLAY_PAGE]>>>,
    flushes: std::sync::atomic::AtomicUsize,
}

const OVERLAY_PAGE: usize = 4096;

impl<D: ReadAt> Overlay<D> {
    pub fn new(base: D) -> Self {
        Overlay {
            base,
            pages: Default::default(),
            flushes: Default::default(),
        }
    }

    /// Byte offsets of the pages written so far.
    pub fn written_pages(&self) -> Vec<u64> {
        self.pages
            .lock()
            .unwrap()
            .keys()
            .map(|p| p * OVERLAY_PAGE as u64)
            .collect()
    }

    /// Number of flushes so far.
    pub fn flushes(&self) -> usize {
        self.flushes.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl<D: ReadAt> ReadAt for Overlay<D> {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or(io::ErrorKind::UnexpectedEof)?;
        self.base.read_exact_at(buf, offset)?;
        let pages = self.pages.lock().unwrap();
        let page = OVERLAY_PAGE as u64;
        for (&p, data) in pages.range(offset / page..end.div_ceil(page)) {
            let (start, stop) = ((p * page).max(offset), ((p + 1) * page).min(end));
            buf[(start - offset) as usize..(stop - offset) as usize]
                .copy_from_slice(&data[(start - p * page) as usize..(stop - p * page) as usize]);
        }
        Ok(())
    }

    fn size(&self) -> io::Result<u64> {
        self.base.size()
    }
}

impl<D: ReadAt> WriteAt for Overlay<D> {
    fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        let end = offset
            .checked_add(buf.len() as u64)
            .filter(|&e| e <= self.base.size().unwrap_or(0))
            .ok_or(io::ErrorKind::WriteZero)?;
        let page = OVERLAY_PAGE as u64;
        for p in offset / page..end.div_ceil(page) {
            let mut current = Box::new([0u8; OVERLAY_PAGE]);
            self.read_exact_at(&mut current[..], p * page)?;
            let (start, stop) = ((p * page).max(offset), ((p + 1) * page).min(end));
            current[(start - p * page) as usize..(stop - p * page) as usize]
                .copy_from_slice(&buf[(start - offset) as usize..(stop - offset) as usize]);
            self.pages.lock().unwrap().insert(p, current);
        }
        Ok(())
    }

    fn flush(&self) -> io::Result<()> {
        self.flushes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }
}

/// One write or flush that reached a member device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceEvent {
    Write { device: usize, offset: u64, data: Vec<u8> },
    Flush { device: usize },
}

/// A device that records its writes and flushes, in one order across all
/// the devices sharing `log` (for replaying every state a crash could
/// leave).
pub struct Recorder<D> {
    inner: D,
    device: usize,
    log: Arc<std::sync::Mutex<Vec<DeviceEvent>>>,
}

impl<D> Recorder<D> {
    pub fn new(inner: D, device: usize, log: Arc<std::sync::Mutex<Vec<DeviceEvent>>>) -> Self {
        Recorder { inner, device, log }
    }
}

impl<D: ReadAt> ReadAt for Recorder<D> {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.inner.read_exact_at(buf, offset)
    }
    fn size(&self) -> io::Result<u64> {
        self.inner.size()
    }
}

impl<D: WriteAt> WriteAt for Recorder<D> {
    fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        self.log.lock().unwrap().push(DeviceEvent::Write {
            device: self.device,
            offset,
            data: buf.to_vec(),
        });
        self.inner.write_all_at(buf, offset)
    }
    fn flush(&self) -> io::Result<()> {
        self.log
            .lock()
            .unwrap()
            .push(DeviceEvent::Flush { device: self.device });
        self.inner.flush()
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
    fn overlay_keeps_writes_in_memory() {
        let base = MemDevice(vec![1u8; 3 * 4096]);
        let o = Overlay::new(&base);
        o.write_all_at(&[9; 5000], 4000).unwrap();
        let mut buf = vec![0u8; 3 * 4096];
        o.read_exact_at(&mut buf, 0).unwrap();
        assert!(buf[..4000].iter().all(|&b| b == 1));
        assert!(buf[4000..9000].iter().all(|&b| b == 9));
        assert!(buf[9000..].iter().all(|&b| b == 1));
        assert_eq!(o.written_pages(), [0, 4096, 8192]);
        assert!(base.0.iter().all(|&b| b == 1), "the device itself is never written");
        assert!(o.write_all_at(&[0; 2], 3 * 4096 - 1).is_err());
    }

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
